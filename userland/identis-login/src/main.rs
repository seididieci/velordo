//! Programma login (ADR-0055): chiede utente e password, calcola l'hash
//! BLAKE2s-256 in locale e lo verifica tramite Identis via IPC.
//! Sostituisce la shell al primo boot.
//!
//! Output su /dev/console (VGA + serial), input da /dev/input/keyboard.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use blake2s::blake2s;
use civis;
use civis::println;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";
const KEYBOARD_PATH: &str = "/dev/input/keyboard";

/// Tag IPC per Identis (single source = syscall-numbers).
const IDENTIS_VERIFY: u64 = 0x70;
/// Marker di fine trasmissione payload.
const MSG_EOF: u64 = 0xFFFFFFFF;
/// Timeout inattività per Identis (tick PIT, 100 Hz -> 5 s).
const IDENTIS_TIMEOUT_TICKS: u64 = 500;

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

fn console_write(data: &[u8]) {
    // VGA
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    // Serial (così si vede anche senza GTK)
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    if data.ends_with(&[b'\n']) {
        civis::flush();
    }
}

/// Entry: apre console e tastiera, chiede credenziali, invia a Identis via IPC.
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

    // ---- Self-test automatico: prova root/root via IPC ----
    let test_name = "root";
    let test_pwd = "root";
    let test_hash = blake2s(test_pwd.as_bytes());
    let test_hash_hex = hex_prefix(&test_hash);

    console_write(b"[login] Self-test: root/root -> hash=");
    console_write(test_hash_hex.as_bytes());
    console_write(b"\n");

    // Costruisci payload binario per il test:
    //   [4B magic "ACLI"][4B versione][4B len_username][username][4B len_hash][32B hash]
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(b"ACLI");                       // magic
    payload.extend_from_slice(&1u32.to_le_bytes());           // versione
    payload.extend_from_slice(&(test_name.len() as u32).to_le_bytes());
    payload.extend_from_slice(test_name.as_bytes());
    payload.extend_from_slice(&(test_hash.len() as u32).to_le_bytes());
    payload.extend_from_slice(&test_hash);

    // Risolvi il servizio Identis.
    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write(b"[login] ERRORE: servizio Identis non disponibile\n");
            civis::exit(1);
        }
    };

    // Invia payload via IPC a chunk da 4 byte (w1), preceduto da total_len.
    match send_login(chan, &payload) {
        Ok(result) => {
            if result == 0 {
                console_write(b"[login] Self-test: SUCCESS (root/root valido)\n");
            } else {
                console_write(b"[login] Self-test: FAIL (root/root NON valido)\n");
            }
        }
        Err(e) => {
            console_write(b"[login] Self-test: ERRORE IPC: ");
            console_write(e.as_bytes());
            console_write(b"\n");
        }
    }

    // Ora chiede credenziali interattive
    console_write(b"[login] Nome utente: ");
    let name = read_line_from_keyboard();
    console_write(b"\n[login] Password: ");
    let pwd = read_line_from_keyboard();
    console_write(b"\n");

    // Calcola hash della password in locale (non passa mai in chiaro via IPC).
    let hash = blake2s(pwd.as_bytes());

    // Costruisci payload binario:
    //   [4B magic "ACLI"][4B versione][4B len_username][username][4B len_hash][32B hash]
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(b"ACLI");                       // magic
    payload.extend_from_slice(&1u32.to_le_bytes());           // versione
    payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(&(hash.len() as u32).to_le_bytes());
    payload.extend_from_slice(&hash);

    println!("[login] payload costruito: {} byte, hash={}", payload.len(), hex_prefix(&hash));

    // Invia payload via IPC a chunk da 4 byte (w1), preceduto da total_len.
    match send_login(chan, &payload) {
        Ok(result) => {
            if result == 0 {
                console_write(b"[login] SUCCESS\n");
            } else {
                console_write(b"[login] FAIL\n");
            }
        }
        Err(e) => {
            console_write(b"[login] ERRORE IPC: ");
            console_write(e.as_bytes());
            console_write(b"\n");
        }
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
                console_write(b"\x08 \x08");
            }
            continue;
        }
        buf.push(b);
        console_write(&[b]);
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

/// Invia il payload a Identis via IPC e attende la reply.
/// Protocollo:
///   1° msg: IDENTIS_VERIFY, w0=total_len, w1=0
///   2°..N°: IDENTIS_VERIFY, w0=offset, w1=chunk_packed (4 byte in w1)
///   Ultimo: IDENTIS_VERIFY, w0=0xFFFFFFFF (EOF), w1=0
///   Reply:  IDENTIS_VERIFY, w0=0 (SUCCESS) o 1 (FAIL)
fn send_login(chan: u64, payload: &[u8]) -> Result<u64, String> {
    let total_len = payload.len() as u64;

    // 1° messaggio: annuncia la dimensione totale.
    let reply = civis::send(chan, IDENTIS_VERIFY, total_len, 0)
        .map_err(|_| "send total_len fallito".to_string())?;
    if reply.w0 != 0 {
        return Err(format!("Identis ha rifiutato total_len (w0={})", reply.w0));
    }

    // Invia chunk da 4 byte in w1.
    let mut offset: usize = 0;
    while offset < payload.len() {
        let remaining = payload.len() - offset;
        let chunk_len = remaining.min(4);
        let chunk = payload[offset..offset + chunk_len].to_vec();
        let mut packed: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            packed |= (b as u32) << (8 * i);
        }
        let reply = civis::send(chan, IDENTIS_VERIFY, offset as u64, packed as u64)
            .map_err(|_| "send chunk fallito".to_string())?;
        if reply.w0 != 0 {
            return Err(format!("Identis ha rifiutato il chunk (w0={})", reply.w0));
        }
        offset += chunk.len();
    }

    // EOF.
    let reply = civis::send(chan, IDENTIS_VERIFY, MSG_EOF, 0)
        .map_err(|_| "send EOF fallito".to_string())?;
    Ok(reply.w0)
}

/// Restituisce gli primi 8 byte dell'hash in esadecimale (per debug).
fn hex_prefix(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let n = bytes.len().min(4);
    let mut out = [0u8; 8];
    for (i, &b) in bytes.iter().take(n).enumerate() {
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
