//! libr — **personalità POSIX** di Velordor (ADR-0015, ADR-0025, ADR-0041).
//!
//! Strato di traduzione sopra il meccanismo neutro `civis`: qui vivono i
//! concetti POSIX (errno, vfd/redirect, argv `NAME=val`, `fork`/`exec`) che il
//! kernel e il wire non conoscono. La separazione è una dipendenza Cargo:
//! `civis` non riferisce mai questo crate; l'unico aggancio consentito è
//! l'hook `civis::persona` (routing stdout), installato da [`entry!`].
//!
//! I binari POSIX linkano **entrambi**: `civis::` per il meccanismo (`import`:
//! `use civis;`), `libr::` per la personalità. Niente facade: il confine è
//! visibile negli import.

#![no_std]

extern crate alloc;

pub mod posix;
pub mod stdio;

/// Socket POSIX sopra `civis::net` (N7.1, ADR-0076).
pub mod sock;

/// Serializzazione argv/env + `exec` per path (personalità POSIX).
pub mod exec;
/// `fork` COW (personalità POSIX; il meccanismo COW è in `civis`).
pub mod fork;

pub use stdio::{
    clear_stdio, set_stdio, stderr_fd, stdin_byte, stdin_fd, stdio_active, stdio_restore,
    stdout_fd, RedirEntry,
};
pub use exec::{exec, exec_env, serialize_argv, serialize_argv_redir, serialize_argv_redir_env};
pub use fork::{fork, ForkResult};
pub use posix::{
    EBADF, EAFNOSUPPORT, EINVAL, EIO, ENOMEM, ENOTCONN, EOPNOTSUPP, ETIMEDOUT, EPERM,
};
pub use sock::{close, connect, recv, recvfrom, send, sendto, socket};

/// Preparazione d'ingresso della personalità (chiamata da [`entry!`] prima di
/// saltare a `main`): installa il routing stdout nel meccanismo e ripristina il
/// redirect dallo stack iniziale. `#[doc(hidden)]`: dettaglio del CRT.
#[doc(hidden)]
pub fn __entry_prepare(sp: u64) {
    civis::persona::set_route_out(stdio::route_out);
    stdio::stdio_restore(sp);
}

/// Entry point POSIX (CRT minimale, ADR-0025/0041): come `civis::entry!` ma
/// prima di saltare a `$main(sp)` chiama [`__entry_prepare`] (hook di routing
/// + `stdio_restore`). Il meccanismo resta invariato; la variante nativa è
/// `civis::entry!` (nessun redirect).
///
/// `$main` è `fn(u64) -> !` (sp in rdi, ABI SysV). Stesso simbolo/sezione
/// (`_start`) della variante meccanismo: un solo `entry!` per binario.
#[macro_export]
macro_rules! entry {
    ($main:ident) => {
        #[unsafe(no_mangle)]
        #[unsafe(naked)]
        pub extern "C" fn _start() -> ! {
            ::core::arch::naked_asm!(
                "mov rdi, rsp",
                "jmp {entry}",
                entry = sym __libr_entry,
            );
        }
        fn __libr_entry(sp: u64) -> ! {
            $crate::__entry_prepare(sp);
            unsafe {
                ::core::arch::asm!(
                    "mov rsp, {sp}",
                    "mov rdi, {sp}",
                    "jmp {main}",
                    sp = in(reg) sp,
                    main = sym $main,
                    options(noreturn),
                )
            }
        }
        // `$main` è referenziata solo dall'asm sopra: senza questo root
        // `--gc-sections` la scarterebbe (undefined symbol al link).
        #[used]
        static _VELORDOR_ENTRY_KEEP: fn(u64) -> ! = $main;
    };
}
