//! Wrapper concreti sopra `vfscli::session` (S2.2): stessa API di prima,
//! delega con lo stato del processo. Generati meccanicamente: non modificare
//! a mano (fonte in `libs/vfscli/src/session.rs`).

use crate::Error;


pub fn post_spawn_copy_child() {
    vfscli::session::post_spawn_copy_child(&super::FSTATE)
}


pub fn fs_child_reinit() -> bool {
    vfscli::session::fs_child_reinit(&super::FSTATE)
}

#[inline]
pub fn is_spawn_copy_child() -> bool {
    vfscli::session::is_spawn_copy_child(&super::FSTATE)
}


pub fn fs_chan() -> i64 {
    vfscli::session::fs_chan(&super::FSTATE)
}


pub fn fs_async_pending() -> bool {
    vfscli::session::fs_async_pending(&super::FSTATE)
}

/// pulito; il figlio che deve fare FS deve prima `exec`-care).
pub fn fs_gate() -> Result<(), Error> {
    vfscli::session::fs_gate(&super::FSTATE)
}


pub fn poll_wait(bound_ticks: i64, period_ticks: i64, mut f: impl FnMut() -> bool) -> bool {
    vfscli::session::poll_wait::<super::CivisSys>(bound_ticks, period_ticks, f)
}


pub fn open_wait(path: &str, flags: u32, bound_ticks: i64, period_ticks: i64) -> Result<i64, Error> {
    vfscli::session::open_wait(&super::FSTATE, path, flags, bound_ticks, period_ticks)
}


pub fn fs_chan_rt() -> i64 {
    vfscli::session::fs_chan_rt(&super::FSTATE)
}


pub fn fs_send(tag: u64, w0: u64, w1: u64) -> Result<crate::ipc::IpcReply, Error> {
    vfscli::session::fs_send(&super::FSTATE, tag, w0, w1)
        .map(|(tag, w0, w1)| crate::ipc::IpcReply { tag, w0, w1 })
}


pub fn fs_rings() -> bool {
    vfscli::session::fs_rings(&super::FSTATE)
}


pub fn fs_light_gate() -> Result<(), Error> {
    vfscli::session::fs_light_gate(&super::FSTATE)
}


pub fn fs_init() -> bool {
    vfscli::session::fs_init(&super::FSTATE)
}


pub fn fs_rehandshake() -> bool {
    vfscli::session::fs_rehandshake(&super::FSTATE)
}


pub fn fs_remap_self() -> bool {
    vfscli::session::fs_remap_self(&super::FSTATE)
}


pub fn ensure_fs_mount(register: fn() -> Result<(), Error>) {
    vfscli::session::ensure_fs_mount(&super::FSTATE, register)
}


pub fn signal_ready(w0: u64) {
    vfscli::session::signal_ready::<super::CivisSys>(w0)
}


pub fn ring_alloc_raw() -> Option<(u64, u64)> {
    vfscli::session::ring_alloc_raw::<super::CivisSys>()
}


pub fn fs_notify_result(tag: u64, rewrite: impl Fn() -> bool) -> Option<(u64, u64, usize)> {
    vfscli::session::fs_notify_result(&super::FSTATE, tag, rewrite)
}


pub fn guard_take(req: i64) {
    vfscli::session::guard_take(&super::FSTATE, req)
}

/// Vedi `guard_take`.
pub fn guard_clear() {
    vfscli::session::guard_clear(&super::FSTATE)
}

/// Vedi `guard_take`.
pub fn guard_get() -> i64 {
    vfscli::session::guard_get(&super::FSTATE)
}

/// Vedi `guard_take`.
pub fn chan_set(chan: i64) {
    vfscli::session::chan_set(&super::FSTATE, chan)
}

/// Vedi `guard_take`.
pub fn chan_get() -> i64 {
    vfscli::session::chan_get(&super::FSTATE)
}


pub fn ring_phys() -> (u64, u64) {
    vfscli::session::ring_phys(&super::FSTATE)
}
/// Variante di `poll_wait` che ritorna il valore prodotto da `f()`
/// (`None` = "non ancora pronto, riprova"). Utile quando serve il risultato
/// del tentativo riuscito (fd, pid, ...), non solo un booleano.
pub fn poll_value<T>(bound_ticks: i64, period_ticks: i64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    vfscli::session::poll_value::<super::CivisSys, T>(bound_ticks, period_ticks, f)
}
pub use vfscli::session::{fs_reply_check, ring_reset};
pub use vfscli::session::POLL_PERIOD_TICKS;
