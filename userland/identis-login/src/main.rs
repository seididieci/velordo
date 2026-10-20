//! Programma login (ADR-0055): chiede utente e password, li verifica contro
//! il bucket `identis:config` (nome:hash BLAKE2s-256), stampa SUCCESS o FAIL.
//! Sostituisce la shell al primo boot. In una fase successiva la verifica
//! passera' attraverso il servizio Identis via IPC.
//!
//! Output su /dev/console (VGA + serial), input da /dev/input/keyboard.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use blake2s::blake2s;
use civis;
use civis::println;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";
const KEYBOARD_PATH: &str = "/dev/input/keyboard";
const CONFIG_PATH: &str = "identis:config";

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

fn console_write(data: &[u8]) {
    // VGA
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    // Serial (cosi' si vede anche senza GTK)
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    if data.ends_with(&[b'\n']) {
        civis::flush();
    }
}

/// Entry: apre console e tastiera, chiede credenziali, verifica contro il bucket.
fn real_main(_sp: u64) -> ! {
    // Apri console per output (VGA + serial).
    let console_fd = match civis::open(CONSOLE_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[login] ERRORE: impossibile aprire console");
            civis::exit(1);
        }
    };
    unsafe { CONSOLE_FD = console_fd; }

    console_write(b"[login] Velordo login\n");

    // Carica il bucket di configurazione (nome:hash per riga).
    let entries = match load_config() {
        Ok(e) => e,
        Err(e) => {
            console_write(b"[login] ERRORE: impossibile leggere il bucket di configurazione\n");
            civis::exit(1);
        }
    };

    console_write(b"[login] Utenti configurati: ");
    let n_str = (entries.len() as u32).to_string();
    console_write(n_str.as_bytes());
    console_write(b"\n");

    for (name, hash) in &entries {
        console_write(b"[login]   utente: ");
        console_write(name.as_bytes());
        console_write(b" hash=");
        console_write(hash.as_bytes());
        console_write(b"\n");
    }

    if entries.is_empty() {
        console_write(b"[login] ERRORE: nessun utente configurato\n");
        civis::exit(1);
    }

    // ---- Self-test automatico: prova root/root ----
    let test_name = "root";
    let test_pwd = "root";
    let test_hash = blake2s(test_pwd.as_bytes());
    let test_hash_hex = hex_encode(&test_hash);

    console_write(b"[login] Self-test: root/root -> hash=");
    console_write(test_hash_hex.as_bytes());
    console_write(b"\n");

    let self_ok = entries.iter().any(|(n, h)| n == test_name && h == &test_hash_hex);
    if self_ok {
        console_write(b"[login] Self-test: SUCCESS (root/root valido)\n");
    } else {
        console_write(b"[login] Self-test: FAIL (root/root NON valido - hash mismatch)\n");
    }

    // Ora chiede credenziali interattive
    console_write(b"[login] Nome utente: ");
    let name = read_line_from_keyboard();
    console_write(b"\n[login] Password: ");
    let pwd = read_line_from_keyboard();
    console_write(b"\n");

    // Hash della password con BLAKE2s-256.
    let hash = blake2s(pwd.as_bytes());
    let hash_hex = hex_encode(&hash);

    console_write(b"[login] Hash calcolato: ");
    console_write(hash_hex.as_bytes());
    console_write(b"\n");

    // Confronta con le entry del bucket.
    let ok = entries.iter().any(|(n, h)| n == &name && h == &hash_hex);

    if ok {
        console_write(b"[login] SUCCESS\n");
    } else {
        console_write(b"[login] FAIL\n");
    }

    // Mantieni il processo vivo.
    loop {
        core::hint::spin_loop();
    }
}

/// Legge una riga terminata da newline dalla tastiera.
fn read_line_from_keyboard() -> String {
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
                // Echo backspace sulla console
                console_write(b"\x08 \x08");
            }
            continue;
        }
        buf.push(b);
        // Echo del carattere sulla console
        console_write(&[b]);
    }

    let _ = civis::close(fd);
    String::from_utf8(buf).unwrap_or_default()
}

/// Carica il bucket `identis:config` come vettore di (nome, hash).
fn load_config() -> Result<Vec<(String, String)>, civis::Error> {
    let fd = civis::open(CONFIG_PATH, 0)?;
    let mut buf = [0u8; 4096];
    let mut out = String::new();
    loop {
        let cap = buf.len();
        let n = civis::read_fs(fd, &mut buf, cap)?;
        if n == 0 {
            break;
        }
        out.push_str(core::str::from_utf8(&buf[..n]).unwrap_or(""));
    }
    let _ = civis::close(fd);

    let mut entries = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ':');
        let name = parts.next().unwrap_or("").to_string();
        let hash = parts.next().unwrap_or("").to_string();
        if !name.is_empty() && !hash.is_empty() {
            entries.push((name, hash));
        }
    }
    Ok(entries)
}

/// Codifica un array di byte in stringa esadecimale (minuscole).
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, &b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0xF) as usize];
    }
    core::str::from_utf8(&out).unwrap().to_string()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[login] panic\n");
    civis::exit(1)
}
