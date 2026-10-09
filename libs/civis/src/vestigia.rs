//! Client del servizio `Log` (Fase 57, L1 nativo, ADR-0039).
//!
//! `vestigia` e' il gateway centrale di logging: parte per primo dopo init,
//! assorbe tutto in RAM e riversa sul bucket `log` nativo alla `LOG_FLUSH`
//! (dual-write dopo). Il kernel non e' nel percorso; lo storage-TCB non
//! chiama mai qui (anti-ciclo).
//!
//! Meccanismo neutro e NON posix (mai POSIX, mai `open/read/write/mount`:
//! solo `R_OBJ_*`/`R_SNAP_*` via questo protocollo + `TIME_NOW` per il giorno).
//! Il client condivide gli anelli col FS in SEQUENZA (entrambi sync, mai
//! interleave: una sola coppia per processo — il kernel mappa ogni coppia
//! sulle stesse VA). `log()` funziona pre-FS e senza cardo (solo allocazione,
//! mai handshake FS nel suo percorso); rifiuta su async-FS in volo (Pending)
//! invece di corrompere il ring condiviso.
//!
//! Attribuzione (ADR-0039): il bucket e' DERIVATO dal server dall'identita'
//! del chiamante (`peer_info` al REG), mai dichiarato. Il `tag` e' solo un
//! hint leggibile nel record (zero semantica); chiavi `<hash>/<giorno>/<seq>`.

use super::*;
use super::fs::ring;
use super::fs::session::{fs_light_gate, ring_phys};
use alloc::vec::Vec;

/// Livelli di log (convenzione al bordo, mai kernel/wire numerato POSIX).
pub const LOG_INFO: u8 = 0;
pub const LOG_WARN: u8 = 1;
pub const LOG_ERR: u8 = 2;

/// Bucket oggetto dei log (≤16 B, bound `OBJ_BUCKET_MAX`).
pub const LOG_BUCKET: &[u8] = b"log";

// ── Formato record (single source client+server) ─────────────────────
// `[tick:8][epoch:8][level:1][taglen:1][tag][msg]`. Tick ed epoch li mette il
// server (monotono PIT + `Time`, retro-datati alla FLUSH per i pre-Time);
// il client invia `[level:1][taglen:1][tag][msg]`.

/// Codifica un record completo (server-side, dopo la timbratura).
pub fn record_encode(tick: u64, epoch: u64, level: u8, tag: &[u8], msg: &[u8]) -> Option<Vec<u8>> {
    if !tag_valid(tag) || !msg_valid(msg) || level > LOG_ERR {
        return None;
    }
    let mut r = Vec::with_capacity(18 + tag.len() + msg.len());
    r.extend_from_slice(&tick.to_le_bytes());
    r.extend_from_slice(&epoch.to_le_bytes());
    r.push(level);
    r.push(tag.len() as u8);
    r.extend_from_slice(tag);
    r.extend_from_slice(msg);
    Some(r)
}

/// Decodifica un record: `(tick, epoch, level, tag, msg)`. Niente panico su
/// input corto o troncato (mai fidarsi dei byte letti dal volume).
pub fn record_decode(rec: &[u8]) -> Option<(u64, u64, u8, &[u8], &[u8])> {
    if rec.len() < 18 {
        return None;
    }
    let tick = u64::from_le_bytes(rec[0..8].try_into().ok()?);
    let epoch = u64::from_le_bytes(rec[8..16].try_into().ok()?);
    let level = rec[16];
    let taglen = rec[17] as usize;
    if level > LOG_ERR || taglen == 0 || taglen > LOG_SRC_MAX || rec.len() < 18 + taglen {
        return None;
    }
    let tag = &rec[18..18 + taglen];
    let msg = &rec[18 + taglen..];
    if tag.contains(&b'/') || tag.contains(&0) || msg.is_empty() || msg.len() > LOG_MSG_MAX {
        return None;
    }
    Some((tick, epoch, level, tag, msg))
}

/// Tag cosmetico valido: 1..=32 B, niente `/` ne' NUL (resta un segmento
/// di chiave se mai riusato come tale; zero semantica di bucket).
#[inline]
pub fn tag_valid(tag: &[u8]) -> bool {
    !tag.is_empty() && tag.len() <= LOG_SRC_MAX && !tag.contains(&b'/') && !tag.contains(&0)
}

/// Messaggio valido: 1..=1024 B (il record resta in un frame).
#[inline]
pub fn msg_valid(msg: &[u8]) -> bool {
    !msg.is_empty() && msg.len() <= LOG_MSG_MAX
}

/// Giorno epoch per la chiave: `wall_secs/86400`, 0 senza `Time` (solo
/// pre-Time/transitorio: alla FLUSH le chiavi giorno-0 non raggiungono mai
/// il disco). Stessa funzione da entrambi i lati.
pub fn log_day() -> u64 {
    time::wall_secs().unwrap_or(0) / 86_400
}

/// Chiave oggetto `<hash16hex>/<giorno8hex>/<seq16hex>` dal `peer_info` del
/// chiamante (FNV-1a dell'ELF). Seq 0 = riservato (nelle READ = "latest").
///
/// Vecchia alias `src_valid` tenuto per compatibilita' dei test: il tag
/// cosmetico riusa gli stessi bound del segmento di chiave.
#[inline]
pub fn src_valid(tag: &[u8]) -> bool {
    tag_valid(tag)
}

/// Chiave oggetto dal bucket-identita' del chiamante.
pub fn log_key(hash: u64, day: u64, seq: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(16 + 1 + 8 + 1 + 16);
    for i in (0..16).rev() {
        k.push(b"0123456789abcdef"[((hash >> (i * 4)) & 0xF) as usize]);
    }
    k.push(b'/');
    for i in (0..8).rev() {
        k.push(b"0123456789abcdef"[((day >> (i * 4)) & 0xF) as usize]);
    }
    k.push(b'/');
    for i in (0..16).rev() {
        k.push(b"0123456789abcdef"[((seq >> (i * 4)) & 0xF) as usize]);
    }
    k
}

/// Chiave dell'indice per-bucket-giorno `<hash16hex>/<giorno8hex>/!idx`
/// (max seq persistito: ricostruzione latest dopo un restart del server).
pub fn log_idx_key(hash: u64, day: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(16 + 1 + 8 + 1 + 4);
    for i in (0..16).rev() {
        k.push(b"0123456789abcdef"[((hash >> (i * 4)) & 0xF) as usize]);
    }
    k.push(b'/');
    for i in (0..8).rev() {
        k.push(b"0123456789abcdef"[((day >> (i * 4)) & 0xF) as usize]);
    }
    k.extend_from_slice(b"/!idx");
    k
}

// ── Client ───────────────────────────────────────────────────────────
// Anelli CONDIVISI col FS in sequenza (mai due coppie: il kernel mappa ogni
// coppia sulle STESSE VA — osservato il cross-talk). `fs_light_gate`
// garantisce anelli allocati (senza handshake FS: il log funziona pre-FS),
// niente fork-aliasing e niente interleave con async-FS in volo. I fisici
// sono quelli di sessione (persistono anche ai restart: le pagine non si
// riallocano mai, la REG di vestigia resta valida). Canale cachato verso
// `Log`: un canale per processo, re-lookup al primo uso e dopo la morte del
// server. Mai unbounded: senza `Log` registrato si ritorna `NotReady`.

static LOG_CHAN: AtomicI64 = AtomicI64::new(-1);
static LOG_REGED: AtomicI64 = AtomicI64::new(-1);

/// Bound re-lookup runtime (~200 tick, come `FS_RELOOKUP_TICKS`): a runtime
/// un restart rotto deve dare `NotReady` rumoroso, non hang.
const LOG_RELOOKUP_TICKS: i64 = 200;

/// Risolve il canale verso `Log` con attesa bounded.
fn log_chan_rt() -> i64 {
    let c = LOG_CHAN.load(Ordering::Relaxed);
    if c >= 0 {
        return c;
    }
    let t0 = sys::get_ticks();
    loop {
        if let Ok(chan) = spawn::service_lookup(Service::Vestigia) {
            LOG_CHAN.store(chan, Ordering::Relaxed);
            return chan;
        }
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
        if sys::get_ticks() - t0 > LOG_RELOOKUP_TICKS {
            return -1;
        }
    }
}

/// Invalida canale + registrazione (morte del server osservata).
fn log_invalidate() {
    LOG_CHAN.store(-1, Ordering::Relaxed);
    LOG_REGED.store(-1, Ordering::Relaxed);
}

/// Handshake LOG_REG (register-only): "i miei ring LOG sono req=w0, resp=w1".
/// Il server archivia (chan→phys) + identita' per l'attribuzione del bucket;
/// la reply porta `(hash_bucket, 0)` (diagnostica: il client conosce il
/// proprio bucket senza poterlo scegliere).
fn log_reg(chan: u64) -> Result<u64, Error> {
    if LOG_REGED.load(Ordering::Relaxed) == chan as i64 {
        return Ok(0);
    }
    // Cancello leggero PRIMA di qualunque uso ring (alloca se serve, mai
    // handshake FS). Senza, scriveremmo su VA non mappate (#PF) o in un
    // async-FS in volo (corruzione del ring condiviso).
    fs_light_gate()?;
    let (req_phys, resp_phys) = ring_phys();
    match ipc::send(chan, LOG_REG, req_phys, resp_phys) {
        Ok(rep) => {
            LOG_REGED.store(chan as i64, Ordering::Relaxed);
            Ok(rep.w0)
        }
        Err(e) => {
            log_invalidate();
            Err(e.into())
        }
    }
}

/// Append di una riga: ritorna il `seq` assegnato dal server per il bucket
/// del chiamante. Su `NOHANDSHAKE` rifa REG + 1 resend (frame intatto, mai
/// riscritto: stampo FS); sul canale morto re-lookup + 1 retry; poi
/// `NotReady`. Mai loop infiniti. Sempre sync-su-RAM: il disco non e' mai
/// nel percorso del chiamante.
pub fn log_append(level: u8, tag: &[u8], msg: &[u8]) -> Result<u64, Error> {
    if level > LOG_ERR || !tag_valid(tag) || !msg_valid(msg) {
        return Err(Error::Invalid);
    }
    let mut frame = Vec::with_capacity(2 + tag.len() + msg.len());
    frame.push(level);
    frame.push(tag.len() as u8);
    frame.extend_from_slice(tag);
    frame.extend_from_slice(msg);

    // Write-once DOPO la REG (che alloca gli anelli propri): sul NOHANDSHAKE
    // il server non ha mai toccato i ring (il frame e' intatto, basta re-REG
    // + resend); riscriverlo duplicherebbe il frame (desync). Mai scrivere
    // prima della REG: gli anelli non esisterebbero ancora (#PF).
    let w0 = frame.len() as u64;
    let write = || ring::req_ring_write(LOG_APPEND as u32, w0, 0, &frame);
    let mut written = false;
    let mut retried = false;
    loop {
        let chan = log_chan_rt();
        if chan < 0 {
            return Err(Error::NotReady);
        }
        let chan = chan as u64;
        if log_reg(chan).is_err() {
            return Err(Error::NotReady);
        }
        if !written {
            if !write() {
                return Err(Error::RingFull);
            }
            written = true;
        }
        match ipc::send(chan, LOG_APPEND, w0, 0) {
            Ok(rep) => {
                if rep.w0 == ring::ERR_NOHANDSHAKE && !retried {
                    LOG_REGED.store(-1, Ordering::Relaxed);
                    retried = true;
                    continue;
                }
                if rep.w0 == ring::ERR {
                    return Err(Error::Failed);
                }
                if rep.w0 == ring::ERR_NOHANDSHAKE {
                    return Err(Error::NotReady);
                }
                return Ok(rep.w0);
            }
            Err(_) => {
                if !retried {
                    log_invalidate();
                    retried = true;
                    continue;
                }
                return Err(Error::NotReady);
            }
        }
    }
}

/// Scorciatoia per il caso comune (append INFO).
#[inline]
pub fn log(tag: &[u8], msg: &[u8]) -> Result<u64, Error> {
    log_append(LOG_INFO, tag, msg)
}

/// Legge un record del PROPRIO bucket per `(giorno, seq)`; `seq=0` → latest.
/// Ritorna `(seq_servito, record_decodificato)`.
pub fn log_read(day: u64, seq: u64) -> Result<(u64, Vec<u8>), Error> {
    let mut payload = [0u8; 16];
    payload[0..8].copy_from_slice(&day.to_le_bytes());
    payload[8..16].copy_from_slice(&seq.to_le_bytes());

    let chan = log_chan_rt();
    if chan < 0 {
        return Err(Error::NotReady);
    }
    let chan = chan as u64;
    log_reg(chan).map_err(|_| Error::NotReady)?;
    // Write DOPO la REG (gli anelli propri nascono li': scrivere prima e'
    // #PF su VA non mappate).
    let w0 = payload.len() as u64;
    if !ring::req_ring_write(LOG_READ as u32, w0, 0, &payload) {
        return Err(Error::RingFull);
    }
    let rep = ipc::send(chan, LOG_READ, w0, 0).map_err(|_| Error::NotReady)?;
    if rep.w0 == ring::ERR || rep.w0 == ring::ERR_NOHANDSHAKE {
        return Err(Error::NotReady);
    }
    if rep.w0 == ERR_NOTFOUND {
        return Err(Error::NotFound);
    }
    let total = rep.w0 as usize;
    if total == 0 || total > ring::RING_MAX_PAYLOAD {
        ring::resp_ring_consume(16);
        return Err(Error::Invalid);
    }
    let avail = match ring::resp_ring_read() {
        Some((_, _, n)) => n,
        None => return Err(Error::NotReady),
    };
    let to_read = total.min(avail).min(ring::RING_MAX_PAYLOAD);
    let mut buf = alloc::vec![0u8; to_read];
    ring::resp_ring_read_payload(&mut buf, to_read);
    if record_decode(&buf).is_none() {
        return Err(Error::Invalid);
    }
    Ok((rep.w1, buf))
}

/// Chiede al server il riversamento su volume (solo init, dopo fs+time:
/// gli altri ricevono `Denied`). Il riversamento e' incrementale lato
/// server; la reply (0,0) conferma solo l'accettazione.
pub fn log_flush() -> Result<(), Error> {
    let chan = log_chan_rt();
    if chan < 0 {
        return Err(Error::NotReady);
    }
    let chan = chan as u64;
    log_reg(chan).map_err(|_| Error::NotReady)?;
    let rep = ipc::send(chan, LOG_FLUSH, 0, 0).map_err(|_| Error::NotReady)?;
    if rep.w0 == 0 {
        Ok(())
    } else {
        Err(Error::Denied)
    }
}

/// Seal esplicito del bucket `log` (snapshot per retention): ritorna lo
/// `snap_id`.
pub fn log_seal() -> Result<u64, Error> {
    let chan = log_chan_rt();
    if chan < 0 {
        return Err(Error::NotReady);
    }
    let chan = chan as u64;
    log_reg(chan).map_err(|_| Error::NotReady)?;
    let rep = ipc::send(chan, LOG_SEAL, 0, 0).map_err(|_| Error::NotReady)?;
    if rep.w0 == ring::ERR || rep.w0 == ring::ERR_NOHANDSHAKE {
        return Err(Error::NotReady);
    }
    Ok(rep.w0)
}

/// Contatori: `(appended, evicted, durable, last_seal)`.
pub fn log_stats() -> Result<(u64, u64, bool, u64), Error> {
    let chan = log_chan_rt();
    if chan < 0 {
        return Err(Error::NotReady);
    }
    let chan = chan as u64;
    log_reg(chan).map_err(|_| Error::NotReady)?;
    let rep = ipc::send(chan, LOG_STATS, 0, 0).map_err(|_| Error::NotReady)?;
    if rep.w0 == ring::ERR || rep.w0 == ring::ERR_NOHANDSHAKE {
        return Err(Error::NotReady);
    }
    let appended = rep.w0;
    let evicted = rep.w1;
    let avail = match ring::resp_ring_read() {
        Some((_, _, n)) => n,
        None => return Err(Error::NotReady),
    };
    if avail < 16 {
        ring::resp_ring_consume(16);
        return Err(Error::Invalid);
    }
    let mut frame = [0u8; 16];
    ring::resp_ring_read_payload(&mut frame, 16);
    let durable = u64::from_le_bytes(frame[0..8].try_into().unwrap_or([0; 8])) != 0;
    let last_seal = u64::from_le_bytes(frame[8..16].try_into().unwrap_or([0; 8]));
    Ok((appended, evicted, durable, last_seal))
}
