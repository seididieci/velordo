//! usershell — Shell interattiva per Velordo (Fase 9.4, utility Fase 18).
//!
//! Client del terminale: NON mappa il VGA. Apre `/dev/input/keyboard` e usa lo
//! stesso fd per leggere i tasti (read) e per scrivere l'output (write): il
//! console server possiede la VGA, disegna l'output e fa l'echo dei tasti
//! (Opzione B). La shell gestisce solo la linea logica dei comandi.
//! Comandi: ls, cat, touch, mkdir, mount, umount, echo, clear, wc, hexdump,
//! kill, cd, pwd, cp, mv, rm, rmdir, source, run, jobs, wait, fg, bg, exit,
//! help (+ redirect/pipe/env/PATH in 12-utilities). Tutti i path passano per
//! `resolve()`: la shell tiene una cwd client-side e accetta path relativi
//! (Fase 18.1).

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::vec;
use civis;

mod cmd_fs;
mod cmd_info;
mod cmd_run;
mod cmd_source;
mod cwd;
mod parser;
mod redirect;
mod repl;
mod term;

struct PanicSink;

impl core::fmt::Write for PanicSink {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let _ = civis::print_string(s.as_bytes());
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let _ = write!(PanicSink, "[shell] panic: {}\n", info);
    civis::exit(1)
}
