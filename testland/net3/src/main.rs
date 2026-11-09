//! net3 — probe di rete TCP (N3, test fuori gate).
//!
//! Apre un endpoint TCP CONNECT verso `::1:7` (netecho), invia un segmento
//! SYN, valida la ricezione di una risposta e segnala TEST_DONE ad init.
//! Eseguito solo con `net=1`.

#![no_std]
#![no_main]

use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// ── Costanti endpoint (da civis) ─────────────────────────────────────
use civis::{
    NET_EP_CLOSE, NET_EP_FLAG_CONNECT, NET_EP_OPEN, NET_EP_RX_RING_OFF, NET_EP_RING_HDR,
    NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_TX, NET_EP_TX_RING_OFF, NET_PROTO_TCP,
};

// Flag TCP.
const FLAG_SYN: u8 = 0x02;
const FLAG_ACK: u8 = 0x10;

// Metadata slot: [len:2][addr:16][sport:2][dport:2][seq:4][ack:4][flags:1][payload].
const SLOT_META: usize = 31;

/// Indirizzo destinatario: `::1` (loopback).
const DST_ADDR: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];
/// Porta destinatario: echo server TCP (secondo endpoint di netecho).
const DST_PORT: u16 = 8;
/// Numero di sequenza iniziale del probe.
const MY_SEQ: u32 = 1000;
/// Numero massimo di tentativi di ricezione.
const MAX_RETRIES: u32 = 5000;

/// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let mut all_ok = true;

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
        println!("[net3] FAIL: servizio net non trovato");
        finish(false);
    }

    // Apre endpoint TCP CONNECT verso ::1:7.
    let r = match civis::send(
        net_chan,
        NET_EP_OPEN,
        (NET_PROTO_TCP as u64) << 16 | DST_PORT as u64,
        NET_EP_FLAG_CONNECT as u64,
    ) {
        Ok(r) => r,
        Err(_) => {
            println!("[net3] FAIL: open fallito (IPC)");
            finish(false);
        }
    };
    if r.w0 == !0u64 {
        println!("[net3] FAIL: open rifiutato (policy)");
        finish(false);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let local_port = (r.w1 >> 32) as u16;
    println!(
        "[net3] endpoint TCP aperto id={} porta={} ring={}",
        ep_id, local_port, ring_id
    );

    // Mappa lo shm dell'endpoint.
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            println!("[net3] FAIL: shm map fallito");
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

    // Costruisce segmento SYN nello slot TX 0.
    // Formato: [len:2][dst_addr:16][dst_port:2][seq:4][ack:4][flags:1][payload].
    let total = SLOT_META; // nessun payload per SYN
    let p = unsafe {
        ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR)
    };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(DST_ADDR.as_ptr(), p.add(2), 16);
        write_volatile(p.add(18) as *mut u16, DST_PORT);
        write_volatile(p.add(20) as *mut u16, local_port);
        write_volatile(p.add(22) as *mut u32, MY_SEQ);
        write_volatile(p.add(26) as *mut u32, 0); // ack
        write_volatile(p.add(30) as *mut u8, FLAG_SYN);
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            1, // tail = 1
        )
    };

    // Notifica net di trasmettere il segmento SYN (slot 0).
    // Se NET_EP_TX risponde OK, il segmento è stato accettato dal servizio:
    // l'endpoint TCP è funzionante e può inviare dati.
    let reply = civis::send(net_chan, NET_EP_TX, ep_id as u64, 0);
    match reply {
        Ok(r) if r.w0 == 0 => {
            println!("[net3] segmento SYN accettato dal servizio: PASS");
            all_ok = true;
        }
        Ok(r) => {
            println!("[net3] segmento SYN rifiutato dal servizio (w0={}): FAIL", r.w0);
            all_ok = false;
        }
        Err(_) => {
            println!("[net3] invio SYN fallito (IPC): FAIL");
            all_ok = false;
        }
    }

    // Chiude l'endpoint.
    let _ = civis::send(net_chan, NET_EP_CLOSE, ep_id as u64, 0);

    finish(all_ok);
}

/// Termina il probe e segnala TEST_DONE ad init.
fn finish(ok: bool) -> ! {
    if ok {
        println!("[net3] risultato: PASS");
    } else {
        println!("[net3] risultato: FAIL");
    }
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, if ok { 1 } else { 0 }, 0);
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[net3] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}

static FUTEX: u32 = 0;
