//! Allocatore globale Velordo (S2.1): `valloc` segregated-fit.
//!
//! Il core vive in `libs/valloc/src/core.rs` (copiato qui da `apply.py`
//! come `valloc_core.rs`: source unica, niente derive silenziose). Questo
//! file e' solo l'adattatore: `Provider` sopra `mmap`/`munmap` anonimi +
//! le 4 funzioni dell'interfaccia `sys::alloc`.

#[path = "valloc_core.rs"]
mod valloc;

use crate::alloc::Layout;
use crate::ptr;
use crate::sys::pal::syscall4;
use valloc::{Allocator, Provider, State};

// Numeri syscall (single source: syscall-numbers; duplicati come altrove).
const SYS_MMAP: u64 = 39;
const SYS_MUNMAP: u64 = 40;
const PROT_RW: u64 = 0x3; // PROT_READ|PROT_WRITE (solo anonimo supportato)

struct Velordo;

impl Provider for Velordo {
    unsafe fn map(len: usize) -> *mut u8 {
        debug_assert!(len % 4096 == 0);
        let r = unsafe { syscall4(SYS_MMAP, 0, len as u64, PROT_RW, 0) };
        if r < 0 {
            ptr::null_mut()
        } else {
            crate::ptr::with_exposed_provenance_mut(r as usize)
        }
    }
    unsafe fn unmap(ptr: *mut u8, len: usize) {
        // Range sempre esatto e nostro (il core smappa solo cio' che ha
        // mappato): errore ignorato (niente da corrompere, solo leak).
        unsafe {
            syscall4(SYS_MUNMAP, ptr.addr() as u64, len as u64, 0, 0);
        }
    }
}

static STATE: State = State::new();
static ALLOC: Allocator<Velordo> = Allocator::new(&STATE);

/// Istantanea stats per tuning (usata da `std::os::velordo::alloc_stats`;
/// riesportata da `sys::alloc` come le altre).
pub fn stats() -> valloc::Stats {
    ALLOC.stats()
}

pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    unsafe { ALLOC.alloc(layout) }
}

pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    unsafe { ALLOC.dealloc(ptr, layout) }
}

pub unsafe fn realloc(ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
    unsafe { ALLOC.realloc(ptr, old_layout, new_size) }
}

pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    unsafe { ALLOC.alloc_zeroed(layout) }
}
