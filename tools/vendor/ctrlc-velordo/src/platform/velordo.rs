//! Piattaforma Velordo per ctrlc (S2.5b, fork): niente segnali POSIX.
//!
//! Stub onesto: `init_os_handler` fallisce sempre (il driver non installa
//! handler: niente `install_ctrlc_handler` nel path, vedi ADR-0048 riga 9);
//! `block_ctrl_c` dorme per sempre (i segnali non arrivano mai). Esiste solo
//! per compilare la dipendenza; mai eseguito in pratica.

use std::fmt;

/// Segnali della piattaforma (nessuno reale su Velordo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Segnaposto (mai emesso).
    Ctrlc,
}

/// Errori della piattaforma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Per il confronto EEXIST in `error.rs` (mai costruito qui).
    EEXIST,
    /// Installazione impossibile: niente segnali.
    NoSignals,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::EEXIST => write!(f, "handler already registered"),
            Error::NoSignals => write!(f, "signals unsupported on Velordo"),
        }
    }
}

impl std::error::Error for Error {}

/// Installa l'handler OS: sempre Err (niente segnali da catturare).
///
/// # Safety
///
/// Chiamabile sempre (non tocca stato): ritorna senza effetti.
#[inline]
pub unsafe fn init_os_handler(_overwrite: bool) -> Result<(), Error> {
    Err(Error::NoSignals)
}

/// Blocca finche' arriva Ctrl-C: su Velordo dorme per sempre (i segnali non
/// arrivano mai). `park` invece di spin: niente CPU bruciata.
///
/// # Safety
///
/// Chiamabile sempre (non tocca stato).
#[inline]
pub unsafe fn block_ctrl_c() -> Result<(), Error> {
    loop {
        std::thread::park();
    }
}
