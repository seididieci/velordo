//! Thread Velordo 1:1 (S2.3): spawn sopra SYS_THREAD_CREATE (54), join via
//! futex su TCB, sleep/yield sopra SYS_FUTEX_WAIT con deadline.
//! Forma dell'arm uefi in `sys/thread/mod.rs`.
//!
//! Protocollo (tutto userspace, zero syscall nuove):
//! - il TCB vive in cima allo stack del thread (RSP iniziale = TCB: il
//!   kernel parte con `stack` = RSP passato, vedi `spawn_thread`); FS = TCB
//!   (casa futura della TLS; oggi solo-ID, `thread_local!` resta `no_threads`).
//! - il trampolino legge RSP (= TCB), esegue `ThreadInit`, poi done=1 +
//!   WAKE + thread_exit. Il join dorme sul futex `done` (deadline 0 = mai).
//! - detach (Drop senza join): protocollo Dekker done/detached in SeqCst —
//!   libera esattamente una parte tra uscita-thread e Drop (vedi sotto).
//! - panic = abort di PAL: un panic in un figlio abortisce il PROCESSO.
//! - available_parallelism = 1 (scheduler single-core); current_os_id =
//!   None (niente gettid); set_name = no-op (niente naming nel kernel).

use alloc::boxed::Box;
use core::ffi::CStr;
use core::hint::unreachable_unchecked;
use core::mem::{forget, size_of};
use core::ptr::{self, with_exposed_provenance_mut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

use crate::alloc::{Layout, alloc, dealloc};
use crate::io;
use crate::num::NonZero;
use crate::thread::ThreadInit;
use crate::time::Duration;

use crate::sys::sync::futex::velordo as ftx;

pub const DEFAULT_MIN_STACK_SIZE: usize = 64 * 1024;

/// Blocco di controllo thread in cima allo stack (align 16, size 64 =
/// multiplo di 16: RSP resta allineato per le call del trampolino).
#[repr(C, align(16))]
pub struct Tcb {
    /// `Box<ThreadInit>` travasato (null dopo il take del trampolino).
    init: *mut ThreadInit,
    /// 0 = gira, 1 = finito (futex del join).
    done: AtomicU32,
    /// Drop senza join (vedi protocollo sotto).
    detached: AtomicBool,
    /// Allocazione stack da restituire (TCB incluso in cima).
    stack_base: *mut u8,
    stack_size: usize,
    stack_align: usize,
    /// Tabella TLS del thread (65 voci azzerate, da `key::register_slot`).
    tls_table: *mut u8,
    _pad: [u8; 16],
}

unsafe impl Send for Tcb {}
unsafe impl Sync for Tcb {}

impl Tcb {
    const fn const_new() -> Tcb {
        Tcb {
            init: ptr::null_mut(),
            done: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            stack_base: ptr::null_mut(),
            stack_size: 0,
            stack_align: 0,
            tls_table: ptr::null_mut(),
            _pad: [0; 16],
        }
    }
}

/// TCB del main thread (statico: mai liberato). `_start` ci punta FS prima
/// di qualunque codice Rust (vedi `init_main_tls`).
static MAIN_TCB: Tcb = Tcb::const_new();

/// Programma FS sul TCB del main thread. Chiamato da `_start` (sys/pal).
pub fn init_main_tls() {
    unsafe {
        crate::sys::pal::syscall4(
            crate::sys::pal::numbers::SYS_THREAD_SET_FS,
            ptr::from_ref(&MAIN_TCB).addr() as u64,
            0,
            0,
            0,
        );
    }
}

pub struct Thread {
    tcb: *mut Tcb,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

/// Restituisce stack+TCB all'allocatore (chiamato una sola volta: vedi Drop).
unsafe fn free_tcb(tcb: *mut Tcb) {
    let (base, size, align, table) =
        unsafe { ((*tcb).stack_base, (*tcb).stack_size, (*tcb).stack_align, (*tcb).tls_table) };
    if !table.is_null() {
        unsafe { dealloc(table, Layout::new::<[u8; 65 * 8]>()) };
    }
    if !base.is_null() && size > 0 {
        let layout = Layout::from_size_align(size, align)
            .unwrap_or(Layout::new::<u8>());
        unsafe { dealloc(base, layout) };
    }
}

/// Entry nuda del thread (S2.3b): il kernel parte con RSP = TCB. DEVE
/// essere naked: un prologo normale sposterebbe RSP prima della lettura
/// (bug: 6 push = -64, il TCB letto era zeri freschi → #PF @ 0x0).
/// Passa il TCB in RDI e SALTA (jmp, mai call: RSP resta il TCB).
#[unsafe(naked)]
extern "C" fn trampoline_entry() -> ! {
    core::arch::naked_asm!(
        "mov rdi, rsp",
        "jmp {real}",
        real = sym trampoline_real,
    );
}

extern "C" fn trampoline_real(tcb: *mut Tcb) -> ! {
    let init = unsafe { Box::from_raw((*tcb).init as *mut ThreadInit) };
    unsafe {
        (*tcb).init = ptr::null_mut();
    }
    let rust_start = init.init();
    rust_start();
    unsafe { crate::sys::thread_local::key::destroy_tls() };
    unsafe {
        (*tcb).done.store(1, SeqCst);
            ftx::futex_wake(unsafe { &(*tcb).done });
        if unsafe { (*tcb).detached.load(SeqCst) } {
            free_tcb(tcb);
        }
        crate::sys::pal::syscall4(crate::sys::pal::numbers::SYS_THREAD_EXIT, 0, 0, 0, 0);
        unreachable_unchecked()
    }
}

impl Thread {
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        let size = stack
            .max(DEFAULT_MIN_STACK_SIZE)
            .checked_add(size_of::<Tcb>() + 16)
            .ok_or_else(|| io::const_error!(io::ErrorKind::Uncategorized, "thread stack too large"))?;
        let layout = Layout::from_size_align(size, 16).map_err(|_| {
            io::const_error!(io::ErrorKind::Uncategorized, "bad thread stack layout")
        })?;
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return Err(io::const_error!(
                io::ErrorKind::Uncategorized,
                "unable to allocate thread stack"
            ));
        }
        // Tabella TLS (65 voci azzerate: zero = chiave mai usata).
        let table = unsafe { alloc(Layout::new::<[u8; 65 * 8]>()) };
        if table.is_null() {
            unsafe { dealloc(base, layout) };
            return Err(io::const_error!(
                io::ErrorKind::Uncategorized,
                "unable to allocate thread TLS table"
            ));
        }
        unsafe { table.write_bytes(0, 65 * 8) };
        // TCB in cima, align-down a 16.
        let tcb = (base.addr() + size - size_of::<Tcb>()) & !15usize;
        unsafe {
            ptr::write(with_exposed_provenance_mut::<Tcb>(tcb), Tcb::const_new());
            (* with_exposed_provenance_mut::<Tcb>(tcb)).init = Box::into_raw(init) as *mut ThreadInit;
            (* with_exposed_provenance_mut::<Tcb>(tcb)).stack_base = base;
            (* with_exposed_provenance_mut::<Tcb>(tcb)).stack_size = size;
            (* with_exposed_provenance_mut::<Tcb>(tcb)).stack_align = 16;
            (* with_exposed_provenance_mut::<Tcb>(tcb)).tls_table = table;
        }
        // Registro TLS: niente slot = niente thread (Err loud, mai meta').
        if !crate::sys::thread_local::key::register_slot(base.addr(), size, table) {
            unsafe {
                drop(Box::from_raw(
                    (*with_exposed_provenance_mut::<Tcb>(tcb)).init as *mut ThreadInit,
                ));
                dealloc(table, Layout::new::<[u8; 65 * 8]>());
                dealloc(base, layout);
            }
            return Err(io::const_error!(
                io::ErrorKind::Uncategorized,
                "no TLS slot for thread"
            ));
        }
        let tid = unsafe {
            crate::sys::pal::syscall4(
                crate::sys::pal::numbers::SYS_THREAD_CREATE,
                trampoline_entry as usize as u64,
                tcb as u64,
                tcb as u64,
                0,
            )
        };
        if tid < 0 {
            unsafe {
                drop(Box::from_raw(
                    (*with_exposed_provenance_mut::<Tcb>(tcb)).init as *mut ThreadInit,
                ));
                // Lo slot TLS resta prenotato ma base e' nota: rilascialo
                // via unregister (il thread non partira' mai).
                crate::sys::thread_local::key::release_base(base.addr());
                dealloc(table, Layout::new::<[u8; 65 * 8]>());
                dealloc(base, layout);
            }
            return Err(io::const_error!(
                io::ErrorKind::Uncategorized,
                "unable to create thread!"
            ));
        }
        Ok(Thread { tcb: with_exposed_provenance_mut::<Tcb>(tcb) })
    }

    pub fn join(self) {
        // Dorme sul futex `done` (deadline 0 = mai): risvegli solo da WAKE
        // del trampolino o spurie/timeout (ricontrolla in loop).
        loop {
            if unsafe { (*self.tcb).done.load(SeqCst) } == 1 {
                break;
            }
            ftx::wait_on(unsafe { &(*self.tcb).done }, 0, 0);
        }
        unsafe { free_tcb(self.tcb) };
        forget(self);
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        let tcb = self.tcb;
        // Dekker con l'exit-path del trampolino (tutto SeqCst): se il thread
        // ha gia' finito libero io; altrimenti stacco e ricontrollo (se nel
        // frattempo ha deciso di NON liberare — ha visto detached=0 prima
        // del mio store — libero io al secondo check).
        if unsafe { (*tcb).done.load(SeqCst) } == 1 {
            unsafe { free_tcb(tcb) };
        } else {
            unsafe { (*tcb).detached.store(true, SeqCst) };
            if unsafe { (*tcb).done.load(SeqCst) } == 1 {
                unsafe { free_tcb(tcb) };
            }
        }
    }
}

/// Word statica per sleep/yield (pre-touchata: mai lazy al WAIT).
static SLEEPER: AtomicU32 = AtomicU32::new(0);

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    // Scheduler single-core: 1.
    Ok(NonZero::new(1).unwrap())
}

pub fn current_os_id() -> Option<u64> {
    // Niente gettid nel kernel: nessun ID esposto.
    None
}

#[inline]
pub fn sleep(dur: Duration) {
    let ms = dur.as_millis() as u64;
    if ms == 0 {
        return;
    }
    // 10 ms per tick, arrotonda in eccesso + 1 di margine (stessa stanza di
    // futex_wait: deadline assoluta, attesa vera invece di spin IF=1).
    let deadline = crate::sys::pal::ticks().wrapping_add(ms.div_ceil(10) + 1);
    let cur = SLEEPER.load(SeqCst);
    ftx::wait_on(&SLEEPER, cur, deadline);
}

#[inline]
pub fn yield_now() {
    // Fino al prossimo tick; se nessun altro e' runnable il kernel ritorna
    // subito spuria (vera yield single-core, mai attesa a vuoto).
    let cur = SLEEPER.load(SeqCst);
    ftx::wait_on(&SLEEPER, cur, crate::sys::pal::ticks().wrapping_add(1));
}

pub fn set_name(_name: &CStr) {
    // Niente naming dei thread nel kernel: no-op.
}
