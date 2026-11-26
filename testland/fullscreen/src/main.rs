//! fullscreen — un'applicazione a **schermo intero** sulla sessione (ADR-0098).
//!
//! Non e' un editor: e' la **base**. Prende la tela (`ESC[?1049h`), nasconde il
//! cursore, disegna con posizionamento assoluto (`ESC[r;cH`), cancella
//! (`ESC[2J`, `ESC[K`), legge i tasti e **si riadatta** quando la geometria
//! cambia (display o tile: il resize non lo ha chiesto lui). All'uscita lascia
//! la tela e il testo di prima torna com'era, scrollback compreso.
//!
//! Tasti: `hjkl` muovono la stella, `m` cambia risoluzione (per provare il
//! riadattamento: il resize non lo chiede l'app), `p` disegna una **toppa**
//! (ADR-0099: barra + linea in pixel, sotto i glifi), `q` esce.
//!
//! Il disegno passa dal **canvas a celle** (`libs/tabula`): il frame si compone
//! in memoria, `Screen::present` lo manda in una feed e il canvas manda solo il
//! **differenziale** (ADR-0098 addendum). Un tasto costa ~34 byte e una IPC:
//! disegnare cella per cella erano ~660 render+flush per tasto, e il frame si
//! vedeva arrivare a pezzi (il "lampeggio").
//!
//! Seedato in `usr/bin/fullscreen.bin` (uso manuale dalla shell) e provato da
//! `scripts/run-tui-tests.sh`, fuori dal gate: prende il terminale, quindi non
//! puo' girare nella suite.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use civis;
use civis::sess::Screen;
use pictura::{rgb, Canvas};
use tabula::Canvas as Cells;

civis::entry!(real_main);

/// Byte del marker sul **seriale** (il log che legge lo script): la tela e'
/// occupata dall'app, quindi i marker non passano dalla sessione.
fn mark(text: &str) {
    let line = format!("[fullscreen] {}\n", text);
    let _ = civis::print_string(line.as_bytes());
}

/// Disegna una **toppa**: 12 celle × 1 riga di pixel veri (una barra e una
/// linea, non glifi) e ne verifica l'effetto sul conteggio del server.
fn draw_patch(scr: &Screen) -> bool {
    let Ok((cw, ch)) = civis::sess::metrics() else {
        mark("patch ERRORE: metriche non disponibili");
        return false;
    };
    let cells = 12u16;
    let mut canvas = Canvas::new(cells as usize * cw as usize, ch as usize, rgb(24, 24, 24));
    canvas.rect(
        0,
        0,
        canvas.w,
        canvas.h,
        1,
        rgb(141, 150, 168),
    );
    canvas.bar(
        2,
        2,
        canvas.w.saturating_sub(4),
        canvas.h.saturating_sub(4),
        640,
        rgb(24, 24, 24),
        rgb(91, 124, 255),
    );
    canvas.line(2, canvas.h / 2, canvas.w - 3, canvas.h / 2, 1, rgb(255, 255, 255));
    let Some(sess) = civis::sess::current() else {
        mark("patch ERRORE: nessuna sessione");
        return false;
    };
    let (_, rows) = scr.size();
    let at = rows.saturating_sub(3);
    // Prova visiva: il CRC della **riga di pixel** prima e dopo. Se i pixel
    // della toppa arrivano al framebuffer, il CRC cambia (ADR-0099).
    let crc0 = row_crc(at);
    match sess.patch(2, at, cells, 1, canvas.bytes(), b"== bar ==") {
        Ok(()) => {}
        Err(_) => {
            mark("patch ERRORE: rifiutata");
            return false;
        }
    }
    let bytes = civis::sess::patch_bytes().unwrap_or(0);
    let crc1 = row_crc(at);
    mark(&format!(
        "patch bytes={} at={} crc={:x}/{:x}",
        bytes,
        at,
        crc0.unwrap_or(0),
        crc1.unwrap_or(0)
    ));
    true
}

/// CRC della riga di celle `row` (tag di test della console, T4).
fn row_crc(row: u16) -> Option<u32> {
    let console = civis::service_lookup(civis::Service::Console).ok()?;
    let r = civis::send(
        console as u64,
        civis::syscall_numbers::CONSOLE_ROW_CRC,
        row as u64,
        0,
    )
    .ok()?;
    if r.w1 == 0 {
        return None;
    }
    Some(r.w0 as u32)
}

/// Chiede al compositore una risoluzione diversa (D4): l'host la segue, il DE
/// ridispone il layout, la sessione cambia misura.
fn set_mode((w, h): (u16, u16)) -> bool {
    let Ok(console) = civis::service_lookup(civis::Service::Console) else {
        return false;
    };
    match civis::send(
        console as u64,
        civis::syscall_numbers::CONSOLE_SET_MODE,
        w as u64,
        h as u64,
    ) {
        Ok(r) if r.w0 == 0 => {
            mark(&format!("modeset {}x{}", w, h));
            true
        }
        _ => {
            mark(&format!("modeset {}x{} rifiutato", w, h));
            false
        }
    }
}

/// Compone il frame **nel canvas** (celle, nessuna IPC) e manda il
/// differenziale: un tasto tocca due celle (la stella vecchia e la nuova) piu'
/// la riga di stato, non 660.
/// I limiti della stella: dentro il bordo e sotto l'intestazione (colonne e
/// righe **1-based**, come le sequenze ANSI).
fn star_bounds(scr: &Screen) -> ((u16, u16), (u16, u16)) {
    let (w, h) = scr.size();
    let (right, bottom) = (w.saturating_sub(1), h.saturating_sub(1));
    ((2, right.saturating_sub(1)), (4, bottom.saturating_sub(1)))
}

/// La stella **nel modello**: una posizione fuori dalla tela non esiste, non
/// "si nasconde" (se il modello va oltre il bordo, il disegno resta incollato
/// al bordo e le pressioni successive si perdono).
fn clamp_star(star: (u16, u16), scr: &Screen) -> (u16, u16) {
    let ((x0, x1), (y0, y1)) = star_bounds(scr);
    (star.0.clamp(x0, x1.max(x0)), star.1.clamp(y0, y1.max(y0)))
}

fn draw(canvas: &mut Cells, scr: &mut Screen, star: (u16, u16), keys: u32) {
    let (w, h) = canvas.size();
    let (w, h) = (w as u16, h as u16);
    canvas.clear();
    // Bordo: quattro lati, in celle (1-based nelle sequenze, 0-based qui).
    let right = w.saturating_sub(1);
    let bottom = h.saturating_sub(1);
    for col in 1..=right {
        canvas.set(col as usize - 1, 0, '─');
        canvas.set(col as usize - 1, bottom as usize - 1, '─');
    }
    for row in 1..=bottom {
        canvas.set(0, row as usize - 1, '│');
        canvas.set(right as usize - 1, row as usize - 1, '│');
    }
    for (c, r, g) in [
        (1u16, 1u16, '┌'),
        (right, 1, '┐'),
        (1, bottom, '└'),
        (right, bottom, '┘'),
    ] {
        canvas.set(c as usize - 1, r as usize - 1, g);
    }
    // Intestazione e stato: solo le celle che cambiano finiscono sul filo (il
    // canvas confronta con la copia ombra).
    let title = format!(" fullscreen {}x{} ", w, h);
    canvas.text(2, 1, &title);
    let info: String = format!("tasti={}  stella=({},{})  q=esci", keys, star.0, star.1);
    canvas.text(2, 2, &info);
    // La stella: si scrive **dove** dice la posizione. Il clamp e' nel
    // **modello** (`clamp_star`), non qui: clampare solo al disegno lascia la
    // posizione vera fuori dallo schermo e servirebbero piu' pressioni per
    // rientrare (difetto visto a schermo).
    canvas.set(star.0.saturating_sub(1) as usize, star.1.saturating_sub(1) as usize, '*');
    // **Un frame, una feed** (ADR-0098 addendum): il costo si dichiara, cosi'
    // il lampeggio e' una misura e non un'impressione.
    let bytes = canvas.diff();
    let n = bytes.len();
    if scr.write(bytes).is_err() {
        mark("frame ERRORE: scrittura rifiutata");
        return;
    }
    frame_mark(scr, n, star);
}

/// Chiede alla console un **repaint completo** (`CONSOLE_REPAINT`, hook di
/// test): il frame raggiunto per differenziale deve essere **identico** a quello
/// di un repaint da zero (T4, t64) — e' cosi' che si prova che il percorso
/// incrementale non dipinge un fondo sbagliato.
fn repaint() {
    match civis::service_lookup(civis::Service::Console) {
        Ok(chan) => {
            let _ = civis::send(chan as u64, civis::syscall_numbers::CONSOLE_REPAINT, 0, 0);
            mark("repaint");
        }
        Err(_) => mark("repaint ERRORE: console non trovata"),
    }
}

/// Manda il frame e riporta il costo sul seriale (`bytes` = il differenziale).
fn frame_mark(scr: &mut Screen, bytes: usize, star: (u16, u16)) {
    let chunks = scr.chunks();
    match scr.present() {
        Ok(_) => mark(&format!(
            "frame bytes={} chunks={} star=({},{})",
            bytes, chunks, star.0, star.1
        )),
        Err(_) => mark("frame ERRORE: non presentato"),
    }
}

fn real_main(_sp: u64) -> ! {
    // Il lettore e' l'app (l'eco del modello spenta da `attach`).
    if civis::input::attach().is_err() {
        mark("ERRORE: sessione non raggiungibile");
        civis::exit(1);
    }
    let mut scr = match Screen::open() {
        Ok(s) => s,
        Err(_) => {
            mark("ERRORE: tela non disponibile");
            civis::exit(1);
        }
    };
    let (w, h) = scr.size();
    let mut canvas = Cells::new(w as usize, h as usize);
    let mut star = clamp_star((w / 2, h / 2), &scr);
    // Il frame va **solo sullo schermo** (`write_screen`): il mirror seriale
    // resta il transcript dell'output, non dei ridisegni (D8c2). Per questo i
    // marker di stato viaggiano sul seriale (`print_string`), non dal frame.
    mark(&format!("start {}x{} star=({},{})", w, h, star.0, star.1));
    let mut keys = 0u32;
    draw(&mut canvas, &mut scr, star, keys);

    loop {
        let mut buf = [0u8; 16];
        let n = civis::input::read(&mut buf);
        if n == 0 {
            // Niente tasti: dorme sull'hint. Un resize arriva come hint, ma
            // la coda puo' essere vuota: si controlla a ogni risveglio.
            if !civis::input::wait() {
                mark("ERRORE: lettore perso");
                break;
            }
            if scr.poll_resize() {
                let (w, h) = scr.size();
                mark(&format!("resized {}x{}", w, h));
                star = clamp_star((w / 2, h / 2), &scr);
                // Il canvas scarta le due copie: dopo un resize si ridisegna.
                canvas.resize(w as usize, h as usize);
                draw(&mut canvas, &mut scr, star, keys);
            }
            continue;
        }
        let mut quit = false;
        for &b in &buf[..n] {
            // `r` e' un **hook di test**, non un tasto: non si conta e non
            // deve cambiare il frame. Cosi' il confronto "incrementale vs
            // repaint" misura il disegno, non il proprio rumore.
            if b == b'r' {
                repaint();
                continue;
            }
            keys += 1;
            match b {
                b'q' | b'Q' => quit = true,
                // Un passo = **una** cella (era 2 in orizzontale e 2 in basso
                // contro 1 in alto: asimmetrico e sbagliato), e il clamp e'
                // nel modello.
                b'h' => star = clamp_star((star.0.saturating_sub(1), star.1), &scr),
                b'l' => star = clamp_star((star.0.saturating_add(1), star.1), &scr),
                b'k' => star = clamp_star((star.0, star.1.saturating_sub(1)), &scr),
                b'j' => star = clamp_star((star.0, star.1.saturating_add(1)), &scr),
                // `m`: modeset **chiesto dall'app** (il caso peggiore: la
                // tela cambia misura mentre l'app la occupa).
                b'm' => {
                    let _ = set_mode(if scr.size().0 > 110 { (1024, 768) } else { (1280, 800) });
                }
                b'p' => {
                    let _ = draw_patch(&scr);
                }
                b'r' => repaint(),
                _ => {}
            }
        }
        if quit {
            break;
        }
        draw(&mut canvas, &mut scr, star, keys);
    }

    let _ = scr.close();
    mark(&format!("exit star=({},{}) keys={}", star.0, star.1, keys));
    // Sulla tela principale: la shell ridisegna il suo prompt subito dopo.
    let _ = civis::sess::out(b"fullscreen: uscita\n");
    civis::exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[fullscreen] panic\n");
    civis::exit(1)
}
