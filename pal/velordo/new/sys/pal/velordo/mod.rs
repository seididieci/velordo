//! PAL Velordo (S1.3): entry/exit + unsupported.
//!
//! Modello xous/uefi: init no-op (argc/argv gestiti da sys::args, il kernel
//! passa argc=0), abort/exit via syscall raw, `_start` che chiama il wrapper
//! `main` generato dal compilatore (== lang_start -> rt::init -> user main).

#![forbid(unsafe_op_in_unsafe_fn)]

use core::arch::asm;

pub(crate) mod numbers;

// Numeri ABI generati da apply.py (S2.2, single source = syscall-numbers).
// `con` e' una maniglia console, non un fd POSIX.
use numbers::{CONSOLE_OUT, SYS_EXIT, SYS_GET_TICKS, SYS_SBRK, SYS_WRITE};

/// Syscall raw a 4 argomenti (ABI Velordo: rax=num, rdi/rsi/rdx/r10=args).
#[inline(always)]
pub(crate) unsafe fn syscall4(n: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let r: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => r,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            out("rcx") _,
            out("r11") _,
            options(nostack, preserves_flags),
        );
    }
    r
}

pub(crate) fn write_console(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let r = unsafe {
        syscall4(SYS_WRITE, CONSOLE_OUT, buf.as_ptr().addr() as u64, buf.len() as u64, 0)
    };
    if r < 0 { 0 } else { r as usize }
}

/// Ticks PIT (100 Hz) dal boot.
pub(crate) fn ticks() -> u64 {
    let r = unsafe { syscall4(SYS_GET_TICKS, 0, 0, 0, 0) };
    if r < 0 { 0 } else { r as u64 }
}

/// Estende l'heap di `inc` byte (solo VA, pagine lazy). Ritorna il vecchio
/// break o valore negativo.
pub(crate) fn sbrk(inc: usize) -> isize {
    unsafe { syscall4(SYS_SBRK, inc as u64, 0, 0, 0) as isize }
}

pub(crate) fn exit_process(code: i32) -> ! {
    unsafe {
        syscall4(SYS_EXIT, code as u64, 0, 0, 0);
    }
    // SYS_EXIT non ritorna mai; se lo facesse, spin (maihalt in userspace).
    loop {
        core::hint::spin_loop()
    }
}

/// Uscita per `sys::exit`.
pub fn exit(code: i32) -> ! {
    exit_process(code)
}

pub unsafe fn init(argc: isize, argv: *const *const u8, _sigpipe: u8) {
    // argv dal kernel (argc=0 se spawn senza args): registra argc/argv.
    unsafe {
        crate::sys::args::init(argc, argv);
    }
    // envp segue argv[argc] == NULL (layout steso da `layout_argv`).
    let envp = argv.wrapping_add(argc.max(0) as usize + 1);
    crate::sys::env::init(envp as *const *const crate::ffi::c_char);
    // Niente SIGPIPE (niente segnali), niente sanitize fd (niente fd).
}

pub unsafe fn cleanup() {}

pub fn abort_internal() -> ! {
    exit_process(134);
}

pub fn unsupported<T>() -> crate::io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> crate::io::Error {
    crate::io::Error::UNSUPPORTED_PLATFORM
}

#[cfg(not(test))]
mod c_compat {
    /// Entry del programma (S1.3, S2.0): il kernel salta a USER_CODE
    /// (= `_start`, KEEP nel linker script) con RSP allo stack user. DEVE
    /// essere naked: un prologo normale sposterebbe RSP prima della lettura
    /// di argc (con argc=0 funzionava per caso). Cattura RSP subito e salta
    /// alla continuazione Rust.
    #[unsafe(no_mangle)]
    #[unsafe(naked)]
    pub extern "C" fn _start() -> ! {
        core::arch::naked_asm!("mov rdi, rsp", "jmp {entry}", entry = sym __velordo_start,);
    }

    unsafe extern "C" fn __velordo_start(sp: u64) -> ! {
        // Layout kernel (`layout_argv`): [argc][argv..][NULL][envp..][NULL].
        // Lo stack sotto `sp` e' libero (prologo/call sicuri, allineati).
        let (argc, argv): (isize, *const *const u8);
        unsafe {
            let base = core::ptr::with_exposed_provenance::<u64>(sp as usize);
            argc = (base as *const isize).read();
            argv = (base as *const *const u8).add(1);
        }
        unsafe extern "C" {
            fn main(argc: isize, argv: *const *const u8) -> isize;
        }
        let code = unsafe { main(argc, argv) };
        super::exit_process(code as i32)
    }
}
