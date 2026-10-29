pub mod ops_async;
pub mod ring;
pub mod session;
pub mod sync;
pub mod obj;
pub mod cwd;
pub mod rights;

use vfscli::{State, Sys};

/// Backing syscall per il client FS di questo processo (S2.2): wrapper
/// sottili sopra i meccanismi esistenti (nessuna logica qui).
pub(crate) struct CivisSys;

impl Sys for CivisSys {
    unsafe fn ring_alloc() -> Option<(u64, u64)> {
        let (rax, rdi, _, _, _) = unsafe { crate::syscall4_out(crate::SYS_RING_ALLOC, 0, 0, 0, 0) };
        if rax < 0 {
            None
        } else {
            Some((rax as u64, rdi))
        }
    }
    unsafe fn cardo_channel() -> Result<u64, ()> {
        crate::spawn::service_lookup(crate::Service::Cardo)
            .map(|c| c as u64)
            .map_err(|_| ())
    }
    unsafe fn send(chan: u64, tag: u64, w0: u64, w1: u64) -> Result<(u64, u64, u64), ()> {
        crate::ipc::send(chan, tag, w0, w1).map(|r| (r.tag, r.w0, r.w1)).map_err(|_| ())
    }
    unsafe fn remap_rings(req_phys: u64, resp_phys: u64) -> bool {
        use vfscli::ring::{REQ_RING_VA, RESP_RING_VA};
        if crate::spawn::map_physical(req_phys, REQ_RING_VA, 1).is_err() {
            return false;
        }
        crate::spawn::map_physical(resp_phys, RESP_RING_VA, 1).is_ok()
    }
    unsafe fn try_signal_parent(tag: u64, w0: u64) -> bool {
        crate::ipc::send_async(crate::ipc::CHANNEL_PARENT, tag, w0, 0).is_ok()
    }
    fn ticks() -> i64 {
        crate::sys::get_ticks()
    }
    unsafe fn getcwd_raw(buf: *mut u8, len: usize) -> i64 {
        unsafe { crate::syscall4(crate::SYS_GETCWD, buf as u64, len as u64, 0, 0) }
    }
    unsafe fn chdir_raw(ptr: *const u8, len: usize) -> i64 {
        unsafe { crate::syscall4(crate::SYS_CHDIR, ptr as u64, len as u64, 0, 0) }
    }
}

/// Stato FS del processo (uno solo: i wrapper sotto lo usano tutti).
pub(crate) static FSTATE: State<CivisSys> = State::new();
