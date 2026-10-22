//! create-user (ADR-0055, Fase 3.1): crea un nuovo utente chiedendo a
//! Identis via IPC di accodare l'hash della password nel bucket
//! `identis:config`.
//!
//! - Il **token di sessione** arriva dall'ambiente (`VELORDO_SESSION`,
//!   hex): e' la capability concessa dal login. Senza, il programma non puo'
//!   fare nulla (Identis rifiuta comunque, ma falliamo subito e pulito).
//! - Il **nome utente** arriva da argv (`create-user alice`) o e' chiesto
//!   alla tastiera.
//! - La **password** e' sempre digitata alla tastiera (con conferma): non
//!   transita in argv ne' in ambiente, e l'hash BLAKE2s-256 e' calcolato
//!   **in locale** — nel payload IPC c'e' solo l'hash.
//!
//! Output su /dev/console (VGA + serial), input da /dev/input/keyboard.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use blake2s::blake2s;
use civis;
use civis::println;
use identis_proto as proto;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";
const KEYBOARD_PATH: &str = "/dev/input/keyboard";

/// Variabile d'ambiente con il token di sessione (hex).
const ENV_SESSION: &str = "VELORDO_SESSION";

/// Tentativi in caso di servizio occupato (`R_BUSY`).
const RETRIES: u32 = 20;

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

fn console_write(data: &[u8]) {
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    // Flush SEMPRE: la console e' interattiva (un prompt senza newline
    // resterebbe nel buffer seriale e nessuno lo vedrebbe).
    civis::flush();
}

/// Entry: token di sessione, nome utente, password, richiesta a Identis.
fn real_main(sp: u64) -> ! {
    // Console per l'output.
    let console_fd = match civis::open(CONSOLE_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[create-user] ERRORE: impossibile aprire console");
            civis::exit(1);
        }
    };
    unsafe { CONSOLE_FD = console_fd; }

    // Token di sessione: senza, niente da fare.
    let token = match read_session_token(sp) {
        Ok(t) => t,
        Err(msg) => {
            console_write(b"[create-user] ERRORE: ");
            console_write(msg.as_bytes());
            console_write(b"\n");
            civis::exit(1);
        }
    };

    // Nome utente: da argv, altrimenti chiesto.
    let name = match argv_name(sp) {
        Some(n) => n,
        None => {
            console_write(b"[create-user] Nome utente: ");
            let n = read_line_from_keyboard(false);
            console_write(b"\n");
            n
        }
    };

    // Validazione anticipata (fail-fast lato client): Identis rivalida
    // comunque, ma non ha senso chiedere una password per un nome che
    // rifiutera'.
    if !proto::valid_name(&name) {
        console_write(b"[create-user] ERRORE: nome utente non valido (solo [a-z0-9_-], max 32 caratteri)\n");
        civis::exit(1);
    }

    // Password (conferma inclusa).
    console_write(b"[create-user] Password: ");
    let pwd = read_line_from_keyboard(true);
    console_write(b"\n[create-user] Conferma: ");
    let pwd2 = read_line_from_keyboard(true);
    console_write(b"\n");
    if pwd != pwd2 {
        console_write(b"[create-user] ERRORE: le password non coincidono\n");
        civis::exit(1);
    }
    if pwd.is_empty() {
        console_write(b"[create-user] ERRORE: password vuota\n");
        civis::exit(1);
    }

    // L'hash si calcola qui: la password non lascia mai il processo.
    let hash = blake2s(pwd.as_bytes());
    let payload = proto::build_add_user(token, &name, &hash);

    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write(b"[create-user] ERRORE: servizio Identis non disponibile\n");
            civis::exit(1);
        }
    };

    match proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES) {
        Ok(r) if r.ok() => {
            console_write(b"[create-user] utente '");
            console_write(name.as_bytes());
            console_write(b"' creato\n");
            civis::exit(0);
        }
        Ok(r) => {
            console_write(b"[create-user] ERRORE: ");
            console_write(r.describe().as_bytes());
            console_write(b"\n");
            civis::exit(1);
        }
        Err(_) => {
            console_write(b"[create-user] ERRORE IPC\n");
            civis::exit(1);
        }
    }
}

/// Legge il token di sessione dall'ambiente (hex a 16 cifre).
fn read_session_token(sp: u64) -> Result<u64, &'static str> {
    let env = civis::env_from_stack(sp).ok_or("ambiente non leggibile")?;
    let raw = env
        .get(ENV_SESSION)
        .ok_or("nessuna sessione: login non riuscito o VELORDO_SESSION assente")?;
    parse_hex_u64(raw).ok_or("token di sessione malformato")
}

/// Nome utente da argv[1] (se presente e non vuoto).
fn argv_name(sp: u64) -> Option<String> {
    let args = civis::args_from_stack(sp)?;
    let raw = args.get(1)?;
    if raw.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(raw).into_owned())
}

/// Parsa una stringa esadecimale in u64 (`None` se malformata).
fn parse_hex_u64(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() || raw.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &b in raw {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u64;
    }
    Some(v)
}

/// Legge una riga terminata da newline dalla tastiera. Con `mask` l'eco e'
/// mascherato (la password non finisce in chiaro nel log).
fn read_line_from_keyboard(mask: bool) -> String {
    let fd = match civis::open(KEYBOARD_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            console_write(b"[create-user] ERRORE: impossibile aprire tastiera\n");
            civis::exit(1);
        }
    };

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let n = civis::read_fs(fd, &mut byte, 1).unwrap_or(0);
        if n == 0 {
            core::hint::spin_loop();
            continue;
        }
        let b = byte[0];
        if b == b'\n' || b == b'\r' {
            break;
        }
        if b == 0x7F || b == 0x08 {
            if !buf.is_empty() {
                buf.pop();
                // L'eco della cancellazione **anche** nel campo non
                // mascherato: senza, lo schermo mostra piu' caratteri di
                // quanti ne restino nel buffer (si autenticava una stringa
                // diversa da quella mostrata).
                console_write(b"\x08 \x08");
            }
            continue;
        }
        buf.push(b);
        if mask {
            console_write(b"*");
        } else {
            console_write(&[b]);
        }
    }

    let _ = civis::close(fd);
    String::from_utf8(buf).unwrap_or_default()
}

/// Risolve il servizio Identis per nome (con retry).
fn resolve_identis() -> Result<u64, &'static str> {
    for _ in 0..50 {
        match civis::service_lookup(civis::Service::Identis) {
            Ok(c) => return Ok(c as u64),
            Err(_) => {
                core::hint::spin_loop();
            }
        }
    }
    Err("Identis non trovato")
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[create-user] panic\n");
    civis::exit(1)
}
