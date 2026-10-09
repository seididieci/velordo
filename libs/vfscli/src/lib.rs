//! `vfscli` — client FS condiviso (S2.2): sessione ring + op cardo.
//!
//! Estratto da `civis` (stessa disciplina, zero cambi di comportamento):
//! codec frame, handshake/lookup, guard 1-in-volo, op sync, resolve cwd.
//! `no_std` + `alloc` (niente `#[global_allocator]` qui: l'allocatore lo da'
//! il consumatore). Backing syscall astratto via [`Sys`] (`civis` e la PAL
//! `std` lo implementano); costanti ABI via [`abi`] (re-export nel repo,
//! generate nella copia vendored in `std`).
//!
//! Stato per-istanza (una per processo/test): dentro le funzioni generiche
//! (una istanza per `S`, pattern `valloc`). Tutto l'accesso condiviso e'
//! atomico Relaxed come in origine (stesse garanzie di prima: niente lock
//! di sessione — debito noto per S2.3 con i thread, come l'heap `civis`).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod abi;
pub mod cwd;
pub mod error;
pub mod ring;
pub mod session;
pub mod sync;

pub use error::Error;

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

/// Backing syscall del client FS. Implementato da `civis` (wrapper esistenti)
/// e dalla PAL `std` (syscall raw). Tutto `unsafe` tranne `ticks` (lettura).
pub trait Sys {
    /// Alloca la coppia di ring (ritorna i fisici; il kernel mappa le VA).
    /// None su OOM.
    unsafe fn ring_alloc() -> Option<(u64, u64)>;
    /// Canale verso cardo (lookup per nome). Errore se non registrato.
    unsafe fn cardo_channel() -> Result<u64, ()>;
    /// Send bloccante con reply diretta `(tag, w0, w1)` (o morte peer). Il
    /// kernel sveglia il mittente su morte (Fase 14): niente wait/queue
    /// user-side qui.
    unsafe fn send(chan: u64, tag: u64, w0: u64, w1: u64) -> Result<(u64, u64, u64), ()>;
    /// Rimappa i propri ring alle VA fisse (dopo injection altrui).
    unsafe fn remap_rings(req_phys: u64, resp_phys: u64) -> bool;
    /// Un tentativo fire-and-forget al parent (il chiamante ritenta).
    unsafe fn try_signal_parent(tag: u64, w0: u64) -> bool;
    /// Ticks per i bound dei retry (stesse unita' e segno di prima: i64).
    fn ticks() -> i64;
    /// GETCWD raw nel buffer (bytes senza NUL). Negativo su errore.
    unsafe fn getcwd_raw(buf: *mut u8, len: usize) -> i64;
    /// CHDIR raw (path gia' assoluto e validato). Negativo su errore.
    unsafe fn chdir_raw(ptr: *const u8, len: usize) -> i64;
}

/// Stato per-istanza (era globale per processo, ora uno per consumatore).
/// Solo atomici: `Sync` automatico, accesso Relaxed come in origine (il lock
/// di sessione e' debito S2.3, vedi sopra). Il parametro `S` lega lo stato
/// al provider (inferenza automatica nelle chiamate interne, mai turbofish).
pub struct State<S> {
    chan: AtomicI64,
    inited: AtomicBool,
    pending: AtomicI64,
    req: AtomicU64,
    resp: AtomicU64,
    child: AtomicBool,
    _s: PhantomData<fn() -> S>,
}

impl<S> State<S> {
    /// Stato zero (canale invalido, non-iniziato, niente in volo).
    pub const fn new() -> Self {
        State {
            chan: AtomicI64::new(-1),
            inited: AtomicBool::new(false),
            pending: AtomicI64::new(-1),
            req: AtomicU64::new(0),
            resp: AtomicU64::new(0),
            child: AtomicBool::new(false),
            _s: PhantomData,
        }
    }
}
