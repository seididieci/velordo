//! testproto — costanti e helper condivisi per le suite di test.
//!
//! Unifica le definizioni speculari che vivevano in
//! `testland/usertests/src/helpers.rs`, `flavours/posix/tests/src/helpers.rs`,
//! `testland/usertest-client/src/main.rs` e `testland/usertest-spin/src/main.rs`.
//! no_std + alloc.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::format_args;
use core::option::Option::{self, Some, None};
use core::result::Result::{Err, Ok};
use civis::println;

// ── Tag protocollo (T_*) ─────────────────────────────────────────────
pub const T_CFG: u64 = 100;
pub const T_ACK: u64 = 101;
pub const T_REQ: u64 = 102;
pub const T_DONE: u64 = 103;
pub const T_STOP: u64 = 104;
pub const T_OPENED: u64 = 105;
pub const T_GO: u64 = 106;
pub const T_READY: u64 = 107;
pub const T_FORKREP: u64 = 108;

// ── Mode client (M_* / MODE_*) ───────────────────────────────────────
pub const M_ECHO: u64 = 0;
pub const M_ZERO: u64 = 1;
pub const M_SRV: u64 = 3;
pub const M_CHURN: u64 = 4;
pub const M_KILLME: u64 = 5;
pub const M_SRVDIE: u64 = 6;
pub const M_SYNCWAIT: u64 = 7;
pub const M_MNTDIE: u64 = 8;
pub const M_OPENDIE: u64 = 9;
pub const M_MAPHAMMER: u64 = 10;
pub const M_FLOOD: u64 = 11;
pub const M_NEST: u64 = 12;
// Fase 29: fault di protezione (helper muore con FAULT_EXIT_CODE).
pub const M_FAULT_RO: u64 = 13;
pub const M_FAULT_NONE: u64 = 14;
pub const M_FAULT_NX: u64 = 15;
pub const M_FAULT_GUARD: u64 = 16;
pub const M_FAULT_GPF: u64 = 17;
pub const M_SHMDEMO: u64 = 18;
pub const M_FAULT_CODE: u64 = 19;
// Fase 33: COW su regione condivisa.
pub const M_COWDEMO: u64 = 20;
// Fase 34: fork COW di se stesso.
pub const M_FORKDEMO: u64 = 21;
// Fase 35: tentativi ostili che devono fallire.
pub const M_ORPHAN: u64 = 22;
pub const M_HARDEN: u64 = 23;
// Fase 36: driver sacrificale / sonda squat.
pub const M_REG51: u64 = 24;
// Fase 37: demo exec in-place.
pub const M_EXECDEMO: u64 = 25;
// Fase 40: diritti GRANT+PIPE.
pub const M_DUPCLAIM: u64 = 26;
pub const M_DUPGRANT: u64 = 27;
pub const M_DUPSIBCLAIM: u64 = 28;
pub const M_SEEKDENY: u64 = 29;
pub const M_SUSPENDENY: u64 = 30;
pub const M_SIGCATCH: u64 = 31;
pub const M_GRANTDENY: u64 = 32;

// ── VA per i test map_physical/aliasing ──────────────────────────────
// Zona libera tra USER_FS_BUFFER e lo heap:
// 0x4000_0020_0000..0x4000_0040_0000.
pub const VA_A: u64 = 0x0000_4000_0030_0000;
pub const VA_B: u64 = 0x0000_4000_0038_0000;
pub const SPIN_VA: u64 = 0x0000_4000_003C_0000;

// ── Fixture / magic ──────────────────────────────────────────────────
pub const HELLO: &[u8] = b"Hello from Velordo ramfs!\n";
pub const ARCA_HELLO: &[u8] = b"Hello from Velordo ramfs!\n";
pub const ARCA_HELLO_PATH: &str = "/hello.txt";
pub const ACFS_MAGIC: &[u8; 4] = b"ACFS";

// w0 magico per la sonda di squat di usertest-spin (binario diverso da
// testcli → hash diverso per il negativo). Speculare a SPIN_SQUAT_MAGIC
# [deprecated(note = "usare SPIN_SQUAT_MAGIC")]
pub const SQUAT_MAGIC: u64 = 0x5351_5541_5435_31;
pub const SPIN_SQUAT_MAGIC: u64 = 0x5351_5541_5435_31;

// ── Mini-harness ─────────────────────────────────────────────────────

/// Registra il risultato di un test con prefisso custom (es. "[usertests]" o
/// "[posixtests]").
pub fn report(prefix: &str, total: &mut u32, ok: &mut u32, name: &str, pass: bool) {
    *total += 1;
    if pass {
        *ok += 1;
        println!("[{}] {}: PASS", prefix, name);
    } else {
        println!("[{}] {}: FAIL", prefix, name);
    }
}

/// Spawna un helper da disco (`/test/*.bin`) e gli invia la CFG (modo=w0,
/// param=w1) sul canale di nascita (ADR-0008). Ritorna
/// (canale verso il figlio, ack.w0).
pub fn spawn_cfg(path: &str, name: &str, prio: u8, mode: u64, param: u64) -> Option<(u64, u64)> {
    let img = civis::load_file(path)?;
    let meta = civis::SpawnMeta::new(name, prio, &[])?;
    let chan = civis::spawn_image(&img, &meta).ok()? as u64;
    let ack = civis::send(chan, T_CFG, mode, param).ok()?;
    Some((chan, ack.w0))
}

/// Attende un T_DONE da uno dei canali in `chans` (risponde col request-id;
/// scarta gli estranei con reply, le notifiche di morte senza). Ritorna
/// (ok, canale del mittente).
pub fn recv_done(chans: &[u64]) -> (bool, u64) {
    loop {
        match civis::recv() {
            Ok(m) => {
                let _ = civis::reply(T_ACK, 0, 0);
                if m.tag == T_DONE && chans.contains(&m.channel) {
                    return (m.w0 == 1, m.channel);
                }
            }
            Err(_) => return (false, 0),
        }
    }
}

/// Attende un messaggio con `tag` dal canale `chan`, rispondendo e scartando
/// qualunque messaggio estraneo arrivi prima. Ritorna true se il messaggio
/// atteso e' arrivato con w0==1.
pub fn recv_expect(chan: u64, tag: u64) -> bool {
    loop {
        match civis::recv() {
            Ok(m) => {
                let _ = civis::reply(T_ACK, 0, 0);
                if m.tag == tag && m.channel == chan {
                    return m.w0 == 1;
                }
            }
            Err(_) => return false,
        }
    }
}

/// Svuota i messaggi residui in coda (EXIT_NOTIFY scartate senza reply).
pub fn drain_stray() {
    while let Some(m) = civis::recv_poll() {
        if !civis::is_exit_notify(&m) {
            let _ = civis::reply(T_ACK, 0, 0);
        }
    }
}

/// Attende EXIT_NOTIFY sul canale `chan` (figlio morto: w0 = exit code,
/// w1 = pid). Gli estranei vengono ignorati.
pub fn wait_exit(chan: u64) -> Option<(i64, i64)> {
    loop {
        match civis::recv() {
            Ok(m) if m.channel == chan && civis::is_exit_notify(&m) => {
                return Some((m.w0 as i64, m.w1 as i64));
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Attende T_READY(w0, w1) sul canale `chan` (handshake NEST).
pub fn recv_ready(chan: u64) -> Option<(u64, u64)> {
    loop {
        match civis::recv() {
            Ok(m) if m.channel == chan && m.tag == T_READY => {
                let _ = civis::reply(T_ACK, 0, 0);
                return Some((m.w0, m.w1));
            }
            Ok(m) if civis::is_exit_notify(&m) => {}
            Ok(_) => {
                let _ = civis::reply(T_ACK, 0, 0);
            }
            Err(_) => return None,
        }
    }
}

/// Ferma il flooder (T_STOP sync) e lo reaped. Mai hang.
pub fn stop_flooder(fchan: u64) {
    if civis::send(fchan, T_STOP, 0, 0).is_ok() {
        let _ = recv_done(&[fchan]);
    }
    let _ = wait_exit(fchan);
}

/// Attesa throttled che `ps_info(pid)` sparisce.
pub fn poll_gone(pid: u64, bound_ticks: i64) -> bool {
    let t0 = civis::get_ticks();
    loop {
        if civis::ps_info(pid as u32).is_none() {
            return true;
        }
        if civis::get_ticks() - t0 > bound_ticks {
            return false;
        }
        for _ in 0..512 {
            core::hint::spin_loop();
        }
    }
}

/// Attesa throttled della prima snapshot `ps` di `pid`.
pub fn poll_parent(pid: u64, bound_ticks: i64) -> Option<Option<u32>> {
    let t0 = civis::get_ticks();
    loop {
        if let Some(e) = civis::ps_info(pid as u32) {
            return Some(e.parent);
        }
        if civis::get_ticks() - t0 > bound_ticks {
            return None;
        }
        for _ in 0..512 {
            core::hint::spin_loop();
        }
    }
}

/// Legge `want` entry di una dir e dice se contiene `needle`.
pub fn dir_contains(path: &str, needle: &str) -> bool {
    let mut e = [0u8; 2048];
    let Ok(n) = civis::readdir(path, &mut e, 2048) else {
        return false;
    };
    let mut found = false;
    civis::test::each_name(&e, n, |name| {
        if name == needle {
            found = true;
        }
    });
    found
}

/// true se il buffer readdir (voci NUL-separate) contiene `name` intero.
pub fn readdir_contains(buf: &[u8], name: &[u8]) -> bool {
    let mut i = 0usize;
    while i < buf.len() && buf[i] != 0 {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if &buf[start..i] == name {
            return true;
        }
        i += 1;
    }
    false
}

/// Legge tutto il file `fd` in `dst` (bound `dst.len()`).
pub fn read_all(fd: i64, dst: &mut [u8]) -> usize {
    let mut got = 0usize;
    while got < dst.len() {
        let rest = dst.len() - got;
        let n = match civis::read_fs(fd, &mut dst[got..], rest) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        got += n;
    }
    got
}

/// Legge `total` byte da `fd` in `out`. Ritorna true se letti tutti.
pub fn read_all_vec(fd: i64, out: &mut Vec<u8>, total: usize) -> bool {
    let mut got = 0usize;
    while got < total {
        let mut chunk = [0u8; 2000];
        let n = match civis::read_fs(fd, &mut chunk, 2000) {
            Ok(n) => n,
            Err(_) => return false,
        };
        if n == 0 {
            return false;
        }
        out.extend_from_slice(&chunk[..n]);
        got += n;
    }
    got == total
}
