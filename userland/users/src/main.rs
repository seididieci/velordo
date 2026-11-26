//! users — elenca gli utenti registrati (Fase 1, ADR-0055).
//!
//! Programma esterno estratto dalla shell nativa. Output testuale sulla
//! sessione di terminale. Richiede un'identita' attestata: il **server** la
//! risolve per pid (`SESS_AUTH` + ancestry, S3.3c), non dall'ambiente.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;

use civis;
use identis_proto as proto;

civis::entry!(real_main);

fn console_write(data: &[u8]) {
    if !civis::sess::out(data) {
        let _ = civis::print_string(data);
    }
}

fn console_write_str(s: &str) {
    console_write(s.as_bytes());
}

/// Risolve Identis per nome (con retry).
fn resolve_identis() -> Result<u64, &'static str> {
    for _ in 0..50 {
        match civis::service_lookup(civis::Service::Identis) {
            Ok(c) => return Ok(c as u64),
            Err(_) => core::hint::spin_loop(),
        }
    }
    Err("Identis non trovato")
}

/// Modalita' testuale: output su console (comportamento originale).
///
/// L'elenco lo da' **Identis** (`IDENTIS_LIST`, pull a 4 byte): un
/// `nome:ruoli` per riga, mai l'hash. Il bucket `sys:identis` e' protetto e
/// il programma non lo apre (ADR-0096): chi elenca non vede le credenziali.
fn run_text_mode() {
    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write(b"ERRORE: servizio Identis non disponibile\n");
            return;
        }
    };

    let Some(list) = proto::fetch_list(chan, 4096) else {
        console_write(b"ERRORE: elenco utenti non disponibile (sessione attiva?)\n");
        return;
    };
    let text = core::str::from_utf8(&list).unwrap_or("");
    let rows: alloc::vec::Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();

    console_write(b"Utenti registrati (");
    console_write_str(&alloc::format!("{}", rows.len()));
    console_write(b"):\n");
    for row in rows {
        let mut parts = row.trim().splitn(2, ':');
        let name = parts.next().unwrap_or("");
        let roles = parts.next().unwrap_or("user");
        console_write(b"  ");
        console_write(name.as_bytes());
        console_write(b"  [");
        console_write(roles.as_bytes());
        console_write(b"]\n");
    }
}

/// Entry: elenca gli utenti. L'identita' e' attestata dal kernel (S3.3c:
/// nessun token): Identis risolve il chiamante per pid.
fn real_main(_sp: u64) -> ! {
    run_text_mode();
    civis::exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[users] panic\n");
    civis::exit(1)
}
