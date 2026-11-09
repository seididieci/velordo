//! dns — probe di rete DNS (N5, test fuori gate).
//!
//! Apre un endpoint UDP presso il servizio `dns`, invia una query DNS per
//! `example.com` tipo A e valida che la risposta contenga l'indirizzo
//! 93.184.216.34. Segnala TEST_DONE ad init.
//! Eseguito solo con `net=1`.

#![no_std]
#![no_main]

use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// ── Costanti endpoint DNS (da civis) ─────────────────────────────────
use civis::{
    DNS_CLOSE, DNS_OPEN, DNS_TX, DNS_PORT, NET_EP_FLAG_EPHEMERAL,
    NET_EP_RX_RING_OFF, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE,
    NET_EP_TX_RING_OFF, NET_PROTO_DNS, NET_PROTO_UDP,
};

// Eventi slot DNS.
const EV_DATA: u8 = 0;

// Metadata slot SIIT/DNS: [len:2][dst_ipv4:4][dst_port:2][src_port:2][evento:1][payload].
const SLOT_META: usize = 11; // 2+4+2+2+1

/// Numero massimo di tentativi di ricezione.
const MAX_RETRIES: u32 = 100;

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

    // Costruisce query DNS per example.com tipo A.
    let mut query = [0u8; 512];
    let mut off = 0;
    // Header DNS: id=0x1234, flags=0x0100 (RD), qdcount=1
    query[0..2].copy_from_slice(&0x1234u16.to_be_bytes());
    query[2..4].copy_from_slice(&0x0100u16.to_be_bytes());
    query[4..6].copy_from_slice(&1u16.to_be_bytes());
    query[6..8].copy_from_slice(&0u16.to_be_bytes());
    query[8..10].copy_from_slice(&0u16.to_be_bytes());
    query[10..12].copy_from_slice(&0u16.to_be_bytes());
    off = 12;
    // Nome: example.com -> 7 example 3 com 0
    query[off] = 7; off += 1;
    query[off..off + 7].copy_from_slice(b"example"); off += 7;
    query[off] = 3; off += 1;
    query[off..off + 3].copy_from_slice(b"com"); off += 3;
    query[off] = 0; off += 1;
    // Tipo A, Classe IN
    query[off..off + 2].copy_from_slice(&1u16.to_be_bytes()); off += 2;
    query[off..off + 2].copy_from_slice(&1u16.to_be_bytes()); off += 2;
    let query_len = off;

    // Scrive nello slot TX 0: payload = query DNS.
    // Formato: [len:2][dst_ipv4:4][dst_port:2][src_port:2][evento:1][payload].
    let total = SLOT_META + query_len;
    if total > NET_EP_SLOT_SIZE {
        println!("[dns] FAIL: payload troppo grande per lo slot");
        finish(false);
    }
    let p = unsafe {
        ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR)
    };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        // dst_ipv4 = indirizzo del servizio dns (usa 127.0.0.1 come placeholder)
        let dst_ip: [u8; 4] = [127, 0, 0, 1];
        dst_ip.as_ptr().copy_to(p.add(2), 4);
        write_volatile(p.add(6) as *mut u16, DNS_PORT);
        write_volatile(p.add(8) as *mut u16, local_port);
        write_volatile(p.add(10) as *mut u8, EV_DATA);
        query[..query_len].as_ptr().copy_to(p.add(11), query_len);
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            1, // tail = 1
        )
    };

    // Notifica dns di processare la query (slot 0).
    let reply = civis::send(dns_chan, DNS_TX, ep_id as u64, 0);
    match reply {
        Ok(r) if r.w0 == 0 => {
            println!("[dns] query DNS accettata da dns: PASS");
        }
        Ok(r) => {
            println!("[dns] FAIL: dns ha rifiutato la query (w0={})", r.w0);
            all_ok = false;
        }
        Err(_) => {
            println!("[dns] FAIL: invio query fallito (IPC)");
            all_ok = false;
        }
    }

    // Aspetta risposta nel RX ring.
    let mut rx_head: u32 = 0;
    let mut dns_ok = false;
    let mut retries = 0u32;
    for _ in 0..MAX_RETRIES {
        retries += 1;
        let rx_tail = unsafe { read_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        if rx_head != rx_tail {
            let slot = (rx_head % NET_EP_RING_SLOTS as u32) as usize;
            let p = unsafe {
                ring_base.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
            };
            let total = unsafe { read_volatile(p as *const u16) } as usize;
            if total > 11 {
                let mut src_ipv4 = [0u8; 4];
                unsafe {
                    core::ptr::copy_nonoverlapping(p.add(2), src_ipv4.as_mut_ptr(), 4);
                }
                let src_port = unsafe { read_volatile(p.add(6) as *const u16) };
                let _dst_port = unsafe { read_volatile(p.add(8) as *const u16) };
                let evento = unsafe { read_volatile(p.add(10) as *const u8) };

                println!(
                    "[dns] RX: da={}.{}.{}.{}:{} evento={} len={}",
                    src_ipv4[0], src_ipv4[1], src_ipv4[2], src_ipv4[3],
                    src_port, evento, total - 11
                );

                if src_port == DNS_PORT && evento == EV_DATA {
                    // Verifica che la risposta contenga 93.184.216.34
                    let payload = unsafe {
                        core::slice::from_raw_parts(p.add(11), total - 11)
                    };
                    let mut found = false;
                    for i in 0..payload.len().saturating_sub(3) {
                        if payload[i] == 93 && payload[i + 1] == 184
                            && payload[i + 2] == 216 && payload[i + 3] == 34
                        {
                            found = true;
                            break;
                        }
                    }
                    if found {
                        dns_ok = true;
                        println!("[dns] risposta DNS valida (contiene 93.184.216.34)");
                    } else {
                        println!("[dns] risposta DNS ricevuta ma senza indirizzo atteso");
                    }
                }
            }
            // Avanza head.
            unsafe {
                write_volatile(
                    ring_base.add(NET_EP_RX_RING_OFF) as *mut u32,
                    rx_head.wrapping_add(1),
                )
            };
            rx_head = rx_head.wrapping_add(1);
            break;
        }
        if retries % 100 == 0 {
            println!("[dns] in attesa risposta DNS... retries={}", retries);
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }

    if dns_ok {
        println!("[dns] risoluzione DNS completata: PASS");
    } else {
        println!("[dns] risposta DNS non ricevuta o non valida: FAIL");
        all_ok = false;
    }

    // Chiude l'endpoint.
    let _ = civis::send(dns_chan, DNS_CLOSE, ep_id as u64, 0);

    finish(all_ok);
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
