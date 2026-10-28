// Futex WAIT/WAKE (S-T T3, ADR-0046): attesa su indirizzo user con
// deadline a tick. Chiave (mm_owner, addr): gli indirizzi sono per-address
// space (stesso addr in gruppi diversi = code diverse). Tabella fissa nello
// Scheduler (sotto il suo lock: niente ordinamenti tra lock, stesso pattern
// di `ipc_recv` — check-then-block atomici, mai lost-wakeup).
// Join/detach/mutex userspace vivono sopra (PAL/libc): il kernel da'
// solo dormi/sveglia, mai code di join.

use crate::ordo::process::State;
use core::sync::atomic::Ordering;
use super::ctx::{SCHED, INITIALIZED, switch_to};

/// Waiter massimi concorrenti (fail-loud oltre: -1, mai coda infinita).
pub(super) const FUTEX_MAX_WAITERS: usize = 64;

#[derive(Clone, Copy)]
pub(super) struct FutexWaiter {
    pub(super) used: bool,
    /// Owner mm (leader del gruppo): addr valido solo nel suo spazio.
    pub(super) mm: usize,
    pub(super) addr: u64,
    pub(super) tid: usize,
    /// Tick assoluto di scadenza (0 = mai).
    pub(super) deadline: u64,
}

impl FutexWaiter {
    pub(super) const fn empty() -> Self {
        Self { used: false, mm: 0, addr: 0, tid: 0, deadline: 0 }
    }
}

/// Scaduti al tick `now`: sveglia con timeout. Chiamato da `on_tick` a lock
/// tenuto. Solo entry ancora `used` (= ancora addormentate: le svegliate
/// sono liberate da `futex_wake`, mai viste qui).
pub(super) fn tick_deadlines(sched: &mut super::queue::Scheduler, now: u64) {
    for i in 0..FUTEX_MAX_WAITERS {
        let (hit, tid) = {
            let w = &sched.futex_waiters[i];
            (w.used && w.deadline != 0 && now >= w.deadline, w.tid)
        };
        if !hit {
            continue;
        }
        sched.futex_waiters[i].used = false;
        if tid < sched.processes.len() && sched.processes[tid].state == State::Blocked {
            sched.processes[tid].state = State::Ready;
            sched.processes[tid].futex_woken = Some(false);
            sched.set_ready(tid);
        }
    }
}

/// Libera le entry del `tid` morto (chiamato da `terminate`: mai waiter
/// orfani che bloccano slot o svegliano PID riusati).
pub(super) fn purge_tid(sched: &mut super::queue::Scheduler, tid: usize) {
    for w in sched.futex_waiters.iter_mut() {
        if w.used && w.tid == tid {
            w.used = false;
        }
    }
}

/// wait(addr, expected, deadline): dorme se `*addr == expected` fino a WAKE o
/// deadline (tick assoluti, 0 = mai). Ritorna 0 = svegliato, 1 = non
/// svegliato (mismatch immediato, timeout, spuria da solo-runnable), -1 =
/// argomenti invalidi o tabella piena.
pub fn futex_wait(addr: u64, expected: u32, deadline: u64) -> i64 {
    // Niente check bounds separato: `is_mapped_page` sotto richiede il bit U
    // (kernel-half e basso canonico non passano mai).
    if !INITIALIZED.load(Ordering::Acquire) {
        return -1;
    }
    let mut guard = SCHED.lock();
    let action: Option<(Option<usize>, usize, i64)> = {
        let sched = guard.as_mut().expect("scheduler non inizializzato");
        let cur = match sched.current {
            Some(c) => c,
            None => return -1,
        };
        let mm = sched.processes[cur].thread_group.unwrap_or(cur);
        // Leggibilita': walk di presenza sul cr3 del gruppo (read-only via
        // direct map, mai fault). Sotto lock: niente munmap concorrenti
        // sullo stesso gruppo in single-core. La word futex va inizializzata
        // prima del primo WAIT (uso standard): pagina riservata-ma-mai-toccata
        // (lazy) = -1, si tocca e si riprova. Non mappata = -1 loud (mai
        // fault supervisor su memoria user).
        let cr3 = sched.processes[mm].cr3;
        if !crate::arc::vmm_user::is_mapped_page(cr3, addr & !0xfff) {
            return -1;
        }
        let val = unsafe { core::ptr::read_volatile(addr as *const u32) };
        if val != expected {
            return 1; // mismatch: niente blocco (pattern futex standard)
        }
        let slot = match sched.futex_waiters.iter().position(|w| !w.used) {
            Some(s) => s,
            None => return -1, // tabella piena: loud, mai attesa infinita
        };
        sched.futex_waiters[slot] = FutexWaiter {
            used: true, mm, addr, tid: cur, deadline,
        };
        sched.processes[cur].futex_woken = None;
        sched.processes[cur].state = State::Blocked;
        sched.clear_ready(cur);
        match sched.pick_next() {
            Some(n) if n != cur => Some((Some(cur), n, 0)),
            _ => {
                // Solo runnable: nessuno potra' mai svegliarci (single-core,
                // siamo gli unici vivi) — torna subito spuria.
                sched.processes[cur].state = State::Ready;
                sched.set_ready(cur);
                sched.processes[cur].futex_woken = None;
                sched.futex_waiters[slot].used = false;
                Some((None, cur, 1))
            }
        }
    };
    match action {
        Some((prev, next, ret)) => {
            if prev.is_some() {
                switch_to(prev, next, guard);
            }
            // Al risveglio (WAKE o deadline): esito dal PCB.
            if ret != 0 {
                return ret;
            }
            let guard2 = SCHED.lock();
            let sched = guard2.as_ref().expect("scheduler non inizializzato");
            let cur = match sched.current {
                Some(c) => c,
                None => return -1,
            };
            match sched.processes[cur].futex_woken {
                Some(true) => 0,
                _ => 1,
            }
        }
        None => -1,
    }
}

/// wake(addr, n): sveglia fino a `n` waiter su `(mm, addr)` (n=0 → nessuno,
/// convenzione Linux). Ritorna gli svegliati.
pub fn futex_wake(addr: u64, n: u32) -> i64 {
    if !INITIALIZED.load(Ordering::Acquire) {
        return -1;
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");
    let cur = match sched.current {
        Some(c) => c,
        None => return -1,
    };
    let mm = sched.processes[cur].thread_group.unwrap_or(cur);
    let mut woken = 0i64;
    for i in 0..FUTEX_MAX_WAITERS {
        if woken as u32 >= n {
            break;
        }
        let (hit, tid) = {
            let w = &sched.futex_waiters[i];
            (w.used && w.mm == mm && w.addr == addr, w.tid)
        };
        if !hit {
            continue;
        }
        sched.futex_waiters[i].used = false;
        if tid < sched.processes.len() && sched.processes[tid].state == State::Blocked {
            sched.processes[tid].state = State::Ready;
            sched.processes[tid].futex_woken = Some(true);
            sched.set_ready(tid);
        }
        woken += 1;
    }
    woken
}
