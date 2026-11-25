//! floorposix — prova il floor di classe dalla **personalità POSIX** (S3.4b,
//! ADR-0093).
//!
//! La shell nativa lo lancia con floor `ThirdParty` (`usr/bin/floorposix.bin`,
//! firmato third-party). Il programma usa `libr::exec` (la porta POSIX) per
//! provare a eseguire:
//!  1. un'immagine `System` (`/bin/interpres.bin`) -> rifiutata;
//!  2. un'immagine `Untrusted` (`/test/demo.bin`) -> rifiutata dal floor.
//!
//! Serve a chiudere la "finestra" della personalità: non basta che il kernel
//! protegga i binari nativi, la stessa regola deve valere per `libr::exec`.

#![no_std]
#![no_main]

extern crate alloc;

use civis;
use civis::println;

libr::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    // 1. Da ThirdParty a System: no elevazione.
    let sys_refused = libr::exec("/bin/interpres.bin", &[]).is_err();
    println!(
        "[floorposix] exec System: {}",
        if sys_refused { "rifiutato" } else { "AMMESSO" }
    );

    // 2. Da ThirdParty a Untrusted: il floor lo vieta.
    let untrusted_refused = libr::exec("/test/demo.bin", &[]).is_err();
    println!(
        "[floorposix] exec Untrusted: {}",
        if untrusted_refused { "rifiutato" } else { "AMMESSO" }
    );

    let ok = sys_refused && untrusted_refused;
    if ok {
        println!("[floorposix] risultato: PASS");
    } else {
        println!("[floorposix] risultato: FAIL");
    }
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[floorposix] panic\n");
    civis::exit(1)
}
