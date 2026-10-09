//! Estensioni `std::os::velordo` (S1.3, bonifica POSIX): solo ffi per
//! OsString/OsStr, definizione propria byte-based (i path Velordo sono
//! byte-string; nessun riuso del modulo `unix`).
//!
//! S2.1: `alloc_stats` per il tuning (high-water per dimensionare la RAM).

#![unstable(feature = "velordo_std", issue = "none")]
#![doc(cfg(target_os = "velordo"))]
#![forbid(unsafe_op_in_unsafe_fn)]

#[path = "os_str.rs"]
pub mod ffi;

/// Istantanea allocatore (allocs, frees, live, high-water): best-effort,
/// solo tuning (S2.1).
pub fn alloc_stats() -> (usize, usize, usize, usize) {
    let s = crate::sys::alloc::stats();
    (s.allocs, s.frees, s.live, s.high)
}
