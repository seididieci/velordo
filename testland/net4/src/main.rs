//! net4 — probe di rete IPv4/SIIT (N4, test fuori gate).
//!
//! Apre un endpoint UDP presso il servizio `siit`, invia un payload a un
//! indirizzo IPv4 (in formato SIIT 64:ff9b::/96) e valida che il
//! traduttore lo abbia accettato. Segnala TEST_DONE ad init.
//! Eseguito solo con `net=1`.

#![no_std]
#![no_main]

use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// ── Costanti endpoint SIIT (da civis) ─────────────────────────────────
use civis::{
    NET_EP_FLAG_EPHEMERAL, NET_EP_OPEN, NET_EP_RX_RING_OFF, NET_EP_RING_HDR,
    NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_TX, NET_EP_TX_RING_OFF,
    NET_PROTO_IPV4, SIIT_EP_CLOSE, SIIT_EP_OPEN, SIIT_EP_TX,
};

// Eventi slot SIIT.
const EV_DATA: u8 = 0;

// Metadata slot SIIT: [len:2][dst_ipv4:4][dst_port:2][src_port:2][evento:1][payload].
const SLOT_META: usize = 11; // 2+4+2+2+1

/// Indirizzo IPv4 destinatario (10.0.0.1, mappato in IPv6 SIIT).
const DST_IPV4: [u8; 4] = [10, 0, 0, 1];
/// Porta destinataria.
const DST_PORT: u16 = 53;
/// Numero massimo di tentativi.
const MAX_RETRIES: u32 = 100;

/// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let mut all_ok = true;

    // Risolve il servizio siit.
    let mut siit_chan = 0u64;
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::Siit) {
            if c > 0 {
                siit_chan = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if siit_chan == 0 {
        println!("[net4] FAIL: servizio siit non trovato");
        finish(false);
    }

    // Apre endpoint UDP ephemeral presso siit.
    let r = match civis::send(
        siit_chan,
        SIIT_EP_OPEN,
        (NET_PROTO_IPV4 as u64) << 16, // porta 0 = ephemeral
        NET_EP_FLAG_EPHEMERAL as u64,
    ) {
        Ok(r) => r,
        Err(_) => {
            println!("[net4] FAIL: open fallito (IPC)");
            finish(false);
        }
    };
    if r.w0 == !0u64 {
        println!("[net4] FAIL: open rifiutato (policy)");
        finish(false);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let local_port = (r.w1 >> 32) as u16;
    println!(
        "[net4] endpoint SIIT aperto id={} porta={} ring={}",
        ep_id, local_port, ring_id
    );

    // Mappa lo shm dell'endpoint.
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            println!("[net4] FAIL: shm map fallito");
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

    // Scrive nello slot TX 0: payload + destinatario IPv4.
    // Formato: [len:2][dst_ipv4:4][dst_port:2][src_port:2][evento:1][payload].
    let payload: &[u8] = b"net4-siiit-probe";
    let total = SLOT_META + payload.len();
    if total > NET_EP_SLOT_SIZE {
        println!("[net4] FAIL: payload troppo grande per lo slot");
        finish(false);
    }
    let p = unsafe {
        ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR)
    };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(DST_IPV4.as_ptr(), p.add(2), 4);
        write_volatile(p.add(6) as *mut u16, DST_PORT);
        write_volatile(p.add(8) as *mut u16, local_port);
        write_volatile(p.add(10) as *mut u8, EV_DATA);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(11), payload.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            1, // tail = 1
        )
    };

    // Notifica siit di tradurre e inviare (slot 0).
    let reply = civis::send(siit_chan, SIIT_EP_TX, ep_id as u64, 0);
    match reply {
        Ok(r) if r.w0 == 0 => {
            println!(
                "[net4] payload IPv4 accettato da siit: PASS (dst={}.{}.{}.{}:{})",
                DST_IPV4[0], DST_IPV4[1], DST_IPV4[2], DST_IPV4[3], DST_PORT
            );
            all_ok = true;
        }
        Ok(r) => {
            println!(
                "[net4] FAIL: siit ha rifiutato il payload (w0={})",
                r.w0
            );
            all_ok = false;
        }
        Err(_) => {
            println!("[net4] FAIL: invio a siit fallito (IPC)");
            all_ok = false;
        }
    }

    // Chiude l'endpoint.
    let _ = civis::send(siit_chan, SIIT_EP_CLOSE, ep_id as u64, 0);

    finish(all_ok);
}

/// Termina il probe e segnala TEST_DONE ad init.
fn finish(ok: bool) -> ! {
    if ok {
        println!("[net4] risultato: PASS");
    } else {
        println!("[net4] risultato: FAIL");
    }
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, if ok { 1 } else { 0 }, 0);
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[net4] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}

static FUTEX: u32 = 0;
