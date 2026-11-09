//! net — servizio di rete nativo (ADR-0069, N2+N3).
//!
//! Registra `Service::Net`, si attacca al driver `netdev` (`NET_ATTACH`),
//! drena il ring RX, passa i frame al core `net` e gestisce endpoint a
//! capability per i client (`NET_EP_OPEN/TX/CLOSE/STATS`).
//! IPv6-first, core puro in `libs/net`, niente syscall di rete nel kernel.
//! Il cliente scrive solo payload applicativo nello slot TX; `net` incapsula
//! l'header TCP/UDP, costruisce i frame Ethernet e gestisce sequenze/ack.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use civis;
use net::{Config, Event, FrameSink, Ipv6Addr, MacAddr, Net, NEXT_TCP};
use net::tcp;
use net::ipv6;
use net::eth::{self, ETHERTYPE_IPV6};

// ── Costanti endpoint (da civis/syscall-numbers) ───────────────────
use civis::{
    NET_EP_CLOSE, NET_EP_FLAG_CONNECT, NET_EP_FLAG_EPHEMERAL, NET_EP_FLAG_LISTEN, NET_EP_OPEN,
    NET_EP_RX_RING_OFF, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_STATS,
    NET_EP_TX, NET_EP_TX_RING_OFF, NET_PROTO_TCP, NET_PROTO_UDP,
};

static FUTEX: u32 = 0;

/// Stato di un endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EpState {
    Free,
    Bound,
}

/// Numero massimo di endpoint concorrenti.
const MAX_ENDPOINTS: usize = 8;

/// Stati TCP semplificati (N3).
const TCP_CLOSED: u8 = 0;
const TCP_LISTEN: u8 = 1;
const TCP_SYN_SENT: u8 = 2;
const TCP_SYN_RECV: u8 = 3;
const TCP_ESTABLISHED: u8 = 4;

/// Eventi trasportati nello slot ring per endpoint TCP.
const EV_DATA: u8 = 0;   // payload applicativo
const EV_SYN: u8 = 1;    // handshake SYN (TX cliente → servizio)
const EV_SYNACK: u8 = 2; // handshake SYN+ACK (RX cliente, consegnato da net)
const EV_FIN: u8 = 3;    // fine connessione
const EV_RST: u8 = 4;    // reset connessione

/// Tabella endpoint.
#[derive(Clone, Copy)]
struct Endpoint {
    state: EpState,
    ep_id: u16,
    local_port: u16,
    remote_addr: Ipv6Addr,
    remote_port: u16,
    ring_id: u32,
    ring_base: *mut u8,
    peer_chan: u64,
    peer_hash: u64,
    rx_pkts: u64,
    tx_pkts: u64,
    // Stato TCP (N3).
    tcp_state: u8,
    tcp_seq: u32,   // prossimo numero di sequenza da inviare
    tcp_ack: u32,   // prossimo numero di sequenza atteso
}

impl Endpoint {
    const fn free() -> Self {
        Self {
            state: EpState::Free,
            ep_id: 0,
            local_port: 0,
            remote_addr: Ipv6Addr::UNSPECIFIED,
            remote_port: 0,
            ring_id: 0,
            ring_base: core::ptr::null_mut(),
            peer_chan: 0,
            peer_hash: 0,
            rx_pkts: 0,
            tx_pkts: 0,
            tcp_state: TCP_CLOSED,
            tcp_seq: 0,
            tcp_ack: 0,
        }
    }
}

/// Stato del servizio.
pub struct NetService {
    pub net: Net,
    pub sink_ring: *mut u8,        // ring driver↔servizio (N1)
    pub driver_chan: u64,          // canale verso netdev
    pub endpoints: [Endpoint; MAX_ENDPOINTS],
    pub next_ep_id: u16,
    pub ephemeral_port: u16,
    pub policy_allow_wk: bool,     // consente bind porte ben note (config)
}

impl NetService {
    pub fn new() -> Self {
        Self {
            net: Net::new(Config {
                mac: MacAddr::default(),
                ra_accept: true,
                ra_router: None,
                ping: true,
                ping_target: None,
                seed: civis::rdtsc(),
            }),
            sink_ring: core::ptr::null_mut(),
            driver_chan: 0,
            endpoints: [Endpoint::free(); MAX_ENDPOINTS],
            next_ep_id: 1,
            ephemeral_port: 49152,
            policy_allow_wk: false,
        }
    }

    /// Alloca un nuovo endpoint.
    pub fn alloc_endpoint(&mut self, peer_chan: u64, peer_hash: u64) -> Option<usize> {
        for i in 0..MAX_ENDPOINTS {
            if self.endpoints[i].state == EpState::Free {
                let ep_id = self.next_ep_id;
                self.next_ep_id = self.next_ep_id.wrapping_add(1);
                if self.next_ep_id == 0 {
                    self.next_ep_id = 1;
                }
                self.endpoints[i] = Endpoint {
                    state: EpState::Bound,
                    ep_id,
                    local_port: 0,
                    remote_addr: Ipv6Addr::UNSPECIFIED,
                    remote_port: 0,
                    ring_id: 0,
                    ring_base: core::ptr::null_mut(),
                    peer_chan,
                    peer_hash,
                    rx_pkts: 0,
                    tx_pkts: 0,
                    tcp_state: TCP_CLOSED,
                    tcp_seq: 0,
                    tcp_ack: 0,
                };
                return Some(i);
            }
        }
        None
    }

    /// Libera un endpoint.
    pub fn free_endpoint(&mut self, idx: usize) {
        let ep = self.endpoints[idx];
        if ep.ring_id != 0 && !ep.ring_base.is_null() {
            let ring_len = NET_EP_RING_HDR * 2
                + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2;
            let _ = civis::munmap(ep.ring_base as usize, ring_len);
        }
        self.endpoints[idx] = Endpoint::free();
    }

    /// Trova endpoint per ID.
    pub fn find_by_id(&self, ep_id: u16) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|e| e.state != EpState::Free && e.ep_id == ep_id)
    }

    /// Trova endpoint per ID con riferimento mutabile.
    pub fn find_by_id_mut(&mut self, ep_id: u16) -> Option<&mut Endpoint> {
        self.endpoints
            .iter_mut()
            .find(|e| e.state != EpState::Free && e.ep_id == ep_id)
    }

    /// Trova endpoint per porta locale (per delivery RX).
    pub fn find_by_local_port(&self, port: u16) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|e| e.state == EpState::Bound && e.local_port == port)
    }

    /// Layout di uno slot ring (N2+N3):
    /// `[len:u16][src_addr:16][src_port:2][dst_port:2][event:1][payload]`.
    /// Per UDP `event` è sempre 0. Per TCP indica il tipo di segmento/controllo.
    const SLOT_META: usize = 23; // 2+16+2+2+1

    /// Scrive uno slot nel ring RX (service → client) dato un ring base.
    fn ep_rx_write_full(
        ring: *mut u8,
        src_addr: &Ipv6Addr,
        src_port: u16,
        dst_port: u16,
        event: u8,
        payload: &[u8],
    ) -> bool {
        let total = Self::SLOT_META + payload.len();
        if total > NET_EP_SLOT_SIZE {
            return false;
        }
        let head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
        let tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        if tail.wrapping_sub(head) >= NET_EP_RING_SLOTS as u32 {
            return false;
        }
        let slot = (tail % NET_EP_RING_SLOTS as u32) as usize;
        let p = unsafe {
            ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
        };
        unsafe {
            write_volatile(p as *mut u16, total as u16);
            core::ptr::copy_nonoverlapping(src_addr.0.as_ptr(), p.add(2), 16);
            write_volatile(p.add(18) as *mut u16, src_port);
            write_volatile(p.add(20) as *mut u16, dst_port);
            write_volatile(p.add(22) as *mut u8, event);
            core::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(23), payload.len());
        }
        fence(Ordering::Release);
        unsafe {
            write_volatile(
                ring.add(NET_EP_RX_RING_OFF + 4) as *mut u32,
                tail.wrapping_add(1),
            )
        };
        true
    }

    /// Legge uno slot dal ring TX (client → service) dato un ring base.
    /// Ritorna (payload_len, dst_addr, dst_port, event).
    fn ep_tx_read_full(
        ring: *mut u8,
        slot_idx: usize,
        buf: &mut [u8],
    ) -> Option<(usize, Ipv6Addr, u16, u8)> {
        if slot_idx >= NET_EP_RING_SLOTS {
            return None;
        }
        let p = unsafe {
            ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot_idx * NET_EP_SLOT_SIZE)
        };
        let total = unsafe { read_volatile(p as *const u16) } as usize;
        if total < Self::SLOT_META || total > NET_EP_SLOT_SIZE {
            return None;
        }
        let mut addr = [0u8; 16];
        unsafe {
            core::ptr::copy_nonoverlapping(p.add(2), addr.as_mut_ptr(), 16);
        }
        let dst_port = unsafe { read_volatile(p.add(18) as *const u16) };
        let event = unsafe { read_volatile(p.add(22) as *const u8) };
        let payload_len = total - Self::SLOT_META;
        if payload_len > buf.len() {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(p.add(23), buf.as_mut_ptr(), payload_len);
        }
        Some((payload_len, Ipv6Addr(addr), dst_port, event))
    }

    /// Avanza la tail del TX ring (slot consumato) dato un ring base.
    fn ep_tx_consume_raw(ring: *mut u8) {
        let head = unsafe { read_volatile(ring.add(NET_EP_TX_RING_OFF) as *const u32) };
        unsafe {
            write_volatile(
                ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
                head.wrapping_add(1),
            )
        };
    }

    /// Carica la policy da `sys:net.conf`.
    pub fn load_policy(&mut self) {
        let Ok(data) = civis::obj_get(b"sys", b"net.conf") else {
            return;
        };
        let txt = core::str::from_utf8(&data).unwrap_or("");
        for line in txt.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }
            let _hash = u64::from_str_radix(parts[0], 16).unwrap_or(0);
            let _port: u16 = parts[1].parse().unwrap_or(0);
            self.policy_allow_wk = true;
        }
    }

    /// Verifica se un peer può bindare `port`.
    pub fn can_bind(&self, _peer_hash: u64, port: u16) -> bool {
        if port >= 49152 {
            return true;
        }
        self.policy_allow_wk
    }
}

// ── Sink driver (frame Ethernet verso netdev) ──────────────────────
struct DriverSink {
    ring: *mut u8,
}

impl FrameSink for DriverSink {
    fn send(&mut self, frame: &[u8]) {
        if frame.len() > civis::NET_SLOT_SIZE - 2 {
            return;
        }
        let head = unsafe {
            read_volatile(self.ring.add(civis::NET_TX_RING_OFF) as *const u32)
        };
        let tail = unsafe {
            read_volatile(self.ring.add(civis::NET_TX_RING_OFF + 4) as *const u32)
        };
        if tail.wrapping_sub(head) >= civis::NET_RING_SLOTS as u32 {
            return;
        }
        let slot = (tail % civis::NET_RING_SLOTS as u32) as usize;
        let p = unsafe {
            self.ring.add(
                civis::NET_TX_RING_OFF + civis::NET_RING_HDR + slot * civis::NET_SLOT_SIZE,
            )
        };
        unsafe {
            write_volatile(p as *mut u16, frame.len() as u16);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), p.add(2), frame.len());
        }
        fence(Ordering::Release);
        unsafe {
            write_volatile(
                self.ring.add(civis::NET_TX_RING_OFF + 4) as *mut u32,
                tail.wrapping_add(1),
            )
        };
    }

    fn deliver(&mut self, _src: &Ipv6Addr, _dst: &Ipv6Addr, _sp: u16, _dp: u16, _pl: &[u8]) {
        // Non usato: la delivery locale passa per EpSink.
    }
}

// ── Sink endpoint (consegna locale nel ring RX) ────────────────────
struct EpSink<'a> {
    driver_ring: *mut u8,
    endpoints: &'a mut [Endpoint; MAX_ENDPOINTS],
}

impl<'a> FrameSink for EpSink<'a> {
    fn send(&mut self, frame: &[u8]) {
        if frame.len() > civis::NET_SLOT_SIZE - 2 {
            return;
        }
        let head = unsafe {
            read_volatile(self.driver_ring.add(civis::NET_TX_RING_OFF) as *const u32)
        };
        let tail = unsafe {
            read_volatile(self.driver_ring.add(civis::NET_TX_RING_OFF + 4) as *const u32)
        };
        if tail.wrapping_sub(head) >= civis::NET_RING_SLOTS as u32 {
            return;
        }
        let slot = (tail % civis::NET_RING_SLOTS as u32) as usize;
        let p = unsafe {
            self.driver_ring.add(
                civis::NET_TX_RING_OFF + civis::NET_RING_HDR + slot * civis::NET_SLOT_SIZE,
            )
        };
        unsafe {
            write_volatile(p as *mut u16, frame.len() as u16);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), p.add(2), frame.len());
        }
        fence(Ordering::Release);
        unsafe {
            write_volatile(
                self.driver_ring.add(civis::NET_TX_RING_OFF + 4) as *mut u32,
                tail.wrapping_add(1),
            )
        };
    }

    fn deliver(
        &mut self,
        src: &Ipv6Addr,
        dst: &Ipv6Addr,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) {
        // Consegna diretta nel ring RX dell'endpoint in ascolto su dst_port.
        // Il cliente legge payload + evento dal ring RX.
        let idx = match self
            .endpoints
            .iter()
            .position(|e| e.state == EpState::Bound && e.local_port == dst_port)
        {
            Some(i) => i,
            None => return,
        };
        let ep = &self.endpoints[idx];
        if ep.remote_port != 0 && ep.remote_port != src_port {
            return;
        }
        if !ep.remote_addr.is_unspecified() && ep.remote_addr != *src {
            return;
        }
        let ring = ep.ring_base;
        let peer_chan = ep.peer_chan;
        let ep_id = ep.ep_id;
        let _ = ep;
        if NetService::ep_rx_write_full(ring, src, src_port, dst_port, EV_DATA, payload) {
            if peer_chan != 0 {
                let _ = civis::send_async(peer_chan, NET_EP_TX, ep_id as u64, 0);
            }
        }
    }
}

/// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    if civis::service_register(civis::Service::Net).is_ok() {
        let _ = civis::print_string(b"[net] registered as service Net\n");
    }

    let mut svc = NetService::new();
    svc.load_policy();

    // Risolve il driver.
    let mut driver = 0u64;
    for _ in 0..100 {
        if let Ok(c) = civis::service_lookup(civis::Service::NetDev) {
            if c > 0 {
                driver = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if driver == 0 {
        let _ = civis::print_string(b"[net] netdev non trovato\n");
        civis::exit(1);
    }
    svc.driver_chan = driver;

    // Attach al driver.
    let r = match civis::send(driver, civis::NET_ATTACH, 1, 0) {
        Ok(r) => r,
        Err(_) => {
            let _ = civis::print_string(b"[net] attach fallito\n");
            civis::exit(1);
        }
    };
    if r.w0 == !0u64 {
        let _ = civis::print_string(b"[net] attach rifiutato\n");
        civis::exit(1);
    }
    let mac = MacAddr::from_u64(r.w0);
    let ring_id = r.w1 as u32;
    let ring_base = match civis::shm_map(
        ring_id,
        0,
        civis::PROT_READ | civis::PROT_WRITE,
    ) {
        Ok(b) => b,
        Err(_) => {
            let _ = civis::print_string(b"[net] shm map fallita\n");
            civis::exit(1);
        }
    };
    svc.sink_ring = ring_base as *mut u8;
    svc.net = Net::new(Config {
        mac,
        ra_accept: true,
        ra_router: None,
        ping: true,
        ping_target: None,
        seed: civis::rdtsc(),
    });

    let link = match civis::send(driver, civis::NET_INFO, 0, 0) {
        Ok(r) => r.w0 != 0,
        Err(_) => false,
    };

    let now = civis::get_ticks() as u64;
    if link {
        let mut sink = DriverSink { ring: svc.sink_ring };
        svc.net.link_up(now, &mut sink, &mut |e| log_event(e));
    }
    civis::signal_ready(1);

    loop {
        let now = civis::get_ticks() as u64;

        // Client: richieste endpoint (NET_EP_*).
        while let Some(m) = civis::recv_poll() {
            handle_client(&mut svc, m);
        }

        // Poll del driver.
        let _ = civis::send(driver, civis::NET_POLL, 0, 0);

        // Drena il ring RX del driver.
        let head = unsafe {
            read_volatile(svc.sink_ring.add(civis::NET_RX_RING_OFF) as *const u32)
        };
        let tail = unsafe {
            read_volatile(svc.sink_ring.add(civis::NET_RX_RING_OFF + 4) as *const u32)
        };
        let mut h = head;
        while h != tail {
            let slot = (h % civis::NET_RING_SLOTS as u32) as usize;
            let p = unsafe {
                svc.sink_ring.add(
                    civis::NET_RX_RING_OFF + civis::NET_RING_HDR + slot * civis::NET_SLOT_SIZE,
                )
            };
            let len = unsafe { read_volatile(p as *const u16) } as usize;
            if len > 0 && len <= civis::NET_SLOT_SIZE - 2 {
                let frame = unsafe { core::slice::from_raw_parts(p.add(2), len) };
                let mut epsink = EpSink {
                    driver_ring: svc.sink_ring,
                    endpoints: &mut svc.endpoints,
                };
                svc.net.on_frame(frame, now, &mut epsink, &mut |e| log_event(e));
            }
            h = h.wrapping_add(1);
        }
        unsafe {
            write_volatile(
                svc.sink_ring.add(civis::NET_RX_RING_OFF) as *mut u32,
                h,
            )
        }

        // Timer.
        let mut sink = DriverSink { ring: svc.sink_ring };
        svc.net.on_tick(now, &mut sink, &mut |e| log_event(e));

        // TX: il driver consuma il ring.
        let _ = civis::send(driver, civis::NET_TX, 0, 0);

        // Dorme un tick.
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (now + 1) as u64,
        );
    }
}

/// Gestisce un messaggio client (controllo endpoint).
fn handle_client(svc: &mut NetService, m: civis::IpcMsg) {
    match m.tag {
        NET_EP_OPEN => {
            let proto = (m.w0 >> 16) as u16;
            let local_port = (m.w0 & 0xffff) as u16;
            let flags = m.w1;
            let peer_hash = match civis::peer_info(m.channel) {
                Ok(h) => h,
                Err(_) => {
                    let _ = civis::reply(NET_EP_OPEN, !0u64, 0);
                    return;
                }
            };
            if proto != NET_PROTO_UDP as u16 && proto != NET_PROTO_TCP as u16 {
                let _ = civis::reply(NET_EP_OPEN, !0u64, 0);
                return;
            }
            let port = if local_port == 0 || flags & NET_EP_FLAG_EPHEMERAL != 0 {
                let p = svc.ephemeral_port;
                svc.ephemeral_port = svc.ephemeral_port.wrapping_add(1);
                if svc.ephemeral_port < 49152 {
                    svc.ephemeral_port = 49152;
                }
                p
            } else {
                local_port
            };
            if !svc.can_bind(peer_hash, port) {
                let _ = civis::reply(NET_EP_OPEN, !0u64, 0);
                return;
            }
            let Some(idx) = svc.alloc_endpoint(m.channel, peer_hash) else {
                let _ = civis::reply(NET_EP_OPEN, !0u64, 0);
                return;
            };
            // Crea shm per il ring dell'endpoint (RX + TX).
            let ring_len = (NET_EP_RING_HDR * 2
                + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2)
                .next_power_of_two();
            let ring_id = match civis::shm_create(ring_len) {
                Ok(id) => id,
                Err(_) => {
                    svc.free_endpoint(idx);
                    let _ = civis::reply(NET_EP_OPEN, !0u64, 0);
                    return;
                }
            };
            let ring_base = match civis::shm_map(
                ring_id,
                0,
                civis::PROT_READ | civis::PROT_WRITE,
            ) {
                Ok(b) => b,
                Err(_) => {
                    svc.free_endpoint(idx);
                    let _ = civis::reply(NET_EP_OPEN, !0u64, 0);
                    return;
                }
            } as *mut u8;
            // Inizializza head/tail (shm_create azzera già, ma sia espliciti).
            unsafe {
                write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
                write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
                write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
                write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
            }
            let ep = &mut svc.endpoints[idx];
            ep.local_port = port;
            ep.ring_id = ring_id;
            ep.ring_base = ring_base;
            // Stato TCP (N3).
            if proto == NET_PROTO_TCP as u16 {
                if flags & NET_EP_FLAG_LISTEN != 0 {
                    ep.tcp_state = TCP_LISTEN;
                } else if flags & NET_EP_FLAG_CONNECT != 0 {
                    ep.tcp_state = TCP_SYN_SENT;
                    ep.tcp_seq = svc.net.tcp_next_seq();
                } else {
                    ep.tcp_state = TCP_CLOSED;
                }
            }
            let ep_id = ep.ep_id;
            let ring_id_ret = ep.ring_id;
            let _ = civis::reply(
                NET_EP_OPEN,
                ep_id as u64,
                (ring_id_ret as u64) | ((port as u64) << 32),
            );
        }
        NET_EP_TX => {
            let ep_id = m.w0 as u16;
            let slot_idx = m.w1 as usize;
            let Some(idx) = svc.find_by_id(ep_id) else {
                let _ = civis::reply(NET_EP_TX, !0u64, 0);
                return;
            };
            let ep = svc.endpoints[idx]; // Copy
            let ring = ep.ring_base;
            let mut payload = [0u8; NET_EP_SLOT_SIZE - 2];
            let (payload_len, dst, dst_port, event) =
                match NetService::ep_tx_read_full(ring, slot_idx, &mut payload) {
                    Some(v) => v,
                    None => {
                        let _ = civis::reply(NET_EP_TX, !0u64, 0);
                        return;
                    }
                };
            let is_tcp = ep.tcp_state != TCP_CLOSED;

            // Consegna locale (loopback o indirizzo proprio): scrivi direttamente
            // nel ring RX del client destinatario, senza passare per il driver.
            if dst.is_loopback() || svc.net.is_our_address(dst) {
                let src_addr = if dst.is_loopback() {
                    Ipv6Addr::LOOPBACK
                } else {
                    svc.net.link_local()
                };
                let delivered = if let Some(dst_idx) = svc.find_by_local_port(dst_port) {
                    let dst_ep = svc.endpoints[dst_idx]; // Copy
                    // Verifica che il sorgente corrisponda al peer atteso (se vincolato).
                    if dst_ep.remote_port != 0 && dst_ep.remote_port != ep.local_port {
                        false
                    } else {
                        // Per TCP, l'evento viene passato così com'è: spetta al
                        // destinatario interpretarlo (es. SYN -> rispondi SYN+ACK).
                        if NetService::ep_rx_write_full(
                            dst_ep.ring_base,
                            &src_addr,
                            ep.local_port,
                            dst_port,
                            event,
                            &payload[..payload_len],
                        ) {
                            if dst_ep.peer_chan != 0 {
                                let _ = civis::send_async(
                                    dst_ep.peer_chan,
                                    NET_EP_TX,
                                    dst_ep.ep_id as u64,
                                    0,
                                );
                            }
                            true
                        } else {
                            false // ring RX pieno
                        }
                    }
                } else {
                    false // nessun endpoint in ascolto su dst_port
                };
                NetService::ep_tx_consume_raw(ring);
                // Aggiorna contatori del mittente.
                if let Some(ep_mut) = svc.find_by_id_mut(ep_id) {
                    ep_mut.tx_pkts = ep_mut.tx_pkts.saturating_add(1);
                    if is_tcp && event == EV_SYN {
                        ep_mut.tcp_state = TCP_SYN_RECV;
                    }
                }
                let _ = civis::reply(
                    NET_EP_TX,
                    if delivered { 0 } else { !0u64 },
                    0,
                );
                return;
            }

            // Consegna remota: invia frame Ethernet via driver.
            let sent = if is_tcp {
                // Per TCP, `net` incapsula il payload applicativo in un segmento
                // completo con header TCP, sequenze e checksum.
                let dst_mac = if dst.is_link_local() {
                    match svc.net.neighbor_lookup(dst) {
                        Some(m) => m,
                        None => {
                            let _ = civis::reply(NET_EP_TX, !0u64, 0);
                            return;
                        }
                    }
                } else {
                    match (svc.net.router(), svc.net.router_mac()) {
                        (Some(_), Some(m)) => m,
                        _ => {
                            let _ = civis::reply(NET_EP_TX, !0u64, 0);
                            return;
                        }
                    }
                };
                let (flags, seq) = if event == EV_SYN {
                    (tcp::FLAG_SYN, ep.tcp_seq)
                } else {
                    (tcp::FLAG_ACK, ep.tcp_seq)
                };
                let mut tcp_seg = [0u8; 1500 - ipv6::IPV6_HDR - eth::ETH_HDR];
                let seg_len = tcp::write_checked(
                    &mut tcp_seg,
                    ep.local_port,
                    dst_port,
                    seq,
                    ep.tcp_ack,
                    flags,
                    65535,
                    &svc.net.link_local(),
                    &dst,
                    &payload[..payload_len],
                );
                let mut frame = [0u8; 1514];
                let mut off = eth::write(&mut frame, dst_mac, svc.net.mac(), ETHERTYPE_IPV6);
                off += ipv6::write(
                    &mut frame[off..],
                    NEXT_TCP,
                    ipv6::HOP_LIMIT,
                    svc.net.link_local(),
                    dst,
                    seg_len as u16,
                );
                frame[off..off + seg_len].copy_from_slice(&tcp_seg[..seg_len]);
                let mut sink = DriverSink { ring: svc.sink_ring };
                sink.send(&frame[..off + seg_len]);

                // Avanza stato e sequenza del mittente.
                if let Some(ep_mut) = svc.find_by_id_mut(ep_id) {
                    if event == EV_SYN {
                        ep_mut.tcp_state = TCP_SYN_RECV;
                    } else {
                        ep_mut.tcp_seq = ep_mut.tcp_seq.wrapping_add(seg_len as u32);
                    }
                }
                true
            } else {
                svc.net.send_udp(
                    dst,
                    ep.local_port,
                    dst_port,
                    &payload[..payload_len],
                    &mut DriverSink { ring: svc.sink_ring },
                )
            };
            if sent {
                NetService::ep_tx_consume_raw(ring);
                if let Some(ep_mut) = svc.find_by_id_mut(ep_id) {
                    ep_mut.tx_pkts = ep_mut.tx_pkts.saturating_add(1);
                }
                let _ = civis::reply(NET_EP_TX, 0, 0);
            } else {
                let _ = civis::reply(NET_EP_TX, !0u64, 0);
            }
        }
        NET_EP_CLOSE => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                svc.free_endpoint(idx);
            }
            let _ = civis::reply(NET_EP_CLOSE, 0, 0);
        }
        NET_EP_STATS => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                let ep = svc.endpoints[idx];
                let _ = civis::reply(NET_EP_STATS, ep.rx_pkts, ep.tx_pkts);
            } else {
                let _ = civis::reply(NET_EP_STATS, !0u64, 0);
            }
        }
        civis::EXIT_NOTIFY => {
            let dead_chan = m.channel;
            for i in 0..MAX_ENDPOINTS {
                if svc.endpoints[i].state != EpState::Free
                    && svc.endpoints[i].peer_chan == dead_chan
                {
                    svc.free_endpoint(i);
                }
            }
        }
        _ => {}
    }
}

fn log_event(e: Event) {
    match e {
        Event::LinkUp => {
            let _ = civis::print_string(b"[net] link up\n");
        }
        Event::Ra { router, prefix } => match prefix {
            Some(p) => civis::println!(
                "[net] RA from {} prefix={:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}::/64",
                router,
                p[0],
                p[1],
                p[2],
                p[3],
                p[4],
                p[5],
                p[6],
                p[7]
            ),
            None => civis::println!("[net] RA from {} (senza prefix)", router),
        },
        Event::Address(a) => civis::println!("[net] address {}", a),
        Event::EchoReply { from, seq } => {
            civis::println!("[net] echo reply from {} seq={}", from, seq)
        }
        Event::EchoTimeout => {
            let _ = civis::print_string(b"[net] echo timeout\n");
        }
        Event::Dropped(r) => civis::println!("[net] drop: {}", r),
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[net] panic\n");
    civis::exit(1)
}
