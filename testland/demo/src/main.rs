//! Demo utente (Fase 6.4): processo in ring 3 che usa `civis` per chiamare le
//! syscall del kernel.
//!
//! Flusso:
//!   1. `getpid()` e stampa un messaggio iniziale (write su fd 1).
//!   2. busy-loop in ring 3: periodicamente scrive un "tick". La preemption
//!      del timer continua a far girare gli altri processi (es. uptime) mentre
//!      la demo e' in user mode — dimostrazone che la preemption vale anche in
//!      ring 3.
//!
//! Entry: il kernel carica RIP a USER_CODE, quindi la prima funzione in `.text`
//! (qui `_start`, forzata prima via `demo.ld` KEEP) deve essere `no_mangle`.

#![no_std]
#![no_main]

use civis;
use civis::{println};

/// Scrive il messaggio iniziale con il pid del processo.
fn start() {
    let pid = civis::getpid();
    println!("[demo] hello from userland, pid={}", pid);
}

// Entry della demo: messaggio iniziale poi busy-loop con tick periodici.
// Il kernel carica RIP a USER_CODE = indirizzo di `_start` (inizio `.text`).
civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    start();

    let msg: &[u8] = b"[demo] tick\n";
    let mut counter: u64 = 0;
    loop {
        counter += 1;
        // Ogni ~100M iterazioni scrive un tick: tempo abbastanza lungo per
        // lasciar girare gli altri processi tra un tick e l'altro.
        if counter % 100_000_000 == 0 {
            let _ = civis::write(1, msg.as_ptr(), msg.len());
            counter = 0;
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[demo] panic");
    civis::exit(1)
}
