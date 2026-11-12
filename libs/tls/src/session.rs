//! Sessione TLS 1.3: `connect()` e I/O applicativo.
//!
//! Il chiamante possiede i due buffer record (`RECORD_READ_BUF` /
//! `RECORD_WRITE_BUF`, in `Box`: l'heap civis cresce on-demand) e il
//! trasporto. La `Session` li prende in prestito: niente struct
//! auto-referenziali, niente `unsafe`.
//!
//! Il provider (RNG + verifier) vive **solo** durante l'handshake: dopo
//! `open()` la connessione non lo referenzia piu'. Sta sullo stack del
//! chiamante (`~4.9 KiB`): i due buffer record da 16 KiB restano nel `Box`
//! dell'heap civis, quindi lo stack non deve contenerli.

use embedded_tls::blocking::{
    Aes128GcmSha256, CryptoProvider, TlsClock, TlsConfig, TlsConnection, TlsContext, TlsVerifier,
};
use embedded_tls::{CryptoRngCore, TlsError};

use crate::drbg::Drbg;
use crate::transport::{Adapter, Transport};
use crate::trust::{self, Verifier};
use crate::Error;

/// Parametri di connessione.
pub struct Config<'a> {
    /// Nome del server: SNI **e** hostname da verificare nel certificato.
    /// Obbligatorio (fail-closed): vuoto = `Error::Config`.
    pub server_name: &'a str,
    /// Trust anchor X.509 in DER. Obbligatoria: vuota = `Error::Config`.
    pub ca_der: &'a [u8],
    /// Protocolli ALPN offerti (DoT: `&[b"dot"]`). Vuoto = nessun ALPN.
    pub alpn: &'a [&'a [u8]],
}

/// Provider TLS: RNG del crate + verifier `pki`.
///
/// `Drbg` e' inline (non in `Box`) perche' `drbg.rs` lo avvolge in
/// `ChaCha8` a align 8, compatibile con l'heap di civis (align <= 8).
struct Provider<'a, C: TlsClock> {
    rng: Drbg,
    verifier: Verifier<'a, C>,
}

impl<C: TlsClock> CryptoProvider for Provider<'_, C> {
    type CipherSuite = Aes128GcmSha256;
    // Nessun certificato client (mTLS assente): il tipo serve solo a
    // soddisfare il trait, come in `UnsecureProvider`.
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut self.verifier)
    }
}

/// Connessione TLS stabilita.
pub struct Session<'a, T: Transport> {
    conn: TlsConnection<'a, Adapter<T>, Aes128GcmSha256>,
}

/// Apre una connessione TLS 1.3 (handshake completo, certificato verificato).
///
/// `read_buf`/`write_buf` devono essere almeno `RECORD_READ_BUF` /
/// `RECORD_WRITE_BUF` (fail-loud: buffer piu' piccoli = handshake fallito,
/// mai troncato in silenzio). Il trasporto e' consumato dalla sessione.
pub fn connect<'a, T, C>(
    transport: T,
    config: &Config<'_>,
    read_buf: &'a mut [u8],
    write_buf: &'a mut [u8],
) -> Result<Session<'a, T>, Error>
where
    T: Transport + 'a,
    C: TlsClock,
{
    if config.server_name.is_empty() || config.ca_der.is_empty() {
        return Err(Error::Config);
    }
    if read_buf.len() < trust::RECORD_READ_BUF || write_buf.len() < trust::RECORD_WRITE_BUF {
        return Err(Error::Config);
    }

    let tls_config = TlsConfig::new()
        .with_server_name(config.server_name)
        .with_alpn(config.alpn);

    // Provider sullo stack: dopo `open()` non serve più (la connessione non
    // lo referenzia), quindi non serve allocarlo sull'heap. `Provider` e'
    // ~4.9 KiB, compatibile con lo stack user da 16 KiB.
    let provider = Provider {
        rng: Drbg::from_hw(0),
        verifier: trust::verifier::<C>(config.ca_der),
    };

    let mut conn = TlsConnection::new(Adapter(transport), read_buf, write_buf);
    conn.open(TlsContext::new(&tls_config, provider))?;

    Ok(Session { conn })
}

impl<T: Transport> Session<'_, T> {
    /// Scrive tutti i byte, forzando un record per ogni chiamata (il
    /// chiamante TLS non deve indovinare quando il record parte: dopo
    /// `write_all` i byte sono **sul filo**, non in un buffer interno).
    pub fn write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
        let mut off = 0;
        while off < buf.len() {
            let n = self.conn.write(&buf[off..])?;
            if n == 0 {
                self.conn.flush()?;
            } else {
                off += n;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Legge almeno un byte (bloccante). `Ok(0)` solo a `buf` vuoto.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(self.conn.read(buf)?)
    }

    /// Legge esattamente `buf.len()` byte o fallisce (timeout/EOF/errore).
    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        let mut off = 0;
        while off < buf.len() {
            let n = self.conn.read(&mut buf[off..])?;
            if n == 0 {
                return Err(Error::Tls(TlsError::ConnectionClosed));
            }
            off += n;
        }
        Ok(())
    }

    /// Chiude la sessione TLS (`close_notify`) e restituisce il trasporto,
    /// cosi' il chiamante puo' chiudere il socket.
    pub fn close(self) -> Result<T, (T, TlsError)> {
        match self.conn.close() {
            Ok(adapter) => Ok(adapter.0),
            Err((adapter, e)) => Err((adapter.0, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::IoError;

    struct NullTransport;
    impl Transport for NullTransport {
        fn read(&mut self, _buf: &mut [u8]) -> Result<usize, IoError> {
            Err(IoError::Closed)
        }
        fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
            Ok(buf.len())
        }
    }

    struct FixedClock;
    impl TlsClock for FixedClock {
        fn now() -> Option<u64> {
            Some(1_700_000_000)
        }
    }

    #[test]
    fn empty_server_name_is_config_error() {
        let cfg = Config {
            server_name: "",
            ca_der: &[0x30, 0x00],
            alpn: &[],
        };
        let mut r = alloc::vec![0u8; trust::RECORD_READ_BUF];
        let mut w = alloc::vec![0u8; trust::RECORD_WRITE_BUF];
        assert!(matches!(
            connect::<_, FixedClock>(NullTransport, &cfg, &mut r, &mut w),
            Err(Error::Config)
        ));
    }

    #[test]
    fn empty_ca_is_config_error() {
        let cfg = Config {
            server_name: "tls.test",
            ca_der: &[],
            alpn: &[b"dot"],
        };
        let mut r = alloc::vec![0u8; trust::RECORD_READ_BUF];
        let mut w = alloc::vec![0u8; trust::RECORD_WRITE_BUF];
        assert!(matches!(
            connect::<_, FixedClock>(NullTransport, &cfg, &mut r, &mut w),
            Err(Error::Config)
        ));
    }

    #[test]
    fn small_buffers_are_config_error() {
        let cfg = Config {
            server_name: "tls.test",
            ca_der: &[0x30, 0x00],
            alpn: &[],
        };
        let mut r = [0u8; 64];
        let mut w = [0u8; 64];
        assert!(matches!(
            connect::<_, FixedClock>(NullTransport, &cfg, &mut r, &mut w),
            Err(Error::Config)
        ));
    }
}

