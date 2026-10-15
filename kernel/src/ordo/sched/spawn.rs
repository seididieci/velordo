// Split from ordo/sched.rs (byte-identical move; see facade).
use super::*;
use crate::ordo::process::Process;
use core::sync::atomic::Ordering;
use super::ctx::{SCHED, INITIALIZED};
use super::queue::Scheduler;

pub fn init() {
    let mut guard = SCHED.lock();
    *guard = Some(Scheduler::new());
    INITIALIZED.store(true, Ordering::Release);
    crate::serial_println!("[ordo] init (preemptive, 32-prio + RR, quantum {} tick)", QUANTUM_TICKS);
}

/// Leader del gruppo (S1.1): la cwd vive sul leader (i thread la
/// condividono come POSIX).
pub(super) fn cwd_owner(sched: &Scheduler, pid: usize) -> usize {
    sched.processes.get(pid).and_then(|p| p.thread_group).unwrap_or(pid)
}

/// Eredita la cwd nel figlio (S1.1): dal gruppo del parent, `/` se orfano
/// del kernel o cwd vuota. Chiamare sotto lock dopo `place_process`.
pub(super) fn inherit_cwd(sched: &mut Scheduler, child: usize, parent: Option<usize>) {
    let (buf, len) = match parent {
        Some(p) if p < sched.processes.len() => {
            let o = cwd_owner(sched, p);
            match sched.processes.get(o) {
                Some(op) => (op.cwd, op.cwd_len),
                None => ([0u8; 256], 0),
            }
        }
        _ => ([0u8; 256], 0),
    };
    if let Some(c) = sched.processes.get_mut(child) {
        if len == 0 {
            c.cwd[0] = b'/';
            c.cwd_len = 1;
        } else {
            c.cwd = buf;
            c.cwd_len = len;
        }
    }
}

pub fn spawn(name: &'static str, priority: Priority, entry: crate::ordo::process::ProcessFn, parent: Option<usize>, parent_chan: Option<usize>) -> Option<usize> {
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");

    let id = sched.alloc_pid()?;
    let process = match Process::create(name, priority, entry, parent, parent_chan, &[]) {
        Some(p) => p,
        None => {
            sched.release_pid(id);
            return None;
        }
    };
    sched.place_process(id, process);
    inherit_cwd(sched, id, parent);
    sched.set_ready(id);
    let p = &sched.processes[id];
    crate::serial_println!(
        "[ordo] process '{}' (id {}), {:?} | cr3={:#x} rsp0={:#x}",
        name, id, priority, p.cr3, p.kernel_stack_top
    );
    Some(id)
}

/// Crea un thread 1:1 nel gruppo del chiamante (S-T, ADR-0046): `entry` =
/// RIP user iniziale, `stack` = RSP iniziale (mappato dal chiamante, di
/// solito mmap), `fs` = base TLS (0 = nessuna). Il gruppo e' quello del
/// chiamante (un thread puo' spawnare fratelli). Solo chiamanti user (i
/// processi kernel non hanno address space condivisibile). Ritorna il tid.
pub fn spawn_thread(entry: u64, stack: u64, fs: u64) -> Option<usize> {
    if entry == 0 || stack == 0 {
        return None;
    }
    // Lo stack e' il TOP (un past-the-end: il primo push scrive [top-8]):
    // valida l'ultima qword mappabile. La mappatura effettiva faulta loud
    // al primo uso se invalida — mai UB nel kernel.
    if !crate::arc::vmm_user::is_user_range(stack.wrapping_sub(8), 8) {
        return None;
    }
    // FS nulla o user-range (canonica: niente basi kernel come TLS).
    if fs != 0 && !crate::arc::vmm_user::is_user_range(fs, 1) {
        return None;
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");
    let caller = sched.current?;
    if sched.processes[caller].cr3 == crate::arc::vmm_user::kernel_cr3() {
        return None; // chiamante kernel: niente thread user
    }
    if sched.processes[caller].state == crate::ordo::process::State::Terminated {
        return None;
    }
    let group = sched.processes[caller].thread_group.unwrap_or(caller);
    if sched.processes[group].state == crate::ordo::process::State::Terminated {
        return None; // gruppo morente: niente nuovi thread
    }
    let (name, name_owned, name_len, priority, cr3, tss_slot, cbs, text_id, img) = {
        let l = &sched.processes[group];
        (l.name, l.name_owned, l.name_len, l.priority, l.cr3, l.tss_slot, l.cbs_server, l.text_id, l.image_hash)
    };
    let id = sched.alloc_pid()?;
    let thread = match unsafe {
        Process::create_thread(
            name, name_owned, name_len, priority, group, cr3, tss_slot,
            cbs, text_id, img, entry, stack, fs,
        )
    } {
        Some(t) => t,
        None => {
            sched.release_pid(id);
            return None;
        }
    };
    sched.place_process(id, thread);
    sched.set_ready(id);
    crate::serial_println!(
        "[ordo] thread '{}' (tid {}), gruppo {} | entry={:#x} stack={:#x} fs={:#x}",
        sched.processes[group].name_str(), id, group, entry, stack, fs
    );
    Some(id)
}

pub unsafe fn create_user(
    name: &'static str,
    priority: Priority,
    elf: &[u8],
    parent: Option<usize>,
    parent_chan: Option<usize>,
    io_ranges: &[(u16, u16)],
    detached: bool,
    stack_frames: u32,
    args: Option<&crate::arc::vmm_user::ParsedArgs<'_>>,
) -> Option<usize> {
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");

    let id = sched.alloc_pid()?;
    let process = match unsafe {
        Process::create_user(
            name, priority, elf, parent, parent_chan, io_ranges, detached, stack_frames,
            args,
        )
    } {
        Some(p) => p,
        None => {
            sched.release_pid(id);
            return None;
        }
    };
    sched.place_process(id, process);
    inherit_cwd(sched, id, parent);
    sched.set_ready(id);
    let p = &sched.processes[id];
    crate::serial_println!(
        "[ordo] USER process '{}' (id {}), {:?} | cr3={:#x} rsp0={:#x}",
        name, id, priority, p.cr3, p.kernel_stack_top
    );
    Some(id)
}
