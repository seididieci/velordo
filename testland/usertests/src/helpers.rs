use super::*;
use alloc::string::String;
use alloc::vec::Vec;

// ── Re-export da testproto (single source delle costanti/helper condivisi) ──
pub use testproto::{
    T_CFG, T_ACK, T_REQ, T_DONE, T_STOP, T_OPENED, T_GO, T_READY, T_FORKREP,
    M_ECHO, M_ZERO, M_SRV, M_CHURN, M_KILLME, M_SRVDIE, M_SYNCWAIT, M_MNTDIE,
    M_OPENDIE, M_MAPHAMMER, M_FLOOD, M_NEST, M_FAULT_RO, M_FAULT_NONE, M_FAULT_NX,
    M_FAULT_GUARD, M_FAULT_GPF, M_SHMDEMO, M_FAULT_CODE, M_COWDEMO, M_FORKDEMO,
    M_ORPHAN, M_HARDEN, M_REG51, M_EXECDEMO, M_DUPCLAIM, M_DUPGRANT,
    M_DUPSIBCLAIM, M_SEEKDENY, M_SUSPENDENY, M_SIGCATCH, M_GRANTDENY,
    VA_A, VA_B, SPIN_VA, HELLO, ARCA_HELLO, ARCA_HELLO_PATH, ACFS_MAGIC,
    SPIN_SQUAT_MAGIC,
    spawn_cfg, recv_done, drain_stray, wait_exit, recv_expect, stop_flooder,
    recv_ready, poll_gone, poll_parent, dir_contains, readdir_contains,
    read_all_vec as read_all,
};

// ── report con prefisso della suite ──
pub fn report(total: &mut u32, ok: &mut u32, name: &str, pass: bool) {
    testproto::report("[usertests]", total, ok, name, pass);
}

// ── Helper specifici di usertests (non in testproto) ──
pub fn t33_read_all(fd: i64, dst: &mut [u8]) -> usize {
    let mut got = 0usize;
    while got < dst.len() {
        let rest = dst.len() - got;
        let n = match civis::read_fs(fd, &mut dst[got..], rest) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        got += n;
    }
    got
}

/// Device che ospita il volume ArcaFS root. **Mai per lettera**.
pub fn find_arca_device() -> Option<String> {
    let mut eb = [0u8; 1024];
    let cap = eb.len();
    if civis::readdir("/dev", &mut eb, cap).is_err() {
        return None;
    }
    let mut i = 0usize;
    while i < cap && eb[i] != 0 {
        let start = i;
        while i < cap && eb[i] != 0 {
            i += 1;
        }
        let name = &eb[start..i];
        if name.len() >= 3 && &name[..2] == b"sd" {
            let path = format!("/dev/{}", core::str::from_utf8(name).unwrap_or(""));
            if node_is_arca(&path) {
                return Some(path);
            }
        }
        i += 1;
    }
    None
}

/// Il nodo ha un superblock ArcaFS (magic ACFS) nel settore 0?
fn node_is_arca(path: &str) -> bool {
    let Ok(fd) = civis::open_wait(path, 0, 1000, civis::POLL_PERIOD_TICKS) else {
        return false;
    };
    let mut buf = [0u8; 512];
    let n = civis::read_fs(fd, &mut buf, 512).unwrap_or(0);
    let _ = civis::close(fd);
    n == 512 && &buf[..4] == ACFS_MAGIC
}

/// Il device ArcaFS root e' leggibile raw (settore 0 col magic ACFS)?
pub fn disk_sector0_ok() -> bool {
    match find_arca_device() {
        Some(p) => node_is_arca(&p),
        None => false,
    }
}
