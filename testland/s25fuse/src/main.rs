//! s25fuse — carica lo staticone rustc da FAT e lo spawna (S2.5d, prima
//! accensione del compilatore in guest).
//!
//! Lo staticone (~99 MB) viaggia su `/fat/tool/rustc.bin`: la read FAT e'
//! lineare (solo i settori richiesti, mai materializzazione intera — a
//! differenza di obj_get/ns_read su ArcaFS, quadratici oltre i 16 MiB).
//! Qui: open + read sequenziali da 4 KiB in heap (serve RAM: run a 1G) e
//! `spawn_image` con flag bigstack (lo staticone vuole MB di stack). Poi
//! attende l'EXIT_NOTIFY sul canale di nascita (w0 = exit code) e segnala
//! TEST_DONE a init con l'esito (ok = il figlio e' nato, vissuto ed uscito
//! da solo — qualunque code: anche l'usage-error di argc=0 prova esecuzione
//! di codice rustc).
//!
//! Mai nel gate (run dedicato RUN_S25=1, volume + RAM grandi).

#![no_std]
#![no_main]

extern crate alloc;
use alloc::vec::Vec;

use civis;
use civis::println;

civis::entry!(real_main);

/// Path del payload su FAT (8.3, seedato da inject-bins.sh se esiste).
const PAYLOAD: &str = "/fat/tool/rustc.bin";
/// Slice di lettura (una pagina ring: oltre si splitta comunque).
const SLICE: usize = 4096;

fn real_main(_sp: u64) -> ! {
    println!("[s25fuse] avvio (caricamento staticone da FAT)");
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
    // S2.5e: argv ["rustc", "--version"] (scala graduata: --version non
    // tocca il sysroot; il blocco packed deve vivere fino alla spawn).
    let argblock = match civis::pack_args(&[b"rustc", b"--version"], &[]) {
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
                println!("[s25fuse] staticone uscito code={} pid={}", m.w0 as i64, m.w1);
                done(true);
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
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
