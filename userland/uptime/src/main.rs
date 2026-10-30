//! Uptime user process (Fase 8.3): stampa l'uptime ogni 5 secondi.
//!
//! Legge il contatore PIT via `get_ticks()` syscall e stampa su seriale
//! (fd 1) ogni 500 tick (5 secondi a 100 Hz).
//!
//! **Costo zero**: usa `futex_wait` con deadline per dormirci tra un update
//! del timer — il processo passa nello stato `Blocked`, il scheduler non lo
//! vede come `Ready`. Niente busy-wait.

#![no_std]
#![no_main]

use civis;
use civis::{println};

const PERIOD_TICKS: i64 = 500; // 100 Hz -> 5 s

// Word futex per-address: zero-initialized nel BSS, mai toccata da altri.
static FUTEX_WORD: u32 = 0;

// Entry: loop infinito che stampa l'uptime ogni 5 secondi.
civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    println!("[uptime] user process up");
    let mut deadline = civis::get_ticks() + PERIOD_TICKS;

    loop {
        let now = civis::get_ticks();

        if now >= deadline {
            // Disabilitato temporaneamente per output seriale pulito.
            /*
            let secs = now / 100;
            let cents = now % 100;
            if cents < 10 {
                println!("[uptime] {}.0{} s", secs, cents);
            } else {
                println!("[uptime] {}.{} s", secs, cents);
            }
            */
            deadline = now + PERIOD_TICKS; // Mantiene il timer attivo anche senza print.
        }

        // Blocco cooperativo: dorme fino a deadline o risveglio spurio.
        // Costo zero CPU — il processo passa in `Blocked`, il scheduler lo
        // toglie dalla ready-queue. Al risveglio (timeout/spurio) ricontrolla
        // la deadline al ciclo successivo.
        let _ = civis::futex_wait(&FUTEX_WORD as *const u32 as usize, 0, deadline as u64);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[uptime] panic");
    civis::exit(1)
}
