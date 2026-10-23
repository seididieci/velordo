//! Uptime user process (Fase 8.3): stampa l'uptime ogni 5 secondi.
//!
//! Legge il contatore PIT via `get_ticks()` syscall e stampa su seriale
//! (fd 1) ogni 500 tick (5 secondi a 100 Hz).

#![no_std]
#![no_main]

use civis;
use civis::{println};

const PERIOD_TICKS: i64 = 500; // 100 Hz -> 5 s

// Entry: loop infinito che stampa l'uptime ogni 5 secondi.
civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    println!("[uptime] user process up");
    let mut deadline = PERIOD_TICKS;

    loop {
        let now = civis::get_ticks();

        if now >= deadline {
            let secs = now / 100;
            let cents = now % 100;
            if cents < 10 {
                println!("[uptime] {}.0{} s", secs, cents);
            } else {
                println!("[uptime] {}.{} s", secs, cents);
            }
            deadline = now + PERIOD_TICKS;
        }

        // Busy-wait: ogni iterazione consuma una piccola quantita' di tempo.
        // Il timer IRQ continua a girare; la preemption ci fa riprendere
        // dopo il quantum.
        for _ in 0..10_000_000 {
            core::hint::spin_loop();
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[uptime] panic");
    civis::exit(1)
}
