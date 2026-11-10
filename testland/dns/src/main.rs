//! dns — probe di rete DNS (N5+N6.0, test fuori gate).
//!
//! Apre un endpoint UDP presso il servizio `dns` e valida due risoluzioni:
//! 1. `example.com` A → 93.184.216.34 (tabella statica N5);
//! 2. `upstream.test` A → 198.51.100.7 (inoltro upstream, N6.0): PASS se
//!    risponde, SKIP se il resolver non ha upstream configurato (NXDOMAIN),
//!    FAIL se l'upstream è configurato ma non risponde (SERVFAIL).
//! Segnala TEST_DONE ad init. Eseguito solo con `net=1`.

#![no_std]
#![no_main]

use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// ── Costanti endpoint DNS (da civis) ─────────────────────────────────
use civis::{
    DNS_CLOSE, DNS_OPEN, DNS_PORT, DNS_TX, NET_EP_FLAG_EPHEMERAL,
    NET_EP_RX_RING_OFF, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE,
    NET_EP_TX_RING_OFF, NET_PROTO_DNS, NET_PROTO_UDP,
};

// Eventi slot DNS.
const EV_DATA: u8 = 0;

// Metadata slot SIIT/DNS: [len:2][dst_ipv4:4][dst_port:2][src_port:2][evento:1][payload].
const SLOT_META: usize = 11; // 2+4+2+2+1

/// Attesa massima di una risposta locale (tick da 10 ms).
const MAX_TICKS_LOCAL: u32 = 100;
/// Attesa massima di una risposta inoltrata all'upstream (timeout config 2 s).
const MAX_TICKS_UPSTREAM: u32 = 400;

/// Rcode DNS.
const RCODE_SERVFAIL: u16 = 2;
const RCODE_NXDOMAIN: u16 = 3;

/// Esito di una risoluzione.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Risposta ricevuta con l'indirizzo atteso.
    Match,
    /// Risposta ricevuta, NXDOMAIN: nessun upstream configurato.
    NoUpstream,
    /// Risposta ricevuta ma senza l'indirizzo atteso (o SERVFAIL).
    Wrong,
    /// Nessuna risposta entro il timeout.
    Timeout,
}

/// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let mut all_ok = true;

    // Risolve il servizio dns.
    let mut dns_chan = 0u64;
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::Dns) {
            if c > 0 {
                dns_chan = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if dns_chan == 0 {
        println!("[dns] FAIL: servizio dns non trovato");
        finish(false);
    }

    // Apre endpoint UDP presso il servizio dns.
    let r = match civis::send(
        dns_chan,
        DNS_OPEN,
        (NET_PROTO_UDP as u64) << 16 | DNS_PORT as u64,
        NET_EP_FLAG_EPHEMERAL as u64,
    ) {
        Ok(r) => r,
        Err(_) => {
            println!("[dns] FAIL: open fallito (IPC)");
            finish(false);
        }
    };
    if r.w0 == !0u64 {
        println!("[dns] FAIL: open rifiutato (policy)");
        finish(false);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let local_port = (r.w1 >> 32) as u16;
    println!(
        "[dns] endpoint aperto id={} porta={} ring={}",
        ep_id, local_port, ring_id
    );

    // Mappa lo shm dell'endpoint.
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            println!("[dns] FAIL: shm map fallito");
            finish(false);
        }
    } as *mut u8;

    // Inizializza head/tail.
    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    // ── 1) Tabella statica N5 ──────────────────────────────────────
    let mut query = [0u8; 128];
    let qlen = build_a_query(&mut query, 0x1234, b"example.com");
    let mut rx_head = 0u32;
    let static_a: [u8; 4] = [93, 184, 216, 34];
    let out = ask(
        dns_chan,
        ep_id,
        ring_base,
        &mut rx_head,
        0,
        &query[..qlen],
        &static_a,
        MAX_TICKS_LOCAL,
    );
    match out {
        Some((len, rcode, matches)) => {
            println!("[dns] risposta statica: len={} rcode={}", len, rcode);
            if matches {
                println!("[dns] risoluzione DNS completata: PASS");
            } else {
                println!("[dns] risposta DNS non valida: FAIL");
                all_ok = false;
            }
        }
        None => {
            println!("[dns] risposta DNS non ricevuta: FAIL");
            all_ok = false;
        }
    }

    // ── 2) Inoltro upstream (N6.0), se configurato ─────────────────
    let qlen = build_a_query(&mut query, 0x2345, b"upstream.test");
    match ask(
        dns_chan,
        ep_id,
        ring_base,
        &mut rx_head,
        1,
        &query[..qlen],
        &UPSTREAM_A,
        MAX_TICKS_UPSTREAM,
    ) {
        Some((len, rcode, matches)) => {
            println!("[dns] risposta upstream: len={} rcode={}", len, rcode);
            if matches {
                println!("[dns] upstream: PASS");
            } else if rcode == RCODE_NXDOMAIN {
                // Nessun upstream configurato: comportamento N5, non un errore.
                println!("[dns] upstream: SKIP (nessun upstream configurato)");
            } else if rcode == RCODE_SERVFAIL {
                println!("[dns] upstream: FAIL (SERVFAIL)");
                all_ok = false;
            } else {
                println!("[dns] upstream: FAIL (risposta senza indirizzo atteso)");
                all_ok = false;
            }
        }
        None => {
            println!("[dns] upstream: FAIL (nessuna risposta)");
            all_ok = false;
        }
    }

    // Chiude l'endpoint.
    let _ = civis::send(dns_chan, DNS_CLOSE, ep_id as u64, 0);

    finish(all_ok);
}

/// Indirizzo A atteso per `upstream.test` (TEST-NET-2, RFC 5737).
const UPSTREAM_A: [u8; 4] = [198, 51, 100, 7];

/// Costruisce una query DNS tipo A per `name` e ritorna la lunghezza.
fn build_a_query(out: &mut [u8], id: u16, name: &[u8]) -> usize {
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // RD
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // qdcount
    out[6..12].fill(0);
    let mut off = 12;
    for label in name.split(|b| *b == b'.') {
        out[off] = label.len() as u8;
        off += 1;
        out[off..off + label.len()].copy_from_slice(label);
        off += label.len();
    }
    out[off] = 0;
    off += 1;
    out[off..off + 2].copy_from_slice(&1u16.to_be_bytes()); // A
    off += 2;
    out[off..off + 2].copy_from_slice(&1u16.to_be_bytes()); // IN
    off += 2;
    off
}

/// Scrive la query nello slot TX `slot`, la invia e attende la risposta.
/// Ritorna `(len, rcode, contiene_l'indirizzo_atteso)`.
fn ask(
    dns_chan: u64,
    ep_id: u16,
    ring_base: *mut u8,
    rx_head: &mut u32,
    slot: usize,
    query: &[u8],
    want: &[u8; 4],
    max_ticks: u32,
) -> Option<(usize, u16, bool)> {
    let total = SLOT_META + query.len();
    if total > NET_EP_SLOT_SIZE {
        return None;
    }
    let p = unsafe { ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        [127u8, 0, 0, 1].as_ptr().copy_to(p.add(2), 4);
        write_volatile(p.add(6) as *mut u16, DNS_PORT);
        write_volatile(p.add(8) as *mut u16, 0); // sorgente: la assegna il servizio
        write_volatile(p.add(10) as *mut u8, EV_DATA);
        query.as_ptr().copy_to(p.add(SLOT_META), query.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            (slot + 1) as u32,
        )
    }
    if civis::send(dns_chan, DNS_TX, ep_id as u64, slot as u64).is_err() {
        return None;
    }

    for tick in 0..max_ticks {
        let tail = unsafe { read_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        if *rx_head != tail {
            let idx = (*rx_head % NET_EP_RING_SLOTS as u32) as usize;
            let q = unsafe {
                ring_base.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + idx * NET_EP_SLOT_SIZE)
            };
            let total = unsafe { read_volatile(q as *const u16) } as usize;
            let mut out = None;
            if total > SLOT_META && total <= NET_EP_SLOT_SIZE {
                let payload = unsafe { core::slice::from_raw_parts(q.add(SLOT_META), total - SLOT_META) };
                if payload.len() >= 12 {
                    let rcode = u16::from_be_bytes([payload[2], payload[3]]) & 0xF;
                    out = Some((payload.len(), rcode, contains(want, payload)));
                }
            }
            // Consuma lo slot (il cliente avanza la head del ring RX).
            unsafe {
                write_volatile(
                    ring_base.add(NET_EP_RX_RING_OFF) as *mut u32,
                    rx_head.wrapping_add(1),
                )
            };
            *rx_head = rx_head.wrapping_add(1);
            return out;
        }
        if tick % 50 == 0 && tick > 0 {
            println!("[dns] in attesa risposta... tick={}", tick);
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    None
}

/// `true` se `payload` contiene l'indirizzo atteso (4 byte consecutivi).
fn contains(want: &[u8; 4], payload: &[u8]) -> bool {
    if payload.len() < 4 {
        return false;
    }
    for i in 0..=payload.len() - 4 {
        if &payload[i..i + 4] == want {
            return true;
        }
    }
    false
}

/// Termina il probe e segnala TEST_DONE ad init.
fn finish(ok: bool) -> ! {
    if ok {
        println!("[dns] risultato: PASS");
    } else {
        println!("[dns] risultato: FAIL");
    }
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, if ok { 1 } else { 0 }, 0);
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[dns] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}

static FUTEX: u32 = 0;
