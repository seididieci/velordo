//! subtreechild — figlio di `subtreehelper` (S3.4 subtree).
//!
//! Eredita il subtree `tmp` dal padre per attestazione: legge
//! `tmp/subtree.txt` (ok) e `hello.txt` (negato). Riporta al padre.

#![no_std]
#![no_main]

use civis;

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

fn real_main(_sp: u64) -> ! {
    let inside = read_one("tmp/subtree.txt");
    let outside_refused = !read_one("hello.txt");
    let _ = civis::send(
        civis::CHANNEL_PARENT,
        VERDICT,
        (inside as u64) | ((outside_refused as u64) << 1),
        0,
    );
    civis::exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::send(civis::CHANNEL_PARENT, VERDICT, 0, 0);
    civis::exit(1)
}
