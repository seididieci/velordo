// Split from syscall.rs (byte-identical move; see facade).
use core::ptr::addr_of_mut;
use super::entry::PERCPU;
use super::ipc::{sys_send, sys_send_async, sys_recv, sys_recv_nonblock, sys_reply};
use super::service::{sys_service_register, sys_service_lookup, sys_service_pid, sys_peer_pid, sys_peer_info};
use super::spawn::{sys_spawn, sys_spawn_image, sys_spawn_copy};
use super::exec::sys_exec;
use super::cwd::{sys_chdir, sys_getcwd};
use super::thread::{sys_thread_create, sys_thread_exit, sys_thread_set_fs};
use super::mem::{sys_mmap, sys_munmap, sys_mprotect, sys_shm_create, sys_shm_map, sys_map_physical, sys_map_mmio, sys_sbrk, sys_ring_alloc, sys_map_in, sys_dma_alloc, sys_dma_free};
use super::misc::{sys_exit, sys_write, sys_getpid, sys_kill, sys_suspend, sys_resume, sys_get_ticks, sys_cbs_create, sys_cbs_attach, sys_cbs_get_info, sys_ps_info, sys_text_stats, sys_meminfo, sys_boot_cmdline};

/// Handler di dispatch: legge gli argomenti riempiti dall'entry e chiama la
/// syscall richiesta. Firmato `extern "C" fn() -> i64` per essere invocabile
/// dall'assembly; il risultato torna in RAX a `sysretq`.
#[unsafe(no_mangle)]
pub(super) extern "C" fn syscall_handler() -> i64 {
    unsafe {
        let p = addr_of_mut!(PERCPU);
        // Reset del flag di ritorno multi-register: di default restituiamo i
        // registri user preservati (solo RAX cambia). Le syscall IPC lo
        // impostano per svuotare rdi/rsi/rdx/r10 con i valori di risposta.
        (*p).ipc_override = 0;
        match (*p).number {
            syscall_numbers::SYS_EXIT => sys_exit((*p).arg1 as i64),
            syscall_numbers::SYS_WRITE => sys_write((*p).arg1, (*p).arg2 as *const u8, (*p).arg3 as usize),
            syscall_numbers::SYS_GETPID => sys_getpid(),
            syscall_numbers::SYS_SEND => sys_send((*p).arg1 as usize, (*p).arg2, (*p).arg3, (*p).arg4),
            syscall_numbers::SYS_SEND_ASYNC => sys_send_async((*p).arg1 as usize, (*p).arg2, (*p).arg3, (*p).arg4),
            syscall_numbers::SYS_RECV => sys_recv(),
            syscall_numbers::SYS_RECV_NONBLOCK => sys_recv_nonblock(),
            syscall_numbers::SYS_REPLY => sys_reply((*p).arg1, (*p).arg2, (*p).arg3),
            syscall_numbers::SYS_SERVICE_REGISTER => sys_service_register((*p).arg1),
            syscall_numbers::SYS_SERVICE_LOOKUP => sys_service_lookup((*p).arg1),
            syscall_numbers::SYS_SPAWN => sys_spawn((*p).arg1, (*p).arg2 as usize),
            syscall_numbers::SYS_MAP_PHYSICAL => sys_map_physical((*p).arg1, (*p).arg2, (*p).arg3 as usize),
            // L0b (ADR-0057): BAR MMIO device UC, solo owner di Service::Gpu.
            syscall_numbers::SYS_MAP_MMIO => sys_map_mmio((*p).arg1, (*p).arg2, (*p).arg3 as usize),
            syscall_numbers::SYS_GET_TICKS => sys_get_ticks(),
            syscall_numbers::SYS_SBRK => sys_sbrk((*p).arg1),
            // Ring buffer SPSC per-processo (Fase 10.2).
            syscall_numbers::SYS_RING_ALLOC => sys_ring_alloc(),
            // Fase 38.1: staging DMA (frame contigui + phys al chiamante).
            syscall_numbers::SYS_DMA_ALLOC => sys_dma_alloc((*p).arg1 as usize, (*p).arg2),
            // D4: libera uno slot DMA (il buffer device si rialloca al resize).
            syscall_numbers::SYS_DMA_FREE => sys_dma_free((*p).arg1),
            syscall_numbers::SYS_MAP_IN => sys_map_in((*p).arg1 as usize, (*p).arg2, (*p).arg3, (*p).arg4 as usize),
            // CBS bandwidth reservation (Fase 11.4).
            syscall_numbers::SYS_CBS_CREATE => sys_cbs_create((*p).arg1, (*p).arg2),
            syscall_numbers::SYS_CBS_ATTACH => sys_cbs_attach(),
            syscall_numbers::SYS_CBS_GET_INFO => sys_cbs_get_info((*p).arg1),
            // Fase 14 (ADR-0010): kill di un processo user.
            syscall_numbers::SYS_KILL => sys_kill((*p).arg1, (*p).arg2 as i64),
            // Fase 44a (job control): suspend/resume di un processo user.
            syscall_numbers::SYS_SUSPEND => sys_suspend((*p).arg1),
            syscall_numbers::SYS_RESUME => sys_resume((*p).arg1),
            // Fase 14 (init-restart): pid dell'owner di un servizio.
            syscall_numbers::SYS_SERVICE_PID => sys_service_pid((*p).arg1),
            // Fase 19.1: snapshot `ps` di un processo.
            syscall_numbers::SYS_PS_INFO => sys_ps_info((*p).arg1 as usize),
            // Fase 21: spawn dal binario in memoria (servizi da disco).
            syscall_numbers::SYS_SPAWN_IMAGE => {
                sys_spawn_image((*p).arg1, (*p).arg2 as usize, (*p).arg3, (*p).arg4 as usize)
            }
            // Fase 28: mmap/munmap anonimi nel basso canonico.
            syscall_numbers::SYS_MMAP => sys_mmap((*p).arg1, (*p).arg2 as usize, (*p).arg3, (*p).arg4),
            syscall_numbers::SYS_MUNMAP => sys_munmap((*p).arg1, (*p).arg2 as usize),
            // Fase 29: mprotect su VMA intere.
            syscall_numbers::SYS_MPROTECT => sys_mprotect((*p).arg1, (*p).arg2 as usize, (*p).arg3),
            // Fase 30: memoria condivisa tra processi.
            syscall_numbers::SYS_SHM_CREATE => sys_shm_create((*p).arg1),
            syscall_numbers::SYS_SHM_MAP => sys_shm_map((*p).arg1, (*p).arg2, (*p).arg3, (*p).arg4),
            // Fase 32: contatori shared text (debug/test).
            syscall_numbers::SYS_TEXT_STATS => sys_text_stats(),
            // Fase 34: figlio-copia COW dell'address space.
            syscall_numbers::SYS_SPAWN_COPY => sys_spawn_copy(),
            // Fase 37: exec in-place (stesso PID, nuova immagine; argv in 37.1.2).
            syscall_numbers::SYS_EXEC => sys_exec((*p).arg1, (*p).arg2 as usize, (*p).arg3, (*p).arg4 as usize),
            // Fase 35: pid del peer di un canale (policy server-side).
            syscall_numbers::SYS_PEER_PID => sys_peer_pid((*p).arg1 as usize),
            // Fase 36: hash immagine del peer di un canale (policy su identita').
            syscall_numbers::SYS_PEER_INFO => sys_peer_info((*p).arg1 as usize),
            // Fase 52 (P3 durabilita'): fermo immagine RAM (free/total/used).
            syscall_numbers::SYS_MEMINFO => sys_meminfo(),
            // Fase 2 (root su volume): cmdline PVH salvata al boot.
            syscall_numbers::SYS_BOOT_CMDLINE => sys_boot_cmdline((*p).arg1, (*p).arg2 as usize),
            // S-T (ADR-0046): thread 1:1 + TLS (futex in T3).
            syscall_numbers::SYS_THREAD_CREATE => {
                sys_thread_create((*p).arg1, (*p).arg2, (*p).arg3, (*p).arg4)
            }
            syscall_numbers::SYS_THREAD_EXIT => sys_thread_exit((*p).arg1 as i64),
            syscall_numbers::SYS_THREAD_SET_FS => sys_thread_set_fs((*p).arg1),
            // S1.1: directory di lavoro (cwd nel PCB, solo assoluti).
            syscall_numbers::SYS_CHDIR => sys_chdir((*p).arg1, (*p).arg2 as usize),
            syscall_numbers::SYS_GETCWD => sys_getcwd((*p).arg1, (*p).arg2 as usize),
            // S-T (T3, ADR-0046): futex WAIT/WAKE con deadline a tick.
            syscall_numbers::SYS_FUTEX_WAIT => crate::ordo::sched::futex_wait(
                (*p).arg1,
                (*p).arg2 as u32,
                (*p).arg3,
            ),
            syscall_numbers::SYS_FUTEX_WAKE => {
                crate::ordo::sched::futex_wake((*p).arg1, (*p).arg2 as u32)
            }
            _ => -1,
        }
    }
}

/// Applica il risultato di una primitiva IPC ai registri di ritorno della
/// syscall: imposta i valori di ritorno e il flag `ipc_override` perche'
/// l'entry riempia rdi/rsi/rdx/r10. Ritorna il valore di `rax` (stato).
pub(super) fn apply_ipc(r: crate::ordo::sched::IpcResult) -> i64 {
    unsafe {
        let p = addr_of_mut!(PERCPU);
        (*p).ipc_override = 1;
        (*p).ret_rdi = r.rdi;
        (*p).ret_rsi = r.rsi;
        (*p).ret_rdx = r.rdx;
        (*p).ret_r10 = r.r10;
    }
    r.rax
}
