//! users — elenca gli utenti registrati (Fase 1, ADR-0055).
//!
//! Programma esterno estratto dalla shell nativa. Output testuale su console.
//! Richiede una sessione attiva (VELORDO_SESSION).

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;

use civis;
use identis_proto as proto;

civis::entry!(real_main);

/// Variabile d'ambiente con il token di sessione (hex).
const ENV_SESSION: &str = "VELORDO_SESSION";

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

/// Parsa una stringa esadecimale in u64.
fn hex_parse_u64(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() || raw.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &b in raw {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'a' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u64;
    }
    Some(v)
}

/// Modalita' testuale: output su console (comportamento originale).
fn run_text_mode(token: u64) {
    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write(b"ERRORE: servizio Identis non disponibile\n");
            return;
        }
    };

    // LIST: w0 = 0 (nessun payload), w1 = token. Reply: w0 = codice, w1 = conteggio.
    match civis::ipc::send(chan, civis::IDENTIS_LIST, 0, token) {
        Ok(r) if r.w0 == proto::R_OK => {
            let n = r.w1 as usize;
            console_write(b"Utenti registrati (");
            console_write_str(&alloc::format!("{}", n));
            console_write(b"):\n");

            // Legge i nomi direttamente dal bucket.
            let fd = match civis::open("identis:config", 0) {
                Ok(f) => f,
                Err(_) => {
                    console_write(b"ERRORE: impossibile leggere il bucket utenti\n");
                    return;
                }
            };

            let mut buf = [0u8; 4096];
            let mut out = String::new();
            loop {
                let cap = buf.len();
                let n = match civis::read_fs(fd, &mut buf, cap) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                out.push_str(core::str::from_utf8(&buf[..n]).unwrap_or(""));
            }
            let _ = civis::close(fd);

            for line in out.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // Formato `nome:hash:ruoli` (ADR-0080, fase 5).
                let mut parts = line.splitn(3, ':');
                if let Some(name) = parts.next() {
                    let _hash = parts.next();
                    let roles = parts.next().unwrap_or("user");
                    console_write(b"  ");
                    console_write(name.as_bytes());
                    console_write(b"  [");
                    console_write(roles.as_bytes());
                    console_write(b"]\n");
                }
            }
        }
        Ok(r) if r.w0 == proto::R_TOKEN_INVALID => {
            console_write(b"ERRORE: token di sessione scaduto\n");
        }
        Ok(r) => {
            console_write(b"ERRORE LIST: ");
            match r.w0 {
                proto::R_BUSY => console_write(b"servizio occupato\n"),
                _ => console_write(b"errore di protocollo\n"),
            }
        }
        Err(_) => console_write(b"ERRORE IPC\n"),
    }
}

/// Entry: apre console, legge il token, elenca gli utenti.
fn real_main(sp: u64) -> ! {
    let env = match civis::env_from_stack(sp) {
        Some(e) => e,
        None => {
            console_write(b"[users] ERRORE: ambiente non leggibile\n");
            civis::exit(1);
        }
    };

    let token = match env.get(ENV_SESSION) {
        Some(r) => match hex_parse_u64(r) {
            Some(t) => t,
            None => {
                console_write(b"ERRORE: nessuna sessione attiva (login richiesto)\n");
                civis::exit(1);
            }
        },
        None => {
            console_write(b"ERRORE: nessuna sessione attiva (login richiesto)\n");
            civis::exit(1);
        }
    };

    run_text_mode(token);
    civis::exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[users] panic\n");
    civis::exit(1)
}
