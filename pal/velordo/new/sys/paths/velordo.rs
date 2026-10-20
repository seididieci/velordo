//! Path di sistema su Velordo (S2.0, modello hermit per il resto).
//!
//! `getcwd`/`chdir` chiamano davvero il kernel (SYS_GETCWD/SYS_CHDIR da
//! S1.1); `temp_dir` e' `/tmp` (ramfs); `current_exe`/`home_dir` restano
//! unsupported (niente `/proc/self/exe`, niente concetto di home — rustc
//! usa `--sysroot` esplicito).

use crate::ffi::OsStr;
use crate::io;
use crate::os::velordo::ffi::OsStrExt;
use crate::path::{Path, PathBuf};
use crate::sys::pal::velordo::numbers::{SYS_CHDIR, SYS_GETCWD};
use crate::sys::pal::{syscall4, unsupported_err};

pub fn getcwd() -> io::Result<PathBuf> {
    // CWD_MAX (256) + NUL: 512 per margine futuro, il kernel scrive ≤257.
    let mut buf = [0u8; 512];
    let r = unsafe {
        syscall4(SYS_GETCWD, buf.as_mut_ptr().addr() as u64, buf.len() as u64, 0, 0)
    };
    if r < 0 {
        return Err(unsupported_err());
    }
    let s: &OsStr = OsStrExt::from_bytes(&buf[..r as usize]);
    Ok(PathBuf::from(s))
}

pub fn chdir(path: &Path) -> io::Result<()> {
    // Il kernel vuole l'assoluto gia' risolto (civis risolve client-side);
    // qui passa i byte cosi' come sono (OsStr, mai UTF-8 obbligatorio).
    let b: &OsStr = path.as_ref();
    let b = b.as_bytes();
    let r = unsafe { syscall4(SYS_CHDIR, b.as_ptr().addr() as u64, b.len() as u64, 0, 0) };
    if r < 0 { Err(unsupported_err()) } else { Ok(()) }
}

pub fn temp_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

/// Separatore delle liste di path (`PATH`, `RUST_TARGET_PATH`): `:`, la
/// convenzione che i toolchain si aspettano (il meccanismo non ne impone una).
const PATH_SEP: u8 = b':';

/// `std::env::split_paths` (S2.5f/F2): rustc la chiama SEMPRE per risolvere
/// il target host (`RUST_TARGET_PATH`, anche vuota) — lo stub unsupported
/// panicava e abortiva il compilatore prima di leggere la sorgente.
pub struct SplitPaths<'a>(core::slice::Split<'a, u8, fn(&u8) -> bool>);

pub fn split_paths(unparsed: &OsStr) -> SplitPaths<'_> {
    fn is_sep(b: &u8) -> bool {
        *b == PATH_SEP
    }
    SplitPaths(unparsed.as_bytes().split(is_sep as fn(&u8) -> bool))
}

impl<'a> Iterator for SplitPaths<'a> {
    type Item = PathBuf;

    fn next(&mut self) -> Option<PathBuf> {
        self.0.next().map(|b| PathBuf::from(OsStrExt::from_bytes(b)))
    }
}
