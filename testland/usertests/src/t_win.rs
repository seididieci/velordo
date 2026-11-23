//! t_win — D8b (ADR-0088): finestre del compositore aperte da un cliente.
//!
//! Il test esercita il protocollo `WIN_*` come lo userebbe un'applicazione:
//!
//! 1. un handle di tile **inesistente** e' negato (la tile e' una
//!    capability: non si apre una finestra "a caso");
//! 2. `WIN_OPEN` sulla tile primaria crea la superficie e restituisce l'id;
//! 3. il cliente mappa la superficie in RW e ci disegna un colore noto;
//! 4. il primo `WIN_COMMIT` rende la finestra visibile e **cambia il frame**
//!    (prova che i pixel del cliente sono arrivati allo schermo);
//! 5. `WIN_STATS` conta la finestra in piu';
//! 6. `WIN_CLOSE` la toglie e il frame torna **byte-identico** a prima
//!    (l'area resta al terminale: la tile `Direct` viene ridisegnata).
//!
//! La tile primaria ha id `1` in D8b (una sola tile, schermo intero); in D8c
//! l'handle arrivera' allo spawn, non sara' un numero noto al cliente.
//!
//! Senza surface pixel (fallback VGA) il compositor non si usa: PASS con SKIP,
//! come `t63`/`t64`.

use super::*;
use crate::t_display::console_chan;

/// La tile primaria (unica) del terminale in D8b.
const TILE: u32 = 1;

/// `(visibili, totali)` dal compositor.
fn win_stats(chan: u64) -> Option<(u64, u64)> {
    let r = civis::send(chan, civis::WIN_STATS, 0, 0).ok()?;
    Some((r.w0, r.w1))
}

/// CRC del framebuffer visibile (`None` nel fallback VGA: `w1 == 0`).
fn frame_crc(chan: u64) -> Option<u64> {
    let r = civis::send(chan, civis::syscall_numbers::CONSOLE_FRAME_CRC, 0, 0).ok()?;
    if r.w1 == 0 {
        None
    } else {
        Some(r.w0)
    }
}

pub fn t_window_lifecycle() -> bool {
    let Some(console) = console_chan() else {
        println!("[t73] console non registrato: SKIP");
        return true;
    };
    if frame_crc(console).is_none() {
        println!("[t73] fallback VGA: SKIP");
        return true;
    }
    let before = win_stats(console).unwrap_or((0, 0));

    // 1. un handle inesistente e' negato (nessuna finestra "a caso").
    if civis::win::open(99, (16 << 16) | 16).is_ok() {
        println!("[t73] FAIL: handle di tile inesistente accettato");
        return false;
    }

    // 2. apri una finestra 32x32 sulla tile primaria.
    let open = match civis::win::open(TILE, (32 << 16) | 32) {
        Ok(v) => v,
        Err(_) => {
            println!("[t73] FAIL: apertura negata sulla tile primaria");
            return false;
        }
    };
    let (win, shm) = (open.win, open.shm_id);
    if win == 0 || shm == 0 {
        println!("[t73] FAIL: reply di apertura vuota (win={} shm={})", win, shm);
        return false;
    }

    // 3. mappa RW la superficie condivisa e riempila di magenta opaco (BGRA).
    let va = match civis::shm_map(shm, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(v) => v,
        Err(_) => {
            println!("[t73] FAIL: shm_map della superficie");
            let _ = civis::win::close(win);
            return false;
        }
    };
    let px = va as *mut u32;
    let color = 0x00FF_00FFu32; // BGRA: B=FF G=00 R=FF A=00 (opaco)
    for i in 0..(32u32 * 32) {
        unsafe { px.add(i as usize).write(color) };
    }
    // 4. primo commit: la finestra diventa visibile e il frame cambia.
    let crc_before = frame_crc(console).unwrap_or(0);
    if civis::win::commit(win, 0).is_err() {
        println!("[t73] FAIL: commit rifiutato");
        let _ = civis::win::close(win);
        return false;
    }
    let crc_after = frame_crc(console).unwrap_or(0);
    if crc_after == crc_before {
        println!("[t73] FAIL: il commit non ha cambiato il frame");
        let _ = civis::win::close(win);
        return false;
    }

    // 5. la finestra e' contata dal compositor.
    let during = win_stats(console).unwrap_or((0, 0));
    if during.1 != before.1 + 1 {
        println!("[t73] FAIL: stats attese {} totali, lette {}", before.1 + 1, during.1);
        let _ = civis::win::close(win);
        return false;
    }

    // 6. chiudi: la finestra sparisce e il frame torna com'era. Si smappa la
    //    superficie del cliente (a pagine) prima di uscire dal test.
    if civis::win::close(win).is_err() {
        let _ = civis::munmap(va, 4096);
        println!("[t73] FAIL: close rifiutato");
        return false;
    }
    let _ = civis::munmap(va, 4096);
    let after = win_stats(console).unwrap_or((0, 0));
    if after.1 != before.1 {
        println!("[t73] FAIL: la finestra non e' stata rimossa ({} -> {})", during.1, after.1);
        return false;
    }
    let crc_restored = frame_crc(console).unwrap_or(0);
    if crc_restored != crc_before {
        println!(
            "[t73] FAIL: la chiusura non ha ripristinato il frame ({:#x} != {:#x})",
            crc_restored, crc_before
        );
        return false;
    }

    println!(
        "[t73] finestra: open/commit/close, frame {:#x} -> {:#x} -> {:#x}",
        crc_before, crc_after, crc_restored
    );
    true
}
