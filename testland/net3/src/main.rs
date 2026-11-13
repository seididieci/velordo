//! net3 — probe di rete TCP (N3, test fuori gate).
//!
//! Apre un endpoint TCP CONNECT verso `::1:8` (netecho), invia un payload
//! applicativo tramite il servizio `net` (che incapsula e fa l'handshake),
//! valida SYN+ACK ed eco e segnala TEST_DONE ad init.
//! Eseguito solo con `net=1`.
//!
//! Dal N7.0 ring, slot ed eventi sono in `civis::net` (ADR-0074).

#![no_std]
#![no_main]

use civis::net::{Endpoint, Event, Flow, Proto, SendError, PAYLOAD_MAX};
use civis::println;

static FUTEX: u32 = 0;

/// Indirizzo destinatario: `::1` (loopback).
const DST_ADDR: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
/// Porta destinatario: echo server TCP (secondo endpoint di netecho).
const DST_PORT: u16 = 8;
/// Payload del probe.
const HELLO: &[u8] = b"net3-hello";
/// Numero massimo di tentativi di ricezione.
const MAX_RETRIES: u32 = 5000;

// Entry point.
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

    // Apre endpoint TCP CONNECT verso ::1:8 (porta locale effimera).
    let mut ep = match Endpoint::open(net_chan, Proto::Tcp, Flow::Connect) {
        Ok(ep) => ep,
        Err(_) => {
            println!("[net3] FAIL: open fallito (IPC o policy)");
            finish(false);
        }
    };
    println!(
        "[net3] endpoint TCP aperto id={} porta={}",
        ep.id(),
        ep.local_port()
    );

    // N6.1c: il handshake è del core. Inviamo un payload: il servizio apre la
    // connessione al primo `NET_EP_TX`, il core fa SYN/SYN+ACK e poi spedisce
    // i dati. Il riscontro dell'handshake arriva come evento `SynAck`.
    match ep.send(DST_ADDR, DST_PORT, Event::Data, HELLO) {
        Ok(()) => println!("[net3] payload accettato dal servizio: PASS"),
        Err(SendError::Full) => {
            println!("[net3] payload rifiutato dal servizio (ring TX pieno): FAIL");
            all_ok = false;
        }
        Err(_) => {
            println!("[net3] invio payload fallito (IPC): FAIL");
            all_ok = false;
        }
    }

    // Attende SYNACK (handshake) e poi l'eco dei dati.
    let mut buf = [0u8; PAYLOAD_MAX];
    let mut synack_ok = false;
    let mut echo_ok = false;
    let mut retries = 0u32;
    while retries < MAX_RETRIES && !(synack_ok && echo_ok) {
        retries += 1;
        if let Some(r) = ep.recv(&mut buf) {
            println!(
                "[net3] RX: da porta={} evento={:?} len={}",
                r.port, r.event, r.len
            );
            if r.addr == DST_ADDR && r.port == DST_PORT {
                match r.event {
                    Event::SynAck => {
                        synack_ok = true;
                        println!("[net3] handshake TCP completato (SYNACK)");
                    }
                    Event::Data => {
                        if !r.truncated && &buf[..r.len] == HELLO {
                            echo_ok = true;
                            println!("[net3] eco TCP valido ({} byte)", r.len);
                        }
                    }
                    Event::Rst => println!("[net3] reset dal peer"),
                    _ => {}
                }
            }
            continue;
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
    if echo_ok {
        println!("[net3] echo TCP end-to-end: PASS");
    } else {
        println!("[net3] echo TCP non ricevuto: FAIL");
        all_ok = false;
    }

    ep.close();

    finish(all_ok);
}

/// Termina il probe e segnala TEST_DONE ad init.
fn finish(ok: bool) -> ! {
    if ok {
        println!("[net3] risultato: PASS");
    } else {
        println!("[net3] risultato: FAIL");
    }
    let _ = civis::send(
        civis::CHANNEL_PARENT,
        civis::TEST_DONE,
        if ok { 1 } else { 0 },
        0,
    );
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[net3] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
