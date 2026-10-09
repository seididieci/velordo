//! Directory di lavoro (S1.1): `chdir`/`getcwd` + risoluzione relativi.
//!
//! Niente cache client-side per disegno: ogni risoluzione fa una SYS_GETCWD
//! (una syscall in piu' per op con path — mai stato stale tra thread, fork
//! ed exec, dove una cache dovrebbe invalidarsi: la correttezza prima, la
//! cache quando i profili lo chiederanno). Il kernel tiene la cwd sul leader
//! del gruppo (condivisa ai thread come POSIX), sopravvive a exec, ereditata
//! da fork/spawn. Solo assoluti nel kernel; `..` resta testuale (nessuna
//! normalizzazione da nessuna parte, come prima).

use super::abi::*;
use super::{State, Sys};
use super::error::Error;
use super::sync;
use alloc::vec::Vec;

/// `getcwd()`: directory di lavoro del gruppo (sempre assoluta, `/` default).
/// Ritorna i byte senza NUL.
pub fn getcwd<S: Sys>() -> Result<Vec<u8>, Error> {
    let mut buf = [0u8; CWD_MAX + 1];
    let r = unsafe { S::getcwd_raw(buf.as_mut_ptr(), buf.len()) };
    if r < 0 {
        return Err(Error::Invalid);
    }
    Ok(buf[..r as usize].to_vec())
}

/// Unisce `path` alla cwd se relativo (assoluti invariati). Vuoto = invariato
/// (i chiamanti mantengono la semantica empty-path di prima).
pub fn resolve<S: Sys>(path: &str) -> Result<Vec<u8>, Error> {
    if path.is_empty() || path.starts_with('/') {
        return Ok(path.as_bytes().to_vec());
    }
    let cwd = getcwd::<S>()?;
    let mut out = Vec::with_capacity(cwd.len() + 1 + path.len());
    out.extend_from_slice(&cwd);
    if !cwd.is_empty() && *cwd.last().unwrap_or(&0) != b'/' {
        out.push(b'/');
    }
    out.extend_from_slice(path.as_bytes());
    if out.len() > CWD_MAX {
        return Err(Error::Invalid);
    }
    Ok(out)
}

/// `chdir(path)`: cambia la cwd del gruppo. Relativi uniti alla cwd corrente;
/// il target deve ESISTERE ed essere una dir (stat lstat: i symlink contano
/// come dir solo se... no — stat riporta il link (kind 3): chdir su link =
/// INVALID diretto qui? POSIX segue. Seguiamo: se link, risolvi un hop via
/// readlink relativo alla dir del link, poi stat del target).
/// Errori: NotFound/NotDir/Invalid dal kernel o dallo stat.
pub fn chdir<S: Sys>(st: &'static State<S>, path: &str) -> Result<(), Error> {
    let abs = resolve::<S>(path)?;
    let abs_str = core::str::from_utf8(&abs).map_err(|_| Error::Invalid)?;
    // Il target deve esistere ed essere dir (il kernel non ha FS: stat qui).
    let mut meta = sync::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
    sync::stat(st, abs_str, &mut meta)?;
    if !meta.is_dir() {
        return Err(Error::NotDir);
    }
    let r = unsafe { S::chdir_raw(abs.as_ptr(), abs.len()) };
    if r < 0 {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}


