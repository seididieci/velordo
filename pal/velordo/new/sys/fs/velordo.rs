//! `std::fs` sopra cardo via `vfscli` vendored (S2.2, sagoma `hermit.rs`).
//!
//! Backend: sessione ring + op del client FS condiviso (`super::vfslib`);
//! `VelordoSys` implementa il provider con syscall raw. Mappature oneste:
//! - `mtime` e' epoch-secondi veri; `accessed`/`created` = unsupported.
//! - `set_permissions`/`try_clone`/`set_len`/`hard_link` = Err (mai bugie:
//!   chmod e' accept-only, DUP e' handoff non clone, niente ftruncate/hardlink).
//! - `sync_all`/`sync_data` = Ok (write-through: a reply avvenuta i dati sono
//!   stabili; la policy FLUSH resta channel-level).
//! - `set_times` = no-op (precedente hermit, innocuo); `canonicalize` =
//!   assolutizza senza normalizzare `..` (testuale come ovunque).
//! - `File` tiene il path (niente fstat per-fd: `metadata()` = stat sul path).
//! - `R_STAT` non segue i link: `metadata()` li segue a mano (max 8 hop,
//!   come `open` nel kernel).

#![allow(dead_code)] // voci future (lock/set_len) tenute per la sagoma completa

#[path = "vfslib/mod.rs"]
mod vfslib;

use self::vfslib::{cwd, sync};
use self::vfslib::{State, Sys};
use self::vfslib::error::Error as FsError;
use crate::ffi::{OsStr, OsString};
use crate::fs::TryLockError;
use crate::io::{self, BorrowedCursor, ErrorKind, IoSlice, IoSliceMut, SeekFrom};
use crate::os::velordo::ffi::{OsStrExt, OsStringExt};
use crate::path::{Path, PathBuf};
use crate::sys::pal::velordo::numbers::{
    OPEN_APPEND, OPEN_CREATE, OPEN_TRUNC, SEEK_CURRENT, SEEK_END, SEEK_START, SERVICE_CARDO,
    STAT_DEVICE, STAT_DIR, STAT_FILE, STAT_READONLY, STAT_SYMLINK, SYS_MAP_PHYSICAL, SYS_RING_ALLOC,
    SYS_SEND, SYS_SEND_ASYNC, SYS_SERVICE_LOOKUP, USER_FS_BUFFER, USER_RESP_RING,
};
use crate::sys::pal::{syscall4, syscall4_out};
pub use crate::sys::fs::common::{Dir, copy, exists};

struct VelordoSys;

impl Sys for VelordoSys {
    unsafe fn ring_alloc() -> Option<(u64, u64)> {
        let (rax, rdi, _, _, _) = unsafe { syscall4_out(SYS_RING_ALLOC, 0, 0, 0, 0) };
        if rax < 0 {
            None
        } else {
            Some((rax as u64, rdi))
        }
    }
    unsafe fn cardo_channel() -> Result<u64, ()> {
        // Id servizio dal generato (enum Service, single source).
        let r = unsafe { syscall4(SYS_SERVICE_LOOKUP, SERVICE_CARDO, 0, 0, 0) };
        if r < 0 { Err(()) } else { Ok(r as u64) }
    }
    unsafe fn send(chan: u64, tag: u64, w0: u64, w1: u64) -> Result<(u64, u64, u64), ()> {
        let (rax, _rdi, rsi, rdx, r10) = unsafe { syscall4_out(SYS_SEND, chan, tag, w0, w1) };
        if rax < 0 {
            Err(())
        } else {
            Ok((rsi, rdx, r10))
        }
    }
    unsafe fn remap_rings(req_phys: u64, resp_phys: u64) -> bool {
        let a = unsafe { syscall4(SYS_MAP_PHYSICAL, req_phys, USER_FS_BUFFER, 1, 0) };
        if a < 0 {
            return false;
        }
        unsafe { syscall4(SYS_MAP_PHYSICAL, resp_phys, USER_RESP_RING, 1, 0) >= 0 }
    }
    unsafe fn try_signal_parent(tag: u64, w0: u64) -> bool {
        unsafe { syscall4(SYS_SEND_ASYNC, 0, tag, w0, 0) >= 0 }
    }
    fn ticks() -> i64 {
        crate::sys::pal::ticks() as i64
    }
    unsafe fn getcwd_raw(buf: *mut u8, len: usize) -> i64 {
        unsafe { syscall4(crate::sys::pal::velordo::numbers::SYS_GETCWD, buf.addr() as u64, len as u64, 0, 0) }
    }
    unsafe fn chdir_raw(ptr: *const u8, len: usize) -> i64 {
        unsafe { syscall4(crate::sys::pal::velordo::numbers::SYS_CHDIR, ptr.addr() as u64, len as u64, 0, 0) }
    }
}

static FSTATE: State<VelordoSys> = State::new();

/// Traduzione errore nativo → `io` (varianti stabili, mai numeri).
fn io_err(e: FsError) -> io::Error {
    let kind = match e {
        FsError::NotFound => ErrorKind::NotFound,
        FsError::NotDir => ErrorKind::NotADirectory,
        FsError::IsDir => ErrorKind::IsADirectory,
        FsError::Exists => ErrorKind::AlreadyExists,
        FsError::ReadOnly => ErrorKind::ReadOnlyFilesystem,
        FsError::Invalid => ErrorKind::InvalidInput,
        FsError::Denied => ErrorKind::PermissionDenied,
        FsError::Busy => ErrorKind::ResourceBusy,
        FsError::Pending | FsError::RingFull | FsError::Empty => ErrorKind::WouldBlock,
        FsError::ServerDied => ErrorKind::ConnectionAborted,
        FsError::Closed => ErrorKind::BrokenPipe,
        FsError::NoSpace => ErrorKind::StorageFull,
        FsError::TooBig => ErrorKind::FileTooLarge,
        FsError::NotReady | FsError::Failed | FsError::NoMemory => ErrorKind::Other,
    };
    io::Error::new(kind, "Velordo FS error")
}

/// Path → &str (i path sono byte; non-UTF8 o NUL interni = errore onesto,
/// mai lossy: i frame ring sono NUL-terminati).
fn path_str(p: &Path) -> io::Result<&str> {
    let b = p.as_os_str().as_bytes();
    if b.contains(&0) {
        return Err(io::Error::new(ErrorKind::InvalidInput, "NUL nel path"));
    }
    core::str::from_utf8(b).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "non-UTF8 path"))
}

#[derive(Debug)]
pub struct File {
    fd: i64,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct FileAttr {
    size: u64,
    readonly: bool,
    kind: u8,
    mtime: u64,
}

impl FileAttr {
    fn from_stat(size: u64, kind: u64, mtime: u64) -> Self {
        FileAttr { size, readonly: kind & STAT_READONLY != 0, kind: (kind & 0x3) as u8, mtime }
    }
    pub fn modified(&self) -> io::Result<crate::sys::time::SystemTime> {
        Ok(crate::sys::time::SystemTime::new(self.mtime, 0))
    }
    pub fn accessed(&self) -> io::Result<crate::sys::time::SystemTime> {
        crate::sys::pal::unsupported()
    }
    pub fn created(&self) -> io::Result<crate::sys::time::SystemTime> {
        crate::sys::pal::unsupported()
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn perm(&self) -> FilePermissions {
        FilePermissions { readonly: self.readonly }
    }
    pub fn file_type(&self) -> FileType {
        FileType { kind: self.kind }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilePermissions {
    readonly: bool,
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    pub fn set_readonly(&mut self, _readonly: bool) {
        // Firma () imposta da std: impossibile fallire onestamente. No-op
        // come hermit (chmod e' accept-only; documentato, mai silenzioso
        // altrove: qui std non lascia scelta).
    }
    pub fn mode(&self) -> u32 {
        if self.readonly { 0o444 } else { 0o666 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileTimes {}

impl FileTimes {
    pub fn set_accessed(&mut self, _t: crate::sys::time::SystemTime) {}
    pub fn set_modified(&mut self, _t: crate::sys::time::SystemTime) {}
}

#[derive(Copy, Clone, Eq, Debug)]
pub struct FileType {
    kind: u8,
}

impl PartialEq for FileType {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl core::hash::Hash for FileType {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.kind.hash(state)
    }
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.kind == STAT_DIR as u8
    }
    pub fn is_file(&self) -> bool {
        self.kind == STAT_FILE as u8
    }
    pub fn is_symlink(&self) -> bool {
        self.kind == STAT_SYMLINK as u8
    }
}

pub struct ReadDir {
    entries: Vec<DirEntry>,
    idx: usize,
}

impl core::fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.entries.iter()).finish()
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        if self.idx >= self.entries.len() {
            return None;
        }
        let e = self.entries[self.idx].clone();
        self.idx += 1;
        Some(Ok(e))
    }
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    root: PathBuf,
    name: OsString,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.root.join(&self.name)
    }
    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }
    pub fn metadata(&self) -> io::Result<FileAttr> {
        stat(&self.path())
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        stat(&self.path()).map(|a| a.file_type())
    }
    pub fn ino(&self) -> u64 {
        0 // niente inode: identita' = path
    }
    pub fn file_name_os_str(&self) -> &OsStr {
        &self.name
    }
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions { read: false, write: false, append: false, truncate: false, create: false, create_new: false }
    }
    pub fn read(&mut self, read: bool) {
        self.read = read;
    }
    pub fn write(&mut self, write: bool) {
        self.write = write;
    }
    pub fn append(&mut self, append: bool) {
        self.append = append;
    }
    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }
    pub fn create(&mut self, create: bool) {
        self.create = create;
    }
    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }
}

#[derive(Debug)]
pub struct DirBuilder {
    mode: u32,
}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder { mode: 0o777 }
    }
    pub fn mkdir(&self, path: &Path) -> io::Result<()> {
        let s = path_str(path)?;
        sync::mkdir(&FSTATE, s).map_err(io_err)
    }
    pub fn set_mode(&mut self, mode: u32) {
        self.mode = mode;
    }
}

fn open_flags(opts: &OpenOptions) -> io::Result<u32> {
    if opts.create_new {
        // Niente create esclusiva atomica in cardo: check-then-create
        // correrebbe. Errore onesto (serve a tempfile/cargo: S2.4).
        return Err(io::Error::new(ErrorKind::Unsupported, "create_new esclusiva assente"));
    }
    let mut flags = 0u32;
    if opts.create {
        flags |= OPEN_CREATE as u32;
    }
    if opts.truncate {
        flags |= OPEN_TRUNC as u32;
    }
    if opts.append {
        flags |= OPEN_APPEND as u32;
    }
    Ok(flags)
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let s = path_str(path)?;
        let flags = open_flags(opts)?;
        let fd = sync::open(&FSTATE, s, flags).map_err(io_err)?;
        Ok(File { fd, path: path.to_path_buf() })
    }
    pub fn file_attr(&self) -> io::Result<FileAttr> {
        stat(&self.path)
    }
    pub fn fsync(&self) -> io::Result<()> {
        // Write-through: a reply avvenuta i dati sono stabili.
        Ok(())
    }
    pub fn datasync(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn lock(&self) -> io::Result<()> {
        Err(io::Error::new(ErrorKind::Unsupported, "file lock assente"))
    }
    pub fn lock_shared(&self) -> io::Result<()> {
        Err(io::Error::new(ErrorKind::Unsupported, "file lock assente"))
    }
    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::Error::new(ErrorKind::Unsupported, "file lock assente")))
    }
    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::Error::new(ErrorKind::Unsupported, "file lock assente")))
    }
    pub fn unlock(&self) -> io::Result<()> {
        Err(io::Error::new(ErrorKind::Unsupported, "file lock assente"))
    }
    pub fn truncate(&self, _size: u64) -> io::Result<()> {
        Err(io::Error::new(ErrorKind::Unsupported, "ftruncate assente"))
    }
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        sync::read_fs(&FSTATE, self.fd, buf, buf.len()).map_err(io_err)
    }
    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut n = 0;
        for b in bufs.iter_mut() {
            let m = self.read(b)?;
            n += m;
            if m < b.len() {
                break;
            }
        }
        Ok(n)
    }
    pub fn is_read_vectored(&self) -> bool {
        false
    }
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        sync::write_fs(&FSTATE, self.fd, buf, buf.len()).map_err(io_err)
    }
    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        // Temp sullo stack (1 KiB: gli stack user sono 16 KiB) + append:
        // niente alloc per chiamata, niente unsafe oltre `read`.
        while cursor.written() < cursor.capacity() {
            let mut tmp = [0u8; 1024];
            let n = self.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            cursor.append(&tmp[..n]);
        }
        Ok(())
    }
    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut n = 0;
        for b in bufs.iter() {
            let m = self.write(b)?;
            n += m;
            if m < b.len() {
                break;
            }
        }
        Ok(n)
    }
    pub fn is_write_vectored(&self) -> bool {
        false
    }
    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let (off, whence) = match pos {
            SeekFrom::Start(off) => {
                let off = i64::try_from(off)
                    .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "offset oltre i64"))?;
                (off, SEEK_START)
            }
            SeekFrom::End(off) => (off, SEEK_END),
            SeekFrom::Current(off) => (off, SEEK_CURRENT),
        };
        sync::lseek(&FSTATE, self.fd, off, whence).map_err(io_err)
    }
    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|a| a.size()))
    }
    pub fn tell(&self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }
    pub fn duplicate(&self) -> io::Result<File> {
        // DUP e' handoff single-use al figlio, non clone indipendente.
        Err(io::Error::new(ErrorKind::Unsupported, "try_clone senza DUP indipendente"))
    }
    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        Err(io::Error::new(ErrorKind::Unsupported, "chmod e' accept-only"))
    }
    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        Ok(())
    }
}

/// Pipe anonime condivise con `sys::pipe` (S2.4c1): stessa sessione FSTATE
/// (i ring sono singleton per processo). Wrapper sottili, mai duplicati.
pub(crate) fn pipe_raw() -> io::Result<(i64, i64)> {
    sync::pipe(&FSTATE).map_err(io_err)
}
pub(crate) fn pipe_read(fd: i64, buf: &mut [u8]) -> io::Result<usize> {
    sync::read_fs(&FSTATE, fd, buf, buf.len()).map_err(io_err)
}
pub(crate) fn pipe_write(fd: i64, buf: &[u8]) -> io::Result<usize> {
    sync::write_fs(&FSTATE, fd, buf, buf.len()).map_err(io_err)
}
pub(crate) fn pipe_close(fd: i64) {
    let _ = sync::close(&FSTATE, fd);
}
pub(crate) fn pipe_grant(fd: i64) -> io::Result<u64> {
    sync::dup_grant(&FSTATE, fd).map_err(io_err)
}

/// Stat senza seguire i link (R_STAT = lstat) → FileAttr.
fn stat_raw(path: &Path) -> io::Result<FileAttr> {
    let s = path_str(path)?;
    let mut st = vfslib_stat();
    sync::stat(&FSTATE, s, &mut st).map_err(io_err)?;
    Ok(FileAttr::from_stat(st.size, st.kind, st.mtime))
}

fn vfslib_stat() -> sync::Stat {
    sync::Stat { size: 0, kind: 0, readonly: false, mtime: 0 }
}

pub fn stat(path: &Path) -> io::Result<FileAttr> {
    stat_raw(path)
}

pub fn lstat(path: &Path) -> io::Result<FileAttr> {
    stat_raw(path)
}

/// Stat seguendo i link (max 8 hop, come `open` nel kernel): i relativi si
/// risolvono contro la dir del link.
pub fn metadata(path: &Path) -> io::Result<FileAttr> {
    let mut cur = path.to_path_buf();
    for _ in 0..8 {
        let a = stat_raw(&cur)?;
        if !a.file_type().is_symlink() {
            return Ok(a);
        }
        let target = readlink(&cur)?;
        cur = if target.is_absolute() {
            target
        } else {
            cur.parent().map(|p| p.join(&target)).unwrap_or(target)
        };
    }
    Err(io::Error::new(ErrorKind::FilesystemLoop, "troppi symlink"))
}

pub fn readdir(path: &Path) -> io::Result<ReadDir> {
    let s = path_str(path)?;
    // Una chiamata sola con buffer generoso (niente paginazione nel protocollo).
    let mut buf = Vec::new();
    buf.resize(32768, 0);
    let cap = buf.len();
    let n = sync::readdir(&FSTATE, s, &mut buf, cap).map_err(io_err)?;
    // Formato "name\0name\0...\0\0" (come `ls`).
    let mut entries = Vec::new();
    let mut i = 0;
    while i < n {
        if buf[i] == 0 {
            break;
        }
        let start = i;
        while i < n && buf[i] != 0 {
            i += 1;
        }
        entries.push(DirEntry {
            root: path.to_path_buf(),
            name: OsStringExt::from_vec(buf[start..i].to_vec()),
        });
    }
    Ok(ReadDir { entries, idx: 0 })
}

pub fn unlink(path: &Path) -> io::Result<()> {
    let s = path_str(path)?;
    sync::remove(&FSTATE, s).map_err(io_err)
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let o = path_str(old)?;
    let n = path_str(new)?;
    sync::rename(&FSTATE, o, n).map_err(io_err)
}

pub fn set_perm(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    Err(io::Error::new(ErrorKind::Unsupported, "chmod e' accept-only"))
}

pub fn set_perm_nofollow(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    Err(io::Error::new(ErrorKind::Unsupported, "chmod e' accept-only"))
}

pub fn set_times(_p: &Path, _times: FileTimes) -> io::Result<()> {
    Ok(())
}

pub fn set_times_nofollow(_p: &Path, _times: FileTimes) -> io::Result<()> {
    Ok(())
}

pub fn rmdir(path: &Path) -> io::Result<()> {
    let s = path_str(path)?;
    sync::remove(&FSTATE, s).map_err(io_err)
}

pub fn remove_dir_all(_path: &Path) -> io::Result<()> {
    crate::sys::pal::unsupported()
}

pub fn readlink(p: &Path) -> io::Result<PathBuf> {
    let s = path_str(p)?;
    let bytes = sync::readlink(&FSTATE, s).map_err(io_err)?;
    let os: &OsStr = OsStrExt::from_bytes(&bytes);
    Ok(Path::new(os).to_path_buf())
}

pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    let o = path_str(original)?;
    let l = path_str(link)?;
    sync::symlink(&FSTATE, l, o).map_err(io_err)
}

pub fn link(_original: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(ErrorKind::Unsupported, "hard link assenti"))
}

pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    // Assolutizza senza normalizzare `..` (testuale come ovunque in Velordo).
    let s = path_str(p)?;
    let abs = cwd::resolve::<VelordoSys>(s).map_err(io_err)?;
    let os: &OsStr = OsStrExt::from_bytes(&abs);
    Ok(Path::new(os).to_path_buf())
}
