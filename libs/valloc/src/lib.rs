//! `valloc` — allocatore segregated-fit per Velordo (S2.1, core in `core.rs`).
//!
//! Il core e' condiviso con la PAL std (copiato da `apply.py` in
//! `sys/alloc/valloc_core.rs`): niente `#![...]` in `core.rs`, solo qui.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod core;

pub use core::{Allocator, Provider, State, Stats};
