//! Verifica diritti su oggetti ArcaFS (Fase S3.2b): `R_RIGHTS_EXECUTE`.
//!
//! Protocollo: scrivere il payload `[bucket\0key\0]` nel request ring con tag
//! `R_RIGHTS_EXECUTE`, inviare FS_NOTIFY, leggere la risposta dal response ring.
//! Reply: w0 = 0 OK / ERR_DENIED (!0u64 - 12); **w1 = `ImageClass`** (ADR-0078
//! fase 2: 0 = `Untrusted`, 1 = `ThirdParty`, 2 = `System`).

use super::ring::{req_ring_write, resp_ring_consume};
use crate::{fs_gate, fs_notify_result, fs_reply_check, Error, ImageClass, FS_NOTIFY};

/// Verifica se il canale ha RIGHTS_EXECUTE + READ per `bucket/key` **e**
/// riporta la classe dell'immagine verificata (ADR-0078 fase 2).
///
/// `(true, classe)` = eseguibile; `(false, _)` = negato (`ERR_DENIED`).
/// La classe e' una proprieta' del **contenuto** (sidecar indicizzato dal
/// digest): due copie degli stessi byte hanno la stessa classe.
pub fn rights_execute_verdict(
    bucket: &[u8],
    key: &[u8],
) -> Result<(bool, ImageClass), Error> {
    // Cancello ring (Fase 57): senza, un processo che non ha mai fatto FS
    // scriverebbe su VA mai mappate (#PF).
    fs_gate()?;

    // Costruisce il payload col formato condiviso
    // `[bucket_len:1][bucket]\0[key_len:1][key]\0` (single source
    // `arcafs::wire`, lo stesso che parsa cardo): i bound li applica il
    // builder, mai troncamento silenzioso.
    let payload = match arcafs::wire::obj_prefix(bucket, key) {
        Some(p) => p,
        None => return Err(Error::Invalid),
    };
    let payload_len = payload.len();

    // Scrive il frame nel request ring: [tag:4][w0:8][w1:8][payload].
    // w0 = lunghezza payload: cardo consuma ESATTAMENTE il dichiarato (con
    // w0=0 il frame veniva scartato come payload mancante → sempre ERR).
    let frame = || req_ring_write(crate::R_RIGHTS_EXECUTE, payload_len as u64, 0, &payload);
    if !frame() {
        return Err(Error::RingFull);
    }

    // Invia FS_NOTIFY e legge la risposta.
    match fs_notify_result(FS_NOTIFY, frame) {
        Some((result, w1, _payload_len)) => {
            // Consuma l'intestazione del frame (16 byte).
            resp_ring_consume(16);
            // w1 = classe; per un negato non significa nulla (nessuna
            // promozione possibile: il client la ignora se `ok == false`).
            let class = ImageClass::from_u64(w1);
            // w0 = 0 OK / ERR_DENIED (!0u64 - 12).
            match fs_reply_check(result) {
                Ok(v) if v == 0 => Ok((true, class)), // Permesso
                Ok(_) => Ok((false, class)),          // Negato (sentinella ignota)
                Err(Error::Denied) => Ok((false, class)), // Negato (ERR_DENIED)
                Err(e) => Err(e),                     // Errore di dominio
            }
        }
        None => Err(Error::NotReady),
    }
}

/// Verifica se il canale ha RIGHTS_EXECUTE + READ per `bucket/key`.
/// Ritorna `true` se autorizzato, `false` (ERR_DENIED) altrimenti.
///
/// Variante senza classe: la usano i client che vogliono solo il permesso
/// (l'interprete nativo). Le regole di firma sono applicate da cardo in
/// entrambe le varianti, quindi il verdetto e' sempre quello autorevole.
pub fn rights_execute(bucket: &[u8], key: &[u8]) -> Result<bool, Error> {
    rights_execute_verdict(bucket, key).map(|(ok, _)| ok)
}
