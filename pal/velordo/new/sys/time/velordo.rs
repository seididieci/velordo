//! Orologio Velordo (S1.3): Instant monotonico su ticks PIT (100 Hz).
//! SystemTime resta unsupported qui dentro (wall-clock vera richiede il
//! servizio Time via IPC: S2) — copiata pari pari da `time/unsupported.rs`
//! perche' `time/mod.rs` non ha modulo unsupported.

use crate::sys::pal::{ticks as pal_ticks};
use crate::time::Duration;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(Duration);

pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::from_secs(0));

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(Duration);

impl Instant {
    pub fn now() -> Instant {
        // 1 tick = 10 ms esatti (PIT 100 Hz).
        Instant(Duration::from_millis(pal_ticks().saturating_mul(10)))
    }

    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_sub(*other)?))
    }
}

impl SystemTime {
    pub const MAX: SystemTime = SystemTime(Duration::MAX);

    pub const MIN: SystemTime = SystemTime(Duration::ZERO);

    /// Costruisce da epoch-secondi + nanosecondi (S2.2: serve a `FileAttr`
    /// per l'mtime di cardo; `now()` resta unsupported senza servizio Time).
    pub const fn new(secs: u64, nanos: u32) -> SystemTime {
        SystemTime(Duration::new(secs, nanos))
    }

    pub fn now() -> SystemTime {
        panic!("wall-clock non implementata su Velordo v1 (serve il servizio Time: S2)")
    }

    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        self.0.checked_sub(other.0).ok_or_else(|| other.0 - self.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_sub(*other)?))
    }
}
