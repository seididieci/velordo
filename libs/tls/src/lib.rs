//! tls — adattatore TLS 1.3 per Velordo (N6.2, ADR-0072).
//!
//! Il crate e' **puro**: nessuna syscall, nessuna dipendenza da `civis`. Il
//! trasporto (nel sistema reale l'endpoint TCP del servizio `net`) e
//! l'orologio (nel sistema reale `civis::time`) sono **iniettati**: cosi' il
//! crate si prova sull'host in millisecondi (`scripts/run-unit-tests.sh`)
//! senza boot e senza allocatore di sistema. Il collante col sistema reale
//! (endpoint TCP + orologio) e' il crate `libs/tls-net`.
//!
//! Confini:
//! - `transport`: `trait Transport` + adapter `embedded-io` blocking;
//! - `drbg`: DRBG ChaCha20 seminato da RDRAND (fallback dichiarato debole);
//! - `trust`: verifica catena + hostname, `CERT_SIZE` fail-loud;
//! - `session`: `connect()` e la sessione (record buffer 16 KiB del chiamante).
//!
//! Scelte (ADR-0072): suite `Aes128GcmSha256`, un solo named group
//! (`secp256r1`), ALPN del chiamante (`dot` per DoT). `embedded-tls` resta
//! **dentro** questo crate: fuori si vedono solo `Transport`, `Config`,
//! `Session`, `Error`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

// Allocator per i test host (non usato nel build freestanding).
#[cfg(test)]
#[global_allocator]
static A: std::alloc::System = std::alloc::System;

pub mod drbg;
pub mod session;
pub mod transport;
pub mod trust;

pub use drbg::Drbg;
pub use session::{connect, Config, Session};
pub use transport::{IoError, Transport};
pub use trust::{CERT_SIZE, RECORD_READ_BUF, RECORD_WRITE_BUF};

pub use embedded_tls::blocking::TlsError;
/// Orologio iniettato: `now()` = secondi epoch UTC. Il sistema reale usa
/// `tls_net::SystemClock`; i test host un clock fisso.
pub use embedded_tls::blocking::TlsClock;

/// Errore dell'adattatore.
#[derive(Debug, Clone, Copy)]
pub enum Error {
    /// Errore del protocollo TLS (handshake, record, certificato).
    Tls(TlsError),
    /// Configurazione non valida: SNI o trust anchor mancanti. Fail-closed:
    /// mai un handshake senza nome da verificare o senza CA.
    Config,
}

impl From<TlsError> for Error {
    fn from(e: TlsError) -> Self {
        Error::Tls(e)
    }
}
