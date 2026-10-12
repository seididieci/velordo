//! `vfscli` — client FS condiviso (S2.2): sessione ring + op cardo.
//!
//! Estratto da `civis` (stessa disciplina, zero cambi di comportamento):
//! codec frame, handshake/lookup, guard 1-in-volo, op sync, resolve cwd.
//! `no_std` + `alloc` (niente `#[global_allocator]` qui: l'allocatore lo da'
//! il consumatore). Backing syscall astratto via [`Sys`] (`civis` e la PAL
//! `std` lo implementano); costanti ABI via [`abi`] (re-export nel repo,
//! generate nella copia vendored in `std`).
//!
//! Stato per-istanza esplicito ([`core::State`], posseduto dal consumatore).
//! Tutto l'accesso condiviso e' atomico Relaxed come in origine (stesse
//! garanzie di prima: niente lock di sessione — debito noto per S2.3 con i
//! thread, come l'heap `civis`).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod abi;
pub mod base;
pub mod cwd;
pub mod error;
pub mod ring;
pub mod session;
pub mod sync;

pub use base::{State, Sys};
pub use error::Error;

