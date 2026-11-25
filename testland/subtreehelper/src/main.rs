//! subtreehelper — prova l'**eredita' del subtree** dei path (S3.4 subtree).
//!
//! Il padre (lanciato da `t77` come `tst`):
//!  1. crea `tmp/subtree.txt` (ha ops ALL);
//!  2. carica i propri byte (serve al figlio, e va fatto **prima** del drop);
//!  3. restringe il proprio canale a `tmp` con `R_RIGHTS_DROP`
//!     (`OPEN|READ|READDIR`);
//!  4. spawna sé stesso con argv `child`: il figlio **eredita** `tmp` per
//!     attestazione (`ps_info.parent`);
//!  5. attende il verdetto: dentro `tmp` si legge, fuori no.
//!
//! Il figlio prova `tmp/subtree.txt` (ok) e `hello.txt` (negato dal subtree
//! ereditato) e riporta sul canale di nascita.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use civis;
use civis::println;

civis::entry!(real_main);

const VERDICT: u64 = 0x79;

fn read_one(path: &str) -> bool {
    match civis::open(path, 0) {
        Ok(fd) => {
            let mut buf = [0u8; 8];
            let ok = civis::read_fs(fd, &mut buf, 8).map(|n| n > 0).unwrap_or(false);
            let _ = civis::close(fd);
            ok
        }
        Err(_) => false,
    }
}

fn parent(_sp: u64) -> ! {
    // 1. I byte del figlio si leggono PRIMA di tutto (dopo il drop `/test`
    // e' fuori dal subtree).
    let img = civis::obj_get(b"tst", b"test/subtreechild.bin").unwrap_or_default();
    if img.is_empty() {
        println!("[subtreehelper] figlio non caricabile");
        println!("[subtreehelper] risultato: FAIL");
        civis::exit(1);
    }
    let _ = civis::fs::rights::rights_execute_verdict(b"tst", b"test/subtreechild.bin");

    // 2. File di prova dentro `tmp` (prima del drop: serve WRITE).
    let created = match civis::open("tmp/subtree.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC) {
        Ok(fd) => {
            let ok = civis::write_fs(fd, b"subtree", 7).is_ok();
            let _ = civis::close(fd);
            ok
        }
        Err(_) => false,
    };
    if !created {
        println!("[subtreehelper] creazione tmp/subtree.txt fallita");
        println!("[subtreehelper] risultato: FAIL");
        civis::exit(1);
    }

    // 3. Restringe: da qui solo `tmp`, sola lettura.
    if civis::rights_drop(
        civis::RIGHTS_OPEN | civis::RIGHTS_READ | civis::RIGHTS_READDIR,
        Some("tmp"),
    )
    .is_err()
    {
        println!("[subtreehelper] rights_drop(tmp) fallita");
        println!("[subtreehelper] risultato: FAIL");
        civis::exit(1);
    }

    // 4. Spawna il figlio con argv `child` (byte gia' in memoria).
    let meta = match civis::SpawnMeta::new("subtreechild", 16, &[]) {
        Some(m) => m,
        None => {
            println!("[subtreehelper] SpawnMeta non valida");
            println!("[subtreehelper] risultato: FAIL");
            civis::exit(1);
        }
    };
    let chan = match civis::spawn_image(&img, &meta) {
        Ok(c) => c,
        Err(_) => {
            println!("[subtreehelper] spawn del figlio fallito");
            println!("[subtreehelper] risultato: FAIL");
            civis::exit(1);
        }
    };

    // 5. Verdetto.
    let mut verdict = None;
    let t0 = civis::get_ticks();
    while civis::get_ticks() - t0 < 500 {
        match civis::recv_timeout((civis::get_ticks() + 10) as u64) {
            Ok(m) if m.tag == VERDICT => {
                let _ = civis::reply(VERDICT, 0, 0);
                verdict = Some(m.w0);
                break;
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    let ok = match verdict {
        Some(bits) => {
            let inside = bits & 1 != 0;
            let outside_refused = bits & 2 != 0;
            println!(
                "[subtreehelper] dentro={} fuori_negato={} (canale {})",
                inside, outside_refused, chan
            );
            inside && outside_refused
        }
        None => {
            println!("[subtreehelper] nessun verdetto dal figlio");
            false
        }
    };
    if ok {
        println!("[subtreehelper] risultato: PASS");
    } else {
        println!("[subtreehelper] risultato: FAIL");
    }
    civis::exit(if ok { 0 } else { 1 })
}

fn real_main(_sp: u64) -> ! {
    parent(_sp)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[subtreehelper] panic\n");
    civis::exit(1)
}
