//! Programma login (ADR-0055): chiede utente e password, li verifica contro
//! il bucket `identis:config` (nome:hash BLAKE2s-256), stampa SUCCESS o FAIL.
//! Sostituisce la shell al primo boot. In una fase successiva la verifica
//! passera' attraverso il servizio Identis via IPC.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use civis;
use civis::println;

civis::entry!(real_main);

const KEYBOARD_PATH: &str = "/dev/input/keyboard";
const CONFIG_PATH: &str = "identis:config";

/// Entry: apre la tastiera, chiede credenziali, verifica contro il bucket.
fn real_main(_sp: u64) -> ! {
    println!("[login] Velordo login");

    // Carica il bucket di configurazione (nome:hash per riga).
    let entries = match load_config() {
        Ok(e) => e,
        Err(_) => {
            println!("[login] ERRORE: impossibile leggere il bucket di configurazione");
            civis::exit(1);
        }
    };

    if entries.is_empty() {
        println!("[login] ERRORE: nessun utente configurato");
        civis::exit(1);
    }

    println!("[login] Inserisci nome utente: ");
    let name = read_line_from_keyboard();
    println!("[login] Inserisci password: ");
    let pwd = read_line_from_keyboard();

    // Hash della password con BLAKE2s-256.
    let hash = blake2s(pwd.as_bytes());
    let hash_hex = hex_encode(&hash);

    // Confronta con le entry del bucket.
    let ok = entries.iter().any(|(n, h)| n == &name && h == &hash_hex);

    if ok {
        println!("[login] SUCCESS");
    } else {
        println!("[login] FAIL");
    }

    // Mantieni il processo vivo (non uscire subito).
    loop {
        core::hint::spin_loop();
    }
}

/// Legge una riga terminata da newline dalla tastiera.
fn read_line_from_keyboard() -> String {
    let fd = match civis::open(KEYBOARD_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[login] ERRORE: impossibile aprire tastiera");
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
                let _ = civis::write_fs(fd, &[0x08, b' ', 0x08], 3);
            }
            continue;
        }
        buf.push(b);
        let _ = civis::write_fs(fd, &[b], 1);
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

/// One-shot BLAKE2s-256 di `data` (implementazione inline).
fn blake2s(data: &[u8]) -> [u8; 32] {
    const IV: [u32; 8] = [
        0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
        0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
    ];
    const SIGMA: [[u8; 16]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
        [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
        [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
        [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
        [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
        [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
        [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
        [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
        [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    ];

    let mut h = IV;
    let mut buf = [0u8; 64];
    let mut buflen = 0;
    let mut t0: u64 = 0;
    let mut t1: u64 = 0;

    let mut input = data;
    while !input.is_empty() {
        let take = (64 - buflen).min(input.len());
        buf[buflen..buflen + take].copy_from_slice(&input[..take]);
        buflen += take;
        input = &input[take..];
        if buflen == 64 && !input.is_empty() {
            t0 = t0.wrapping_add(64);
            if t0 < 64 {
                t1 = t1.wrapping_add(1);
            }
            compress(&mut h, &buf, t0, t1, false);
            buflen = 0;
        }
    }
    for b in buf[buflen..].iter_mut() {
        *b = 0;
    }
    t0 = t0.wrapping_add(buflen as u64);
    if t0 < (buflen as u64) {
        t1 = t1.wrapping_add(1);
    }
    compress(&mut h, &buf, t0, t1, true);
    let mut out = [0u8; 32];
    for (i, w) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn compress(h: &mut [u32; 8], block: &[u8], t0: u64, t1: u64, final_block: bool) {
    let mut v = [0u32; 16];
    v[..8].copy_from_slice(h);
    // IV è già in h, quindi v[8..] = IV
    v[8] = 0x6A09E667; v[9] = 0xBB67AE85; v[10] = 0x3C6EF372; v[11] = 0xA54FF53A;
    v[12] = 0x510E527F; v[13] = 0x9B05688C; v[14] = 0x1F83D9AB; v[15] = 0x5BE0CD19;
    v[12] ^= t0 as u32;
    v[13] ^= (t0 >> 32) as u32;
    v[14] ^= t1 as u32;
    v[15] ^= (t1 >> 32) as u32;
    if final_block {
        v[14] = !v[14];
    }
    let m = block_words(block);
    for r in 0..10 {
        let s = &SIGMA[r];
        g(&mut v, 0, 4, 8, 12, m[s[0] as usize], m[s[1] as usize]);
        g(&mut v, 1, 5, 9, 13, m[s[2] as usize], m[s[3] as usize]);
        g(&mut v, 2, 6, 10, 14, m[s[4] as usize], m[s[5] as usize]);
        g(&mut v, 3, 7, 11, 15, m[s[6] as usize], m[s[7] as usize]);
        g(&mut v, 0, 5, 10, 15, m[s[8] as usize], m[s[9] as usize]);
        g(&mut v, 1, 6, 11, 12, m[s[10] as usize], m[s[11] as usize]);
        g(&mut v, 2, 7, 8, 13, m[s[12] as usize], m[s[13] as usize]);
        g(&mut v, 3, 4, 9, 14, m[s[14] as usize], m[s[15] as usize]);
    }
    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

fn block_words(block: &[u8]) -> [u32; 16] {
    let mut m = [0u32; 16];
    for (i, w) in m.iter_mut().enumerate() {
        let o = i * 4;
        *w = u32::from_le_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]]);
    }
    m
}

const SIGMA: [[u8; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

fn g(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
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
    println!("[login] panic");
    civis::exit(1)
}
