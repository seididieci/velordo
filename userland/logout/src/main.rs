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
use civis::println;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

fn console_write(data: &[u8]) {
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    // Flush SEMPRE: la console e' interattiva.
    civis::flush();
}

/// Entry: apre console, annuncia il logout.
fn real_main(_sp: u64) -> ! {
    let console_fd = match civis::open(CONSOLE_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[logout] ERRORE: impossibile aprire console");
            civis::exit(1);
        }
    };
    unsafe { CONSOLE_FD = console_fd; }

    console_write(b"logout: riavvio del login...\n");
    civis::exit(0)
}
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[logout] panic\n");
    civis::exit(1)
}
