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
