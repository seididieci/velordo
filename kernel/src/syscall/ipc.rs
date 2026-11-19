// Split from syscall.rs (byte-identical move; see facade).
use super::dispatch::apply_ipc;

/// ADR-0008 — `send(channel, tag, w0, w1)`: invia su canale (0 = canale di
/// nascita verso il parent). Ritorna la reply (tag,w0,w1) in rsi/rdx/r10.
pub(super) fn sys_send(channel: usize, tag: u64, w0: u64, w1: u64) -> i64 {
    apply_ipc(crate::ordo::sched::ipc_send(channel, tag, w0, w1))
}

/// Fase 13 — `send_async(channel, tag, w0, w1)`: come send ma NON blocca il
/// mittente. Ritorna il req_id (>= 1) in rax, o -1 se coda piena / canale
/// morto. Solo il valore di rax e' significativo (nessun ipc_override).
pub(super) fn sys_send_async(channel: usize, tag: u64, w0: u64, w1: u64) -> i64 {
    crate::ordo::sched::ipc_send_async(channel, tag, w0, w1).rax
}

/// ADR-0008 — `recv(deadline)`: riceve il prossimo messaggio.
/// Ritorna (channel, tag, w0, w1) in rdi/rsi/rdx/r10; `rax = -2` alla
/// scadenza della deadline (E1, ADR-0083).
pub(super) fn sys_recv(deadline: u64) -> i64 {
    apply_ipc(crate::ordo::sched::ipc_recv_until(deadline))
}

/// Fase 13 — `recv_nonblock()`: come recv ma ritorna -1 subito se la coda e'
/// vuota (nessun blocco). Ritorna (req_id o channel, tag, w0, w1).
pub(super) fn sys_recv_nonblock() -> i64 {
    apply_ipc(crate::ordo::sched::ipc_recv_nonblock())
}

/// ADR-0008 — `reply(tag, w0, w1)`: risponde al mittente del messaggio che il
/// chiamante sta elaborando (canale impostato da recv).
pub(super) fn sys_reply(tag: u64, w0: u64, w1: u64) -> i64 {
    apply_ipc(crate::ordo::sched::ipc_reply(tag, w0, w1))
}
