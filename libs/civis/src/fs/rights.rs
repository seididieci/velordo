//! Verifica diritti su oggetti ArcaFS (Fase S3.2b): `R_RIGHTS_EXECUTE`.
//!
//! Protocollo: scrivere il payload `[bucket\0key\0]` nel request ring con tag
//! `R_RIGHTS_EXECUTE`, inviare FS_NOTIFY, leggere la risposta dal response ring.
//! Reply: w0 = 0 OK / ERR_DENIED (!0u64 - 12).

use super::ring::{req_ring_write, resp_ring_consume};
use crate::{fs_gate, fs_notify_result, fs_reply_check, Error, FS_NOTIFY};

/// Verifica se il canale ha RIGHTS_EXECUTE + READ per `bucket/key`.
/// Ritorna `true` se autorizzato, `false` (ERR_DENIED) altrimenti.
pub fn rights_execute(bucket: &[u8], key: &[u8]) -> Result<bool, Error> {
    // Cancello ring (Fase 57): senza, un processo che non ha mai fatto FS
    // scriverebbe su VA mai mappate (#PF).
    fs_gate()?;

    // Costruisce il payload: `[bucket\0key\0]`.
    let payload_len = bucket.len() + 1 + key.len() + 1;
    if payload_len == 0 || payload_len > 512 {
        return Err(Error::Invalid);
    }

    let mut payload = alloc::vec::Vec::with_capacity(payload_len);
    payload.extend_from_slice(bucket);
    payload.push(b'\0');
    payload.extend_from_slice(key);
    payload.push(b'\0');

    // Scrive il frame nel request ring: [tag:4][w0:8][w1:8][payload].
    let frame = || req_ring_write(crate::R_RIGHTS_EXECUTE, 0, 0, &payload);
    if !frame() {
        return Err(Error::RingFull);
    }

    // Invia FS_NOTIFY e legge la risposta.
    match fs_notify_result(FS_NOTIFY, frame) {
        Some((size, _w1, _payload_len)) => {
            // Consuma l'intestazione del frame (16 byte).
            resp_ring_consume(16);
            // w0 = 0 OK / ERR_DENIED (!0u64 - 12).
            match fs_reply_check(size) {
                Ok(v) if v == 0 => Ok(true),   // Permesso
                Ok(_) => Ok(false),             // Negato (ERR_DENIED)
                Err(e) => Err(e),               // Errore di dominio
            }
        }
        None => Err(Error::NotReady),
    }
}
