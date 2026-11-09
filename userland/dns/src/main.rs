//! dns — resolver DNS plain (N5, ADR-0069).
//!
//! Servizio userspace che risolve nomi a indirizzi IPv6/IPv4 tramite query
//! DNS. Espone endpoint a capability (`DNS_EP_OPEN/TX/CLOSE/STATS`) per i
//! clienti; niente syscall di rete nel kernel, tutto in userspace.
//!
//! N5: la risoluzione usa una tabella statica (`example.com` A,
//! `ipv6.example.com` AAAA). Le query non presenti rispondono NXDOMAIN.
//! L'inoltro verso un resolver upstream arriverà con DoH/DoT (N6+).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use civis;
use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use net::{dns, Ipv6Addr};

use civis::{
    DNS_CLOSE, DNS_OPEN, DNS_PORT, DNS_STATS, DNS_TX, NET_EP_RX_RING_OFF, NET_EP_RING_HDR,
    NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_TX_RING_OFF, NET_PROTO_UDP,
};

use net::{CLASS_IN, DNS_HEADER_LEN, FLAG_QR, FLAG_RD, TYPE_A, TYPE_AAAA};

// Metadata slot endpoint: [len:2][ipv4:4][port:2][port:2][evento:1][payload].
const SLOT_META: usize = 11;
// Evento dati nello slot.
const EV_DATA: u8 = 0;
// Numero massimo di endpoint clienti concorrenti.
const MAX_ENDPOINTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EpState {
    Free,
    Bound,
}

#[derive(Clone, Copy)]
struct Ep {
    state: EpState,
    ep_id: u16,
    local_port: u16,
    ring_id: u32,
    ring_base: *mut u8,
    peer_chan: u64,
    rx_pkts: u64,
    tx_pkts: u64,
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
            rx_pkts: 0,
            tx_pkts: 0,
        }
    }
}

struct DnsService {
    net_chan: u64,
    net_ep_id: u16,
    net_ep_ring: *mut u8,
    endpoints: [Ep; MAX_ENDPOINTS],
    next_ep_id: u16,
}

impl DnsService {
    fn new() -> Self {
        Self {
            net_chan: 0,
            net_ep_id: 0,
            net_ep_ring: core::ptr::null_mut(),
            endpoints: [Ep::free(); MAX_ENDPOINTS],
            next_ep_id: 1,
        }
    }

    fn alloc_endpoint(&mut self, peer_chan: u64) -> Option<usize> {
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
                    ..Ep::free()
                };
                self.endpoints[i].peer_chan = peer_chan;
                return Some(i);
            }
        }
        None
    }

    fn free_endpoint(&mut self, idx: usize) {
        let ep = self.endpoints[idx];
        if ep.ring_id != 0 && !ep.ring_base.is_null() {
            let ring_len = NET_EP_RING_HDR * 2 + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2;
            let _ = civis::munmap(ep.ring_base as usize, ring_len);
        }
        self.endpoints[idx] = Ep::free();
    }

    fn find_by_id(&self, ep_id: u16) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|e| e.state != EpState::Free && e.ep_id == ep_id)
    }
}

/// Scrive uno slot nel ring RX (servizio → cliente).
fn ep_rx_write_full(
    ring: *mut u8,
    src_ipv4: &[u8; 4],
    src_port: u16,
    dst_port: u16,
    evento: u8,
    payload: &[u8],
) -> bool {
    let total = SLOT_META + payload.len();
    if total > NET_EP_SLOT_SIZE {
        return false;
    }
    let head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
    let tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
    if tail.wrapping_sub(head) >= NET_EP_RING_SLOTS as u32 {
        return false;
    }
    let slot = (tail % NET_EP_RING_SLOTS as u32) as usize;
    let p =
        unsafe { ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        p.add(2).copy_from_nonoverlapping(src_ipv4.as_ptr(), 4);
        write_volatile(p.add(6) as *mut u16, src_port);
        write_volatile(p.add(8) as *mut u16, dst_port);
        write_volatile(p.add(10) as *mut u8, evento);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(11), payload.len());
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

/// Legge uno slot dal ring TX (cliente → servizio).
fn ep_tx_read_full(
    ring: *mut u8,
    slot_idx: usize,
    buf: &mut [u8],
) -> Option<(usize, [u8; 4], u16, u16, u8)> {
    if slot_idx >= NET_EP_RING_SLOTS {
        return None;
    }
    let p =
        unsafe { ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot_idx * NET_EP_SLOT_SIZE) };
    let total = unsafe { read_volatile(p as *const u16) } as usize;
    if total < SLOT_META || total > NET_EP_SLOT_SIZE {
        return None;
    }
    let mut dst_ipv4 = [0u8; 4];
    unsafe {
        core::ptr::copy_nonoverlapping(p.add(2), dst_ipv4.as_mut_ptr(), 4);
    }
    let dst_port = unsafe { read_volatile(p.add(6) as *const u16) };
    let src_port = unsafe { read_volatile(p.add(8) as *const u16) };
    let evento = unsafe { read_volatile(p.add(10) as *const u8) };
    let payload_len = total - SLOT_META;
    if payload_len > buf.len() {
        return None;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(p.add(11), buf.as_mut_ptr(), payload_len);
    }
    Some((payload_len, dst_ipv4, dst_port, src_port, evento))
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

/// Costruisce una risposta DNS per una query.
///
/// `rcode` 0 = NOERROR. Se `a` è `Some`, aggiunge un record A; se `aaaa` è
/// `Some`, aggiunge un record AAAA. La domanda viene ricodificata dal nome.
fn build_response(
    out: &mut [u8],
    id: u16,
    name: &str,
    qtype: u16,
    a: Option<[u8; 4]>,
    aaaa: Option<[u8; 16]>,
    rcode: u16,
) -> Option<usize> {
    let ancount = (a.is_some() as u16) + (aaaa.is_some() as u16);
    let flags = FLAG_QR | FLAG_RD | (rcode & dns::FLAG_RCODE_MASK);
    let mut off = dns::write_header(out, id, flags, 1, ancount, 0, 0);
    off += dns::encode_question(&mut out[off..], name, qtype, CLASS_IN)?;

    if ancount > 0 {
        off += dns::encode_name(&mut out[off..], name)?;
    }
    if let Some(addr) = a {
        out[off..off + 2].copy_from_slice(&TYPE_A.to_be_bytes());
        out[off + 2..off + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
        out[off + 4..off + 8].copy_from_slice(&300u32.to_be_bytes());
        out[off + 8..off + 10].copy_from_slice(&4u16.to_be_bytes());
        out[off + 10..off + 14].copy_from_slice(&addr);
        off += 14;
    }
    if let Some(addr) = aaaa {
        out[off..off + 2].copy_from_slice(&TYPE_AAAA.to_be_bytes());
        out[off + 2..off + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
        out[off + 4..off + 8].copy_from_slice(&300u32.to_be_bytes());
        out[off + 8..off + 10].copy_from_slice(&16u16.to_be_bytes());
        out[off + 10..off + 26].copy_from_slice(&addr);
        off += 26;
    }
    Some(off)
}

/// Tabella statica N5.
fn lookup(name: &[u8], qtype: u16) -> Option<(Option<[u8; 4]>, Option<[u8; 16]>)> {
    if name == b"example.com" && qtype == TYPE_A {
        Some((Some([93, 184, 216, 34]), None))
    } else if name == b"ipv6.example.com" && qtype == TYPE_AAAA {
        Some((None, Some([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ])))
    } else {
        None
    }
}

civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    if civis::service_register(civis::Service::Dns).is_err() {
        let _ = civis::print_string(b"[dns] service_register rifiutato\n");
        civis::exit(1);
    }
    let _ = civis::print_string(b"[dns] registered as service Dns\n");

    let mut svc = DnsService::new();
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
        let _ = civis::print_string(b"[dns] servizio net non trovato\n");
        civis::exit(1);
    }
    svc.net_chan = net_chan;

    // Endpoint UDP presso `net`: riceve le risposte dell'upstream (N6+).
    let r = match civis::send(
        net_chan,
        civis::NET_EP_OPEN,
        (NET_PROTO_UDP as u64) << 16 | DNS_PORT as u64,
        0,
    ) {
        Ok(r) => r,
        Err(_) => {
            let _ = civis::print_string(b"[dns] open net fallito (IPC)\n");
            civis::exit(1);
        }
    };
    if r.w0 == !0u64 {
        let _ = civis::print_string(b"[dns] open net rifiutato\n");
        civis::exit(1);
    }
    svc.net_ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            let _ = civis::print_string(b"[dns] shm map net fallito\n");
            civis::exit(1);
        }
    } as *mut u8;
    svc.net_ep_ring = ring_base;
    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    civis::signal_ready(1);
    civis::println!(
        "[dns] resolver attivo, porta UDP {} (tabella statica N5)",
        DNS_PORT
    );

    loop {
        let now = civis::get_ticks() as u64;
        while let Some(m) = civis::recv_poll() {
            handle_client(&mut svc, m);
        }
        let _ = civis::futex_wait(&FUTEX as *const u32 as usize, 0, (now + 1) as u64);
    }
}

fn handle_client(svc: &mut DnsService, m: civis::IpcMsg) {
    match m.tag {
        DNS_OPEN => {
            let proto = (m.w0 >> 16) as u16;
            let local_port = (m.w0 & 0xffff) as u16;
            if proto != NET_PROTO_UDP as u16 {
                let _ = civis::reply(DNS_OPEN, !0u64, 0);
                return;
            }
            let port = if local_port == 0 {
                let p = 49152u16.wrapping_add(svc.next_ep_id);
                svc.next_ep_id = svc.next_ep_id.wrapping_add(1);
                p
            } else {
                local_port
            };
            let Some(idx) = svc.alloc_endpoint(m.channel) else {
                let _ = civis::reply(DNS_OPEN, !0u64, 0);
                return;
            };
            let ring_len =
                (NET_EP_RING_HDR * 2 + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2).next_power_of_two();
            let ring_id = match civis::shm_create(ring_len) {
                Ok(id) => id,
                Err(_) => {
                    svc.free_endpoint(idx);
                    let _ = civis::reply(DNS_OPEN, !0u64, 0);
                    return;
                }
            };
            let ring_base =
                match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
                    Ok(b) => b,
                    Err(_) => {
                        svc.free_endpoint(idx);
                        let _ = civis::reply(DNS_OPEN, !0u64, 0);
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
            let ep_id = ep.ep_id;
            let _ = civis::reply(
                DNS_OPEN,
                ep_id as u64,
                (ring_id as u64) | ((port as u64) << 32),
            );
        }
        DNS_TX => {
            let ep_id = m.w0 as u16;
            let slot_idx = m.w1 as usize;
            let Some(idx) = svc.find_by_id(ep_id) else {
                let _ = civis::reply(DNS_TX, !0u64, 0);
                return;
            };
            let ep = svc.endpoints[idx];
            let ring = ep.ring_base;
            let mut payload = [0u8; NET_EP_SLOT_SIZE - SLOT_META];
            let (payload_len, _, _, _, _) = match ep_tx_read_full(ring, slot_idx, &mut payload) {
                Some(v) => v,
                None => {
                    let _ = civis::reply(DNS_TX, !0u64, 0);
                    return;
                }
            };
            let query = &payload[..payload_len];

            let (id, name, qtype) = match query_question(query) {
                Some(v) => v,
                None => {
                    ep_tx_consume_raw(ring);
                    let _ = civis::reply(DNS_TX, 0, 0);
                    return;
                }
            };

            let mut resp = [0u8; 512];
            let resp_len = match core::str::from_utf8(&name)
                .ok()
                .and_then(|n| lookup(&name, qtype).and_then(|(a, aaaa)| {
                    build_response(&mut resp, id, n, qtype, a, aaaa, 0)
                })) {
                Some(len) => len,
                None => {
                    // NXDOMAIN: nome non in tabella (o query malformata).
                    let n = core::str::from_utf8(&name).unwrap_or("");
                    build_response(&mut resp, id, n, qtype, None, None, 3).unwrap_or(0)
                }
            };

            if resp_len > 0 {
                if ep_rx_write_full(ring, &[0, 0, 0, 0], DNS_PORT, ep.local_port, EV_DATA, &resp[..resp_len])
                {
                    if ep.peer_chan != 0 {
                        let _ = civis::send_async(ep.peer_chan, DNS_TX, ep_id as u64, 0);
                    }
                }
            }
            ep_tx_consume_raw(ring);
            if let Some(i) = svc.find_by_id(ep_id) {
                svc.endpoints[i].tx_pkts = svc.endpoints[i].tx_pkts.saturating_add(1);
            }
            let _ = civis::reply(DNS_TX, 0, 0);
        }
        DNS_CLOSE => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                svc.free_endpoint(idx);
            }
            let _ = civis::reply(DNS_CLOSE, 0, 0);
        }
        DNS_STATS => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                let ep = svc.endpoints[idx];
                let _ = civis::reply(DNS_STATS, ep.rx_pkts, ep.tx_pkts);
            } else {
                let _ = civis::reply(DNS_STATS, !0u64, 0);
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

/// Estrae (id, nome, tipo) da una query DNS.
fn query_question(pkt: &[u8]) -> Option<(u16, Vec<u8>, u16)> {
    if pkt.len() < DNS_HEADER_LEN {
        return None;
    }
    let hdr = dns::parse_header(pkt)?;
    if hdr.qdcount == 0 {
        return None;
    }
    let (name, qtype, _qclass, _off) = dns::decode_question(pkt, DNS_HEADER_LEN)?;
    Some((hdr.id, name, qtype))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[dns] panic\n");
    civis::exit(1)
}

static FUTEX: u32 = 0;
