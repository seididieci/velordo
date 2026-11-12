//! Fiducia: verifica della catena e dell'hostname, clock, dimensioni.
//!
//! La verifica X.509 e' **di `embedded-tls`** (modulo `pki`, feature
//! `rustpki`): qui c'e' solo il cablaggio — trust anchor in DER, orologio
//! reale, nome da verificare — e la scelta fail-loud delle dimensioni.

use embedded_tls::blocking::{Aes128GcmSha256, Certificate, TlsClock};
use embedded_tls::pki::CertVerifier;

/// Dimensione del buffer del certificato server nel verifier
/// (`OwnedCertificate<CERT_SIZE>`: due `heapless::Vec` da N).
///
/// **Fail-loud**: una catena piu' grande fa fallire l'handshake con
/// `OutOfMemory`, mai un handshake "riuscito" senza verifica. 4 KiB copre
/// una catena Let's Encrypt (leaf + intermediate ~2.7 KiB) e lascia margine;
/// il buffer e' nel `Box` del provider (vedi `session.rs`), non sullo stack.
pub const CERT_SIZE: usize = 4096;

/// Buffer record in lettura: 16640 e' il massimo di un record TLS 1.3
/// (16 KiB di plaintext + overhead); `embedded-tls` lo documenta come valore
/// sicuro. Allocato dal chiamante (in `Box`, heap civis).
pub const RECORD_READ_BUF: usize = 16640;

/// Buffer record in scrittura: deve contenere almeno un handshake
/// (ClientHello con key share ~ 1.3 KiB). 16 KiB tiene il passo del buffer di
/// lettura senza frammentare gli handshake.
pub const RECORD_WRITE_BUF: usize = 16384;

/// Verifier concreto: catena `pki` + clock iniettato + `CERT_SIZE`.
pub type Verifier<'a, C> = CertVerifier<'a, Aes128GcmSha256, C, CERT_SIZE>;

/// Costruisce il verifier da una trust anchor X.509 in DER.
///
/// L'hostname verification la attiva `TlsConnection::open` chiamando
/// `set_hostname_verification` col `server_name` del `TlsConfig`: qui non
/// serve altro.
pub fn verifier<C>(ca_der: &[u8]) -> Verifier<'_, C>
where
    C: TlsClock,
{
    CertVerifier::new(Certificate::X509(ca_der))
}

/// Orologio di sistema (feature `civis-clock`): epoch UTC dal servizio
/// `Time` (`civis::time`). La verifica di validita' dei certificati e' quindi
/// reale, non "sempre valido".
#[cfg(feature = "civis-clock")]
pub struct SystemClock;

#[cfg(feature = "civis-clock")]
impl TlsClock for SystemClock {
    fn now() -> Option<u64> {
        // `None` (servizio Time assente) = niente controllo date: la catena
        // e' comunque verificata crittograficamente. Meglio di un panic in
        // handshake; il chiamante puo' loggare il degrado.
        civis::time::wall_secs().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_tls::blocking::TlsClock;

    struct FixedClock;
    impl TlsClock for FixedClock {
        fn now() -> Option<u64> {
            Some(1_700_000_000)
        }
    }

    #[test]
    fn verifier_accepts_der_slice() {
        // Il verifier non valida nulla alla costruzione: solo il cablaggio.
        let der = [0x30u8, 0x00];
        let _v: Verifier<'_, FixedClock> = verifier(&der);
        assert_eq!(CERT_SIZE, 4096);
        assert!(RECORD_READ_BUF >= 16640);
        assert!(RECORD_WRITE_BUF >= 16384);
    }

    #[test]
    fn fixed_clock_is_stable() {
        assert_eq!(FixedClock::now(), Some(1_700_000_000));
    }
}
