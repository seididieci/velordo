use super::*;

// ── Re-export da testproto (single source delle costanti/helper condivisi) ──
pub use testproto::{
    T_CFG, T_ACK, T_REQ, T_DONE, T_STOP,
    M_SRV, M_KILLME, M_HARDEN, M_DUPCLAIM, M_DUPGRANT, M_DUPSIBCLAIM,
    M_SEEKDENY, M_SUSPENDENY, M_SIGCATCH,
    spawn_cfg, recv_done, drain_stray, wait_exit,
};

// ── report con prefisso della suite ──
pub fn report(total: &mut u32, ok: &mut u32, name: &str, pass: bool) {
    testproto::report("[posixtests]", total, ok, name, pass);
}
