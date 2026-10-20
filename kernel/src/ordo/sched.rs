//! Scheduler unico di Velordo: RT a 32 priorita' + CBS (Fase 11).
//!
//! 32 livelli di priorita' (0 = idle, 31 = massima) con run queue per-priorita'
//! O(1) tramite bitmask `u128` + `leading_zeros()`, piu' Constant Bandwidth
//! Server (`ordo::aegis`) per la bandwidth reservation. Esposto come
//! `crate::ordo::sched` (vedi `ordo.rs`): i chiamanti usano quel path senza
//! conoscere i dettagli RT.

mod queue;
mod futex;
mod spawn;
mod tick;
mod ipc;
mod ps;
mod lifecycle;
pub(crate) mod ctx;
mod spawn_copy;
mod exec;

pub use spawn::{init, spawn, create_user, spawn_thread};
pub use futex::{futex_wait, futex_wake};
pub use spawn_copy::spawn_copy_current;
pub use exec::exec_current;
pub use tick::{on_tick, notify_irq};
pub use ipc::{IpcResult, ipc_send, ipc_send_async, ipc_recv, ipc_recv_nonblock, ipc_reply};
// Compat: PsSnap era `pub` prima dello split (nessun uso interno attuale).
#[allow(unused_imports)]
pub use ps::PsSnap;
pub use ps::{process_state, process_ps, set_owned_name, set_parent_chan, parent_channel, process_of, process_name, process_cr3, process_image_hash, set_thread_fs, group_leader, set_cwd, get_cwd};
pub use lifecycle::{exit_current, exit_process, kill, suspend, resume};

/// Quanto dura il timeslice in tick di PIT (100 Hz) → 2 tick = 20 ms.
const QUANTUM_TICKS: u64 = 2;

/// Massimo numero di PID / entita' schedulabili CONCORRENTI (S-T: processi +
/// thread 1:1 condividono lo stesso pool). `ready_by_prio` usa `u128` (bit i
/// = entity i pronta) → 128 totali, come le tabelle mm (`MAX_PROCS` in
/// `layout.rs`, VMA, rings: gia' a 128). Dal Fase 14 (ADR-0010) i PID dei
/// processi reclamati vengono RIUSATI: il limite e' di concorrenza, non piu'
/// il numero totale di processi creati dal boot.
pub(crate) const MAX_PIDS: usize = 128;

/// Priorita' a 32 livelli (0 = idle, 31 = massima). Newtype struct con
/// costanti alias (`Priority::High`/`Normal`/`Low`) per leggibilita'.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[allow(non_upper_case_globals)]
pub struct Priority(pub u8);

#[allow(non_upper_case_globals)]
impl Priority {
    pub const High: Priority = Priority(31);
    pub const Normal: Priority = Priority(16);
    pub const Low: Priority = Priority(1);
    pub const Idle: Priority = Priority(0);
}
