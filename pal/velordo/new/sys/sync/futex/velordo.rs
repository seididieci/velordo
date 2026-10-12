//! Futex Velordo (S2.3): backend per i sync primitivi comuni di std
//! (Mutex/RwLock/Condvar/Once/Parker sono gratis sopra queste 3 funzioni).
//! Forma dell'arm hermit in `sys/sync/futex/mod.rs`.
//!
//! Semantica kernel (SYS_FUTEX_WAIT=56/WAKE=57): WAIT dorme se `*addr ==
//! expected` fino a WAKE o deadline (tick assoluti, 0 = mai); ritorna
//! 0 = svegliato, 1 = timeout/mismatch/spuria, -1 = errore (tabella 64
//! piena, pagina non mappata). Errori e spurie sono indistinguibili per i
//! chiamanti (ricontrollano la condizione in loop): mai deadlock, solo
//! risvegli in piu'. Pre-touch della word prima del WAIT (le pagine heap
//! lazy non ancora toccate farebbero fallire il check di mappatura).

use crate::sync::atomic::{Atomic, AtomicU32, Ordering};
use crate::time::Duration;

/// An atomic for use as a futex that is at least 32-bits but may be larger
pub type Futex = Atomic<Primitive>;
/// Must be the underlying type of Futex
pub type Primitive = u32;

/// An atomic for use as a futex that is at least 8-bits but may be larger.
pub type SmallFutex = Atomic<SmallPrimitive>;
/// Must be the underlying type of SmallFutex
pub type SmallPrimitive = u32;

fn wait_raw(addr: usize, expected: u32, deadline: u64) -> i64 {
    unsafe {
        crate::sys::pal::syscall4(
            crate::sys::pal::numbers::SYS_FUTEX_WAIT,
            addr as u64,
            expected as u64,
            deadline,
            0,
        )
    }
}

fn wake_raw(addr: usize, n: u32) -> i64 {
    unsafe {
        crate::sys::pal::syscall4(
            crate::sys::pal::numbers::SYS_FUTEX_WAKE,
            addr as u64,
            n as u64,
            0,
            0,
        )
    }
}

fn deadline_of(timeout: Option<Duration>) -> u64 {
    match timeout {
        // 0 = mai (attesa infinita).
        None => 0,
        Some(dur) => {
            let ms = dur.as_millis() as u64;
            // 10 ms per tick, arrotonda in eccesso + 1 di margine.
            crate::sys::pal::ticks().wrapping_add(ms.div_ceil(10) + 1)
        }
    }
}

pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    // Pre-touch: materializza la pagina lazy prima del check kernel.
    let _ = futex.load(Ordering::Relaxed);
    wait_raw(futex.as_ptr().addr(), expected, deadline_of(timeout)) == 0
}

#[inline]
pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    wake_raw(futex.as_ptr().addr(), 1) > 0
}

#[inline]
pub fn futex_wake_all(futex: &Atomic<u32>) {
    wake_raw(futex.as_ptr().addr(), u32::MAX);
}

/// Attesa nuda su word statica (sleep/yield del modulo thread): `expected`
/// deve coincidere col valore corrente per dormire davvero.
pub fn wait_on(addr: &AtomicU32, expected: u32, deadline: u64) {
    let _ = addr.load(Ordering::Relaxed);
    let _ = wait_raw(addr.as_ptr().addr(), expected, deadline);
}
