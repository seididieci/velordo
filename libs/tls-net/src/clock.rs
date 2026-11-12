//! Orologio reale per la verifica dei certificati.

/// Secondi epoch UTC dal servizio `Time` (`usertime`).
///
/// `None` (servizio assente o morto) = nessun controllo delle date: la catena
/// resta verificata crittograficamente. Meglio di un panic dentro l'handshake;
/// chi chiama puo' registrare il degrado.
pub struct SystemClock;

impl tls::TlsClock for SystemClock {
    fn now() -> Option<u64> {
        civis::time::wall_secs().ok()
    }
}