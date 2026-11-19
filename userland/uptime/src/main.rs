//! Uptime user process (Fase 8.3): tick periodico ogni 5 secondi.
//!
//! Legge il contatore PIT via `get_ticks()` syscall (100 Hz).
//!
//! **Costo zero**: `task::block_on(Sleep::until(..))` dorme sulla deadline
//! dell'executor — una sola `recv_timeout` armata con la scadenza minima fra
//! i task (E1, ADR-0083): il processo passa `Blocked`, il PIT lo sveglia alla
//! scadenza, nessun risveglio a vuoto e niente busy-wait. E' l'uso reale del
//! timer dell'executor in produzione (e nel gate).
//!
//! Dominio: nessun protocollo in ingresso (nessuno manda messaggi a uptime);
//! l'executor scarta il traffico fuori dominio, come per ogni `block_on`.

#![no_std]
#![no_main]

use civis;
use civis::task::{block_on, Sleep};
use civis::println;

const PERIOD_TICKS: i64 = 500; // 100 Hz -> 5 s

// Entry: loop infinito che dorme 5 secondi alla volta.
civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    println!("[uptime] user process up");
    let mut deadline = civis::get_ticks() + PERIOD_TICKS;

    loop {
        block_on(Sleep::until(deadline.max(0) as u64));
        // Il periodo si riarma da adesso: nessuna deriva accumulata nei
        // risvegli (stessa politica del vecchio futex a deadline).
        deadline = civis::get_ticks() + PERIOD_TICKS;
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[uptime] panic");
    civis::exit(1)
}
