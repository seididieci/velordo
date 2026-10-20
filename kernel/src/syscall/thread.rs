// Thread 1:1 + TLS (S-T, ADR-0046): create/exit/set_fs. Il futex vive in
// `futex.rs` (T3). La rendezvous col resto del lifecycle (morte di gruppo,
// reclaim differito, mm condiviso) e' in `ordo/sched`.
use super::entry::current_id;

/// Programma la base TLS (FS) della CPU corrente (MSR FS_BASE). Chiamato alla
/// creazione/switch (T2) e dal SET_FS immediato qui sotto.
pub(crate) fn write_fs_base(base: u64) {
    const MSR_FS_BASE: u32 = 0xC000_0100;
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") MSR_FS_BASE,
            in("eax") base as u32,
            in("edx") (base >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}

/// thread_create(entry, stack, fs, flags): thread nel gruppo del chiamante
/// (stesso mm/canali/fd, kernel stack + TSS propri). `flags` deve essere 0
/// (forward-compat: futuri flag restano rifiutati). Ritorna il tid o -1.
pub(super) fn sys_thread_create(entry: u64, stack: u64, fs: u64, flags: u64) -> i64 {
    if flags != 0 {
        return -1;
    }
    match crate::ordo::sched::spawn_thread(entry, stack, fs) {
        Some(tid) => tid as i64,
        None => -1,
    }
}

/// thread_exit(code): termina SOLO il thread corrente (solo la morte del
/// leader chiude il gruppo, con la via completa di exit).
pub(super) fn sys_thread_exit(code: i64) -> ! {
    crate::ordo::sched::exit_current(code)
}

/// thread_set_fs(base): aggiorna la base TLS del thread corrente (0 = via).
/// Programma SUBITO l'MSR (il thread continua a girare: aspettare lo switch
/// lascerebbe la TLS vecchia attiva) + PCB per gli switch futuri.
pub(super) fn sys_thread_set_fs(base: u64) -> i64 {
    if base != 0 && !crate::arc::vmm_user::is_user_range(base, 1) {
        return -1;
    }
    write_fs_base(base);
    crate::ordo::sched::set_thread_fs(current_id() as usize, base)
}
