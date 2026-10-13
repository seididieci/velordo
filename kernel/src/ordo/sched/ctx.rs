// Split from ordo/sched.rs (byte-identical move; see facade).
use core::sync::atomic::AtomicBool;
use spin::Mutex;
use crate::ordo::context::CpuContext;
use super::queue::Scheduler;

pub(super) static INITIALIZED: AtomicBool = AtomicBool::new(false);
pub(super) static SCHED: Mutex<Option<Scheduler>> = Mutex::new(None);



/// Contesto del "processo" di boot (main). Non e' un vero PCB: serve solo a
/// salvare lo stato di main al primo switch, quando `current` e' ancora None.
/// Main non viene mai rischedulato (resta in `hlt` nel suo loop).
pub(crate) static mut BOOT_CONTEXT: CpuContext = CpuContext::ZERO;

/// Inizializza l'area fxsave del contesto di boot (S2.5b, da `cpu::init`,
/// prima di qualunque switch: single-threaded, niente lock). Usa lo slot 0
/// del pool (condiviso con idle/pid 0 in sequenza: il boot termina prima che
/// idle giri; entrambi re-inizializzati al proprio init).
///
/// # Safety
/// Chiamare una sola volta a boot.
pub(crate) unsafe fn init_boot_fpu() {
    unsafe {
        BOOT_CONTEXT.fpu = crate::ordo::context::fpu_slot(0);
        crate::ordo::context::fpu_init(BOOT_CONTEXT.fpu as *mut [u128; 32]);
    }
}

pub(super) fn switch_to(
    prev: Option<usize>,
    next: usize,
    mut guard: spin::MutexGuard<Option<Scheduler>>,
) {
    {
        let sched = guard.as_mut().expect("scheduler non inizializzato");
        sched.current = Some(next);
        sched.ticks_current = 0;
    }

    let (cur_ptr, next_ptr): (*mut CpuContext, *mut CpuContext) = {
        let sched = guard.as_mut().expect("scheduler non inizializzato");
        let base = sched.processes.as_mut_ptr();
        let cur = match prev {
            Some(pi) => unsafe { &mut (*base.add(pi)).saved as *mut CpuContext },
            None => core::ptr::addr_of_mut!(BOOT_CONTEXT),
        };
        let next = unsafe { &mut (*base.add(next)).saved as *mut CpuContext };
        (cur, next)
    };

    let (next_cr3, next_kstack_top, next_tss_sel, next_mm, next_fs) = unsafe {
        let sched = guard.as_mut().expect("scheduler non inizializzato");
        let n = &*sched.processes.as_ptr().add(next);
        (n.cr3, n.kernel_stack_top, n.tss_sel, n.thread_group.unwrap_or(next), n.fs_base)
    };

    crate::syscall::set_current(next, next_kstack_top, next_cr3, next_mm as u64);

    drop(guard);

    crate::gdt::load_process_tss(next_tss_sel);
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) next_cr3, options(nostack, preserves_flags));
    }
    // S-T (T2): base TLS del prossimo thread (MSR FS_BASE; FS user libero —
    // il kernel usa solo GS per PERCPU). Sempre scritta (correttezza prima:
    // il costo ~100 cicli e' frazione dello switch con ltr+cr3).
    crate::syscall::thread::write_fs_base(next_fs);

    unsafe { crate::ordo::context::switch_to(cur_ptr, next_ptr) };
}
