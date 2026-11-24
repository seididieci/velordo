//! logout — torna al prompt di login (ADR-0075).
//!
//! Programma esterno estratto dalla shell nativa. Stampa il messaggio di
//! uscita; il **bounce** del login lo fa la shell, che e' figlia di init:
//! `INIT_BOUNCE` viaggia sul canale di nascita, che per un nipote sarebbe la
//! shell e non init (vedi `execute_command` in `userland/interpres`).

#![no_std]
#![no_main]

extern crate alloc;

use civis;

civis::entry!(real_main);

fn console_write(data: &[u8]) {
    if !civis::sess::out(data) {
        let _ = civis::print_string(data);
    }
}

/// Entry: apre console, annuncia il logout.
fn real_main(_sp: u64) -> ! {
    console_write(b"logout: riavvio del login...\n");
    civis::exit(0)
}
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[logout] panic\n");
    civis::exit(1)
}
