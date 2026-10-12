//! Core condiviso `vfscli` (S2.2): trait [`Sys`] + [`State`].
//!
//! Niente `#![...]` qui (questo file viene anche copiato in `std`): solo
//! item. `lib.rs` dichiara i moduli e riesporta.

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
    pub(crate) chan: AtomicI64,
    pub(crate) inited: AtomicBool,
    pub(crate) pending: AtomicI64,
    pub(crate) req: AtomicU64,
    pub(crate) resp: AtomicU64,
    pub(crate) child: AtomicBool,
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
