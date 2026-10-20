//! Orologio Velordo: `Instant` monotonico su ticks PIT (100 Hz),
//! `SystemTime::now()` dal servizio `Time` (Fase 50: RTC CMOS all'avvio +
//! PIT) via IPC grezza (la PAL non dipende da `civis`). Servizio assente =
//! `UNIX_EPOCH`, mai panic: un compilatore non deve morire per un orologio
//! mancante (S2.5f/F2).

use crate::sys::pal::numbers::{SERVICE_TIME, SYS_SEND, SYS_SERVICE_LOOKUP, TIME_NOW};
use crate::sys::pal::{syscall4, syscall4_out};
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
        match wall_clock() {
            // `w1` = centesimi nel secondo corrente (PIT 100 Hz).
            Some((secs, centis)) => SystemTime(
                Duration::from_secs(secs) + Duration::from_millis(centis.min(99) * 10),
            ),
            // Servizio `Time` assente/morto: orologio non inventato, ma
            // nemmeno panic (rustc chiama `SystemTime::now()` all'avvio).
            None => UNIX_EPOCH,
        }
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

/// `(secondi epoch UTC, centesimi)` dal servizio `Time`, o `None` se il
/// servizio non e' registrato / e' morto. Una `send` sincrona, niente cache.
fn wall_clock() -> Option<(u64, u64)> {
    let chan = unsafe { syscall4(SYS_SERVICE_LOOKUP, SERVICE_TIME, 0, 0, 0) };
    if chan < 0 {
        return None;
    }
    let (rax, _chan, tag, w0, w1) =
        unsafe { syscall4_out(SYS_SEND, chan as u64, TIME_NOW, 0, 0) };
    if rax < 0 || tag != TIME_NOW {
        return None;
    }
    Some((w0, w1))
}
