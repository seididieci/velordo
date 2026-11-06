//! t_scroll — T4 (ADR-0066): equivalenza scroll-a-shift vs ricomposizione.
//!
//! Scrive righe sul terminale **una per write** (feed interattivo: il console
//! usa lo shift dei pixel), poi chiede un full repaint e confronta il CRC del
//! framebuffer. Con il wallpaper `bg_rows_equal == false` e lo shift non si
//! userebbe: il test abilita una bg uniforme via `CONSOLE_TEST_MODE` (bit0)
//! per esercitare davvero la strada dello shift. Alla fine ripristina il
//! wallpaper.
//!
//! Senza surface pixel (fallback VGA) il CRC non esiste: PASS con SKIP, come
//! fa `t63` per il modeset.

use super::*;
use crate::t_display::{console_chan, console_grid, console_state};
use alloc::string::String;

/// CRC del framebuffer visibile: `(crc, byte hashati)`.
fn frame_crc(chan: u64) -> Option<(u64, u64)> {
    let r = civis::send(chan, civis::syscall_numbers::CONSOLE_FRAME_CRC, 0, 0).ok()?;
    Some((r.w0, r.w1))
}

/// CRC di una riga di celle: `(crc, byte hashati)`.
fn row_crc(chan: u64, row: usize) -> Option<(u64, u64)> {
    let r = civis::send(chan, civis::syscall_numbers::CONSOLE_ROW_CRC, row as u64, 0).ok()?;
    Some((r.w0, r.w1))
}

pub fn t_scroll_equivalence() -> bool {
    let Some(console) = console_chan() else {
        println!("[t64] console non registrato: SKIP");
        return true;
    };
    // Solo con surface pixel: in VGA il CRC non esiste (0/0).
    let Some((_, n0)) = frame_crc(console) else {
        println!("[t64] frame CRC non disponibile: SKIP");
        return true;
    };
    if n0 == 0 {
        println!("[t64] fallback VGA: SKIP");
        return true;
    }
    // bg uniforme (shift possibile) e shift attivo.
    let _ = civis::send(console, civis::syscall_numbers::CONSOLE_TEST_MODE, 1, 0);
    let Ok(fd) = civis::open("/dev/console", 0) else {
        println!("[t64] /dev/console non apribile: SKIP");
        return true;
    };
    // 3 schermate di righe, una per write: ogni newline in fondo provoca uno
    // scroll (shift dei pixel). Icona Nerd (span 2) e glifo largo Powerline
    // coprono lo sbordo.
    let mut s = String::new();
    for i in 0..120u32 {
        s.clear();
        s.push_str("riga ");
        s.push((b'0' + ((i / 100) % 10) as u8) as char);
        s.push((b'0' + ((i / 10) % 10) as u8) as char);
        s.push((b'0' + (i % 10) as u8) as char);
        s.push(' ');
        s.push('\u{F520}');
        s.push('\u{E0B0}');
        s.push_str(" testo di prova\n");
        let _ = civis::write_fs(fd, s.as_bytes(), s.len());
    }
    let _ = civis::close(fd);
    let rows = console_grid(console).map(|(_, r)| r as usize).unwrap_or(0);
    let mut before = alloc::vec::Vec::new();
    for row in 0..rows {
        before.push(row_crc(console, row).map(|(c, _)| c).unwrap_or(0));
    }
    let Some((crc_a, n_a)) = frame_crc(console) else {
        return false;
    };
    let _ = civis::send(console, civis::syscall_numbers::CONSOLE_REPAINT, 0, 0);
    let Some((crc_b, n_b)) = frame_crc(console) else {
        return false;
    };
    let mut first_bad = None;
    for row in 0..rows {
        let after = row_crc(console, row).map(|(c, _)| c).unwrap_or(0);
        if after != before[row] {
            first_bad = Some(row);
            break;
        }
    }
    // Ripristina il wallpaper (bg layer con l'immagine).
    let _ = civis::send(console, civis::syscall_numbers::CONSOLE_TEST_MODE, 0, 0);
    let non_empty = console_state(console).map(|(n, _)| n).unwrap_or(0);
    let ok = crc_a == crc_b && n_a == n_b && n_a > 0 && non_empty > 0;
    println!(
        "[t64] scroll vs repaint: crc_a={:#010x} crc_b={:#010x} byte={} celle={} first_bad_row={:?} -> {}",
        crc_a,
        crc_b,
        n_a,
        non_empty,
        first_bad,
        if ok { "OK" } else { "DIVERSI" }
    );
    ok
}
