//! rightshelper — helper `tst` per il test di **`ceiling_for`** (S3.4b,
//! ADR-0093).
//!
//! Spawnato da `t77` con `ops = RIGHTS_READ`: il tetto del canale FS e'
//! `policy & ops` = sola lettura. Legge `hello.txt` (deve riuscire) e prova a
//! creare/scrivere un file (deve essere **negato** da cardo). Riporta l'esito
//! al padre sul canale di nascita ed esce.

#![no_std]
#![no_main]

use civis;

civis::entry!(real_main);

/// Tag del verdetto: bit 0 = read ok, bit 1 = write permessa.
const VERDICT: u64 = 0x78;

fn real_main(_sp: u64) -> ! {
    // 1. Lettura: consentita (ops READ).
    let read_ok = match civis::open("hello.txt", 0) {
        Ok(fd) => {
            let mut buf = [0u8; 16];
            let ok = civis::read_fs(fd, &mut buf, 16).map(|n| n > 0).unwrap_or(false);
            let _ = civis::close(fd);
            ok
        }
        Err(_) => false,
    };

    // 2. Scrittura: negata (manca `RIGHTS_WRITE` nel tetto del canale).
    let write_ok = match civis::open(
        "rightshelper.txt",
        civis::OPEN_CREATE | civis::OPEN_TRUNC,
    ) {
        Ok(fd) => {
            let w = civis::write_fs(fd, b"x", 1).is_ok();
            let _ = civis::close(fd);
            w
        }
        Err(_) => false,
    };

    let _ = civis::send(
        civis::CHANNEL_PARENT,
        VERDICT,
        (read_ok as u64) | ((write_ok as u64) << 1),
        0,
    );
    civis::exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::send(civis::CHANNEL_PARENT, VERDICT, 0, 0);
    civis::exit(1)
}
