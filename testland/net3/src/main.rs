//! net3 — probe di rete TCP (N3, test fuori gate).
//!
//! Apre un endpoint TCP CONNECT verso `::1:8` (netecho), invia un segmento
//! SYN tramite il servizio `net` (che incapsula il payload applicativo),
//! valida la ricezione del SYN+ACK e segnala TEST_DONE ad init.
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

// Eventi trasportati nello slot ring (devono coincidere con userland/net).
const EV_DATA: u8 = 0;
const EV_SYN: u8 = 1;
const EV_SYNACK: u8 = 2;
const EV_FIN: u8 = 3;
const EV_RST: u8 = 4;

// Metadata slot: [len:2][addr:16][sport:2][dport:2][event:1][payload].
const SLOT_META: usize = 23; // 2+16+2+2+1

/// Indirizzo destinatario: `::1` (loopback).
const DST_ADDR: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];
/// Porta destinatario: echo server TCP (secondo endpoint di netecho).
const DST_PORT: u16 = 8;
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

    // Apre endpoint TCP CONNECT verso ::1:8.
    // local_port = 0 (ephemeral): net assegna una porta libera al client.
    let r = match civis::send(
        net_chan,
        NET_EP_OPEN,
        (NET_PROTO_TCP as u64) << 16,  // porta 0 = ephemeral
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

    // Scrive nello slot TX 0: solo indirizzo destino + evento SYN.
    // net costruirà il segmento TCP completo (header + checksum).
    let total = SLOT_META; // nessun payload per SYN
    let p = unsafe {
        ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR)
    };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(DST_ADDR.as_ptr(), p.add(2), 16);
        write_volatile(p.add(18) as *mut u16, DST_PORT);
        write_volatile(p.add(20) as *mut u16, local_port);
        write_volatile(p.add(22) as *mut u8, EV_SYN);
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

    // Aspetta risposta (SYN+ACK) nel RX ring.
    let mut rx_head: u32 = 0;
    let mut synack_ok = false;
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
            if total >= SLOT_META {
                let mut src_addr = [0u8; 16];
                unsafe {
                    core::ptr::copy_nonoverlapping(p.add(2), src_addr.as_mut_ptr(), 16);
                }
                let src_port = unsafe { read_volatile(p.add(18) as *const u16) };
                let dst_port = unsafe { read_volatile(p.add(20) as *const u16) };
                let event = unsafe { read_volatile(p.add(22) as *const u8) };

                println!(
                    "[net3] RX: da porta={} a porta={} evento={} len={}",
                    src_port, dst_port, event, total - SLOT_META
                );

                // Valida: SYN+ACK (evento=2) da porta DST_PORT.
                // Nota: nel nuovo formato slot non c'è campo ack, quindi
                // validiamo solo evento e porta sorgente.
                if src_port == DST_PORT && event == EV_SYNACK {
                    synack_ok = true;
                    println!("[net3] SYN+ACK valido ricevuto da porta 8");
                } else {
                    println!(
                        "[net3] segmento ricevuto: porta={} evento={} (ignoro)",
                        src_port, event
                    );
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
        if retries % 1000 == 0 {
            println!("[net3] in attesa SYN+ACK... retries={}", retries);
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }

    if synack_ok {
        println!("[net3] handshake TCP completato: PASS");
    } else {
        println!("[net3] SYN+ACK non ricevuto: FAIL");
        all_ok = false;
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
