//! Re-export dell'errore nativo da `vfscli` (S2.2, traslocato: vedi li').
//!
//! Resta `civis::Error` per tutti i chiamanti; la conversione da
//! `WaitReplyError` vive qui (la' vive il tipo).

pub use vfscli::Error;

impl From<crate::ipc::WaitReplyError> for vfscli::Error {
    /// Collassa l'errore di `wait_reply` nel nativo (perde pid/code: chi li
    /// serve matcha `WaitReplyError` direttamente invece di convertire).
    fn from(e: crate::ipc::WaitReplyError) -> vfscli::Error {
        match e {
            crate::ipc::WaitReplyError::ServerDied { .. } => vfscli::Error::ServerDied,
            crate::ipc::WaitReplyError::RecvFailed => vfscli::Error::ServerDied,
            crate::ipc::WaitReplyError::UnexpectedMsg => vfscli::Error::Invalid,
        }
    }
}
