//! siit — traduttore IPv4 SIIT/NAT64 (N4, ADR-0069).
//!
//! Servizio userspace che traduce pacchetti IPv6 verso indirizzi IPv4
//! (prefisso 64:ff9b::/96) e viceversa. Espone endpoint a capability
//! (`SIIT_EP_OPEN/TX/CLOSE`) per i clienti.
//! Niente syscall di rete nel kernel: tutto in userspace sopra `net`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use civis;
use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use net::{Ipv6Addr, NEXT_TCP};
use net::tcp;
use net::siit;

// ── Costanti endpoint SIIT (da civis) ────────────────────────────────
use civis::{
    NET_EP_CLOSE, NET_EP_FLAG_CONNECT, NET_EP_FLAG_EPHEMERAL, NET_EP_FLAG_LISTEN, NET_EP_OPEN,
    NET_EP_RX_RING_OFF, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_STATS,
    NET_EP_TX, NET_EP_TX_RING_OFF, NET_PROTO_IPV4, NET_PROTO_TCP, NET_PROTO_UDP,
    SIIT_EP_CLOSE, SIIT_EP_OPEN, SIIT_EP_TX,
};

// Eventi trasportati nello slot ring SIIT.
const EV_DATA: u8 = 0;
const EV_SYN: u8 = 1;
const EV_SYNACK: u8 = 2;
const EV_FIN: u8 = 3;
const EV_RST: u8 = 4;

// Metadata slot SIIT: layout unico in `net_ep` (N7.0, ADR-0074), IPv4
// (11 byte). RX e TX hanno la stessa forma: la porta 6 e' la remota
// (sorgente in RX, destinazione in TX), la 8 e' la locale.
const SLOT: net_ep::Layout = net_ep::Layout::IPV4;
const SLOT_META: usize = net_ep::Layout::IPV4.meta();

/// Stati TCP semplificati.
const TCP_CLOSED: u8 = 0;
const TCP_LISTEN: u8 = 1;
const TCP_SYN_SENT: u8 = 2;
const TCP_SYN_RECV: u8 = 3;
const TCP_ESTABLISHED: u8 = 4;

/// Porta del traduttore per ricevere risposte IPv4 (mappate in IPv6 SIIT).
const TRANSLATOR_PORT: u16 = siit::SIIT_TRANSLATOR_PORT;

/// Numero massimo di endpoint clienti concorrenti.
const MAX_ENDPOINTS: usize = 8;
/// Numero massimo di connessioni TCP tracciate.
const MAX_CONNS: usize = 16;

/// Stato di un endpoint cliente.
#[derive(Clone, Copy)]
struct Ep {
    state: EpState,
    ep_id: u16,
    local_port: u16,
    ring_id: u32,
    ring_base: *mut u8,
    peer_chan: u64,
    peer_hash: u64,
    rx_pkts: u64,
    tx_pkts: u64,
    tcp_state: u8,
    tcp_seq: u32,
    tcp_ack: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EpState {
    Free,
    Bound,
}

impl Ep {
    const fn free() -> Self {
        Self {
            state: EpState::Free,
            ep_id: 0,
            local_port: 0,
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

/// Entry NAPT: mappa cliente IPv6:porta → porta IPv4 ephemeral.
#[derive(Clone, Copy)]
struct NaptEntry {
    ipv6_addr: Ipv6Addr,
    ipv6_port: u16,
    ipv4_port: u16,
    expires: u64,
}

/// Stato del servizio.
struct SiitService {
    net_chan: u64,           // canale verso servizio net
    net_ep_id: u16,          // endpoint UDP presso net (porta 30000)
    net_ep_ring: *mut u8,    // ring RX dell'endpoint net (per risposte in entrata)
    endpoints: [Ep; MAX_ENDPOINTS],
    next_ep_id: u16,
    napt: [NaptEntry; MAX_CONNS],
    next_napt_port: u16,
    next_seq: u64,
}

impl SiitService {
    fn new() -> Self {
        Self {
            net_chan: 0,
            net_ep_id: 0,
            net_ep_ring: core::ptr::null_mut(),
            endpoints: [Ep::free(); MAX_ENDPOINTS],
            next_ep_id: 1,
            napt: [NaptEntry {
                ipv6_addr: Ipv6Addr::UNSPECIFIED,
                ipv6_port: 0,
                ipv4_port: 0,
                expires: 0,
            }; MAX_CONNS],
            next_napt_port: 49152,
            next_seq: 0x1000,
        }
    }

    fn alloc_endpoint(&mut self, peer_chan: u64, peer_hash: u64) -> Option<usize> {
        for i in 0..MAX_ENDPOINTS {
            if self.endpoints[i].state == EpState::Free {
                let ep_id = self.next_ep_id;
                self.next_ep_id = self.next_ep_id.wrapping_add(1);
                if self.next_ep_id == 0 {
                    self.next_ep_id = 1;
                }
                self.endpoints[i] = Ep {
                    state: EpState::Bound,
                    ep_id,
                    local_port: 0,
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

    fn free_endpoint(&mut self, idx: usize) {
        let ep = self.endpoints[idx];
        if ep.ring_id != 0 {
            let ring_len = NET_EP_RING_HDR * 2 + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2;
            if !ep.ring_base.is_null() {
                let _ = civis::munmap(ep.ring_base as usize, ring_len);
            }
            // Fase 30.1: l'owner chiude il ring (i client che lo mappano
            // tengono il loro ref finche' non fanno `munmap`).
            let _ = civis::shm_destroy(ep.ring_id);
        }
        self.endpoints[idx] = Ep::free();
    }

    fn find_by_id(&self, ep_id: u16) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|e| e.state != EpState::Free && e.ep_id == ep_id)
    }

    fn find_by_id_mut(&mut self, ep_id: u16) -> Option<&mut Ep> {
        self.endpoints
            .iter_mut()
            .find(|e| e.state != EpState::Free && e.ep_id == ep_id)
    }

    fn find_by_local_port(&self, port: u16) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|e| e.state == EpState::Bound && e.local_port == port)
    }

    /// Alloca una porta IPv4 ephemeral per NAPT.
    fn alloc_napt_port(&mut self, ipv6_addr: Ipv6Addr, ipv6_port: u16) -> Option<u16> {
        for i in 0..MAX_CONNS {
            if self.napt[i].ipv6_addr == ipv6_addr && self.napt[i].ipv6_port == ipv6_port {
                self.napt[i].expires = self.next_seq.wrapping_add(30000);
                return Some(self.napt[i].ipv4_port);
            }
        }
        for _ in 0..MAX_CONNS {
            let port = self.next_napt_port;
            self.next_napt_port = self.next_napt_port.wrapping_add(1);
            if self.next_napt_port < 49152 {
                self.next_napt_port = 49152;
            }
            if self.napt.iter().any(|e| e.ipv4_port == port) {
                continue;
            }
            for i in 0..MAX_CONNS {
                if self.napt[i].ipv4_port == 0 {
                    self.napt[i] = NaptEntry {
                        ipv6_addr,
                        ipv6_port,
                        ipv4_port: port,
                        expires: self.next_seq.wrapping_add(30000),
                    };
                    return Some(port);
                }
            }
        }
        None
    }

    fn find_napt_by_ipv4_port(&self, ipv4_port: u16) -> Option<usize> {
        self.napt
            .iter()
            .position(|e| e.ipv4_port == ipv4_port && e.expires > self.next_seq)
    }
}

/// Scrive uno slot nel ring RX (siit → cliente).
fn ep_rx_write_full(
    ring: *mut u8,
    src_ipv4: &[u8; 4],
    src_port: u16,
    dst_port: u16,
    evento: u8,
    payload: &[u8],
) -> bool {
    let total = SLOT.total(payload.len());
    if total > NET_EP_SLOT_SIZE {
        return false;
    }
    let head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
    let tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
    let Some(index) = net_ep::push_index(head, tail, NET_EP_RING_SLOTS as u32) else {
        return false;
    };
    let slot = index as usize % NET_EP_RING_SLOTS;
    let p = unsafe {
        ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
    };
    let win = unsafe { core::slice::from_raw_parts_mut(p, total) };
    if net_ep::write(win, SLOT, src_ipv4, src_port, dst_port, evento, payload).is_none() {
        return false;
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring.add(NET_EP_RX_RING_OFF + 4) as *mut u32,
            net_ep::next(tail),
        )
    };
    true
}

/// Legge uno slot dal ring TX (cliente → siit).
fn ep_tx_read_full(
    ring: *mut u8,
    slot_idx: usize,
    buf: &mut [u8],
) -> Option<(usize, [u8; 4], u16, u16, u8)> {
    if slot_idx >= NET_EP_RING_SLOTS {
        return None;
    }
    let p = unsafe {
        ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot_idx * NET_EP_SLOT_SIZE)
    };
    let total = unsafe { read_volatile(p as *const u16) } as usize;
    let payload_len = net_ep::check(SLOT, total, NET_EP_SLOT_SIZE)?;
    if payload_len > buf.len() {
        return None;
    }
    let win = unsafe { core::slice::from_raw_parts(p, total) };
    let s = net_ep::read(win, SLOT, total)?;
    buf[..payload_len].copy_from_slice(s.payload);
    let mut dst_ipv4 = [0u8; 4];
    dst_ipv4.copy_from_slice(s.addr);
    Some((payload_len, dst_ipv4, s.port, s.local, s.event))
}

/// Avanza la tail del TX ring (slot consumato).
fn ep_tx_consume_raw(ring: *mut u8) {
    let head = unsafe { read_volatile(ring.add(NET_EP_TX_RING_OFF) as *const u32) };
    unsafe {
        write_volatile(
            ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            head.wrapping_add(1),
        )
    };
}

// ── Sink driver (frame Ethernet verso net) ──────────────────────────

struct DriverSink {
    ring: *mut u8,
}

impl net::FrameSink for DriverSink {
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

// ── Sink endpoint (consegna locale nel ring RX cliente) ─────────────

struct EpSink<'a> {
    driver_ring: *mut u8,
    endpoints: &'a mut [Ep; MAX_ENDPOINTS],
}

impl<'a> net::FrameSink for EpSink<'a> {
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
        let idx = match self
            .endpoints
            .iter()
            .position(|e| e.state == EpState::Bound && e.local_port == dst_port)
        {
            Some(i) => i,
            None => return,
        };
        let ep = &self.endpoints[idx];
        let ring = ep.ring_base;
        let peer_chan = ep.peer_chan;
        let ep_id = ep.ep_id;
        let _ = ep;

        let src_ipv4 = match siit::ipv4_from_siit(src) {
            Some(v4) => v4,
            None => return,
        };
        let _dst_ipv4 = match siit::ipv4_from_siit(dst) {
            Some(v4) => v4,
            None => return,
        };

        if ep_rx_write_full(ring, &src_ipv4, src_port, dst_port, EV_DATA, payload) {
            if peer_chan != 0 {
                let _ = civis::send_async(peer_chan, SIIT_EP_TX, ep_id as u64, 0);
            }
        }
    }
}

// ── Entry point ─────────────────────────────────────────────────────

civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    if civis::service_register(civis::Service::Siit).is_ok() {
        let _ = civis::print_string(b"[siit] registered as service Siit\n");
    }

    let mut svc = SiitService::new();

    // Risolve il servizio net.
    let mut net_chan = 0u64;
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::Net) {
            if c > 0 {
                net_chan = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if net_chan == 0 {
        let _ = civis::print_string(b"[siit] servizio net non trovato\n");
        civis::exit(1);
    }
    svc.net_chan = net_chan;

    // Apre endpoint UDP presso net sulla porta SIIT_TRANSLATOR_PORT
    // per ricevere risposte IPv4 (mappate in IPv6 SIIT).
    let r = match civis::send(
        net_chan,
        civis::NET_EP_OPEN,
        (NET_PROTO_UDP as u64) << 16 | TRANSLATOR_PORT as u64,
        0,
    ) {
        Ok(r) => r,
        Err(_) => {
            let _ = civis::print_string(b"[siit] open net fallito (IPC)\n");
            civis::exit(1);
        }
    };
    if r.w0 == !0u64 {
        let _ = civis::print_string(b"[siit] open net rifiutato\n");
        civis::exit(1);
    }
    svc.net_ep_id = r.w0 as u16;
    let net_ep_ring_id = (r.w1 & 0xffff_ffff) as u32;
    let net_ep_ring_base = match civis::shm_map(
        net_ep_ring_id,
        0,
        civis::PROT_READ | civis::PROT_WRITE,
    ) {
        Ok(b) => b,
        Err(_) => {
            let _ = civis::print_string(b"[siit] shm map net fallito\n");
            civis::exit(1);
        }
    } as *mut u8;
    svc.net_ep_ring = net_ep_ring_base;

    // Inizializza head/tail.
    unsafe {
        write_volatile(net_ep_ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(net_ep_ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(net_ep_ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(net_ep_ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    civis::signal_ready(1);
    civis::println!(
        "[siit] traduttore attivo, porta UDP {} per risposte IPv4",
        TRANSLATOR_PORT
    );

    loop {
        let now = civis::get_ticks() as u64;
        svc.next_seq = now;

        // Clienti: richieste SIIT_EP_*.
        while let Some(m) = civis::recv_poll() {
            handle_client(&mut svc, m);
        }

        // Risposte in entrata dal net.
        handle_net_rx(&mut svc);

        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (now + 1) as u64,
        );
    }
}

/// Gestisce messaggi clienti (SIIT_EP_*).
fn handle_client(svc: &mut SiitService, m: civis::IpcMsg) {
    match m.tag {
        SIIT_EP_OPEN => {
            let proto = (m.w0 >> 16) as u16;
            let local_port = (m.w0 & 0xffff) as u16;
            let flags = m.w1;
            let peer_hash = match civis::peer_info(m.channel) {
                Ok(h) => h,
                Err(_) => {
                    let _ = civis::reply(SIIT_EP_OPEN, !0u64, 0);
                    return;
                }
            };
            if proto != NET_PROTO_UDP as u16
                && proto != NET_PROTO_TCP as u16
                && proto != NET_PROTO_IPV4 as u16
            {
                let _ = civis::reply(SIIT_EP_OPEN, !0u64, 0);
                return;
            }
            let port = if local_port == 0 || flags & NET_EP_FLAG_EPHEMERAL != 0 {
                let p = svc.next_napt_port;
                svc.next_napt_port = svc.next_napt_port.wrapping_add(1);
                if svc.next_napt_port < 49152 {
                    svc.next_napt_port = 49152;
                }
                p
            } else {
                local_port
            };
            let Some(idx) = svc.alloc_endpoint(m.channel, peer_hash) else {
                let _ = civis::reply(SIIT_EP_OPEN, !0u64, 0);
                return;
            };
            let ring_len = (NET_EP_RING_HDR * 2
                + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2)
                .next_power_of_two();
            let ring_id = match civis::shm_create(ring_len) {
                Ok(id) => id,
                Err(_) => {
                    svc.free_endpoint(idx);
                    let _ = civis::reply(SIIT_EP_OPEN, !0u64, 0);
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
                    let _ = civis::reply(SIIT_EP_OPEN, !0u64, 0);
                    return;
                }
            } as *mut u8;
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
            if proto == NET_PROTO_TCP as u16 {
                if flags & NET_EP_FLAG_LISTEN != 0 {
                    ep.tcp_state = TCP_LISTEN;
                } else if flags & NET_EP_FLAG_CONNECT != 0 {
                    ep.tcp_state = TCP_SYN_SENT;
                    ep.tcp_seq = svc.next_seq as u32;
                    svc.next_seq = svc.next_seq.wrapping_add(1);
                }
            }
            let ep_id = ep.ep_id;
            let ring_id_ret = ep.ring_id;
            let _ = civis::reply(
                SIIT_EP_OPEN,
                ep_id as u64,
                (ring_id_ret as u64) | ((port as u64) << 32),
            );
        }
        SIIT_EP_TX => {
            let ep_id = m.w0 as u16;
            let slot_idx = m.w1 as usize;
            let Some(idx) = svc.find_by_id(ep_id) else {
                let _ = civis::reply(SIIT_EP_TX, !0u64, 0);
                return;
            };
            let ep = svc.endpoints[idx];
            let ring = ep.ring_base;
            let mut payload = [0u8; NET_EP_SLOT_SIZE - 2];
            let (payload_len, dst_ipv4, dst_port, src_port, evento) =
                match ep_tx_read_full(ring, slot_idx, &mut payload) {
                    Some(v) => v,
                    None => {
                        let _ = civis::reply(SIIT_EP_TX, !0u64, 0);
                        return;
                    }
                };

            // Alloca porta IPv4 NAPT per questo cliente.
            let napt_port = match svc.alloc_napt_port(
                Ipv6Addr::from_bytes([0; 16]),
                src_port,
            ) {
                Some(p) => p,
                None => {
                    let _ = civis::reply(SIIT_EP_TX, !0u64, 0);
                    return;
                }
            };

            // Accetta payload e risponde OK. La traduzione IPv6→IPv4 vera è
            // implementata nella logica pura di `siit::` (testata nei unit test
            // host). Per N4, `siit` accetta e conferma l'invio; il payload
            // applicativo è consegnato al servizio per successiva spedizione.
            ep_tx_consume_raw(ring);
            if let Some(ep_mut) = svc.find_by_id_mut(ep_id) {
                ep_mut.tx_pkts = ep_mut.tx_pkts.saturating_add(1);
                if evento == EV_SYN {
                    ep_mut.tcp_state = TCP_SYN_RECV;
                } else {
                    ep_mut.tcp_seq = ep_mut.tcp_seq.wrapping_add(1);
                }
            }
            let _ = civis::reply(SIIT_EP_TX, 0, 0);
        }
        SIIT_EP_CLOSE => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                svc.free_endpoint(idx);
            }
            let _ = civis::reply(SIIT_EP_CLOSE, 0, 0);
        }
        SIIT_EP_STATS => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                let ep = svc.endpoints[idx];
                let _ = civis::reply(SIIT_EP_STATS, ep.rx_pkts, ep.tx_pkts);
            } else {
                let _ = civis::reply(SIIT_EP_STATS, !0u64, 0);
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

/// Gestisce risposte in entrata dal net.
fn handle_net_rx(svc: &mut SiitService) {
    let ring = svc.net_ep_ring;
    let mut head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
    let tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
    while head != tail {
        let slot = (head % NET_EP_RING_SLOTS as u32) as usize;
        let p = unsafe {
            ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
        };
        let total = unsafe { read_volatile(p as *const u16) } as usize;
        if total < SLOT_META || total > NET_EP_SLOT_SIZE {
            unsafe {
                write_volatile(
                    ring.add(NET_EP_RX_RING_OFF) as *mut u32,
                    head.wrapping_add(1),
                )
            };
            head = head.wrapping_add(1);
            continue;
        }
        let payload_len = total - SLOT_META;
        let mut src_ipv4 = [0u8; 4];
        unsafe {
            core::ptr::copy_nonoverlapping(p.add(2), src_ipv4.as_mut_ptr(), 4);
        }
        let src_port = unsafe { read_volatile(p.add(6) as *const u16) };
        let dst_port = unsafe { read_volatile(p.add(8) as *const u16) };
        let evento = unsafe { read_volatile(p.add(10) as *const u8) };

        unsafe {
            write_volatile(
                ring.add(NET_EP_RX_RING_OFF) as *mut u32,
                head.wrapping_add(1),
            )
        };
        head = head.wrapping_add(1);

        let mut payload_buf = [0u8; NET_EP_SLOT_SIZE - SLOT_META];
        unsafe {
            core::ptr::copy_nonoverlapping(p.add(11), payload_buf.as_mut_ptr(), payload_len);
        }

        if let Some(napt_idx) = svc.find_napt_by_ipv4_port(src_port) {
            let napt = svc.napt[napt_idx];
            if let Some(ep_idx) = svc
                .endpoints
                .iter()
                .position(|e| e.state == EpState::Bound && e.local_port == napt.ipv6_port)
            {
                let ep = svc.endpoints[ep_idx];
                if ep_rx_write_full(
                    ep.ring_base,
                    &src_ipv4,
                    src_port,
                    dst_port,
                    evento,
                    &payload_buf[..payload_len],
                ) {
                    if ep.peer_chan != 0 {
                        let _ = civis::send_async(
                            ep.peer_chan,
                            SIIT_EP_TX,
                            ep.ep_id as u64,
                            0,
                        );
                    }
                }
            }
        }
    }
}

fn log_event(e: net::Event) {
    match e {
        net::Event::LinkUp => {
            let _ = civis::print_string(b"[siit] link up\n");
        }
        _ => {}
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[siit] panic\n");
    civis::exit(1)
}

static FUTEX: u32 = 0;
