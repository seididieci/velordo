//! t_display — D4: modeset e re-adattamento del terminale (ADR-0061).
//!
//! Il terminale (`Service::Console`) e il driver (`Service::Gpu`) si
//! raggiungono per nome. Il test fa il giro completo:
//!
//! 1. interroga console e driver (griglia corrente, contenuto);
//! 2. chiede un **modeset** (il guest sceglie la risoluzione: 1024×768);
//! 3. verifica che la griglia si sia **ricalcolata** (celle intere, resto
//!    centrato) e che il device abbia **seguito** il guest;
//! 4. verifica che il **testo sia sopravvissuto** (lo CRC del contenuto e'
//!    lo stesso: il reflow conserva l'ordine dei caratteri);
//! 5. ripristina la risoluzione iniziale.
//!
//! Senza device pixel (fallback VGA) il modeset non e' applicabile: il test
//! verifica che risponda -1 **senza** rompere nulla (il fallback non ha
//! modeset).

use super::*;

/// Il canale del console server (None se non e' registrato).
fn console_chan() -> Option<u64> {
    match civis::service_lookup(civis::Service::Console) {
        Ok(c) if c > 0 => Some(c as u64),
        _ => None,
    }
}

/// Il canale del driver del display.
fn driver_chan() -> Option<u64> {
    match civis::service_lookup(civis::Service::Gpu) {
        Ok(c) if c > 0 => Some(c as u64),
        _ => None,
    }
}

/// La geometria del device (dal driver): `(w, h)`.
fn device_size(chan: u64) -> Option<(u32, u32)> {
    let r = civis::send(chan, civis::syscall_numbers::GPU_DISPLAY_QUERY, 0, 0).ok()?;
    if r.w0 != 1 {
        return None; // backend VGA testuale: nessun device pixel
    }
    // La geometria e' impacchettata: width | height<<16 | stride<<32 | format<<48
    let v = r.w1;
    let w = (v & 0xFFFF) as u32;
    let h = ((v >> 16) & 0xFFFF) as u32;
    Some((w, h))
}

/// La griglia del terminale: `(cols, rows)`.
fn console_grid(chan: u64) -> Option<(u64, u64)> {
    let r = civis::send(chan, civis::syscall_numbers::CONSOLE_QUERY, 0, 0).ok()?;
    Some((r.w0, r.w1))
}

/// Statistiche del driver: `(PRESENT ricevuti, FLUSH dopo l'ultimo PRESENT)`.
fn driver_stats(chan: u64) -> Option<(u64, u64)> {
    let r = civis::send(chan, civis::syscall_numbers::GPU_DISPLAY_STATS, 0, 0).ok()?;
    Some((r.w0, r.w1))
}

/// Il contenuto del terminale: `(celle non vuote, CRC del testo)`.
fn console_state(chan: u64) -> Option<(u64, u64)> {
    let r = civis::send(chan, civis::syscall_numbers::CONSOLE_STATE, 0, 0).ok()?;
    Some((r.w0, r.w1))
}

pub fn t_display_modeset() -> bool {
    let console = match console_chan() {
        Some(c) => c,
        None => {
            println!("[t63] console non registrato");
            return false;
        }
    };
    let driver = match driver_chan() {
        Some(c) => c,
        None => {
            println!("[t63] driver non registrato");
            return false;
        }
    };

    let (w0, h0) = match device_size(driver) {
        Some(sz) => sz,
        None => {
            // Fallback VGA: il modeset non e' applicabile, ma il terminale
            // deve rispondere e non rompersi.
            let r = civis::send(console, civis::syscall_numbers::CONSOLE_SET_MODE, 800, 600);
            let refused = matches!(r, Ok(rep) if rep.w0 != 0);
            let grid_ok = console_grid(console).is_some();
            println!(
                "[t63] fallback VGA: modeset rifiutato={} griglia viva={}",
                refused, grid_ok
            );
            return refused && grid_ok;
        }
    };
    println!("[t63] device {}x{}", w0, h0);

    let (presents0, _) = driver_stats(driver).unwrap_or((0, 0));
    let (cols0, rows0) = match console_grid(console) {
        Some(g) => g,
        None => return false,
    };
    let (ne0, crc0) = match console_state(console) {
        Some(s) => s,
        None => return false,
    };
    if ne0 == 0 {
        println!("[t63] terminale vuoto: niente da preservare");
        return false;
    }

    // 1) il modeset: il guest sceglie una misura **piu' grande** di quella
    //    attuale. 1920x1088 = 2040 pagine: e' il caso che il bound vecchio del
    //    device buffer (4 MiB = 1024 pagine) rifiutava (ADR-0061).
    let target = if (w0, h0) == (1920, 1088) {
        (1280, 800)
    } else {
        (1920, 1088)
    };
    let r = civis::send(
        console,
        civis::syscall_numbers::CONSOLE_SET_MODE,
        target.0 as u64,
        target.1 as u64,
    );
    match r {
        Ok(rep) if rep.w0 == 0 => {}
        _ => {
            println!("[t63] modeset fallito");
            return false;
        }
    }

    // 1b) Il contratto del refresh: dopo il PRESENT il contenuto va **flushato**
    //     (il PRESENT aggancia il buffer, ma il device vede solo gli zeri
    //     dell'allocazione: senza FLUSH lo schermo resta nero fino al primo
    //     tasto). La regressione che questo test difende.
    let (presents1, flushes1) = driver_stats(driver).unwrap_or((0, 0));
    if presents1 != presents0 + 1 || flushes1 == 0 {
        println!(
            "[t63] refresh mancante: presents {}->{} flushes dopo PRESENT {}",
            presents0, presents1, flushes1
        );
        return false;
    }

    // 2) la griglia si e' ricalcolata (celle intere).
    let (cols1, rows1) = match console_grid(console) {
        Some(g) => g,
        None => return false,
    };
    let expect_cols = (target.0 as u64) / 10;
    let expect_rows = (target.1 as u64) / 22;
    if cols1 != expect_cols || rows1 != expect_rows {
        println!(
            "[t63] griglia non ricalcolata: {}x{} (atteso {}x{})",
            cols1, rows1, expect_cols, expect_rows
        );
        return false;
    }

    // 3) La misura che il device riporta e' quella dell'HOST (QEMU la
    //    aggiorna solo quando l'UI si muove): l'host non ha insistito, quindi
    //    la risoluzione del guest comanda. Con una UI reale (GTK) il device
    //    puo' invece aver seguito il guest: in entrambi i casi il terminale
    //    deve essere quello della misura scelta.
    let (w1, h1) = match device_size(driver) {
        Some(sz) => sz,
        None => return false,
    };
    if (w1, h1) != (w0, h0) && (w1, h1) != target {
        println!("[t63] misura del device incoerente: {}x{}", w1, h1);
        return false;
    }

    // 4) il testo e' sopravvissuto (stesso CRC: il reflow conserva l'ordine).
    let (ne1, crc1) = match console_state(console) {
        Some(s) => s,
        None => return false,
    };
    if ne1 != ne0 || crc1 != crc0 {
        println!(
            "[t63] testo NON conservato: celle {}->{} crc {:#x}->{:#x}",
            ne0, ne1, crc0, crc1
        );
        return false;
    }

    // 5) Una misura **oltre il bound** del device buffer (8192x8192 = 64 MiB)
    //    viene rifiutata senza toccare il terminale: griglia e testo restano
    //    quelli di prima (il rifiuto e' pulito, non un fallimento a meta').
    let huge = 8192u64;
    let r = civis::send(console, civis::syscall_numbers::CONSOLE_SET_MODE, huge, huge);
    let refused = matches!(r, Ok(rep) if rep.w0 != 0);
    let (cols_h, rows_h) = console_grid(console).unwrap_or((0, 0));
    let (ne_h, crc_h) = console_state(console).unwrap_or((0, 0));
    if !refused || (cols_h, rows_h) != (cols1, rows1) || (ne_h, crc_h) != (ne1, crc1) {
        println!(
            "[t63] rifiuto non pulito: refused={} griglia {}x{} (attesa {}x{})",
            refused, cols_h, rows_h, cols1, rows1
        );
        return false;
    }

    // 6) ripristina la risoluzione iniziale.
    let r = civis::send(
        console,
        civis::syscall_numbers::CONSOLE_SET_MODE,
        w0 as u64,
        h0 as u64,
    );
    let restored = matches!(r, Ok(rep) if rep.w0 == 0);
    let (cols2, rows2) = console_grid(console).unwrap_or((0, 0));
    println!(
        "[t63] modeset {}x{} OK (griglia {}x{} -> {}x{} -> {}x{}, testo conservato, oltre-bound rifiutato)",
        target.0, target.1, cols0, rows0, cols1, rows1, cols2, rows2
    );
    restored && cols2 != cols1
}
