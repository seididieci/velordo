//! Protocollo IPC del servizio Custos (ADR-0080, fase 5).
//!
//! UNA sola copia del framing, condivisa da: il servizio `custos`, il servizio
//! `net` (query per il bind) e la suite `usertests` (t69). Il framing e' un
//! dato di protocollo (magic + versione + campi), non una struttura kernel:
//! qui non finisce nulla dell'ABI delle syscall.
//!
//! # Framing (messaggi sincroni su canale IPC)
//!
//! ```text
//! header: tag=OP, w0=total_len, w1=0            (annuncia la dimensione)
//! chunk:  tag=OP, w0=offset,   w1=4 byte payload (little-endian, 1-4 byte)
//! EOF:    tag=OP, w0=0xFFFFFFFF, w1=0            (fine trasmissione)
//! reply:  tag=OP, w0=codice,   w1=esito/extra
//! ```
//!
//! `CUSTOS_STATUS` e' un frame singolo (nessun payload): `w0=0`, `w1=0`; la
//! reply porta il numero di grant in `w1`.
//!
//! # Capability
//!
//! Una capability e' `(kind, arg)`: `NET_BIND` con `arg` = porta,
//! `NET_USE`/`HW_MMIO`/`HW_DMA` con `arg` = 0 (dichiarative in fase 5).
//! Il soggetto e' sempre un `ImageId` (128 bit): la capability e' una
//! proprieta' del **contenuto** del binario, non del path o dell'utente.

#![no_std]

extern crate alloc;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use civis;
use civis::Error;

/// Magic del payload di query.
pub const MAGIC_QUERY: &[u8; 4] = b"ACQS";
/// Magic del payload di grant/revoca.
pub const MAGIC_GRANT: &[u8; 4] = b"ACQG";
/// Magic del payload di autorizzazione (AUTHZ): chiede se un'identita' e'
/// autorizzata per una capability, includendo il consenso per immagine.
pub const MAGIC_AUTHZ: &[u8; 4] = b"AAZS";
/// Magic del payload di consenso (ACNS): scrive/legge una riga di
/// `usr:<nome>:consent` per una immagine (identificata da ImageId).
pub const MAGIC_ACNS: &[u8; 4] = b"AACN";

/// Versione del protocollo (campo del payload).
pub const PROTO_VERSION: u32 = 1;

/// Marcatore di fine trasmissione (w0 del messaggio EOF).
pub const MSG_EOF: u64 = 0xFFFF_FFFF;

/// Chunk massimo per messaggio (byte utili in w1).
pub const CHUNK: usize = 4;

// ── Capability ─────────────────────────────────────────────────────────────

/// Bind su una porta di rete ben nota (`arg` = porta).
pub const CAP_NET_BIND: u64 = 1;
/// Uso generico della rete (dichiarativa in fase 5).
pub const CAP_NET_USE: u64 = 2;
/// Mappatura MMIO di un device (dichiarativa in fase 5).
pub const CAP_HW_MMIO: u64 = 3;
/// Buffer DMA device (dichiarativa in fase 5).
pub const CAP_HW_DMA: u64 = 4;

/// `true` se `kind` e' una capability conosciuta.
pub fn valid_kind(kind: u64) -> bool {
    matches!(kind, CAP_NET_BIND | CAP_NET_USE | CAP_HW_MMIO | CAP_HW_DMA)
}

/// Operazione sul consenso.
pub const CONSENT_READ: u64 = 0;
/// Scrive/aggiorna una riga di consenso (create-or-replace).
pub const CONSENT_WRITE: u64 = 1;

/// Costruttore interno: magic + versione + campi u64.
fn build_base(magic: &[u8; 4], fields: &[u64]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(magic);
    v.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    for &f in fields {
        push_u64(&mut v, f);
    }
    v
}

/// Payload di autorizzazione: magic + versione + identita' + capability.
/// Stessa forma di `build_query` ma con `MAGIC_AUTHZ`.
pub fn build_authz(id_lo: u64, id_hi: u64, kind: u64, arg: u64) -> Vec<u8> {
    build_base(MAGIC_AUTHZ, &[id_lo, id_hi, kind, arg])
}

/// Payload di consenso (scrittura): magic + versione + op=WRITE +
/// nome_len + nome + id_lo + id_hi + decision.
/// (S3.3c: il token di sessione non esiste piu'; l'identita' e' del
/// chiamante, risolta da Custos via `SESS_AUTH`.)
pub fn build_consent_write(
    user: &str,
    id_lo: u64,
    id_hi: u64,
    decision: bool,
) -> Vec<u8> {
    let name = user.as_bytes();
    let mut v = build_base(MAGIC_ACNS, &[
        CONSENT_WRITE as u64,
        name.len() as u64,
        id_lo,
        id_hi,
        decision as u64,
    ]);
    v.extend_from_slice(name);
    v
}

/// Payload di consenso (lettura): magic + versione + op=READ +
/// nome_len + nome + id_lo + id_hi.
pub fn build_consent_read(user: &str, id_lo: u64, id_hi: u64) -> Vec<u8> {
    let name = user.as_bytes();
    let mut v = build_base(
        MAGIC_ACNS,
        &[CONSENT_READ as u64, name.len() as u64, id_lo, id_hi],
    );
    v.extend_from_slice(name);
    v
}

/// `(id_lo, id_hi, kind, arg)` per AUTHZ.
pub fn parse_authz(buf: &[u8]) -> Result<(u64, u64, u64, u64), &'static str> {
    if buf.len() < 8 || &buf[0..4] != MAGIC_AUTHZ {
        return Err("magic non riconosciuto");
    }
    let lo = u64_at(buf, 8)?;
    let hi = u64_at(buf, 16)?;
    let kind = u64_at(buf, 24)?;
    let arg = u64_at(buf, 32)?;
    if !valid_kind(kind) {
        return Err("capability sconosciuta");
    }
    Ok((lo, hi, kind, arg))
}

/// `(op, nome, id_lo, id_hi, decision?)` per ACNS.
/// `decision` e' `Some` solo per `CONSENT_WRITE`.
///
/// Layout (i campi stanno tutti **prima** del nome, come li scrivono i
/// costruttori qui sopra: il nome e' l'ultimo pezzo, di lunghezza `name_len`):
///
/// ```text
/// offset:  0        4     8     16      24      32      40
/// WRITE: [ACNS][ver:4][op:8][name_len:8][id_lo:8][id_hi:8][dec:8][nome]
/// READ:  [ACNS][ver:4][op:8][name_len:8][id_lo:8][id_hi:8][nome]
/// ```
pub fn parse_consent(
    buf: &[u8],
) -> Result<(u64, String, u64, u64, Option<bool>), &'static str> {
    if buf.len() < 16 || &buf[0..4] != MAGIC_ACNS {
        return Err("magic non riconosciuto");
    }
    let op = u64_at(buf, 8)?;
    let (name_len, id_lo, id_hi, decision, name_off) = match op {
        CONSENT_WRITE => (
            u64_at(buf, 16)? as usize,
            u64_at(buf, 24)?,
            u64_at(buf, 32)?,
            Some(u64_at(buf, 40)?),
            8 + 5 * 8,
        ),
        CONSENT_READ => (
            u64_at(buf, 16)? as usize,
            u64_at(buf, 24)?,
            u64_at(buf, 32)?,
            None,
            8 + 4 * 8,
        ),
        _ => return Err("operazione consenso non valida"),
    };
    if buf.len() < name_off + name_len {
        return Err("payload troncato");
    }
    let name = core::str::from_utf8(&buf[name_off..name_off + name_len])
        .map_err(|_| "nome non UTF-8")?
        .to_string();
    if let Some(d) = decision {
        if d > 1 {
            return Err("decisione non valida");
        }
        return Ok((op, name, id_lo, id_hi, Some(d == 1)));
    }
    Ok((op, name, id_lo, id_hi, None))
}

// ── Codici di risposta (w0) ────────────────────────────────────────────────

/// Operazione riuscita.
pub const R_OK: u64 = 0;
/// Nessuna capability (query) / decisione negata.
pub const R_FAIL: u64 = 1;
/// Chiamante senza identita' attestata (nessuna sessione autenticata).
pub const R_NO_IDENTITY: u64 = 2;
/// Errore FS (persistenza di `sys:caps`).
pub const R_FS_ERROR: u64 = 5;
/// Sessione valida ma senza l'autorita' richiesta.
pub const R_UNAUTHORIZED: u64 = 6;
/// Transazione in corso: il client deve ritentare.
pub const R_BUSY: u64 = 7;
/// Richiesta malformata (protocollo).
pub const R_PROTOCOL: u64 = 8;

/// Risposta finale del server (w0 = codice, w1 = esito).
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

    /// Descrizione leggibile del codice (per log e console).
    pub fn describe(&self) -> &'static str {
        match self.w0 {
            R_OK => "OK",
            R_FAIL => "capability negata",
            R_NO_IDENTITY => "chiamante senza identita' attestata",
            R_FS_ERROR => "errore filesystem",
            R_UNAUTHORIZED => "non autorizzato",
            R_BUSY => "servizio occupato",
            _ => "errore di protocollo",
        }
    }
}

// ── Costruzione dei payload ────────────────────────────────────────────────

fn push_u64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// Payload di query: magic + versione + identita' + capability.
pub fn build_query(id_lo: u64, id_hi: u64, kind: u64, arg: u64) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC_QUERY);
    v.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    push_u64(&mut v, id_lo);
    push_u64(&mut v, id_hi);
    push_u64(&mut v, kind);
    push_u64(&mut v, arg);
    v
}

/// Payload di grant: magic + versione + identita' + capability +
/// decisione (`1` concede, `0` revoca).
/// (S3.3c: il token di sessione non esiste piu'; l'identita' e' del
/// chiamante, attestata dal kernel via `SESS_AUTH` / `IDENTIS_WHOAMI` con pid.)
pub fn build_grant(
    id_lo: u64,
    id_hi: u64,
    kind: u64,
    arg: u64,
    decision: bool,
) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC_GRANT);
    v.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    push_u64(&mut v, id_lo);
    push_u64(&mut v, id_hi);
    push_u64(&mut v, kind);
    push_u64(&mut v, arg);
    push_u64(&mut v, decision as u64);
    v
}

// ── Parsing (lato server) ──────────────────────────────────────────────────

fn u64_at(buf: &[u8], off: usize) -> Result<u64, &'static str> {
    if off + 8 > buf.len() {
        return Err("payload troncato");
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    Ok(u64::from_le_bytes(b))
}

/// `(id_lo, id_hi, kind, arg)`.
pub fn parse_query(buf: &[u8]) -> Result<(u64, u64, u64, u64), &'static str> {
    if buf.len() < 8 || &buf[0..4] != MAGIC_QUERY {
        return Err("magic non riconosciuto");
    }
    let lo = u64_at(buf, 8)?;
    let hi = u64_at(buf, 16)?;
    let kind = u64_at(buf, 24)?;
    let arg = u64_at(buf, 32)?;
    if !valid_kind(kind) {
        return Err("capability sconosciuta");
    }
    Ok((lo, hi, kind, arg))
}

/// `(id_lo, id_hi, kind, arg, decision)`.
pub fn parse_grant(buf: &[u8]) -> Result<(u64, u64, u64, u64, bool), &'static str> {
    if buf.len() < 16 || &buf[0..4] != MAGIC_GRANT {
        return Err("magic non riconosciuto");
    }
    let lo = u64_at(buf, 8)?;
    let hi = u64_at(buf, 16)?;
    let kind = u64_at(buf, 24)?;
    let arg = u64_at(buf, 32)?;
    let decision = u64_at(buf, 40)?;
    if !valid_kind(kind) {
        return Err("capability sconosciuta");
    }
    if decision > 1 {
        return Err("decisione non valida");
    }
    Ok((lo, hi, kind, arg, decision == 1))
}

// ── Lato client: invio chunked + retry su BUSY ────────────────────────────

/// Invia `payload` a `chan` col framing chunked e attende la reply finale.
pub fn send_payload(chan: u64, tag: u64, payload: &[u8]) -> Result<Reply, Error> {
    let r = civis::ipc::send(chan, tag, payload.len() as u64, 0)?;
    if r.w0 != R_OK {
        return Ok(Reply { w0: r.w0, w1: r.w1 });
    }
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
    let r = civis::ipc::send(chan, tag, MSG_EOF, 0)?;
    Ok(Reply { w0: r.w0, w1: r.w1 })
}

/// Come `send_payload`, ma ritenta la transazione intera su `R_BUSY`.
pub fn send_payload_retry(
    chan: u64,
    tag: u64,
    payload: &[u8],
    max_retry: u32,
) -> Result<Reply, Error> {
    let mut last = None;
    for _ in 0..max_retry {
        match send_payload(chan, tag, payload) {
            Ok(r) if r.w0 == R_BUSY => {
                last = Some(Ok(r));
                for _ in 0..1000 {
                    core::hint::spin_loop();
                }
            }
            other => return other,
        }
    }
    last.unwrap_or(Err(Error::ServerDied))
}

// ── Lato client: query comoda ─────────────────────────────────────────────

/// `true` se `(kind, arg)` e' concesso all'identita' `(lo, hi)`.
///
/// Fail-closed: errore IPC o risposta non-OK = negato. Il chiamante puo'
/// mettere in cache il risultato (il servizio `net` lo fa per identita'+porta).
pub fn query(
    chan: u64,
    id_lo: u64,
    id_hi: u64,
    kind: u64,
    arg: u64,
) -> bool {
    let payload = build_query(id_lo, id_hi, kind, arg);
    match send_payload(chan, civis::CUSTOS_QUERY, &payload) {
        Ok(r) => r.w0 == R_OK && r.w1 == 1,
        Err(_) => false,
    }
}

/// `true` se l'identita' `(lo, hi)` e' autorizzata per `(kind, arg)`,
/// tenendo conto del consenso per immagine (ADR-0080, fase 6).
///
/// Come `query` ma sul tag `CUSTOS_AUTHORIZE`: il server combina policy
/// admin + consenso prima di rispondere.
pub fn authorize(
    chan: u64,
    id_lo: u64,
    id_hi: u64,
    kind: u64,
    arg: u64,
) -> bool {
    let payload = build_authz(id_lo, id_hi, kind, arg);
    match send_payload(chan, civis::CUSTOS_AUTHORIZE, &payload) {
        Ok(r) => r.w0 == R_OK && r.w1 == 1,
        Err(_) => false,
    }
}

/// Scrive una riga di consenso per `(user, id_lo, id_hi)`: `decision`
/// `true` = consente, `false` = nega. L'identita' e' quella del **chiamante**
/// (attestata dal kernel): Custos la risolve per pid, nessun token viaggia.
/// Ritorna `true` se Custos ha accettato la scrittura (persistenza prima
/// della RAM).
pub fn consent_write(
    chan: u64,
    user: &str,
    id_lo: u64,
    id_hi: u64,
    decision: bool,
) -> bool {
    let payload = build_consent_write(user, id_lo, id_hi, decision);
    match send_payload(chan, civis::CUSTOS_CONSENT, &payload) {
        Ok(r) => r.w0 == R_OK,
        Err(_) => false,
    }
}

/// Legge il consenso per `(user, id_lo, id_hi)`. Ritorna `Some(true)` se
/// c'e' una riga e consente, `Some(false)` se c'e' e nega, `None` se
/// assente o errore.
pub fn consent_read(
    chan: u64,
    user: &str,
    id_lo: u64,
    id_hi: u64,
) -> Option<bool> {
    let payload = build_consent_read(user, id_lo, id_hi);
    match send_payload(chan, civis::CUSTOS_CONSENT, &payload) {
        Ok(r) if r.w0 == R_OK => Some(r.w1 == 1),
        _ => None,
    }
}
