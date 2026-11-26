//! fullscreen — un'applicazione a **schermo intero** sulla sessione (ADR-0098).
//!
//! Non e' un editor: e' la **base**. Prende la tela (`ESC[?1049h`), nasconde il
//! cursore, disegna con posizionamento assoluto (`ESC[r;cH`), cancella
//! (`ESC[2J`, `ESC[K`), legge i tasti e **si riadatta** quando la geometria
//! cambia (display o tile: il resize non lo ha chiesto lui). All'uscita lascia
//! la tela e il testo di prima torna com'era, scrollback compreso.
//!
//! Tasti: `hjkl` muovono la stella, `m` cambia risoluzione (per provare il
//! riadattamento: il resize non lo chiede l'app), `q` esce.
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

civis::entry!(real_main);

/// Byte del marker sul **seriale** (il log che legge lo script): la tela e'
/// occupata dall'app, quindi i marker non passano dalla sessione.
fn mark(text: &str) {
    let line = format!("[fullscreen] {}\n", text);
    let _ = civis::print_string(line.as_bytes());
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

fn draw(scr: &Screen, star: (u16, u16), keys: u32) {
    let (w, h) = scr.size();
    let _ = scr.clear();
    // Bordo: quattro lati disegnati con le posizioni assolute.
    let right = w.saturating_sub(1);
    let bottom = h.saturating_sub(1);
    for col in 1..=right {
        let _ = scr.at(col, 1);
        let _ = scr.write("─".as_bytes());
        let _ = scr.at(col, bottom);
        let _ = scr.write("─".as_bytes());
    }
    for row in 1..=bottom {
        let _ = scr.at(1, row);
        let _ = scr.write("│".as_bytes());
        let _ = scr.at(right, row);
        let _ = scr.write("│".as_bytes());
    }
    for (c, r, s) in [
        (1u16, 1u16, "┌"),
        (right, 1, "┐"),
        (1, bottom, "└"),
        (right, bottom, "┘"),
    ] {
        let _ = scr.at(c, r);
        let _ = scr.write(s.as_bytes());
    }
    // Intestazione e stato al centro.
    let title = format!(" fullscreen {}x{} ", w, h);
    let _ = scr.at(3, 2);
    let _ = scr.write(title.as_bytes());
    let info: String = format!("tasti={}  stella=({},{})  q=esci", keys, star.0, star.1);
    let _ = scr.at(3, 3);
    let _ = scr.write(info.as_bytes());
    // La stella: si scrive **dove** dice la posizione, non dove capita.
    let _ = scr.at(star.0.clamp(2, right.saturating_sub(1)), star.1.clamp(4, bottom.saturating_sub(1)));
    let _ = scr.write("*".as_bytes());
    let _ = civis::flush();
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
    let mut star = (w / 2, h / 2);
    // Il frame va **solo sullo schermo** (`write_screen`): il mirror seriale
    // resta il transcript dell'output, non dei ridisegni (D8c2). Per questo i
    // marker di stato viaggiano sul seriale (`print_string`), non dal frame.
    mark(&format!("start {}x{} star=({},{})", w, h, star.0, star.1));
    let mut keys = 0u32;
    draw(&scr, star, keys);

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
                star = (w / 2, h / 2);
                draw(&scr, star, keys);
            }
            continue;
        }
        let mut quit = false;
        for &b in &buf[..n] {
            keys += 1;
            match b {
                b'q' | b'Q' => quit = true,
                b'h' => star.0 = star.0.saturating_sub(2),
                b'l' => star.0 = star.0.saturating_add(2),
                b'k' => star.1 = star.1.saturating_sub(1),
                b'j' => star.1 = star.1.saturating_add(2),
                // `m`: modeset **chiesto dall'app** (il caso peggiore: la
                // tela cambia misura mentre l'app la occupa).
                b'm' => {
                    let _ = set_mode(if scr.size().0 > 110 { (1024, 768) } else { (1280, 800) });
                }
                _ => {}
            }
        }
        if quit {
            break;
        }
        draw(&scr, star, keys);
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
