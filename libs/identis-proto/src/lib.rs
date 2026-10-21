//! Protocollo IPC del servizio Identis (ADR-0055, Fase 3.1).
//!
//! UNA sola copia del framing, condivisa da: il servizio `identis`, il client
//! `identis-login`, il client `create-user` e il test `identistest`. Il
//! framing e' un dato di protocollo (magic + versione + campi), non una
//! struttura kernel: niente qui finisce nell'ABI delle syscall.
//!
//! # Framing (messaggi sincroni su canale IPC)
//!
//! ```text
//! header: tag=OP, w0=total_len, w1=0            (annuncia la dimensione)
//! chunk:  tag=OP, w0=offset,   w1=4 byte payload (little-endian, 1-4 byte)
//! EOF:    tag=OP, w0=0xFFFFFFFF, w1=0            (fine trasmissione)
//! reply:  tag=OP, w0=codice,   w1=token (solo VERIFY ok: w1 = sessione)
//! ```
//!
//! Ogni chunk e' un round trip (il protocollo e' sincrono: `send` attende la
//! reply). Il tag identifica l'operazione: `IDENTIS_VERIFY`, `IDENTIS_ADD_USER`
//! (chunked) e `IDENTIS_LIST` (una sola richiesta, risposta in w1).
//!
//! # Codici di risposta (w0)
//!
//! `R_OK` ok · `R_FAIL` credenziali errate (VERIFY) · `R_TOKEN_INVALID`
//! token assente/sconosciuto/scaduto · `R_NAME_INVALID` nome utente non
//! conforme · `R_EXISTS` utente gia' presente · `R_FS_ERROR` errore FS
//! (bucket) · `R_UNAUTHORIZED` sessione valida ma non root · `R_BUSY`
//! transazione in corso (un altro client) · `R_PROTOCOL` richiesta malformata.

#![no_std]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use civis;
use civis::Error;

/// Magic del payload di verifica credenziali.
pub const MAGIC_VERIFY: &[u8; 4] = b"ACLI";
/// Magic del payload di creazione utente.
pub const MAGIC_ADD_USER: &[u8; 4] = b"ACUS";
/// Magic del payload di elenco utenti.
pub const MAGIC_LIST: &[u8; 4] = b"ACLS";

/// Versione del protocollo (campo del payload).
pub const PROTO_VERSION: u32 = 1;

/// Marcatore di fine trasmissione (w0 del messaggio EOF).
pub const MSG_EOF: u64 = 0xFFFF_FFFF;

/// Lunghezza dell'hash BLAKE2s-256.
pub const HASH_LEN: usize = 32;

/// Lunghezza massima del nome utente.
pub const MAX_NAME: usize = 32;

/// Chunk massimo per messaggio (byte utili in w1).
pub const CHUNK: usize = 4;

// ── Codici di risposta ─────────────────────────────────────────────────────

/// Operazione riuscita.
pub const R_OK: u64 = 0;
/// Credenziali non valide (VERIFY).
pub const R_FAIL: u64 = 1;
/// Token di sessione assente, sconosciuto o scaduto.
pub const R_TOKEN_INVALID: u64 = 2;
/// Nome utente non conforme (lunghezza o caratteri).
pub const R_NAME_INVALID: u64 = 3;
/// Utente gia' presente.
pub const R_EXISTS: u64 = 4;
/// Errore FS (lettura/scrittura del bucket).
pub const R_FS_ERROR: u64 = 5;
/// Sessione valida ma priva del privilegio (non root).
pub const R_UNAUTHORIZED: u64 = 6;
/// Transazione in corso: il client deve ritentare.
pub const R_BUSY: u64 = 7;
/// Richiesta malformata (protocollo).
pub const R_PROTOCOL: u64 = 8;

/// Risposta finale del server (w0 = codice, w1 = token per VERIFY ok).
#[derive(Clone, Copy, Debug)]
pub struct Reply {
    pub w0: u64,
    pub w1: u64,
}

impl Reply {
    /// true se la richiesta e' stata soddisfatta.
    pub fn ok(&self) -> bool {
        self.w0 == R_OK
    }

    /// Descrizione leggibile del codice (per il log e la console).
    pub fn describe(&self) -> &'static str {
        match self.w0 {
            R_OK => "OK",
            R_FAIL => "credenziali non valide",
            R_TOKEN_INVALID => "token di sessione non valido",
            R_NAME_INVALID => "nome utente non valido",
            R_EXISTS => "utente gia' esistente",
            R_FS_ERROR => "errore filesystem",
            R_UNAUTHORIZED => "non autorizzato (serve una sessione root)",
            R_BUSY => "servizio occupato",
            _ => "errore di protocollo",
        }
    }
}

// ── Costruzione dei payload ───────────────────────────────────────────────

/// Payload di verifica credenziali: magic + versione + nome + hash.
pub fn build_verify(name: &str, hash: &[u8; HASH_LEN]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC_VERIFY);
    v.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    v.extend_from_slice(&(name.len() as u32).to_le_bytes());
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&(hash.len() as u32).to_le_bytes());
    v.extend_from_slice(hash);
    v
}

/// Payload di creazione utente: magic + versione + token + nome + hash.
pub fn build_add_user(token: u64, name: &str, hash: &[u8; HASH_LEN]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC_ADD_USER);
    v.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    v.extend_from_slice(&token.to_le_bytes());
    v.extend_from_slice(&(name.len() as u32).to_le_bytes());
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&(hash.len() as u32).to_le_bytes());
    v.extend_from_slice(hash);
    v
}

// ── Parsing dei payload (lato server) ─────────────────────────────────────

fn u32_at(buf: &[u8], off: usize) -> Result<u32, &'static str> {
    if off + 4 > buf.len() {
        return Err("payload troncato");
    }
    Ok(u32::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
    ]))
}

fn u64_at(buf: &[u8], off: usize) -> Result<u64, &'static str> {
    if off + 8 > buf.len() {
        return Err("payload troncato");
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    Ok(u64::from_le_bytes(b))
}

/// Estrae nome utente + hash da un payload di verifica.
pub fn parse_verify(buf: &[u8]) -> Result<(String, [u8; HASH_LEN]), &'static str> {
    if buf.len() < 8 || &buf[0..4] != MAGIC_VERIFY {
        return Err("magic non riconosciuto");
    }
    let mut off = 8usize;
    let name_len = u32_at(buf, off)? as usize;
    off += 4;
    if off + name_len > buf.len() {
        return Err("nome troncato");
    }
    let name = String::from_utf8_lossy(&buf[off..off + name_len]).into_owned();
    off += name_len;
    let hash_len = u32_at(buf, off)? as usize;
    off += 4;
    if hash_len != HASH_LEN || off + HASH_LEN > buf.len() {
        return Err("hash non conforme");
    }
    let mut hash = [0u8; HASH_LEN];
    hash.copy_from_slice(&buf[off..off + HASH_LEN]);
    Ok((name, hash))
}

/// Estrae token + nome + hash da un payload di creazione utente.
pub fn parse_add_user(buf: &[u8]) -> Result<(u64, String, [u8; HASH_LEN]), &'static str> {
    if buf.len() < 16 || &buf[0..4] != MAGIC_ADD_USER {
        return Err("magic non riconosciuto");
    }
    let token = u64_at(buf, 8)?;
    let mut off = 16usize;
    let name_len = u32_at(buf, off)? as usize;
    off += 4;
    if off + name_len > buf.len() {
        return Err("nome troncato");
    }
    let name = String::from_utf8_lossy(&buf[off..off + name_len]).into_owned();
    off += name_len;
    let hash_len = u32_at(buf, off)? as usize;
    off += 4;
    if hash_len != HASH_LEN || off + HASH_LEN > buf.len() {
        return Err("hash non conforme");
    }
    let mut hash = [0u8; HASH_LEN];
    hash.copy_from_slice(&buf[off..off + HASH_LEN]);
    Ok((token, name, hash))
}

// ── Validazione del nome utente (server-side, autorevole) ─────────────────

/// Un nome utente e' valido se non e' vuoto, non supera `MAX_NAME` byte e
/// contiene solo `[a-z0-9_-]`. La policy e' **server-side**: il client puo'
/// sbagliare, il server no.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

// ── Lato client: invio chunked + retry su BUSY ────────────────────────────

/// Invia `payload` a `chan` col framing chunked e attende la reply finale.
/// Restituisce il codice `w0` e il `w1` (token, per VERIFY ok). Un codice
/// diverso da `R_OK` **prima** dell'EOF significa "richiesta non accettata"
/// (BUSY o protocollo): in quel caso la transazione e' stata abbandonata e
/// va ritentata per intero.
pub fn send_payload(chan: u64, tag: u64, payload: &[u8]) -> Result<Reply, Error> {
    // Header: dimensione totale.
    let r = civis::ipc::send(chan, tag, payload.len() as u64, 0)?;
    if r.w0 != R_OK {
        return Ok(Reply { w0: r.w0, w1: r.w1 });
    }
    // Chunk da 4 byte in w1 (l'ultimo puo' essere piu' corto).
    let mut off = 0usize;
    while off < payload.len() {
        let n = (payload.len() - off).min(CHUNK);
        let mut packed: u32 = 0;
        for i in 0..n {
            packed |= (payload[off + i] as u32) << (8 * i);
        }
        let r = civis::ipc::send(chan, tag, off as u64, packed as u64)?;
        if r.w0 != R_OK {
            return Ok(Reply { w0: r.w0, w1: r.w1 });
        }
        off += n;
    }
    // EOF: la reply finale porta l'esito.
    let r = civis::ipc::send(chan, tag, MSG_EOF, 0)?;
    Ok(Reply { w0: r.w0, w1: r.w1 })
}

/// Come `send_payload`, ma ritenta la transazione intera quando il servizio
/// risponde `R_BUSY` (un altro client sta trasmettendo). Lo spin di attesa
/// e' puro (nessun busy-loop su syscall: mascherebbe gli interrupt e
/// affamerebbe il timer).
pub fn send_payload_retry(
    chan: u64,
    tag: u64,
    payload: &[u8],
    max_retry: u32,
) -> Result<Reply, Error> {
    let mut attempt = 0u32;
    loop {
        let r = send_payload(chan, tag, payload)?;
        if r.w0 != R_BUSY || attempt >= max_retry {
            return Ok(r);
        }
        attempt += 1;
        backoff(attempt);
    }
}

/// Attesa pura (spin) fra due tentativi: cresce col numero di tentativo.
fn backoff(attempt: u32) {
    let spins = 2_000u32 * attempt.min(50);
    for _ in 0..spins {
        core::hint::spin_loop();
    }
}
