//! usertestspin — helper busy-loop della test suite (testland).
//!
//! Spins in ring 3 per un budget di `get_ticks` (PIT) ricevuto via IPC CFG,
//! quindi `send(T_DONE)` all'orchestratore. Modalita':
//!   - w1=0: solo timed (usato per il test di priorita').
//!   - w1=1: mappa anche la pagina scratch `MAP_TEST_PHYS` e incrementa un
//!     contatore a ogni cambio di tick osservato: l'orchestratore, che gira in
//!     parallelo, lo legge mentre è bloccato/spinning → prova che il timer lo
//!     ha preemptato (scheduler RR tra processi Normal).
//!
//! Stesso binario esposto a piu' priorita' (utspin_norm/high, usertestspin
//! Low) tramite piu' righe in `user_binary.rs::NAMED_BINARIES`.

#![no_std]
#![no_main]

use civis::println;
use testproto::*;

unsafe fn counter_ptr() -> *mut u64 {
    SPIN_VA as *mut u64
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    let my_pid = civis::getpid();
    let cfg: civis::IpcMsg = match civis::recv() {
        Ok(m) => m,
        Err(_) => civis::exit(1),
    };
    let parent = civis::CHANNEL_PARENT;
    if cfg.w0 == SPIN_SQUAT_MAGIC {
        let _ = civis::reply(T_ACK, 1, 0);
        // Esito ignorato di proposito: lo squat DEVE fallire (il test lo
        // osserva dal routing, la reply di FS_REGISTER e' sempre ok).
        let _ = civis::fs_register(b"/dev/t51");
        if civis::send(parent, T_READY, 1, 0).is_err() {
            civis::exit(1);
        }
        loop {
            match civis::recv() {
                Ok(m) if m.tag == civis::DEV_OPEN => {
                    let _ = civis::reply(0, 1, 0);
                }
                Ok(m) if m.tag == civis::DEV_CLOSE => {
                    let _ = civis::reply(0, 0, 0);
                }
                Ok(_) => {
                    let _ = civis::reply(T_ACK, 0, 0);
                }
                Err(_) => {}
            }
        }
    }
    let budget = if cfg.w0 == 0 { 20 } else { cfg.w0 as i64 };
    let progress = cfg.w1 == 1;

    let _ = civis::reply(T_ACK, civis::getpid() as u64, 0);

    // Flag: scratch page mappata con successo. Il loop accede a SPIN_VA SOLO
    // se mapped: un accesso a pagina non mappata in user mode = page fault.
    let mut mapped = false;
    if progress {
        mapped = civis::map_physical(civis::MAP_TEST_PHYS, SPIN_VA, 1).is_ok();
        if !mapped {
            println!("[utspin] pid={} WARNING: map_physical FAILED", my_pid);
        }
        if mapped {
            unsafe { core::ptr::write_volatile(counter_ptr(), 0) };
        }
    }

    let t0 = civis::get_ticks();
    let mut last = t0;
    let mut observed = 0i64;
    // Batch: tra due get_ticks (syscall) gira ~512 iterazioni di spin puro.
    // Un busy-loop che chiama get_ticks a ogni iterazione maschera gli
    // interrupt (IF=0 durante la syscall) e affama il timer: il PIT non
    // preempta, il wall-clock non avanza e il budget non scade mai (vedi
    // AGENTS, robustezza scheduler). Con il batch il processo sta in user
    // mode con IF=1 quasi tutto il tempo → il timer scatta regolarmente.
    loop {
        for _ in 0..512 {
            core::hint::spin_loop();
        }
        let now = civis::get_ticks();
        if now != last {
            last = now;
            observed += 1;
            if mapped {
                let c = unsafe { core::ptr::read_volatile(counter_ptr()) };
                unsafe { core::ptr::write_volatile(counter_ptr(), c + 1) };
            }
        }
        if now - t0 >= budget {
            break;
        }
    }

    let _ = civis::send(parent, T_DONE, 1, observed as u64);
    println!("[utspin] pid={} done budget={} observed={}", my_pid, budget, observed);
    civis::exit(0);
}

#[panic_handler]
fn panic_handler(_info: &core::panic::PanicInfo) -> ! {
    println!("[utspin] panic");
    civis::exit(1)
}
