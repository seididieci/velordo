// Split from ordo/sched.rs (byte-identical move; see facade).
use crate::ordo::process::{Process, State};
use super::ctx::SCHED;

/// Stato del processo `pid` (`None` se non esiste o e' terminato).
pub fn process_state(pid: usize) -> Option<State> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if pid < sched.processes.len() {
        let st = sched.processes[pid].state;
        if st == State::Terminated { None } else { Some(st) }
    } else {
        None
    }
}

/// Snapshot dei campi di `ps` per il processo `pid` (Fase 19.1): un solo lock,
/// `None` se lo slot e' vuoto o il processo e' terminato (come `process_state`).
/// Nome come copia owned 16 B (Fase 21: embedded o `spawn_image`, vedi
/// `Process::name_str`): PsSnap esce dal lock, niente borrow.
pub struct PsSnap {
    pub name: [u8; 16],
    pub state: State,
    /// Sospeso via `SYS_SUSPEND` (Fase 44a): `sys_ps_info` lo espone come
    /// Stopped (2), non come Ready/Blocked sottostante.
    pub suspended: bool,
    pub prio: u8,
    pub parent: Option<usize>,
    pub ipc: crate::ordo::process::IpcState,
    pub ticks_used: u64,
}

/// Copia il nome display in un buffer locale (i log avvengono dopo il
/// rilascio dei borrow sul PCB).
pub(super) fn name_buf(p: &crate::ordo::process::Process) -> ([u8; 16], u8) {
    let s = p.name_str();
    let mut b = [0u8; 16];
    let n = s.len().min(16);
    b[..n].copy_from_slice(&s.as_bytes()[..n]);
    (b, n as u8)
}

/// Nome display come &str da un buffer di `name_buf` (sempre UTF-8 valido).
pub(super) fn name_str_of(buf: &([u8; 16], u8)) -> &str {
    core::str::from_utf8(&buf.0[..buf.1 as usize]).unwrap_or("?")
}

pub fn process_ps(pid: usize) -> Option<PsSnap> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if pid >= sched.processes.len() {
        return None;
    }
    let p = &sched.processes[pid];
    if p.state == State::Terminated {
        return None;
    }
    Some(PsSnap {
        name: name_buf(p).0,
        state: p.state,
        suspended: p.suspended,
        prio: p.priority.0,
        parent: p.parent,
        ipc: p.ipc_state,
        ticks_used: p.ticks_used,
    })
}

/// Imposta il nome owned del processo (Fase 21, `spawn_image`): il nome arriva
/// dal chiamante, non dalla tabella statica. No-op su pid invalido.
pub fn set_owned_name(pid: usize, raw: &[u8]) {
    let mut guard = SCHED.lock();
    if let Some(sched) = guard.as_mut() {
        if let Some(p) = sched.processes.get_mut(pid) {
            p.set_owned_name(raw);
        }
    }
}

/// Imposta il canale di nascita di `pid` (creato da sys_spawn, ADR-0008).
pub fn set_parent_chan(pid: usize, chan: Option<usize>) {
    let mut guard = SCHED.lock();
    if let Some(sched) = guard.as_mut() {
        if pid < sched.processes.len() {
            sched.processes[pid].parent_chan = chan;
        }
    }
}

/// Imposta la base TLS (FS) di `pid` (S-T): il chiamante programma anche
/// l'MSR subito (vedi `sys_thread_set_fs`); qui resta per gli switch futuri.
/// Ritorna `false` a pid ignoto/terminato (il chiamante ha gia' scritto
/// l'MSR: fail-stop loud a monte, mai stato mezzo aggiornato in pratica —
/// il check bounds vive nel chiamante prima di qualunque scrittura).
pub fn set_thread_fs(pid: usize, base: u64) -> i64 {
    let mut guard = SCHED.lock();
    if let Some(sched) = guard.as_mut() {
        if pid < sched.processes.len()
            && sched.processes[pid].state != crate::ordo::process::State::Terminated
        {
            sched.processes[pid].fs_base = base;
            return 0;
        }
    }
    -1
}

/// Leader del gruppo di `pid` (S-T T5): se' stesso per processi e leader.
/// Usato dai path che operano sull'identita' di gruppo (lookup servizi,
/// parent di spawn): i thread ereditano le sessioni del leader.
pub fn group_leader(pid: usize) -> usize {
    let guard = SCHED.lock();
    match guard.as_ref() {
        Some(sched) if pid < sched.processes.len() => {
            sched.processes[pid].thread_group.unwrap_or(pid)
        }
        _ => pid,
    }
}

/// Imposta la cwd del GRUPPO di `pid` (S1.1, leader per i thread). Solo
/// assoluti entro CWD_MAX (il chiamante valida il range user). 0 / -1.
pub fn set_cwd(pid: usize, path: &[u8]) -> i64 {
    if path.is_empty()
        || path.len() > syscall_numbers::CWD_MAX
        || path.first() != Some(&b'/')
    {
        return -1;
    }
    let mut guard = SCHED.lock();
    let sched = match guard.as_mut() {
        Some(s) => s,
        None => return -1,
    };
    if pid >= sched.processes.len() {
        return -1;
    }
    let owner = sched.processes[pid].thread_group.unwrap_or(pid);
    match sched.processes.get_mut(owner) {
        Some(o) => {
            o.cwd[..path.len()].copy_from_slice(path);
            o.cwd_len = path.len() as u16;
            0
        }
        None => -1,
    }
}

/// Legge la cwd del GRUPPO di `pid` in `out`, ritorna len. `None` a pid
/// ignoto o cwd mai inizializzata (il chiamante scrive in user).
pub fn get_cwd(pid: usize, out: &mut [u8; syscall_numbers::CWD_MAX]) -> Option<usize> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if pid >= sched.processes.len() {
        return None;
    }
    let owner = sched.processes[pid].thread_group.unwrap_or(pid);
    let o = sched.processes.get(owner)?;
    let n = o.cwd_len as usize;
    if n == 0 || n > syscall_numbers::CWD_MAX {
        return None;
    }
    out[..n].copy_from_slice(&o.cwd[..n]);
    Some(n)
}

/// Canale di nascita del processo `pid` (ADR-0008). `None` se non esiste.
pub fn parent_channel(pid: usize) -> Option<usize> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if pid < sched.processes.len() {
        sched.processes[pid].parent_chan
    } else {
        None
    }
}

pub fn process_of(target: usize) -> Option<*mut Process> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if target < sched.processes.len() {
        Some(sched.processes.as_ptr().wrapping_add(target) as *mut Process)
    } else {
        None
    }
}

/// Nome display del processo (copia owned 16 B + len, Fase 21): il nome puo'
/// venire dalla tabella embedded (statico) o da `spawn_image` (owned nel PCB,
/// non 'static). `([0;16], 0)` se il pid non esiste.
pub fn process_name(pid: usize) -> ([u8; 16], u8) {
    let guard = SCHED.lock();
    match guard.as_ref().and_then(|s| s.processes.get(pid)) {
        Some(p) => {
            let s = p.name_str();
            let mut b = [0u8; 16];
            let n = s.len().min(16);
            b[..n].copy_from_slice(&s.as_bytes()[..n]);
            (b, n as u8)
        }
        None => ([0u8; 16], 0),
    }
}

pub fn process_cr3(pid: usize) -> Option<u64> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if pid < sched.processes.len() {
        Some(sched.processes[pid].cr3)
    } else {
        None
    }
}

/// Identita' misurata dell'immagine del processo `pid` (Fase 36, Strato 2 di
/// ADR-0026): `None` se lo slot e' vuoto o il processo e' terminato (come
/// `process_ps`). I processi kernel (idle) hanno hash 0 = nessuna immagine.
pub fn process_image_id(pid: usize) -> Option<syscall_numbers::ImageId> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if pid >= sched.processes.len() {
        return None;
    }
    let p = &sched.processes[pid];
    if p.state == State::Terminated {
        return None;
    }
    Some(p.image_id)
}

/// Classe di privilegio dell'immagine di `pid` (ADR-0079 fase 3). `None` se il
/// pid non esiste o e' terminato. Meccanismo neutro: il kernel espone la
/// classe, le decisioni (chi puo' fare cosa) vivono nei gate strutturali.
pub fn process_image_class(pid: usize) -> Option<syscall_numbers::ImageClass> {
    let guard = SCHED.lock();
    let sched = guard.as_ref()?;
    if pid >= sched.processes.len() {
        return None;
    }
    let p = &sched.processes[pid];
    if p.state == State::Terminated {
        return None;
    }
    Some(p.image_class)
}

/// Imposta la classe di un processo appena creato (ADR-0079 fase 3): serve ai
/// binari **embedded** nel kernel (TCB per costruzione: `rector`, `cardo`,
/// `block`, `vestigia`), che non passano dalla verifica di cardo e quindi non
/// hanno una trust entry. Solo il kernel la chiama.
pub fn set_image_class(pid: usize, class: syscall_numbers::ImageClass) {
    let mut guard = SCHED.lock();
    let Some(sched) = guard.as_mut() else { return };
    if let Some(p) = sched.processes.get_mut(pid) {
        p.image_class = class;
    }
}
