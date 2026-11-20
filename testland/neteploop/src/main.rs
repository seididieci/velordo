//! neteploop — ciclo di vita degli endpoint (regressione, test fuori gate).
//!
//! Apre e chiude endpoint in serie **senza morire** fra un ciclo e l'altro,
//! via broker (`netbroker`, ADR-0080 fase 6) verso `netecho`:
//!
//! - 24 cicli TCP verso `::1:8`, metà chiusi in modo ordinato (FIN) e metà
//!   **bruschi** (`NET_EP_CLOSE` senza FIN);
//! - 5 cicli UDP verso `::1:7`.
//!
//! Serve a una cosa sola: provare che ogni ciclo **restituisce** le risorse —
//! l'endpoint nel servizio, il figlio accettato dal listener e la regione shm
//! del ring. Prima del fix ogni endpoint consumava una regione per sempre
//! (`munmap` con una lunghezza non multipla di pagina falliva in silenzio) e
//! il 17° endpoint di un boot non si apriva più: il probe TLS falliva con
//! "endpoint TCP non aperto" senza una sola riga di log.
//!
//! Il probe è `tst` (`Untrusted`): passa dal broker, quindi prova anche il
//! percorso `BROKER_CLOSE`/`free` e non solo il teardown alla morte.
//!
//! Eseguito solo con `net=1` (vedi `scripts/run-net-tests.sh`).

#![no_std]
#![no_main]

use civis::broker::{BrokerEndpoint, Event, Flow, Proto};
use civis::println;

static FUTEX: u32 = 0;

/// Indirizzo di `netecho` (loopback).
const DST_ADDR: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
/// Porta TCP di `netecho`.
const TCP_PORT: u16 = 8;
/// Porta UDP di `netecho`.
const UDP_PORT: u16 = 7;
/// Payload del probe.
const PAYLOAD: &[u8] = b"neteploop";
/// Cicli TCP: più del doppio degli slot shm storici (`SHM_MAX = 16`), così un
/// leak si vede subito.
const TCP_CYCLES: usize = 24;
/// Cicli UDP.
const UDP_CYCLES: usize = 5;
/// Tick massimi di attesa per un'eco (10 ms per tick).
const MAX_TICKS: u32 = 300;

// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let mut broker_chan = 0u64;
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::NetBroker) {
            if c > 0 {
                broker_chan = c as u64;
                break;
            }
        }
        tick();
    }
    if broker_chan == 0 {
        println!("[neteploop] FAIL: servizio netbroker non trovato");
        finish(false);
    }

    let mut ok = true;
    let mut tcp_done = 0usize;
    for i in 0..TCP_CYCLES {
        // Metà ordinati (FIN), metà bruschi (solo CLOSE): i due percorsi di
        // morte della connessione devono liberare entrambi il figlio.
        let graceful = i % 2 == 0;
        match tcp_cycle(broker_chan, graceful) {
            Ok(()) => tcp_done += 1,
            Err(why) => {
                println!("[neteploop] ciclo TCP {} fallito: {}", i, why);
                ok = false;
                break;
            }
        }
    }
    println!(
        "[neteploop] cicli TCP: {}/{} ({})",
        tcp_done,
        TCP_CYCLES,
        if ok { "aperti e chiusi" } else { "INTERROTTO" }
    );

    let mut udp_done = 0usize;
    if ok {
        for i in 0..UDP_CYCLES {
            match udp_cycle(broker_chan) {
                Ok(()) => udp_done += 1,
                Err(why) => {
                    println!("[neteploop] ciclo UDP {} fallito: {}", i, why);
                    ok = false;
                    break;
                }
            }
        }
        println!("[neteploop] cicli UDP: {}/{}", udp_done, UDP_CYCLES);
    }

    if ok {
        println!("[neteploop] risultato: PASS");
    } else {
        println!("[neteploop] risultato: FAIL");
    }
    finish(ok);
}

/// Un ciclo TCP completo: open → echo → (FIN) → close.
fn tcp_cycle(broker_chan: u64, graceful: bool) -> Result<(), &'static str> {
    let mut ep = BrokerEndpoint::open(broker_chan, Proto::Tcp, Flow::Connect)
        .map_err(|_| "open rifiutato")?;
    ep.send(DST_ADDR, TCP_PORT, Event::Data, PAYLOAD)
        .map_err(|_| "invio rifiutato")?;

    let mut buf = [0u8; civis::NET_EP_SLOT_SIZE - 23];
    let mut echoed = false;
    for _ in 0..MAX_TICKS {
        let mut got = false;
        while let Some(r) = ep.recv(&mut buf) {
            got = true;
            if r.event == Event::Data && !r.truncated && &buf[..r.len] == PAYLOAD {
                echoed = true;
            }
        }
        if echoed {
            break;
        }
        if !got {
            tick();
        }
    }

    if graceful {
        // FIN ordinato: il peer chiude il proprio lato e il suo endpoint.
        let _ = ep.send(DST_ADDR, TCP_PORT, Event::Fin, &[]);
        for _ in 0..MAX_TICKS {
            let mut fin = false;
            while let Some(r) = ep.recv(&mut buf) {
                if matches!(r.event, Event::Fin | Event::Rst) {
                    fin = true;
                }
            }
            if fin {
                break;
            }
            tick();
        }
    }

    ep.close();
    if echoed {
        Ok(())
    } else {
        Err("eco non ricevuto")
    }
}

/// Un ciclo UDP: open → echo → close.
fn udp_cycle(broker_chan: u64) -> Result<(), &'static str> {
    let mut ep = BrokerEndpoint::open(broker_chan, Proto::Udp, Flow::Ephemeral)
        .map_err(|_| "open rifiutato")?;
    ep.send(DST_ADDR, UDP_PORT, Event::Data, PAYLOAD)
        .map_err(|_| "invio rifiutato")?;

    let mut buf = [0u8; civis::NET_EP_SLOT_SIZE - 23];
    let mut echoed = false;
    for _ in 0..MAX_TICKS {
        let mut got = false;
        while let Some(r) = ep.recv(&mut buf) {
            got = true;
            if r.event == Event::Data && !r.truncated && &buf[..r.len] == PAYLOAD {
                echoed = true;
            }
        }
        if echoed {
            break;
        }
        if !got {
            tick();
        }
    }

    ep.close();
    if echoed {
        Ok(())
    } else {
        Err("eco non ricevuto")
    }
}

/// Un tick di attesa (il probe non ha un produttore che lo notifica).
fn tick() {
    let _ = civis::futex_wait(
        &FUTEX as *const u32 as usize,
        0,
        (civis::get_ticks() + 1) as u64,
    );
}

fn finish(ok: bool) -> ! {
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
    let _ = civis::print_string(b"[neteploop] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
