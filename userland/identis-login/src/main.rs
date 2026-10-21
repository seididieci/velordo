//! Programma login (ADR-0055, Fase 3.1): chiede utente e password, calcola
//! l'hash BLAKE2s-256 **in locale** e lo verifica tramite Identis via IPC.
//!
//! La password non transita MAI in chiaro: nel payload IPC c'e' solo
//! l'hash. La password digitata non viene neanche ecoata a schermo (era un
//! buco: finiva in chiaro nel log seriale).
//!
//! Output su /dev/console (VGA + serial), input da /dev/input/keyboard.
//!
//! Il framing del protocollo e' condiviso (`libs/identis-proto`): qui c'e'
//! solo il lato client.

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

/// Token di sessione dell'ultimo login riuscito. Lo userà la shell nativa
/// (Fase 3.2) per passare la sessione ai programmi che ne hanno bisogno
/// (es. `create-user`): per ora e' solo custodito qui.
#[allow(dead_code)]
static mut SESSION_TOKEN: u64 = 0;

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

fn console_write(data: &[u8]) {
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    if data.ends_with(&[b'\n']) {
        civis::flush();
    }
}

/// Entry: apre console e tastiera, fa il self-test, poi chiede credenziali.
fn real_main(_sp: u64) -> ! {
    let console_fd = match civis::open(CONSOLE_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[login] ERRORE: impossibile aprire console");
            civis::exit(1);
        }
    };
    unsafe { CONSOLE_FD = console_fd; }

    console_write(b"[login] Velordo login\n");

    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write(b"[login] ERRORE: servizio Identis non disponibile\n");
            civis::exit(1);
        }
    };

    // ---- Self-test automatico: root/root via IPC ----
    console_write(b"[login] Self-test: root/root\n");
    let test_hash = blake2s(b"root");
    let payload = proto::build_verify(DEFAULT_USER, &test_hash);
    match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) if r.ok() => console_write(b"[login] Self-test: SUCCESS (root/root valido)\n"),
        Ok(r) => {
            console_write(b"[login] Self-test: FAIL (");
            console_write(r.describe().as_bytes());
            console_write(b")\n");
        }
        Err(_) => console_write(b"[login] Self-test: ERRORE IPC\n"),
    }

    // ---- Login interattivo ----
    console_write(b"\n[login] Nome utente: ");
    let name = read_line_from_keyboard(false);
    console_write(b"\n[login] Password: ");
    let pwd = read_line_from_keyboard(true);
    console_write(b"\n");

    // L'hash e' calcolato in locale: la password non lascia mai il processo.
    let hash = blake2s(pwd.as_bytes());
    let payload = proto::build_verify(&name, &hash);

    match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) if r.ok() => {
            unsafe { SESSION_TOKEN = r.w1; }
            console_write(b"[login] SUCCESS\n");
        }
        Ok(r) => {
            console_write(b"[login] FAIL (");
            console_write(r.describe().as_bytes());
            console_write(b")\n");
        }
        Err(_) => console_write(b"[login] ERRORE IPC\n"),
    }

    // Resta vivo, ma **dormendo**: `recv` blocca finche' non arriva un
    // messaggio. Uno spin puro qui brucerebbe una CPU per sempre (il
    // programma interattivo e' un placeholder: la shell nativa, Fase 3.2,
    // prenderà il posto).
    loop {
        let _ = civis::ipc::recv();
    }
}

/// Utente di default usato dal self-test.
const DEFAULT_USER: &str = "root";

/// Tentativi in caso di servizio occupato (`R_BUSY`).
const RETRIES: u32 = 20;

/// Legge una riga terminata da newline dalla tastiera. Con `mask` l'eco e'
/// mascherato (password: niente in chiaro nel log).
fn read_line_from_keyboard(mask: bool) -> String {
    let fd = match civis::open(KEYBOARD_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            console_write(b"[login] ERRORE: impossibile aprire tastiera\n");
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
                if mask {
                    console_write(b"\x08 \x08");
                }
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
    console_write(b"[login] panic\n");
    civis::exit(1)
}
