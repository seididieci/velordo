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
pub use ipc::{IpcResult, ipc_send, ipc_send_async, ipc_recv_until, ipc_recv_nonblock, ipc_reply};
// Compat: PsSnap era `pub` prima dello split (nessun uso interno attuale).
#[allow(unused_imports)]
pub use ps::PsSnap;
pub use ps::{process_state, process_ps, set_owned_name, set_parent_chan, parent_channel, process_of, process_name, process_cr3, process_image_id, process_image_class, set_image_class, set_thread_fs, group_leader, set_cwd, get_cwd};
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

/// Trust table ADR-0079 fasi 3–4: ImageId -> ImageClass + porte I/O, popolata
/// da cardo via SYS_TRUST_IMAGE. Max 32 entry, mai sovrascritte da altri
/// chiamanti. Il kernel fa lookup allo spawn/exec per assegnare la classe al
/// PCB e le porte al TSS del processo.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrustEntry {
    id: syscall_numbers::ImageId,
    class: syscall_numbers::ImageClass,
    /// Voci valide di `io` (0..=4). Le porte vengono dal manifest firmato
    /// (ADR-0079 fase 4), mai da una dichiarazione del chiamante.
    io_count: u8,
    io: [(u16, u16); syscall_numbers::TRUST_IO_MAX],
    used: bool,
}

const TRUST_MAX: usize = 32;

static mut TRUST_TABLE: [TrustEntry; TRUST_MAX] = [TrustEntry {
    id: syscall_numbers::ImageId::new(0, 0),
    class: syscall_numbers::ImageClass::Untrusted,
    io_count: 0,
    io: [(0, 0); syscall_numbers::TRUST_IO_MAX],
    used: false,
}; TRUST_MAX];

/// Lookup completo per `image_id`: `(classe, porte)`. `None` = nessuna entry
/// (immagine non verificata: `Untrusted`, nessuna porta). Chiamato sotto
/// SCHED.lock da create_user/exec_current: nessun lock qui (accesso statico).
pub(crate) fn trust_lookup(
    id: syscall_numbers::ImageId,
) -> Option<(syscall_numbers::ImageClass, [(u16, u16); syscall_numbers::TRUST_IO_MAX], usize)> {
    unsafe {
        let table = core::ptr::addr_of!(TRUST_TABLE).cast::<TrustEntry>();
        for i in 0..TRUST_MAX {
            let e = &*table.add(i);
            if e.used && e.id == id {
                return Some((e.class, e.io, e.io_count as usize));
            }
        }
    }
    None
}

/// Classe per `image_id` (default `Untrusted`). Comodita' per i gate
/// strutturali, che non hanno bisogno delle porte.
pub(crate) fn trust_class(id: syscall_numbers::ImageId) -> syscall_numbers::ImageClass {
    trust_lookup(id)
        .map(|(c, _, _)| c)
        .unwrap_or(syscall_numbers::ImageClass::Untrusted)
}

/// Registra `id -> (classe, porte)`. Ritorna true se OK, false se tabella
/// piena o chiamante non autorizzato. Chiamato da sys_trust_image (non sotto
/// lock).
pub(crate) fn trust_register(
    id: syscall_numbers::ImageId,
    class: syscall_numbers::ImageClass,
    io: &[(u16, u16)],
    caller_pid: usize,
) -> bool {
    if io.len() > syscall_numbers::TRUST_IO_MAX {
        return false;
    }
    // Solo cardo (owner di Service::Cardo) o init possono registrare: il
    // kernel e' il punto di enforcement, quindi il writer della trust table
    // deve essere UNO (il servizio che verifica le firme). Un figlio di init
    // qualsiasi non basta: sarebbe un hole (chiunque spawato da init
    // promuoverebbe immagini arbitrarie).
    let allowed = caller_pid == 1
        || crate::relay::channels::lookup(syscall_numbers::Service::Cardo) == Some(caller_pid);
    if !allowed {
        return false;
    }
    let mut ranges = [(0u16, 0u16); syscall_numbers::TRUST_IO_MAX];
    ranges[..io.len()].copy_from_slice(io);
    unsafe {
        let table = core::ptr::addr_of_mut!(TRUST_TABLE).cast::<TrustEntry>();
        // Aggiorna entry esistente con stesso id (idempotente).
        for i in 0..TRUST_MAX {
            let e = &*table.add(i);
            if e.used && e.id == id {
                let e_mut = &mut *table.add(i);
                e_mut.class = class;
                e_mut.io = ranges;
                e_mut.io_count = io.len() as u8;
                return true;
            }
        }
        // Altrimenti trova slot libero.
        for i in 0..TRUST_MAX {
            let e = &*table.add(i);
            if !e.used {
                let e_mut = &mut *table.add(i);
                e_mut.id = id;
                e_mut.class = class;
                e_mut.io = ranges;
                e_mut.io_count = io.len() as u8;
                e_mut.used = true;
                return true;
            }
        }
    }
    false
}

/// Priorita' a 32 livelli (0 = idle, 31 = massima). Newtype struct con
/// costanti alias (`Priority::High`/`Normal`/`Low`) per leggibilita'.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[allow(non_upper_case_globals)]
pub struct Priority(pub u8);

#[allow(non_upper_case_globals)]
#[allow(dead_code)] // alias documentati: il codice usa Normal/Idle
impl Priority {
    pub const High: Priority = Priority(31);
    pub const Normal: Priority = Priority(16);
    pub const Low: Priority = Priority(1);
    pub const Idle: Priority = Priority(0);
}
