//! netudp — probe di rete UDP (N2, test fuori gate).
//!
//! Apre un endpoint UDP ephemeral presso il servizio `net`, invia un
//! datagramma a `::1:7` (netecho), valida l'eco ricevuta e segnala
//! TEST_DONE ad init. Eseguito solo con `net=1`.
//!
//! Dal N7.0 apertura, slot e cursori sono in `civis::net` (ADR-0074): qui
//! resta solo la politica del probe.

#![no_std]
#![no_main]

use civis::net::{Endpoint, Event, Flow, Proto, SendError, PAYLOAD_MAX};
use civis::println;

static FUTEX: u32 = 0;

/// Indirizzo destinatario: `::1` (loopback).
const DST_ADDR: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
/// Porta destinatario: echo server.
const DST_PORT: u16 = 7;
/// Payload di test.
const TEST_PAYLOAD: &[u8] = b"velordo-netudp-probe";
/// Numero massimo di tentativi di ricezione.
const MAX_RETRIES: u32 = 2000;

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
        println!("[netudp] FAIL: servizio net non trovato");
        finish(false);
    }

    // Apre endpoint UDP ephemeral.
    let mut ep = match Endpoint::open(net_chan, Proto::Udp, Flow::Ephemeral) {
        Ok(ep) => ep,
        Err(_) => {
            println!("[netudp] FAIL: open fallito (IPC o policy)");
            finish(false);
        }
    };
    println!(
        "[netudp] endpoint aperto id={} porta={}",
        ep.id(),
        ep.local_port()
    );

    // Invia datagramma a ::1:7.
    match ep.send(DST_ADDR, DST_PORT, Event::Data, TEST_PAYLOAD) {
        Ok(()) => println!(
            "[netudp] datagramma inviato a ::1:7 (slot 0, len={})",
            TEST_PAYLOAD.len()
        ),
        Err(SendError::Full) => {
            println!("[netudp] FAIL: ring TX pieno");
            finish(false);
        }
        Err(_) => {
            println!("[netudp] FAIL: invio fallito (servizio o payload)");
            finish(false);
        }
    }

    // Aspetta l'eco sul ring RX.
    let mut buf = [0u8; PAYLOAD_MAX];
    let mut echo_ok = false;
    let mut retries = 0u32;
    while retries < MAX_RETRIES {
        retries += 1;
        if let Some(r) = ep.recv(&mut buf) {
            println!(
                "[netudp] RX slot totale={} payload_len_atteso={}",
                r.len,
                TEST_PAYLOAD.len()
            );
            println!(
                "[netudp] RX da porta={} dst_port={} payload_len={}",
                r.port, r.local, r.len
            );
            if r.event != Event::Data {
                println!("[netudp] evento {:?} ignorato", r.event);
            } else if r.addr != DST_ADDR || r.port != DST_PORT {
                println!(
                    "[netudp] mittente inatteso ({}:{}), ignoro",
                    r.addr[15], r.port
                );
            } else if r.truncated || &buf[..r.len] != TEST_PAYLOAD {
                println!("[netudp] payload NON corrispondente");
            } else {
                echo_ok = true;
                println!("[netudp] payload corrispondente: echo valido");
            }
            break;
        }
        if retries % 500 == 0 {
            println!("[netudp] in attesa echo... retries={}", retries);
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }

    println!(
        "[netudp] fine attesa: retries={} echo_ok={}",
        retries, echo_ok
    );

    if echo_ok {
        println!("[netudp] echo da ::1:7 ricevuto e validato: PASS");
    } else {
        println!("[netudp] echo da ::1:7 non ricevuto o non valido: FAIL");
        all_ok = false;
    }

    // Statistiche (diagnostica) e chiusura.
    let _ = ep.stats();
    ep.close();

    finish(all_ok);
}

/// Termina il probe e segnala TEST_DONE ad init.
fn finish(ok: bool) -> ! {
    if ok {
        println!("[netudp] risultato: PASS");
    } else {
        println!("[netudp] risultato: FAIL");
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
    let _ = civis::print_string(b"[netudp] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
