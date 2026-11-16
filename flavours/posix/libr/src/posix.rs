//! Fase 39 (P0, fondamenta posix) — traduzione POSIX al bordo.
//!
//! Il kernel e il wire restano senza concetti POSIX (ADR-0015/0025): niente
//! errno, path o segnali dentro. Gli errori viaggiano come varianti tipate di
//! [`civis::Error`] (vocabolario condiviso nel meccanismo `civis`, usato da
//! tutti i wrapper) e diventano numeri errno SOLO qui, via [`to_errno`] —
//! unico punto di traduzione (table-tested in t53). Questo modulo e' pura
//! traduzione: vive in `flavours/posix/libr` (ADR-0041); il meccanismo non lo
//! usa mai.
//!
//! In Fase 39 esistono solo le varianti di TRASPORTO (fallimenti osservabili
//! senza aiuto del server) piu' i rifiuti che il client puo' attribuire da
//! solo. Le varianti di DOMINIO (`NotFound`, `ReadOnly`, ...) sono dichiarate
//! con mapping fissato ma senza produttori: li aggiunge la Fase 40, quando
//! cardo iniziera' a inviare codici distinti invece del generico `ERR` (che
//! qui collassa in [`Error::Failed`]).
//!
//! Riesporta [`civis::Error`] per comodita' (`libr::posix::Error` = path valido
//! del vocabolario nativo).

pub use civis::Error;

// ── Numeri errno POSIX standard (solo per `to_errno`, mai nel kernel/wire) ──
pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const EIO: i64 = 5;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EBUSY: i64 = 16;
pub const EEXIST: i64 = 17;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const EFBIG: i64 = 27;
pub const EROFS: i64 = 30;
pub const EAGAIN: i64 = 35;
pub const EPIPE: i64 = 32;
pub const ENOSPC: i64 = 28;

// ── Numeri errno specifici socket (N7.1) ──
pub const EBADF: i64 = 9;
pub const ENOTCONN: i64 = 57;
pub const EAFNOSUPPORT: i64 = 97;
pub const EOPNOTSUPP: i64 = 95;
pub const ETIMEDOUT: i64 = 110;
pub const ECONNRESET: i64 = 104;

/// UNICA traduzione nativo→errno (Fase 39). Totale sul dominio (il compilatore
/// impone un braccio per variante: nessuna nuova variante senza errno). Il caso
/// peggiore di un bug qui e' un numero sbagliato in un messaggio, mai un
/// comportamento errato del sistema (le decisioni usano le varianti, non i numeri).
pub fn to_errno(e: Error) -> i64 {
    match e {
        Error::NotReady => EIO,
        Error::Pending => EAGAIN,
        Error::RingFull => EAGAIN,
        Error::ServerDied => EIO,
        Error::Denied => EACCES,
        Error::NoMemory => ENOMEM,
        Error::Busy => EBUSY,
        Error::Invalid => EINVAL,
        Error::Failed => EIO,
        Error::NotFound => ENOENT,
        Error::NotDir => ENOTDIR,
        Error::IsDir => EISDIR,
        Error::Exists => EEXIST,
        Error::ReadOnly => EROFS,
        Error::TooBig => EFBIG,
        Error::Empty => EAGAIN,
        Error::Closed => EPIPE,
        Error::NoSpace => ENOSPC,
    }
}
