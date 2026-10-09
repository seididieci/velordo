//! Wrapper concreti sopra `vfscli::sync` (S2.2): stessa API di prima,
//! delega con lo stato del processo. Generati meccanicamente: non modificare
//! a mano (fonte in `libs/vfscli/src/sync.rs`).

use crate::Error;

#[inline]
pub fn open(path: &str, flags: u32) -> Result<i64, Error> {
    vfscli::sync::open(&super::FSTATE, path, flags)
}


pub fn read_fs(fd: i64, dst: &mut [u8], max_count: usize) -> Result<usize, Error> {
    vfscli::sync::read_fs(&super::FSTATE, fd, dst, max_count)
}


pub fn write_fs(fd: i64, src: &[u8], count: usize) -> Result<usize, Error> {
    vfscli::sync::write_fs(&super::FSTATE, fd, src, count)
}

#[inline]
pub fn close(fd: i64) -> Result<(), Error> {
    vfscli::sync::close(&super::FSTATE, fd)
}

#[inline]
pub fn readdir(path: &str, entries_buf: &mut [u8], buf_len: usize) -> Result<usize, Error> {
    vfscli::sync::readdir(&super::FSTATE, path, entries_buf, buf_len)
}

#[inline]
pub fn mkdir(path: &str) -> Result<(), Error> {
    vfscli::sync::mkdir(&super::FSTATE, path)
}

#[inline]
pub fn remove(path: &str) -> Result<(), Error> {
    vfscli::sync::remove(&super::FSTATE, path)
}


pub fn rename(old: &str, new: &str) -> Result<(), Error> {
    vfscli::sync::rename(&super::FSTATE, old, new)
}


pub fn symlink(link: &str, target: &str) -> Result<(), Error> {
    vfscli::sync::symlink(&super::FSTATE, link, target)
}


pub fn readlink(path: &str) -> Result<alloc::vec::Vec<u8>, Error> {
    vfscli::sync::readlink(&super::FSTATE, path)
}


pub fn chmod(path: &str, mode: u32) -> Result<(), Error> {
    vfscli::sync::chmod(&super::FSTATE, path, mode)
}

/// Fase 40, oggi `Failed`).
#[inline]
pub fn stat(path: &str, out: &mut Stat) -> Result<(), Error> {
    vfscli::sync::stat(&super::FSTATE, path, out)
}

/// Usato dai test (t32) e da `arca list` (P5).
#[inline]
pub fn disk_list() -> Result<alloc::vec::Vec<(u64, u64)>, Error> {
    vfscli::sync::disk_list(&super::FSTATE)
}

#[inline]
pub fn disk_info(idx: u32) -> Result<DiskDesc, Error> {
    vfscli::sync::disk_info(&super::FSTATE, idx)
}

#[inline]
pub fn disk_sync(mode: u32) -> Result<u64, Error> {
    vfscli::sync::disk_sync(&super::FSTATE, mode)
}

#[inline]
pub fn statvfs(path: &str, out: &mut StatVfs) -> Result<(), Error> {
    vfscli::sync::statvfs(&super::FSTATE, path, out)
}

#[inline]
pub fn get_hash(path: &str, out: &mut [u8; 32]) -> Result<(), Error> {
    vfscli::sync::get_hash(&super::FSTATE, path, out)
}

#[inline]
pub fn mount(source: &str, target: &str) -> Result<(), Error> {
    vfscli::sync::mount(&super::FSTATE, source, target)
}

#[inline]
pub fn umount(target: &str) -> Result<(), Error> {
    vfscli::sync::umount(&super::FSTATE, target)
}

/// offset. A rifiuto l'offset resta quello di prima (two-phase server-side).
#[inline]
pub fn lseek(fd: i64, off: i64, whence: u64) -> Result<u64, Error> {
    vfscli::sync::lseek(&super::FSTATE, fd, off, whence)
}

#[inline]
pub fn dup_grant(fd: i64) -> Result<u64, Error> {
    vfscli::sync::dup_grant(&super::FSTATE, fd)
}

#[inline]
pub fn dup_claim(nonce: u64) -> Result<i64, Error> {
    vfscli::sync::dup_claim(&super::FSTATE, nonce)
}

#[inline]
pub fn dup_cancel(nonce: u64) -> Result<(), Error> {
    vfscli::sync::dup_cancel(&super::FSTATE, nonce)
}

#[inline]
pub fn pipe() -> Result<(i64, i64), Error> {
    vfscli::sync::pipe(&super::FSTATE)
}

#[inline]
pub fn rights_drop(keep_mask: u32, subtree: Option<&str>) -> Result<(), Error> {
    vfscli::sync::rights_drop(&super::FSTATE, keep_mask, subtree)
}

#[inline]
pub fn rights_get(buf: &mut [u8]) -> Result<u32, Error> {
    vfscli::sync::rights_get(&super::FSTATE, buf)
}

#[inline]
pub fn fs_register(prefix: &[u8]) -> Result<(), Error> {
    vfscli::sync::fs_register(&super::FSTATE, prefix)
}

/// Payload = prefix separati da NUL. `Ok` se TUTTI registrati (Fase 39).
pub fn fs_register_multi(prefixes: &[&[u8]]) -> Result<(), Error> {
    vfscli::sync::fs_register_multi(&super::FSTATE, prefixes)
}


pub fn load_file(path: &str) -> Option<alloc::vec::Vec<u8>> {
    vfscli::sync::load_file(&super::FSTATE, path)
}
pub use vfscli::sync::{DiskDesc, Stat, StatVfs, fat_bpb_identity};
