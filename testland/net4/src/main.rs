//! net4 — probe di rete IPv4/SIIT (N4, test fuori gate).
//!
//! Apre un endpoint IPv4 tramite il **broker** (`netbroker`, ADR-0080 fase 6)
//! e invia un payload a un indirizzo IPv6 in formato SIIT (`64:ff9b::/96`):
//! il broker sceglie l'uscita IPv4 e inoltra a `siit`, che traduce. Segnala
//! TEST_DONE ad init. Eseguito solo con `net=1`.
//!
//! Il probe e' un binario di test (`tst`), quindi `Untrusted`: sia `net` sia
//! `siit` gli negano l'endpoint diretto (gate di classe) e la strada
//! legittima e' il broker, che applica allowlist, rate limit e consenso.

#![no_std]
#![no_main]

use civis::broker::{BrokerEndpoint, Event, Flow, Proto, SendError};
use civis::println;

static FUTEX: u32 = 0;

/// Indirizzo IPv4 destinatario (10.0.0.1, mappato in IPv6 SIIT).
const DST_IPV4: [u8; 4] = [10, 0, 0, 1];
/// Porta destinataria.
const DST_PORT: u16 = 53;
/// Payload del probe.
const PAYLOAD: &[u8] = b"net4-siit-probe";

// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    // Risolve il broker.
    let mut broker_chan = 0u64;
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::NetBroker) {
            if c > 0 {
                broker_chan = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if broker_chan == 0 {
        println!("[net4] FAIL: servizio netbroker non trovato");
        finish(false);
    }

    // Endpoint IPv4 (uscita SIIT) con porta effimera.
    let mut ep = match BrokerEndpoint::open(broker_chan, Proto::Ipv4, Flow::Ephemeral) {
        Ok(ep) => ep,
        Err(_) => {
            println!("[net4] FAIL: open IPv4 rifiutato dal broker");
            finish(false);
        }
    };
    println!(
        "[net4] endpoint IPv4 aperto id={} porta={}",
        ep.id(),
        ep.local_port()
    );

    // Indirizzo SIIT: 64:ff9b:: + i 4 byte dell'IPv4.
    let mut dst = [0u8; 16];
    dst[..6].copy_from_slice(&[0x64, 0xff, 0x9b, 0, 0, 0]);
    dst[12..].copy_from_slice(&DST_IPV4);

    let all_ok = match ep.send(dst, DST_PORT, Event::Data, PAYLOAD) {
        Ok(()) => {
            println!(
                "[net4] payload IPv4 accettato: PASS (dst={}.{}.{}.{}:{})",
                DST_IPV4[0], DST_IPV4[1], DST_IPV4[2], DST_IPV4[3], DST_PORT
            );
            true
        }
        Err(SendError::Full) => {
            println!("[net4] FAIL: backpressure dal broker");
            false
        }
        Err(_) => {
            println!("[net4] FAIL: invio rifiutato dal broker");
            false
        }
    };

    // Destinazione **globale** (un IPv4 pubblico): il broker nega il codice
    // non firmato senza consenso. E' la proprieta' della fase 6: la rete
    // pubblica non e' un diritto del binario, e' una concessione dell'utente.
    let mut public = [0u8; 16];
    public[..6].copy_from_slice(&[0x64, 0xff, 0x9b, 0, 0, 0]);
    public[12..].copy_from_slice(&[93, 184, 216, 34]);
    match ep.send(public, 80, Event::Data, PAYLOAD) {
        Err(_) => println!("[net4] destinazione globale negata all'Untrusted: PASS"),
        Ok(()) => {
            println!("[net4] FAIL: destinazione globale concessa senza consenso");
            return finish(false);
        }
    }

    ep.close();
    finish(all_ok);
}

/// Termina il probe e segnala TEST_DONE ad init.
fn finish(ok: bool) -> ! {
    if ok {
        println!("[net4] risultato: PASS");
    } else {
        println!("[net4] risultato: FAIL");
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
    let _ = civis::print_string(b"[net4] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
