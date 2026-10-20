//! s25fuse — carica lo staticone rustc da ArcaFS e lo spawna (S2.5d, prima
//! accensione del compilatore in guest; S2.5f/F1: payload su ArcaFS).
//!
//! Lo staticone (~99 MB) viaggia su `/tool/rustc.bin` (bucket `ns` del
//! volume root, seedato da `scripts/arca-tool.sh`): la read a offset passa
//! da F0 (`ns_read` → `get_range` con cursore LRU), quindi e' O(1) per
//! slice sequenziale — niente materializzazione quadratica del blob. Qui:
//! open + read sequenziali da 4 KiB in heap (serve RAM: run a 1G+ e volume
//! a 384 MiB) e `spawn_image` con flag bigstack (lo staticone vuole MB di
//! stack). Poi attende l'EXIT_NOTIFY sul canale di nascita (w0 = exit code)
//! e segnala TEST_DONE a init con l'esito (ok = il figlio e' nato, vissuto
//! ed uscito da solo — qualunque code: anche l'usage-error di argc=0 prova
//! esecuzione di codice rustc).
//!
//! Mai nel gate (run dedicato RUN_S25=1, volume + RAM grandi).

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use civis;
use civis::println;

civis::entry!(real_main);

/// Path del payload su ArcaFS (bucket `ns` della root, seedato da
/// `scripts/arca-tool.sh` solo se lo staticone e' stato costruito).
const PAYLOAD: &str = "/tool/rustc.bin";
/// Argomenti opzionali per lo staticone (S2.5f/F2): se presente, sostituisce
/// il default `--version` con una compilazione vera.
const ARGS_FILE: &str = "/tool/rustc.args";
/// Slice di lettura (una pagina ring: oltre si splitta comunque).
const SLICE: usize = 4096;

fn real_main(_sp: u64) -> ! {
    println!("[s25fuse] avvio (caricamento staticone da ArcaFS)");
    let fd = match civis::open(PAYLOAD, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[s25fuse] FAIL: open payload");
            done(false);
        }
    };
    let mut img: Vec<u8> = Vec::new();
    let mut buf: Vec<u8> = alloc::vec![0u8; SLICE];
    let mut n: u64 = 0;
    loop {
        match civis::read_fs(fd, &mut buf, SLICE) {
            Ok(0) => break, // EOF
            Ok(r) => {
                img.extend_from_slice(&buf[..r as usize]);
                n += 1;
                if n % 1024 == 0 {
                    println!("[s25fuse] letti {}B", img.len());
                }
            }
            Err(_) => {
                println!("[s25fuse] FAIL: read a {}B", img.len());
                done(false);
            }
        }
    }
    let _ = civis::close(fd);
    if img.len() < 1_000_000 {
        println!("[s25fuse] FAIL: payload corto {}B", img.len());
        done(false);
    }
    println!("[s25fuse] load ok totale={}B", img.len());
    // S2.5e: argv di default ["rustc", "--version"] (scala graduata: non
    // tocca il sysroot). S2.5f/F2: se `/tool/rustc.args` esiste, i suoi
    // token sono gli argomenti veri (compilazione; vedi `scripts/f2/`),
    // con `-o <path>` letto per verificare l'artefatto a fine corsa.
    let cli = read_args();
    let f2 = cli.is_some();
    let out_path = cli.as_ref().and_then(|a| output_arg(a));
    let mut argv: Vec<&[u8]> = Vec::with_capacity(1 + cli.as_ref().map_or(1, |a| a.len()));
    argv.push(b"rustc");
    match &cli {
        Some(tokens) => argv.extend(tokens.iter().map(|t| t.as_slice())),
        None => argv.push(b"--version"),
    }
    if f2 {
        println!(
            "[s25fuse] F2 argomenti: {} token, out={}",
            cli.as_ref().map_or(0, |a| a.len()),
            out_path.as_deref().unwrap_or("<nessuno>")
        );
    }
    let argblock = match civis::pack_args(&argv, &[]) {
        Some(b) => b,
        None => {
            println!("[s25fuse] FAIL: pack_args");
            done(false);
        }
    };
    let meta = match civis::SpawnMeta::new("rustc", 16, &[]) {
        Some(m) => m.bigstack().argv(&argblock),
        None => {
            println!("[s25fuse] FAIL: meta");
            done(false);
        }
    };
    let chan = match civis::spawn_image(&img, &meta) {
        Ok(c) => {
            println!("[s25fuse] staticone spawnato chan={}", c);
            c
        }
        Err(_) => {
            println!("[s25fuse] FAIL: spawn_image rifiutato");
            done(false);
        }
    };
    // Il payload da 99 MB non serve piu': liberarlo prima dell'attesa
    // restituisce i frame al sistema (lo staticone ha i suoi).
    drop(img);
    // Attende la morte del figlio sul canale di nascita (notifica unificata:
    // channel = canale che ci collegava, w0 = exit code, w1 = pid).
    loop {
        match civis::recv() {
            Ok(m) if m.channel == chan as u64 && m.tag == civis::EXIT_NOTIFY => {
                let code = m.w0 as i64;
                println!("[s25fuse] staticone uscito code={} pid={}", code, m.w1);
                if !f2 {
                    // S2.5d: basta che il compilatore sia nato, vissuto ed
                    // uscito da solo (anche l'usage-error prova esecuzione).
                    done(true);
                }
                // F2: verdetto sull'output, non solo sull'exit code.
                let ok = code == 0 && match &out_path {
                    Some(p) => report_artifact(p),
                    None => {
                        println!("[s25fuse] FAIL: args senza -o");
                        false
                    }
                };
                done(ok);
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

/// Token di `/tool/rustc.args` (whitespace ASCII). None = file assente
/// (run S2.5d: resta `--version`) o illeggibile (stesso fallback).
fn read_args() -> Option<Vec<Vec<u8>>> {
    let fd = civis::open(ARGS_FILE, 0).ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    let cap = chunk.len();
    loop {
        match civis::read_fs(fd, &mut chunk, cap) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => {
                let _ = civis::close(fd);
                return None;
            }
        }
    }
    let _ = civis::close(fd);
    let mut tokens = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for &b in buf.iter() {
        if b.is_ascii_whitespace() {
            if !cur.is_empty() {
                tokens.push(core::mem::take(&mut cur));
            }
        } else {
            cur.push(b);
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    Some(tokens)
}

/// Path dopo `-o` (ultima occorrenza vince, come rustc).
fn output_arg(tokens: &[Vec<u8>]) -> Option<String> {
    let mut out = None;
    let mut i = 0;
    while i + 1 < tokens.len() {
        if tokens[i] == b"-o" {
            out = core::str::from_utf8(&tokens[i + 1])
                .ok()
                .map(alloc::string::String::from);
        }
        i += 1;
    }
    out
}

/// Verifica l'artefatto F2: esiste, dimensione e magic ELF (0x7f 'E' 'L' 'F').
fn report_artifact(path: &str) -> bool {
    let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
    if civis::stat(path, &mut st).is_err() {
        println!("[s25fuse] FAIL: {} assente (stat)", path);
        return false;
    }
    let mut head = [0u8; 4];
    let elf = match civis::open(path, 0) {
        Ok(fd) => {
            let cap = head.len();
            let n = civis::read_fs(fd, &mut head, cap).unwrap_or(0);
            let _ = civis::close(fd);
            n == 4 && head == *b"\x7fELF"
        }
        Err(_) => false,
    };
    println!("[s25fuse] F2 out={} size={} elf={}", path, st.size, elf);
    st.size > 0 && elf
}

/// Segnala TEST_DONE a init e termina (noreturn per costruzione).
fn done(ok: bool) -> ! {
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, ok as u64, 0);
    civis::exit(if ok { 0 } else { 1 });
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
