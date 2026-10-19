//! Object store nativo ArcaFS (Fase 55, A1): `R_OBJ_GET`/`R_OBJ_PUT`.
//!
//! Stateless: il server non tiene stato tra le chiamate GET successive.
//! Chunking automatico: GET > RING_MAX_PAYLOAD → loop client con offset++.

use super::ring::{req_ring_write, resp_ring_read_payload, resp_ring_consume, RING_MAX_PAYLOAD};
use crate::{fs_gate, fs_notify_result, fs_reply_check, Error, FS_NOTIFY};
use arcafs::proto::{
    R_OBJ_GET, R_OBJ_PUT, R_SNAP_CREATE, R_SNAP_DELETE, R_SNAP_ROLLBACK, R_SNAP_CLONE,
    R_OBJ_GET_ID, R_OBJ_STAT_ID, R_OBJ_DELETE, R_OBJ_STAT,
};
use arcafs::wire;
use alloc::vec;
use alloc::vec::Vec;

/// GET un oggetto da ArcaFS (stateless, chunking automatico).
/// Ritorna il blob completo o Error (`Invalid` a bucket/chiave oltre bound,
/// mai troncamento silenzioso).
pub fn obj_get(bucket: &[u8], key: &[u8]) -> Result<Vec<u8>, Error> {
    // Cancello ring (Fase 57): senza, un processo che non ha mai fatto FS
    // scriverebbe su VA mai mappate (#PF) o interleaverebbe un async in volo.
    fs_gate()?;
    let mut offset = 0usize;
    let mut result = Vec::new();

    loop {
        let payload = match build_get_payload(bucket, key) {
            Some(p) => p,
            None => return Err(Error::Invalid),
        };
        let frame = || req_ring_write(R_OBJ_GET, payload.len() as u64, offset as u64, &payload);
        if !frame() {
            return Err(Error::RingFull);
        }

        match fs_notify_result(FS_NOTIFY, frame) {
            Some((size, _, payload_len)) => {
                // Sentinella ERR_* → errore di dominio, CONSUMA il frame (16B)
                // prima di uscire: senza, il frame stale resta nel response ring
                // e la successiva op FS legge spazzatura (desync).
                let total = match fs_reply_check(size) {
                    Ok(v) => v as usize,
                    Err(e) => {
                        resp_ring_consume(16);
                        return Err(e);
                    }
                };
                // Blob vuoto o EOF: niente payload, esci.
                let remaining = total.saturating_sub(offset);
                if remaining == 0 {
                    resp_ring_consume(16);
                    break;
                }
                let to_read = remaining.min(payload_len).min(RING_MAX_PAYLOAD);
                if to_read == 0 {
                    // Frame senza payload ma size non-zero: impossibile, esci
                    // consumando per non desincronizzare.
                    resp_ring_consume(16);
                    break;
                }
                let mut buf = vec![0u8; to_read];
                resp_ring_read_payload(&mut buf, to_read);
                result.extend_from_slice(&buf);
                offset += to_read;
                if offset >= total {
                    break;
                }
            }
            None => return Err(Error::NotReady),
        }
    }

    Ok(result)
}

/// PUT un oggetto in ArcaFS. Ritorna size scritta o Error (`Invalid` a
/// bucket/chiave oltre bound, mai troncamento silenzioso).
pub fn obj_put(bucket: &[u8], key: &[u8], data: &[u8]) -> Result<u64, Error> {
    // Cancello ring (Fase 57): vedi `obj_get`.
    fs_gate()?;
    // Valida i nomi una volta sola (il PUT vuoto salta il loop: senza,
    // nomi oltre bound passerebbero con Ok(0)).
    if wire::obj_prefix(bucket, key).is_none() {
        return Err(Error::Invalid);
    }
    let total_len = data.len();
    let mut written = 0usize;

    while written < total_len {
        let want = (total_len - written).min(RING_MAX_PAYLOAD);
        let payload = match build_put_payload(bucket, key, &data[written..written + want]) {
            Some(p) => p,
            None => return Err(Error::Invalid),
        };

        if !req_ring_write(R_OBJ_PUT, payload.len() as u64, written as u64, &payload) {
            return if written > 0 { Ok(written as u64) } else { Err(Error::RingFull) };
        }

        match fs_notify_result(FS_NOTIFY, || {
            req_ring_write(R_OBJ_PUT, payload.len() as u64, written as u64, &payload)
        }) {
            Some((size, _, _)) => {
                resp_ring_consume(16);
                let r = match fs_reply_check(size) {
                    Ok(v) => v,
                    Err(e) => {
                        if written > 0 {
                            return Ok(written as u64);
                        }
                        return Err(e);
                    }
                };
                written += (r as usize).min(want);
            }
            None => {
                if written > 0 {
                    return Ok(written as u64);
                }
                return Err(Error::NotReady);
            }
        }
    }

    Ok(total_len as u64)
}

/// Payload GET: prefisso condiviso (`arcafs::wire`, niente dati).
fn build_get_payload(bucket: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    wire::obj_prefix(bucket, key)
}

/// Payload PUT: prefisso condiviso + chunk dati.
fn build_put_payload(bucket: &[u8], key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut p = wire::obj_prefix(bucket, key)?;
    p.extend_from_slice(data);
    Some(p)
}

// ── Versioni + snapshot (Fase 56.1) ─────────────────────────────────
// Convenzione comune: richiesta con retry di scrittura frame (stesso
// pattern di obj_put), frame risposta da 16 B consumato qui; i payload
// dedicati (STAT) letti dal chiamante subito dopo, prima di qualunque
// altra op sul ring (1-in-volo, Fase 13).

/// Invia una richiesta object/snap e ritorna (w0, w1) della reply.
/// Il frame risposta (header 16 B, niente payload dedicato) e' consumato.
fn obj_request(tag: u32, w0: u64, w1: u64, payload: &[u8]) -> Result<(u64, u64), Error> {
    // Cancello ring (Fase 57): copre snap_*/obj_delete/get_id/stat* (un solo
    // punto invece di N bracci: tutte le scalari passano di qui).
    fs_gate()?;
    let frame = || req_ring_write(tag, w0, w1, payload);
    if !frame() {
        return Err(Error::RingFull);
    }
    match fs_notify_result(FS_NOTIFY, frame) {
        Some((a, b, _)) => {
            resp_ring_consume(16);
            Ok((a, b))
        }
        None => Err(Error::NotReady),
    }
}

/// Richiesta scalare: valore in w0 o sentinella (mappata qui in dominio).
fn obj_scalar(tag: u32, payload: &[u8]) -> Result<u64, Error> {
    let (a, _) = obj_request(tag, payload.len() as u64, 0, payload)?;
    fs_reply_check(a)
}

/// Snapshot del bucket → snap_id.
pub fn snap_create(bucket: &[u8]) -> Result<u64, Error> {
    let p = wire::bucket_only(bucket).ok_or(Error::Invalid)?;
    obj_scalar(R_SNAP_CREATE, &p)
}

/// Elimina uno snapshot (GC delle copie pinnate).
pub fn snap_delete(snap_id: u64) -> Result<(), Error> {
    obj_scalar(R_SNAP_DELETE, &snap_id.to_le_bytes()).map(|_| ())
}

/// Rollback per-chiave dallo snapshot → nuova head size.
pub fn snap_rollback(bucket: &[u8], key: &[u8], snap_id: u64) -> Result<u64, Error> {
    let mut p = Vec::with_capacity(8 + 1 + bucket.len() + 1 + 1 + key.len() + 1);
    p.extend_from_slice(&snap_id.to_le_bytes());
    p.extend_from_slice(&wire::obj_prefix(bucket, key).ok_or(Error::Invalid)?);
    obj_scalar(R_SNAP_ROLLBACK, &p)
}

/// Clona il bucket pinnato in `dst` (nuovi id) → oggetti clonati.
pub fn snap_clone(snap_id: u64, dst: &[u8]) -> Result<u64, Error> {
    let mut p = Vec::with_capacity(8 + 1 + dst.len());
    p.extend_from_slice(&snap_id.to_le_bytes());
    p.extend_from_slice(&wire::bucket_only(dst).ok_or(Error::Invalid)?);
    obj_scalar(R_SNAP_CLONE, &p)
}

/// GET per object_id (chunking automatico come `obj_get`).
pub fn obj_get_id(id: u64) -> Result<Vec<u8>, Error> {
    // Cancello ring (Fase 57): vedi `obj_get`.
    fs_gate()?;
    let mut offset = 0usize;
    let mut result = Vec::new();
    let payload = id.to_le_bytes();
    loop {
        let frame = || req_ring_write(R_OBJ_GET_ID, payload.len() as u64, offset as u64, &payload);
        if !frame() {
            return Err(Error::RingFull);
        }
        match fs_notify_result(FS_NOTIFY, frame) {
            Some((size, _, payload_len)) => {
                let total = match fs_reply_check(size) {
                    Ok(v) => v as usize,
                    Err(e) => {
                        resp_ring_consume(16);
                        return Err(e);
                    }
                };
                let remaining = total.saturating_sub(offset);
                if remaining == 0 {
                    resp_ring_consume(16);
                    break;
                }
                let to_read = remaining.min(payload_len).min(RING_MAX_PAYLOAD);
                if to_read == 0 {
                    resp_ring_consume(16);
                    break;
                }
                let mut buf = vec![0u8; to_read];
                resp_ring_read_payload(&mut buf, to_read);
                result.extend_from_slice(&buf);
                offset += to_read;
                if offset >= total {
                    break;
                }
            }
            None => return Err(Error::NotReady),
        }
    }
    Ok(result)
}

/// Stat per (bucket,key): (id, size head, versioni, mtime head).
pub fn obj_stat(bucket: &[u8], key: &[u8]) -> Result<(u64, u64, u64, u64), Error> {
    // Cancello ring (Fase 57): vedi `obj_get`.
    fs_gate()?;
    let prefix = wire::obj_prefix(bucket, key).ok_or(Error::Invalid)?;
    let frame = || req_ring_write(R_OBJ_STAT, prefix.len() as u64, 0, &prefix);
    if !frame() {
        return Err(Error::RingFull);
    }
    match fs_notify_result(FS_NOTIFY, frame) {
        Some((id, size, _)) => {
            // Prima i registri (a errore il frame e' vuoto: leggerlo
            // sarebbe oltre l'header, nel frame altrui — desync).
            let ok = fs_reply_check(id).and(fs_reply_check(size));
            match ok {
                Ok(_) => {
                    let mut f = [0u8; 16];
                    resp_ring_read_payload(&mut f, 16);
                    let nv = u64::from_le_bytes(f[..8].try_into().map_err(|_| Error::Invalid)?);
                    let mtime = u64::from_le_bytes(f[8..].try_into().map_err(|_| Error::Invalid)?);
                    // id/size gia' validati sopra (valori, non sentinelle).
                    Ok((id, size, nv, mtime))
                }
                Err(e) => {
                    resp_ring_consume(16);
                    Err(e)
                }
            }
        }
        None => Err(Error::NotReady),
    }
}

/// Stat per object_id: (size head, versioni, mtime head).
pub fn obj_stat_id(id: u64) -> Result<(u64, u64, u64), Error> {
    // Cancello ring (Fase 57): vedi `obj_get`.
    fs_gate()?;
    let payload = id.to_le_bytes();
    let frame = || req_ring_write(R_OBJ_STAT_ID, payload.len() as u64, 0, &payload);
    if !frame() {
        return Err(Error::RingFull);
    }
    match fs_notify_result(FS_NOTIFY, frame) {
        Some((size, nv, _)) => {
            let ok = fs_reply_check(size).and(fs_reply_check(nv));
            match ok {
                Ok(_) => {
                    let mut f = [0u8; 8];
                    resp_ring_read_payload(&mut f, 8);
                    let mtime = u64::from_le_bytes(f);
                    Ok((size, nv, mtime))
                }
                Err(e) => {
                    resp_ring_consume(16);
                    Err(e)
                }
            }
        }
        None => Err(Error::NotReady),
    }
}

/// Cancella nome + catena viva (gli snapshot restano validi).
pub fn obj_delete(bucket: &[u8], key: &[u8]) -> Result<(), Error> {
    let prefix = wire::obj_prefix(bucket, key).ok_or(Error::Invalid)?;
    obj_scalar(R_OBJ_DELETE, &prefix).map(|_| ())
}

// ── Debug volume on-disk (Fase 56.2a, scaffold) ─────────────────────
// UN tag + sub-op (`arcafs::proto::ARCA_SUB_*`): primo byte payload.
// Frame risposta SEMPRE consumato (disciplina anti-desync); a errore i
// registri portano sentinelle (via `fs_reply_check`). Il payload dedicato
// (READ/STAT) si legge solo dopo registri-ok (mai oltre l'header).

use arcafs::format::ARCA_NODE_PAYLOAD_LEN;
use arcafs::proto::{
    ARCA_SUB_ALLOC, ARCA_SUB_FREE, ARCA_SUB_OPEN, ARCA_SUB_QUOTA_GET,
    ARCA_SUB_QUOTA_SET, ARCA_SUB_READ, ARCA_SUB_STAT, ARCA_SUB_USEDISK,
    ARCA_SUB_WRITE, ARCA_SUB_XSTATS, R_ARCA_DEBUG,
};

/// Invia un sub-op debug: ritorna (w0, w1, len) della reply SENZA consumare
/// (il chiamante consuma dopo aver controllato i registri).
fn arca_request(sub: u8, payload: &[u8]) -> Result<(u64, u64, usize), Error> {
    // Cancello ring (Fase 57): copre tutti i debug `arca_*` (scaffold test).
    fs_gate()?;
    let mut p = Vec::with_capacity(1 + payload.len());
    p.push(sub);
    p.extend_from_slice(payload);
    let frame = || req_ring_write(R_ARCA_DEBUG, p.len() as u64, 0, &p);
    if !frame() {
        return Err(Error::RingFull);
    }
    match fs_notify_result(FS_NOTIFY, frame) {
        Some((a, b, len)) => Ok((a, b, len)),
        None => Err(Error::NotReady),
    }
}

/// Richiesta scalare: valore in w0 o sentinella (frame vuoto 16 B).
fn arca_scalar(sub: u8, payload: &[u8]) -> Result<u64, Error> {
    let (a, _, _) = arca_request(sub, payload)?;
    let v = fs_reply_check(a);
    resp_ring_consume(16);
    v
}

/// Lega il volume debug alla source (`/dev/sdc`). Fallisce loud se non
/// formattato.
pub fn arca_open(path: &str) -> Result<(), Error> {
    arca_scalar(ARCA_SUB_OPEN, path.as_bytes()).map(|_| ())
}

/// Alloca un blocco (mai 0).
pub fn arca_alloc() -> Result<u64, Error> {
    arca_scalar(ARCA_SUB_ALLOC, &[])
}

/// Libera un blocco (0/mai-allocato/doppio → errore).
pub fn arca_free(block: u64) -> Result<(), Error> {
    arca_scalar(ARCA_SUB_FREE, &block.to_le_bytes()).map(|_| ())
}

/// Legge i primi `RING_MAX_PAYLOAD` byte del payload di un nodo verificato
/// (debug: il payload intero da 64 KiB non entra in un frame ring; il resto
/// e' noto al server e coperto dal checksum, qui non viaggia).
pub fn arca_read_node(block: u64) -> Result<Vec<u8>, Error> {
    let mut req = block.to_le_bytes().to_vec();
    req.extend_from_slice(&(RING_MAX_PAYLOAD as u16).to_le_bytes());
    let (a, _, _) = arca_request(ARCA_SUB_READ, &req)?;
    let n = match fs_reply_check(a) {
        Ok(v) if v == block => v,
        Ok(_) => {
            resp_ring_consume(16);
            return Err(Error::Failed);
        }
        Err(e) => {
            resp_ring_consume(16);
            return Err(e);
        }
    };
    let _ = n;
    let mut buf = vec![0u8; RING_MAX_PAYLOAD];
    resp_ring_read_payload(&mut buf, RING_MAX_PAYLOAD);
    Ok(buf)
}

/// Scrive un nodo opaco (type RAW, gen 0 server-side). `data` e' il prefisso
/// del payload (il server azzera la coda): deve stare in un frame ring.
pub fn arca_write_node(block: u64, data: &[u8]) -> Result<(), Error> {
    if data.len() > RING_MAX_PAYLOAD - 9 {
        return Err(Error::Invalid); // 1 sub + 8 block + data <= frame
    }
    let mut p = Vec::with_capacity(8 + data.len());
    p.extend_from_slice(&block.to_le_bytes());
    p.extend_from_slice(data);
    arca_scalar(ARCA_SUB_WRITE, &p).map(|_| ())
}

/// Seleziona il backend degli op `R_OBJ_*`/`R_SNAP_*` lato server: disco
/// B+tree (`true`, richiede volume legato via `arca_open`) o store in-RAM
/// (`false`, default). Flag secco: niente merge tra backend.
pub fn arca_use_disk(on: bool) -> Result<(), Error> {
    arca_scalar(ARCA_SUB_USEDISK, &[on as u8]).map(|_| ())
}

/// Imposta il budget quota del bucket (0 = toglie il tetto). Solo motore
/// disco (pre-bind = errore loud server-side, A3, ADR-0044).
pub fn arca_quota_set(bucket: &[u8], quota: u64) -> Result<(), Error> {
    let mut p = quota.to_le_bytes().to_vec();
    p.extend_from_slice(&wire::bucket_only(bucket).ok_or(Error::Invalid)?);
    arca_scalar(ARCA_SUB_QUOTA_SET, &p).map(|_| ())
}

/// (quota_blocks, used_blocks misurata) del bucket. Quota 0 = illimitato.
pub fn arca_quota_get(bucket: &[u8]) -> Result<(u64, u64), Error> {
    let p = wire::bucket_only(bucket).ok_or(Error::Invalid)?;
    let (q, u, _) = arca_request(ARCA_SUB_QUOTA_GET, &p)?;
    match fs_reply_check(q).and(fs_reply_check(u)) {
        Ok(_) => {
            resp_ring_consume(16);
            Ok((q, u))
        }
        Err(e) => {
            resp_ring_consume(16);
            Err(e)
        }
    }
}

/// Istogramma layout valori ArcaFS + cache + packed (W2/W3/W5):
/// (oggetti extent, entry totali, max entry singolo, legacy OV, inline,
/// cache hit, cache reads, oggetti packed, blocchi packed, bytes packed,
/// capacita' blocchi packed).
pub fn arca_extent_stats() -> Result<(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64), Error> {
    let (ne, en, _) = arca_request(ARCA_SUB_XSTATS, &[])?;
    match fs_reply_check(ne).and(fs_reply_check(en)) {
        Ok(_) => {
            let mut f = [0u8; 72];
            resp_ring_read_payload(&mut f, 72);
            let mut u = [0u8; 8];
            let mut vals = [0u64; 9];
            for (i, v) in vals.iter_mut().enumerate() {
                u.copy_from_slice(&f[i * 8..(i + 1) * 8]);
                *v = u64::from_le_bytes(u);
            }
            Ok((ne, en, vals[0], vals[1], vals[2], vals[3], vals[4], vals[5], vals[6], vals[7], vals[8]))
        }
        Err(e) => {
            resp_ring_consume(16);
            Err(e)
        }
    }
}

/// (high_water, live, free_head) del volume.
pub fn arca_stat_vol() -> Result<(u64, u64, u64), Error> {
    let (high, live, _) = arca_request(ARCA_SUB_STAT, &[])?;
    let ok = fs_reply_check(high).and(fs_reply_check(live));
    match ok {
        Ok(_) => {
            let mut f = [0u8; 8];
            resp_ring_read_payload(&mut f, 8);
            Ok((high, live, u64::from_le_bytes(f)))
        }
        Err(e) => {
            resp_ring_consume(16);
            Err(e)
        }
    }
}
