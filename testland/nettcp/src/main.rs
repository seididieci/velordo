//! nettcp — prova TCP su rete **reale** (N6.1d, ADR-0069).
//!
//! Due modalità, scelte da `sys:nettcp.conf` (`<ipv6|auto> <porta> [modo]`):
//! - `echo` (default): apre un endpoint TCP, invia un payload più grande di
//!   un MSS e valida l'eco byte-exact;
//! - `dnstcp`: invia una query DNS over TCP (prefisso di lunghezza a 2 byte,
//!   RFC 1035 §4.2.2) e valida la risposta (`QR=1`). È lo stesso trasporto
//!   che userà DoT (N6.3), quindi la prova vale anche per la fase successiva.
//!
//! Nessuna config = probe saltato (il gate non la seeda). Eseguito solo con
//! `net=1`.

#![no_std]
#![no_main]

extern crate alloc;

use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use civis::{
    NET_ADDR, NET_ADDR_ROUTER, NET_EP_FLAG_CONNECT, NET_EP_OPEN, NET_EP_RX_RING_OFF,
    NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_TX, NET_EP_TX_RING_OFF,
    NET_PROTO_TCP,
};

static FUTEX: u32 = 0;

const EV_DATA: u8 = 0;
const EV_SYNACK: u8 = 2;
const EV_FIN: u8 = 3;
const EV_RST: u8 = 4;

/// Metadata slot: [len:2][addr:16][dport:2][sport:2][evento:1][payload].
const SLOT_META: usize = 23;

/// Payload dell'echo: più di un MSS (1440), così il core segmenta davvero.
const ECHO_LEN: usize = 3600;
/// Buffer massimo del payload (echo o query).
const BUF_MAX: usize = 4096;
/// Tick di attesa massimi (100 Hz).
const MAX_TICKS: u32 = 1500;

civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let (cfg_addr, port, dnstcp) = match civis::obj_get(b"sys", b"nettcp.conf") {
        Ok(data) => match parse_conf(&data) {
            Some(v) => v,
            None => {
                println!("[nettcp] SKIP: sys:nettcp.conf invalido");
                finish(true);
            }
        },
        Err(_) => {
            println!("[nettcp] SKIP: nessun sys:nettcp.conf (test fuori gate)");
            finish(true);
        }
    };

    let net_chan = match wait_service(civis::Service::Net) {
        Some(c) => c,
        None => {
            println!("[nettcp] FAIL: servizio net non trovato");
            finish(false);
        }
    };

    let dst = match cfg_addr {
        Some(a) => a,
        None => match router_addr(net_chan) {
            Some(a) => a,
            None => {
                println!("[nettcp] SKIP: nessun router (nessun RA)");
                finish(true);
            }
        },
    };
    println!("[nettcp] peer {}:{} ({})", fmt_addr(&dst), port, if dnstcp { "dnstcp" } else { "echo" });

    // Endpoint TCP CONNECT (porta effimera dal servizio).
    let r = match civis::send(
        net_chan,
        NET_EP_OPEN,
        (NET_PROTO_TCP as u64) << 16,
        NET_EP_FLAG_CONNECT as u64,
    ) {
        Ok(r) => r,
        Err(_) => {
            println!("[nettcp] FAIL: open (IPC)");
            finish(false);
        }
    };
    if r.w0 == !0u64 {
        println!("[nettcp] FAIL: open rifiutato");
        finish(false);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let local_port = (r.w1 >> 32) as u16;
    println!("[nettcp] endpoint TCP id={} porta={}", ep_id, local_port);
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            println!("[nettcp] FAIL: shm map");
            finish(false);
        }
    } as *mut u8;
    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    // Payload di prova.
    let mut payload = alloc::vec![0u8; BUF_MAX];
    let payload_len = if dnstcp {
        let q = build_dns_query(b"example.com");
        payload[0..2].copy_from_slice(&(q.len() as u16).to_be_bytes());
        payload[2..2 + q.len()].copy_from_slice(&q);
        2 + q.len()
    } else {
        for (i, b) in payload[..ECHO_LEN].iter_mut().enumerate() {
            *b = b'a' + (i % 26) as u8;
        }
        ECHO_LEN
    };

    let mut rx = alloc::vec![0u8; BUF_MAX];
    let mut sent = 0usize;
    let mut received = 0usize;
    let mut synack = false;
    let mut tx_slot = 0u32;
    let mut rx_head = 0u32;
    let mut ticks = 0u32;

    println!("[nettcp] invio {} byte", payload_len);
    while ticks < MAX_TICKS {
        // Accoda finché il ring TX ha posto.
        while sent < payload_len {
            let tx_head = unsafe { read_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *const u32) };
            let tx_tail = unsafe { read_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *const u32) };
            if tx_tail.wrapping_sub(tx_head) >= NET_EP_RING_SLOTS as u32 {
                break;
            }
            let chunk = (payload_len - sent).min(NET_EP_SLOT_SIZE - SLOT_META);
            let slot = (tx_slot % NET_EP_RING_SLOTS as u32) as usize;
            write_slot(ring_base, slot, &dst, port, EV_DATA, &payload[sent..sent + chunk]);
            match civis::send(net_chan, NET_EP_TX, ep_id as u64, slot as u64) {
                Ok(r) if r.w0 == 0 => {}
                _ => {
                    println!("[nettcp] FAIL: TX rifiutato");
                    finish(false);
                }
            }
            sent += chunk;
            tx_slot = tx_slot.wrapping_add(1);
        }

        // Drena il ring RX.
        let rx_tail = unsafe { read_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        while rx_head != rx_tail {
            let slot = (rx_head % NET_EP_RING_SLOTS as u32) as usize;
            let p = unsafe {
                ring_base.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
            };
            let total = unsafe { read_volatile(p as *const u16) } as usize;
            if total >= SLOT_META {
                let event = unsafe { read_volatile(p.add(22) as *const u8) };
                let len = total - SLOT_META;
                match event {
                    EV_DATA => {
                        let n = len.min(BUF_MAX - received);
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                p.add(SLOT_META),
                                rx.as_mut_ptr().add(received),
                                n,
                            );
                        }
                        received += n;
                    }
                    EV_SYNACK => synack = true,
                    EV_RST => {
                        println!("[nettcp] FAIL: reset dal peer");
                        finish(false);
                    }
                    _ => {}
                }
            }
            unsafe {
                write_volatile(
                    ring_base.add(NET_EP_RX_RING_OFF) as *mut u32,
                    rx_head.wrapping_add(1),
                )
            };
            rx_head = rx_head.wrapping_add(1);
        }

        // Condizioni di completamento.
        if !dnstcp && received >= payload_len {
            break;
        }
        if dnstcp && received >= 2 {
            let qlen = u16::from_be_bytes([rx[0], rx[1]]) as usize;
            if received >= 2 + qlen.max(12) {
                break;
            }
        }

        ticks += 1;
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }

    if !synack {
        println!("[nettcp] FAIL: handshake non completato");
        finish(false);
    }

    if dnstcp {
        if received < 12 {
            println!("[nettcp] FAIL: risposta DNS assente ({} byte)", received);
            finish(false);
        }
        let qlen = u16::from_be_bytes([rx[0], rx[1]]) as usize;
        let qr = rx[4] & 0x80 != 0;
        let rcode = rx[5] & 0x0f;
        if !qr {
            println!("[nettcp] FAIL: risposta senza bit QR");
            finish(false);
        }
        println!(
            "[nettcp] DNS over TCP su rete reale: risposta {} byte (rcode={})",
            qlen, rcode
        );
    } else {
        if received != payload_len {
            println!("[nettcp] FAIL: echo incompleto ({}/{} byte)", received, payload_len);
            finish(false);
        }
        for i in 0..payload_len {
            if rx[i] != payload[i] {
                println!(
                    "[nettcp] FAIL: byte {} diverso (atteso {:#x}, ricevuto {:#x})",
                    i, payload[i], rx[i]
                );
                finish(false);
            }
        }
        println!("[nettcp] echo TCP su rete reale: {} byte identici", payload_len);
    }

    let _ = civis::send(net_chan, civis::NET_EP_CLOSE, ep_id as u64, 0);
    finish(true);
}

/// Indirizzo del default router (`NET_ADDR`).
fn router_addr(net_chan: u64) -> Option<[u8; 16]> {
    let r = civis::send(net_chan, NET_ADDR, NET_ADDR_ROUTER, 0).ok()?;
    if r.w0 == 0 && r.w1 == 0 {
        return None;
    }
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&r.w0.to_be_bytes());
    a[8..].copy_from_slice(&r.w1.to_be_bytes());
    Some(a)
}

/// Parsa `sys:nettcp.conf`: `<ipv6|auto> <porta> [echo|dnstcp]`.
fn parse_conf(data: &[u8]) -> Option<(Option<[u8; 16]>, u16, bool)> {
    let txt = core::str::from_utf8(data).ok()?;
    let mut it = txt.split_whitespace();
    let addr = it.next()?;
    let port: u16 = it.next()?.parse().ok()?;
    if port == 0 {
        return None;
    }
    let dnstcp = match it.next().unwrap_or("echo") {
        "echo" => false,
        "dnstcp" => true,
        _ => return None,
    };
    if addr == "auto" {
        return Some((None, port, dnstcp));
    }
    Some((Some(parse_ipv6(addr)?), port, dnstcp))
}

/// Parsa un IPv6 testuale (gruppi esadecimali, una sola `::`).
fn parse_ipv6(s: &str) -> Option<[u8; 16]> {
    let mut groups = [0u16; 8];
    let mut n = 0usize;
    let (mut head, mut tail) = (s, "");
    let mut gap = false;
    if let Some(i) = s.find("::") {
        head = &s[..i];
        tail = &s[i + 2..];
        gap = true;
    }
    for g in head.split(':') {
        if g.is_empty() || n >= 8 {
            return None;
        }
        groups[n] = u16::from_str_radix(g, 16).ok()?;
        n += 1;
    }
    if gap {
        let mut t = [0u16; 8];
        let mut tn = 0usize;
        for g in tail.split(':') {
            if g.is_empty() || tn >= 8 {
                return None;
            }
            t[tn] = u16::from_str_radix(g, 16).ok()?;
            tn += 1;
        }
        if n + tn > 7 {
            return None;
        }
        let mut full = [0u16; 8];
        full[..n].copy_from_slice(&groups[..n]);
        full[8 - tn..].copy_from_slice(&t[..tn]);
        groups = full;
    } else if n != 8 {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..8 {
        out[2 * i..2 * i + 2].copy_from_slice(&groups[i].to_be_bytes());
    }
    Some(out)
}

/// Query DNS tipo A per `name` (id fisso, RD=1).
fn build_dns_query(name: &[u8]) -> alloc::vec::Vec<u8> {
    let mut q = alloc::vec![0u8; 12];
    q[0..2].copy_from_slice(&0x4242u16.to_be_bytes());
    q[2..4].copy_from_slice(&0x0100u16.to_be_bytes());
    q[4..6].copy_from_slice(&1u16.to_be_bytes());
    let mut off = 12;
    for label in name.split(|b| *b == b'.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label);
    }
    q.push(0);
    q.extend_from_slice(&1u16.to_be_bytes()); // A
    q.extend_from_slice(&1u16.to_be_bytes()); // IN
    off += 0;
    let _ = off;
    q
}

/// Attende che `svc` sia registrato (fino a ~2 s).
fn wait_service(svc: civis::Service) -> Option<u64> {
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(svc) {
            if c > 0 {
                return Some(c as u64);
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    None
}

/// Scrive lo slot TX `slot`.
fn write_slot(ring_base: *mut u8, slot: usize, dst: &[u8; 16], dst_port: u16, event: u8, payload: &[u8]) {
    let total = SLOT_META + payload.len();
    let p = unsafe { ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(dst.as_ptr(), p.add(2), 16);
        write_volatile(p.add(18) as *mut u16, dst_port);
        write_volatile(p.add(20) as *mut u16, 0); // sorgente: la assegna il servizio
        write_volatile(p.add(22) as *mut u8, event);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(SLOT_META), payload.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            (slot + 1) as u32,
        )
    };
}

/// IPv6 in forma compatta (gruppi esadecimali separati da `:`).
fn fmt_addr(a: &[u8; 16]) -> alloc::string::String {
    use alloc::string::String;
    let mut s = String::new();
    for i in 0..8 {
        let g = u16::from_be_bytes([a[2 * i], a[2 * i + 1]]);
        if i > 0 {
            s.push(':');
        }
        s.push_str(&alloc::format!("{:x}", g));
    }
    s
}

fn finish(ok: bool) -> ! {
    if ok {
        println!("[nettcp] risultato: PASS");
    } else {
        println!("[nettcp] risultato: FAIL");
    }
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, if ok { 1 } else { 0 }, 0);
    civis::exit(if ok { 0 } else { 1 });
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[nettcp] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
