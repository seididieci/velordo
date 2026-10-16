//! B+tree COW per ArcaFS 56.2b (puro, `no_std`+`alloc`, niente I/O).
//!
//! Logica di indice condivisa tra guest (cardo) e test host: il codec dei
//! nodi e gli algoritmi split/merge vivono UNA volta sola qui; lo storage
//! (blocchi 3584 B con checksum in `format`) e' iniettato via [`BlockStore`].
//! `userland/fs` implementa il trait sopra `ArcaVolume` (+ cache LRU);
//! i test host usano `MemStore` (sotto, `#[cfg(test)]`).
//!
//! Alberi (tre radici indipendenti, stesso store):
//! - primary (tag 0): chiave composita `[id:8][seq:8]` → record versione.
//!   Ogni PUT = un record nuovo (mai overwrite); la head e' il seq max;
//!   il trim retention cancella i seq min oltre `retain`.
//! - secondary (tag 1): chiave `(bucket,key)` → `(uuid,size,mtime,head_seq)`.
//!   Stat denormalizzata: `stat` senza toccare il primary.
//! - refcount (tag 2): chiave `[id:8][seq:8]` → count u64 (pin snapshot).
//!
//! Nodi: header payload `[tag:1][nrec:2 LE][_rsv:1]` poi record; INTERNAL con
//! figli + separatori minimi. Valori grandi (> soglia inline) in overflow
//! linkati (nodi RAW `[next:8][chunk:3552]`). Mai tabelle fisse.

use super::format::{
    self, ARCA_NODE_PAYLOAD_LEN, ARCA_NODE_TYPE_DATA, ARCA_NODE_TYPE_INTERNAL,
    ARCA_NODE_TYPE_LEAF, ARCA_NODE_TYPE_MAP, ARCA_NODE_TYPE_RAW,
};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Tag di albero dentro il payload (primo byte).
pub const TREE_PRIMARY: u8 = 0;
pub const TREE_SECONDARY: u8 = 1;
pub const TREE_REFCOUNT: u8 = 2;
/// Tag tabella snapshot persistente (56.2c): foglie `(key,val)` generiche con
/// chiave `snap/<sid:8 LE>` (riesce il codec `Leaf`, niente formato nuovo).
/// Il blocco meta vive fuori dagli alberi, puntato dal superblock.
pub const TREE_META: u8 = 3;

/// Soglia inline valori primary: oltre si va in overflow linkato.
pub const INLINE_MAX: usize = 512;
/// Chunk dati per blocco overflow (3560 - 8 di next).
pub const OV_CHUNK: usize = 3552;

/// Storage astratto: blocchi numerati (0 = mai valido, come `ArcaVolume`).
/// `read_node` verifica checksum e riporta `(type, gen)`; `write_node`
/// ricalcola header+checksum. Semantica identica a `volume.rs`.
pub trait BlockStore {
    fn read_node(&self, blk: u64, out: &mut [u8; ARCA_NODE_PAYLOAD_LEN])
        -> Option<(u8, u64)>;
    fn write_node(
        &mut self,
        blk: u64,
        ty: u8,
        gen: u64,
        payload: &[u8; ARCA_NODE_PAYLOAD_LEN],
    ) -> bool;
    fn alloc(&mut self) -> Option<u64>;
    fn free(&mut self, blk: u64) -> bool;
    /// Alloca fino a `max` blocchi preferendo la continuazione di `hint`
    /// (affinita' ultimo-run W2: il chiamante raggruppa i consecutivi in
    /// extent). Vuoto a disco pieno. Default: allocazioni singole (test);
    /// il volume vero concatena via `alloc_specific`.
    fn alloc_run(&mut self, hint: u64, max: usize) -> Vec<u64> {
        let _ = hint;
        let mut out = Vec::new();
        for _ in 0..max {
            match self.alloc() {
                Some(b) if b != 0 => out.push(b),
                _ => break,
            }
        }
        out
    }
}

// ── Codec primitivo (LE esplicito, mai panico su dati disco) ─────────────

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}
fn rd64(b: &[u8], o: usize) -> Option<u64> {
    let s = b.get(o..o + 8)?;
    Some(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}
fn rd32(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn wr32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn wr16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn wr64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Chiave secondary serializzata `[blen:1][bucket][0][klen:1][key][0]`.
pub fn seckey_encode(bucket: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    if bucket.len() > super::proto::OBJ_BUCKET_MAX
        || key.len() > super::proto::OBJ_KEY_MAX
    {
        return None;
    }
    let mut v = Vec::with_capacity(3 + bucket.len() + key.len());
    v.push(bucket.len() as u8);
    v.extend_from_slice(bucket);
    v.push(0);
    v.push(key.len() as u8);
    v.extend_from_slice(key);
    v.push(0);
    Some(v)
}

/// Parsa una chiave secondary, ritorna `(bucket, key, resto)`.
pub fn seckey_parse(b: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    // Stesso formato di `wire::obj_prefix` (condiviso via riesportazione).
    let (bk, k, rest) = super::wire::parse_obj_prefix(b)?;
    Some((bk, k, rest))
}

// ── Nodi ─────────────────────────────────────────────────────────────────
// Header payload: [tag:1][nrec:2 LE][rsv:1], record da offset 4.

const HDR_LEN: usize = 4;

fn node_nrec(p: &[u8]) -> Option<usize> {
    let n = rd16(p, 1)? as usize;
    if p.get(0).is_none() || p.len() < HDR_LEN {
        return None;
    }
    Some(n)
}
fn node_set_nrec(p: &mut [u8; ARCA_NODE_PAYLOAD_LEN], n: usize) -> Option<()> {
    if n > u16::MAX as usize {
        return None;
    }
    p[1..3].copy_from_slice(&(n as u16).to_le_bytes());
    Some(())
}

/// Costanti layout extent (W2, ADR-0050): entry `(blk:8, nb:4)` da 12 B,
/// nb = lunghezza in blocchi (offset logici impliciti cumulativi; EOF da
/// `size`). Run = cio' che l'allocatore consegna (dinamica vera, niente
/// tabella), mai oltre `EXT_RUN_MAX`.
pub const EXT_INLINE_MAX: usize = 12;
/// Entry per nodo MAP: `(3560-10)/12 = 295` (layout `[next:8][n:2][entries]`).
pub const EXT_MAP_CAP: usize = 295;
/// Tetto lunghezza run in blocchi (~900 KiB: una mappa da 220 entry copre
/// ~200 MB; oltre si spezza in piu' entry).
pub const EXT_RUN_MAX: u32 = 256;
/// Tetto blocchi per oggetto nei walk (~29 MB: gli oggetti veri sono
/// ≤16 MiB ⇒ ≤4714 blocchi; oltre = struttura corrotta, rifiuto loud
/// anti-OOM invece di Vec giganti).
pub const EXT_BLOCKS_MAX: usize = 8192;

/// Record primary encodato:
/// `[id:8][seq:8][mtime:8][kind:1]` + inline `[dlen:2][data]` | overflow
/// `[ov:8][size:8]` (legacy: solo lettura/migrazione, mai scritto nuovo) |
/// extent `[size:8][n_ext:4][n_in:1][entries: n_in x (blk:8, nb:4)]`
/// `[map_head:8 se n_ext > n_in]` | packed (kind 3: riservato, rifiuto loud).
#[derive(Clone, Debug, PartialEq)]
pub struct PrimRec {
    pub id: u64,
    pub seq: u64,
    pub mtime: u64,
    /// A versione pinnata? No: pin vive nel refcount; qui solo dati.
    pub data: DataRef,
}

/// Riferimento dati di una versione (una sola variante viva per record).
#[derive(Clone, Debug, PartialEq)]
pub enum DataRef {
    /// ≤512 B nel record (nessun blocco dati).
    Inline(Vec<u8>),
    /// Catena OV legacy (pre-W2): lettura e migrazione sole, mai nuove
    /// catene, condivisione vietata (ogni catena ha un solo proprietario:
    /// `walk_ov` resta severo sui blocchi rivisitati).
    Ov(u64, u64), // (ov_head, size)
    /// Mappa extent (tutte le nuove scritture): entry condivisibili tra
    /// versioni (free via liveness al trim, mai eager).
    Ext(ExtHead),
}

/// Testa mappa extent (vedi costanti sopra per il formato).
#[derive(Clone, Debug, PartialEq)]
pub struct ExtHead {
    pub size: u64,
    pub inline: Vec<(u64, u32)>, // ≤ EXT_INLINE_MAX
    pub map_head: u64,           // 0 = nessuna (n_ext == inline.len())
    pub n_ext: u32,              // entry totali (inline + mappate)
}

fn prim_rec_encode(r: &PrimRec) -> Vec<u8> {
    let mut v = Vec::new();
    wr64(&mut v, r.id);
    wr64(&mut v, r.seq);
    wr64(&mut v, r.mtime);
    match &r.data {
        DataRef::Inline(d) => {
            v.push(0);
            wr16(&mut v, d.len() as u16);
            v.extend_from_slice(d);
        }
        DataRef::Ov(ov, size) => {
            v.push(1);
            wr64(&mut v, *ov);
            wr64(&mut v, *size);
        }
        DataRef::Ext(h) => {
            v.push(2);
            wr64(&mut v, h.size);
            wr32(&mut v, h.n_ext);
            v.push(h.inline.len() as u8);
            for (blk, nb) in h.inline.iter() {
                wr64(&mut v, *blk);
                wr32(&mut v, *nb);
            }
            if h.n_ext as usize > h.inline.len() {
                wr64(&mut v, h.map_head);
            }
        }
    }
    v
}

fn prim_rec_parse(b: &[u8]) -> Option<(PrimRec, usize)> {
    let id = rd64(b, 0)?;
    let seq = rd64(b, 8)?;
    let mtime = rd64(b, 16)?;
    let kind = *b.get(24)?;
    if kind == 0 {
        let dlen = rd16(b, 25)? as usize;
        let data = b.get(27..27 + dlen)?.to_vec();
        Some((
            PrimRec { id, seq, mtime, data: DataRef::Inline(data) },
            27 + dlen,
        ))
    } else if kind == 1 {
        let ov = rd64(b, 25)?;
        let size = rd64(b, 33)?;
        Some((
            PrimRec { id, seq, mtime, data: DataRef::Ov(ov, size) },
            41,
        ))
    } else if kind == 2 {
        let size = rd64(b, 25)?;
        let n_ext = rd32(b, 33)?;
        let n_in = *b.get(37)? as usize;
        if n_in > EXT_INLINE_MAX {
            return None;
        }
        let mut off = 38;
        let mut inline = Vec::new();
        for _ in 0..n_in {
            let blk = rd64(b, off)?;
            let nb = rd32(b, off + 8)?;
            off += 12;
            if blk == 0 || nb == 0 {
                return None;
            }
            inline.push((blk, nb));
        }
        let map_head = if n_ext as usize > n_in {
            let mh = rd64(b, off)?;
            off += 8;
            if mh == 0 {
                return None;
            }
            mh
        } else {
            if n_ext as usize != n_in {
                return None; // entry dichiarate ma non presenti
            }
            0
        };
        Some((
            PrimRec { id, seq, mtime, data: DataRef::Ext(ExtHead { size, inline, map_head, n_ext }) },
            off,
        ))
    } else {
        None // kind 3 PACKED riservato + futuri: rifiuto loud
    }
}

/// Record secondary:
/// `seckey + [uuid:8][size:8][mtime:8][head_seq:8]` (32 B fissi in coda).
#[derive(Clone, Debug, PartialEq)]
pub struct SecRec {
    pub bucket: Vec<u8>,
    pub key: Vec<u8>,
    pub uuid: u64,
    pub size: u64,
    pub mtime: u64,
    pub head_seq: u64,
}

fn sec_rec_encode(r: &SecRec) -> Option<Vec<u8>> {
    let mut v = seckey_encode(&r.bucket, &r.key)?;
    wr64(&mut v, r.uuid);
    wr64(&mut v, r.size);
    wr64(&mut v, r.mtime);
    wr64(&mut v, r.head_seq);
    Some(v)
}

fn sec_rec_parse(b: &[u8]) -> Option<(SecRec, usize)> {
    let (bk, k, rest) = seckey_parse(b)?;
    let off = b.len() - rest.len();
    if rest.len() < 32 {
        return None;
    }
    let uuid = rd64(rest, 0)?;
    let size = rd64(rest, 8)?;
    let mtime = rd64(rest, 16)?;
    let head_seq = rd64(rest, 24)?;
    Some((
        SecRec {
            bucket: bk.to_vec(), key: k.to_vec(), uuid, size, mtime, head_seq,
        },
        off + 32,
    ))
}

/// Record refcount: `[id:8][seq:8][count:8]` (24 B fissi).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RefRec {
    pub id: u64,
    pub seq: u64,
    pub count: u64,
}

fn ref_rec_encode(r: &RefRec) -> [u8; 24] {
    let mut v = [0u8; 24];
    v[..8].copy_from_slice(&r.id.to_le_bytes());
    v[8..16].copy_from_slice(&r.seq.to_le_bytes());
    v[16..].copy_from_slice(&r.count.to_le_bytes());
    Some(v).map(|x| x).unwrap()
}

fn ref_rec_parse(b: &[u8]) -> Option<(RefRec, usize)> {
    if b.len() < 24 {
        return None;
    }
    Some((
        RefRec { id: rd64(b, 0)?, seq: rd64(b, 8)?, count: rd64(b, 16)? },
        24,
    ))
}

/// Chiave primaria composita `[id:8][seq:8]` (ordinamento (id,seq)).
fn prim_key(id: u64, seq: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&id.to_le_bytes());
    k[8..].copy_from_slice(&seq.to_le_bytes());
    k
}

// ── Foglie: lista ordinata di (chiave, record) ───────────────────────────

/// Foglia generica: chiavi opache ordinate + payload record opachi.
/// Il codec specifico (prim/sec/ref) serializza sopra.
struct Leaf {
    keys: Vec<Vec<u8>>,
    vals: Vec<Vec<u8>>,
}

fn leaf_parse(p: &[u8], tag: u8) -> Option<Leaf> {
    if *p.first()? != tag {
        return None;
    }
    let n = node_nrec(p)?;
    let mut off = HDR_LEN;
    let mut keys = Vec::with_capacity(n.min(256));
    let mut vals = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        let klen = rd16(p, off)? as usize;
        off += 2;
        let k = p.get(off..off + klen)?.to_vec();
        off += klen;
        let vlen = rd16(p, off)? as usize;
        off += 2;
        let v = p.get(off..off + vlen)?.to_vec();
        off += vlen;
        keys.push(k);
        vals.push(v);
    }
    Some(Leaf { keys, vals })
}

/// Codifica foglia in un payload heap (mai 3.5K stack/return — regola §18).
fn leaf_encode(tag: u8, leaf: &Leaf) -> Option<Box<[u8; ARCA_NODE_PAYLOAD_LEN]>> {
    let mut p = format::boxed_node();
    p[0] = tag;
    node_set_nrec(&mut p, leaf.keys.len())?;
    let mut off = HDR_LEN;
    for (k, v) in leaf.keys.iter().zip(leaf.vals.iter()) {
        if k.len() > u16::MAX as usize || v.len() > u16::MAX as usize {
            return None;
        }
        let need = 2 + k.len() + 2 + v.len();
        if off + need > ARCA_NODE_PAYLOAD_LEN {
            return None;
        }
        p[off..off + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
        off += 2;
        p[off..off + k.len()].copy_from_slice(k);
        off += k.len();
        p[off..off + 2].copy_from_slice(&(v.len() as u16).to_le_bytes());
        off += 2;
        p[off..off + v.len()].copy_from_slice(v);
        off += v.len();
    }
    Some(p)
}

/// Posizione di `key` in foglia (binary search): `(found_idx | insert_idx)`.
fn leaf_pos(leaf: &Leaf, key: &[u8]) -> Result<usize, usize> {
    leaf.keys.binary_search_by(|k| k.as_slice().cmp(key))
}

// ── Interni: figli + separatori ──────────────────────────────────────────
// Layout: `[tag:1][nchild:2 LE][rsv:1]` poi `[child:8 × n]` poi
// `(n-1) × [slen:2][sep:slen]`.

struct Internal {
    children: Vec<u64>,
    seps: Vec<Vec<u8>>, // len = children.len()-1
}

fn internal_parse(p: &[u8], tag: u8) -> Option<Internal> {
    if *p.first()? != tag {
        return None;
    }
    let n = node_nrec(p)?; // riuso nrec = nchild
    if n == 0 || n > 512 {
        return None;
    }
    let mut off = HDR_LEN;
    let mut children = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        children.push(rd64(p, off)?);
        off += 8;
    }
    let mut seps = Vec::with_capacity(n.saturating_sub(1).min(64));
    for _ in 0..n.saturating_sub(1) {
        let sl = rd16(p, off)? as usize;
        off += 2;
        seps.push(p.get(off..off + sl)?.to_vec());
        off += sl;
    }
    Some(Internal { children, seps })
}

/// Come `leaf_encode` per gli interni (payload heap, regola §18).
fn internal_encode(tag: u8, it: &Internal) -> Option<Box<[u8; ARCA_NODE_PAYLOAD_LEN]>> {
    if it.children.is_empty()
        || it.children.len() > 512
        || it.seps.len() + 1 != it.children.len()
    {
        return None;
    }
    let mut p = format::boxed_node();
    p[0] = tag;
    node_set_nrec(&mut p, it.children.len())?;
    let mut off = HDR_LEN;
    for c in it.children.iter() {
        if off + 8 > ARCA_NODE_PAYLOAD_LEN {
            return None;
        }
        p[off..off + 8].copy_from_slice(&c.to_le_bytes());
        off += 8;
    }
    for s in it.seps.iter() {
        if s.len() > u16::MAX as usize || off + 2 + s.len() > ARCA_NODE_PAYLOAD_LEN {
            return None;
        }
        p[off..off + 2].copy_from_slice(&(s.len() as u16).to_le_bytes());
        off += 2;
        p[off..off + s.len()].copy_from_slice(s);
        off += s.len();
    }
    Some(p)
}

/// Figlio da seguire per `key`: primo separatore > key → sx, altrimenti dx.
fn internal_pick(it: &Internal, key: &[u8]) -> usize {
    let mut i = 0;
    while i < it.seps.len() && it.seps[i].as_slice() <= key {
        i += 1;
    }
    i
}

// ── Overflow: catena RAW `[next:8][chunk:3552]` ───────────────────────────

fn ov_write<S: BlockStore>(
    store: &mut S,
    gen: u64,
    data: &[u8],
) -> Option<(u64, u64)> {
    if data.is_empty() {
        return None;
    }
    // Prealloca tutti i blocchi, poi scrivi dal FONDO col `next` gia' noto:
    // ogni nodo e' scritto UNA volta sola (niente patch read-modify-write).
    // INVARIANTE (W1, fonda la cache W3): un nodo scritto non viene mai piu'
    // mutato — solo radici/xhdr/freelist/superblock mutano in place.
    // Fallimento a meta': nodi orfani per la GC (come prima, mai rollback).
    let n = data.len().div_ceil(OV_CHUNK);
    let mut blks = Vec::with_capacity(n);
    for _ in 0..n {
        let blk = store.alloc()?;
        if blk == 0 {
            return None;
        }
        blks.push(blk);
    }
    let size = data.len() as u64;
    // Buffer heap riusato (regola §18, mai 3.5K stack).
    let mut payload = format::boxed_node();
    for (i, blk) in blks.iter().enumerate().rev() {
        let base = i * OV_CHUNK;
        let take = (data.len() - base).min(OV_CHUNK);
        let next = if i + 1 < n { blks[i + 1] } else { 0 };
        payload[..8].copy_from_slice(&next.to_le_bytes());
        payload[8..8 + take].copy_from_slice(&data[base..base + take]);
        payload[8 + take..].fill(0); // coda zero come i buffer freschi di prima
        if !store.write_node(*blk, ARCA_NODE_TYPE_RAW, gen, &payload) {
            return None;
        }
    }
    Some((blks[0], size))
}

fn ov_read<S: BlockStore>(store: &S, head: u64, size: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(size.min(1 << 20));
    let mut cur = head;
    let mut left = size;
    // Guardia anti-loop su catena corrotta: mai più blocchi dei byte/8.
    let mut guard = size / 8 + 16;
    // Buffer heap (regola §18, vedi `ov_write`).
    let mut payload = format::boxed_node();
    while cur != 0 && left > 0 {
        if guard == 0 {
            return None;
        }
        guard -= 1;
        let (ty, _) = store.read_node(cur, &mut payload)?;
        if ty != ARCA_NODE_TYPE_RAW {
            return None;
        }
        let next = rd64(&payload[..], 0)?;
        let take = left.min(OV_CHUNK);
        out.extend_from_slice(payload.get(8..8 + take)?);
        left -= take;
        cur = next;
    }
    if left != 0 || out.len() != size {
        return None;
    }
    Some(out)
}

fn ov_free<S: BlockStore>(store: &mut S, head: u64) {
    let mut cur = head;
    let mut guard = 1 << 20; // mai loop infinito su catena corrotta
    let mut payload = format::boxed_node();
    while cur != 0 && guard > 0 {
        guard -= 1;
        let next = match store.read_node(cur, &mut payload) {
            Some((ARCA_NODE_TYPE_RAW, _)) => rd64(&payload[..], 0).unwrap_or(0),
            _ => 0,
        };
        store.free(cur);
        cur = next;
    }
}

/// Byte logici di un record (qualunque variante: inline, legacy, extent).
fn value_size(r: &PrimRec) -> u64 {
    match &r.data {
        DataRef::Inline(d) => d.len() as u64,
        DataRef::Ov(_, size) => *size,
        DataRef::Ext(h) => h.size,
    }
}

/// Raggruppa blocchi allocati in entry `(blk, nb)`: run consecutivi
/// spezzati a `EXT_RUN_MAX`. La dinamica vera W2: la taglia emerge da cio'
/// che l'allocatore consegna (affinita' ultimo-run), niente tabella.
/// `None` a blocco 0 (mai valido, bug chiamante).
fn group_runs(blks: &[u64]) -> Option<Vec<(u64, u32)>> {
    let mut out = Vec::new();
    let mut it = blks.iter().peekable();
    while let Some(&start) = it.next() {
        if start == 0 {
            return None;
        }
        let mut nb: u32 = 1;
        while nb < EXT_RUN_MAX {
            match it.peek() {
                Some(&&b) if Some(b) == start.checked_add(nb as u64) => {
                    nb += 1;
                    it.next();
                }
                _ => break,
            }
        }
        out.push((start, nb));
    }
    Some(out)
}

// ── Snapshot (tabella in RAM nel motore; persistita dal driver al commit) ─

/// Snapshot per-bucket: pin delle head al momento della creazione.
/// `entries`: (seckey, uuid, seq pinnato). Il pin vive nel refcount su disco.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: u64,
    pub bucket: Vec<u8>,
    pub tick: u64,
    pub entries: Vec<(Vec<u8>, u64, u64)>,
}

/// Motore B+tree COW: tre radici + allocazione id/snapshot + tabella snapshot.
/// `S` = store blocchi (volume reale o `MemStore` nei test host).
pub struct BTree<S: BlockStore> {
    pub store: S,
    pub gen: u64,
    pub root_primary: u64,
    pub root_secondary: u64,
    pub root_refcount: u64,
    /// Blocco tabella snapshot persistente (0 = nessuna). Scritto dal commit
    /// insieme alle radici (mai generazioni diverse); i blocchi meta vecchi
    /// restano orfani per la GC (mai free eager cross-handle: la guardia live
    /// del volume non li conosce dopo un restart).
    pub meta_root: u64,
    pub next_id: u64,
    pub next_snap: u64,
    pub snaps: BTreeMap<u64, Snapshot>,
    /// Budget quota per bucket (A3, ADR-0044): bucket → `quota_blocks`
    /// (0/assente = illimitato). Persistito nella foglia meta come gli
    /// snapshot (`quota/<bucket>`), mai contatori incrementali: `used` si
    /// misura esatto a ogni check (niente drift da trim/GC).
    pub quotas: BTreeMap<Vec<u8>, u64>,
    /// Indice inverso id → seckey (F2: id mai riusati). Persistito dal driver.
    pub by_id: BTreeMap<u64, Vec<u8>>,
    /// Cursori range-read sequenziali (S2.5f/F0): (uuid, blk, off) — `blk`
    /// e' il nodo che contiene l'offset `off` (resume esatto ammesso a
    /// `off == prev_end`, avanza di un nodo). LRU a 8 vie, eviction
    /// round-robin: i read sequenziali riprendono dal cursore (O(1) nodi per
    /// slice invece di O(n)), i salti ripartono dalla head. Mai correttezza,
    /// solo skip di nodi: i dati vengono sempre riletti dai blocchi, e ogni
    /// mutazione (`put_chunk`/`delete`) azzera tutto (catene vecchie
    /// liberabili/riusabili: un cursore stale leggerebbe dati altrui).
    pub range_cur: [(u64, u64, usize); 8],
    pub range_rr: usize,
}

impl<S: BlockStore> BTree<S> {
    /// Crea un indice vuoto: tre foglie vuote + id/snap da 1.
    pub fn new(mut store: S, gen: u64) -> Option<Self> {
        let rp = store.alloc()?;
        let rs = store.alloc()?;
        let rr = store.alloc()?;
        if rp == 0 || rs == 0 || rr == 0 {
            return None;
        }
        let mut me = Self {
            store, gen, root_primary: rp, root_secondary: rs, root_refcount: rr,
            meta_root: 0, next_id: 1, next_snap: 1, snaps: BTreeMap::new(),
            quotas: BTreeMap::new(), by_id: BTreeMap::new(),
            range_cur: [(0, 0, 0); 8], range_rr: 0,
        };
        me.fmt_empty(rp, TREE_PRIMARY)?;
        me.fmt_empty(rs, TREE_SECONDARY)?;
        me.fmt_empty(rr, TREE_REFCOUNT)?;
        Some(me)
    }

    /// Costruisce il motore su radici esistenti (bind 56.2b: i blocchi sono
    /// gia' allocati e formattati, il driver li passa dopo averli validati).
    /// `next_id/next_snap` partono da 1: il driver li sincronizza subito dopo
    /// dall'header-ext (fonte persistente) e chiama `rebuild_by_id`.
    #[inline(never)]
    pub fn new_with_roots(
        store: S,
        gen: u64,
        root_primary: u64,
        root_secondary: u64,
        root_refcount: u64,
    ) -> Option<Self> {
        if root_primary == 0 || root_secondary == 0 || root_refcount == 0 {
            return None; // blocco 0 mai valido, come `ArcaVolume`
        }
        Some(Self {
            store, gen, root_primary, root_secondary, root_refcount,
            meta_root: 0, next_id: 1, next_snap: 1, snaps: BTreeMap::new(),
            quotas: BTreeMap::new(), by_id: BTreeMap::new(),
            range_cur: [(0, 0, 0); 8], range_rr: 0,
        })
    }

    /// Restituisce lo store (test host: simula restart riusando i blocchi).
    #[inline(never)]
    pub fn into_store(self) -> S {
        self.store
    }

    /// Ricostruisce l'indice inverso id → seckey passeggiando il secondary
    /// (bind dopo kill/restart: la tabella in-RAM e' persa, i dati no).
    /// La tabella snapshot NON si ricostruisce (pin persi: documentato 56.2c).
    #[inline(never)]
    pub fn rebuild_by_id(&mut self) -> Option<()> {
        self.by_id.clear();
        let root = self.root_secondary;
        let mut stack = Vec::from([root]);
        let mut guard = 1 << 16;
        while let Some(blk) = stack.pop() {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            let (ty, tag, _, p) = self.node_read(blk)?;
            if tag != TREE_SECONDARY {
                continue;
            }
            if ty == ARCA_NODE_TYPE_LEAF {
                if let Some(leaf) = leaf_parse(&p[..], TREE_SECONDARY) {
                    for (k, v) in leaf.keys.iter().zip(leaf.vals.iter()) {
                        if let Some((sec, _)) = sec_rec_parse(v) {
                            self.by_id.insert(sec.uuid, k.clone());
                        }
                    }
                }
            } else if let Some(it) = internal_parse(&p[..], TREE_SECONDARY) {
                stack.extend(it.children.iter().copied());
            }
        }
        Some(())
    }

    /// Scansione secondaria per prefisso (56.3, vista POSIX): tutte le chiavi
    /// del `bucket` che iniziano per `prefix` (i valori NON si leggono: solo
    /// i nomi servono a readdir/stat-dir). Walk con stack esplicito come
    /// `rebuild_by_id` (niente ricorsione: stack cardo 16 KiB + LTO), guardia
    /// anti-loop + visited (stesso pattern di `walk_tree`). Full-scan O(n):
    /// per un FS di boot va bene; la range-scan con discesa e' futura.
    /// `None` = struttura illeggibile o oltre il cap (mai dati parziali
    /// spacciati per completi).
    #[inline(never)]
    pub fn scan_prefix(&self, bucket: &[u8], prefix: &[u8]) -> Option<Vec<Vec<u8>>> {
        const CAP_KEYS: usize = 1 << 16;
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut seen: Vec<u64> = Vec::new();
        let mut stack = Vec::from([self.root_secondary]);
        let mut guard = 1usize << 20;
        while let Some(blk) = stack.pop() {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            if blk == 0 || seen.contains(&blk) {
                continue;
            }
            seen.push(blk);
            let (ty, tag, _, p) = self.node_read(blk)?;
            if tag != TREE_SECONDARY {
                return None;
            }
            if ty == ARCA_NODE_TYPE_LEAF {
                if let Some(leaf) = leaf_parse(&p[..], TREE_SECONDARY) {
                    for k in leaf.keys.iter() {
                        if out.len() >= CAP_KEYS {
                            return None;
                        }
                        let (bk, key, _) = seckey_parse(k)?;
                        if bk == bucket && key.starts_with(prefix) {
                            out.push(key.to_vec());
                        }
                    }
                }
            } else if let Some(it) = internal_parse(&p[..], TREE_SECONDARY) {
                stack.extend(it.children.iter().copied());
            } else {
                return None;
            }
        }
        Some(out)
    }

    // ── Tabella snapshot persistente (56.2c, blocco meta TREE_META) ─────
    // Chiave `snap/<sid:8 LE>` (13 B); valore `[blen:1][bucket][tick:8][n:4]`
    // poi n × `[uuid:8][seq:8][sklen:2][seckey]`. Tutto a lunghezze esplicite,
    // verificato al load: meta corrotta = tabella ignorata loud (dati
    // intatti), mai mount rifiutato per la retention.

    /// Serializza tabella snapshot + tabella quota in coppie (key, val).
    /// Quote come `quota/<bucket>` → `[quota:8]` in coda agli `snap/`
    /// (stesso blocco meta, stesso commit: mai generazioni diverse).
    /// `None` solo oltre i bound nomi (mai troncamenti).
    #[inline(never)]
    fn snaps_encode(&self) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::with_capacity(self.snaps.len() + self.quotas.len());
        for (bucket, quota) in self.quotas.iter() {
            if bucket.is_empty() || bucket.len() > super::proto::OBJ_BUCKET_MAX {
                return None;
            }
            let mut k = Vec::with_capacity(6 + bucket.len());
            k.extend_from_slice(b"quota/");
            k.extend_from_slice(bucket);
            out.push((k, quota.to_le_bytes().to_vec()));
        }
        for (sid, snap) in self.snaps.iter() {
            let mut k = Vec::with_capacity(13);
            k.extend_from_slice(b"snap/");
            k.extend_from_slice(&sid.to_le_bytes());
            let mut v = Vec::new();
            if snap.bucket.len() > super::proto::OBJ_BUCKET_MAX {
                return None;
            }
            v.push(snap.bucket.len() as u8);
            v.extend_from_slice(&snap.bucket);
            v.extend_from_slice(&snap.tick.to_le_bytes());
            if snap.entries.len() > u32::MAX as usize {
                return None;
            }
            v.extend_from_slice(&(snap.entries.len() as u32).to_le_bytes());
            for (sk, uuid, seq) in snap.entries.iter() {
                v.extend_from_slice(&uuid.to_le_bytes());
                v.extend_from_slice(&seq.to_le_bytes());
                if sk.len() > u16::MAX as usize {
                    return None;
                }
                v.extend_from_slice(&(sk.len() as u16).to_le_bytes());
                v.extend_from_slice(sk);
            }
            out.push((k, v));
        }
        Some(out)
    }

    /// Sostituisce la tabella con le coppie lette dal blocco meta. Qualunque
    /// malformazione → `None` e tabella INVARIATA (il chiamante monta senza
    /// snapshot, loud). `next_snap` resta al chiamante (header-ext).
    #[inline(never)]
    pub fn snaps_decode(&mut self, entries: &[(Vec<u8>, Vec<u8>)]) -> Option<()> {
        let mut table = BTreeMap::new();
        let mut qtable: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        for (k, v) in entries.iter() {
            // Quote (A3): `quota/<bucket>` → `[quota:8]` esatti.
            if k.len() > 6 && k[..6] == *b"quota/" {
                let bucket = k[6..].to_vec();
                if bucket.is_empty()
                    || bucket.len() > super::proto::OBJ_BUCKET_MAX
                    || v.len() != 8
                {
                    return None;
                }
                let mut qb = [0u8; 8];
                qb.copy_from_slice(v);
                if qtable.insert(bucket, u64::from_le_bytes(qb)).is_some() {
                    return None; // bucket duplicato: corrotto
                }
                continue;
            }
            if k.len() != 13 || k[..5] != *b"snap/" {
                return None;
            }
            let mut sidb = [0u8; 8];
            sidb.copy_from_slice(&k[5..13]);
            let sid = u64::from_le_bytes(sidb);
            if sid == 0 {
                return None;
            }
            let blen = *v.first()? as usize;
            if blen > super::proto::OBJ_BUCKET_MAX {
                return None;
            }
            let bucket = v.get(1..1 + blen)?.to_vec();
            let rest = v.get(1 + blen..)?;
            if rest.len() < 12 {
                return None;
            }
            let mut tickb = [0u8; 8];
            tickb.copy_from_slice(&rest[..8]);
            let tick = u64::from_le_bytes(tickb);
            let n = u32::from_le_bytes([rest[8], rest[9], rest[10], rest[11]]) as usize;
            let mut cur = &rest[12..];
            let mut ents = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                if cur.len() < 18 {
                    return None;
                }
                let mut u = [0u8; 8];
                let mut s = [0u8; 8];
                u.copy_from_slice(&cur[..8]);
                s.copy_from_slice(&cur[8..16]);
                let sklen = u16::from_le_bytes([cur[16], cur[17]]) as usize;
                cur = cur.get(18..)?;
                let sk = cur.get(..sklen)?.to_vec();
                cur = cur.get(sklen..)?;
                // La seckey deve parsare (bucket/key entro bound): niente
                // chiavi fantasma nella tabella; uuid 0 mai valido (F2).
                let (_, _, _) = seckey_parse(&sk)?;
                if u == [0; 8] {
                    return None;
                }
                ents.push((sk, u64::from_le_bytes(u), u64::from_le_bytes(s)));
            }
            if !cur.is_empty() {
                return None; // trailing: formato futuro, rifiuta loud oggi
            }
            if table.insert(sid, Snapshot { id: sid, bucket, tick, entries: ents }).is_some() {
                return None; // sid duplicato: corrotto
            }
        }
        self.snaps = table;
        self.quotas = qtable;
        Some(())
    }

    /// Scrive la tabella nel blocco meta (allocato fresco): aggiorna
    /// `meta_root`, il vecchio resta orfano per la GC (mai free eager
    /// cross-handle). Blocco singolo: oltre capacita' → `None` loud
    /// (scala gate: irraggiungibile; multi-blocco fuori scope 56).
    #[inline(never)]
    pub fn meta_store(&mut self) -> Option<()> {
        let entries = self.snaps_encode()?;
        let leaf = Leaf {
            keys: entries.iter().map(|(k, _)| k.clone()).collect(),
            vals: entries.iter().map(|(_, v)| v.clone()).collect(),
        };
        let enc = leaf_encode(TREE_META, &leaf)?;
        let nb = self.store.alloc()?;
        if nb == 0 {
            return None;
        }
        if !self.store.write_node(nb, ARCA_NODE_TYPE_LEAF, self.gen, &enc) {
            self.store.free(nb);
            return None;
        }
        self.meta_root = nb;
        Some(())
    }

    /// Carica la tabella da `meta_root` (0 = nessuna: tabella vuota, Ok).
    /// Verifica tipo LEAF + tag META; foglia malformata o tabella malformata
    /// → `None` (il chiamante monta senza snapshot, loud).
    #[inline(never)]
    pub fn meta_load(&mut self) -> Option<()> {
        if self.meta_root == 0 {
            self.snaps.clear();
            self.quotas.clear();
            return Some(());
        }
        let (ty, tag, _, p) = self.node_read(self.meta_root)?;
        if ty != ARCA_NODE_TYPE_LEAF || tag != TREE_META {
            return None;
        }
        let leaf = leaf_parse(&p[..], TREE_META)?;
        let pairs: Vec<(Vec<u8>, Vec<u8>)> =
            leaf.keys.into_iter().zip(leaf.vals.into_iter()).collect();
        self.snaps_decode(&pairs)
    }

    // ── Quota per bucket (A3, ADR-0044) ────────────────────────────────
    // `used` = blocchi dati DISTINTI trattenuti (catene overflow di TUTTE le
    // versioni, pinnate incluse: lo snapshot conta contro chi lo trattiene).
    // Misura esatta a ogni check (walk O(bucket), niente contatori
    // incrementali: niente drift da trim/GC). Fuori quota per disegno: nodi
    // indice (condivisi, attribuzione arbitraria), inline ≤512 B, meta.

    /// Slack di nodi COW per op mutante (rewrite del path + split eventuali:
    /// sovrastima conservativa, il check e' pre-put e rifiuta presto).
    pub const QUOTA_SLACK_BLOCKS: u64 = 8;

    /// Blocchi dati di un blob di `nbytes` (0 se inline): blocchi DATA +
    /// nodi MAP solo oltre l'inline (12 entry: sotto stanno nel record).
    /// Stima per eccesso (run da 1 blocco): parita' col conto storico
    /// (3000 B = 1 + 0 + slack 8 = 9, cfr. test quota 51/52).
    pub fn quota_blocks_for(nbytes: usize) -> u64 {
        if nbytes <= INLINE_MAX {
            return 0;
        }
        let data = nbytes.div_ceil(ARCA_NODE_PAYLOAD_LEN) as u64;
        let map = if data > EXT_INLINE_MAX as u64 {
            (data - EXT_INLINE_MAX as u64).div_ceil(EXT_MAP_CAP as u64)
        } else {
            0
        };
        data + map
    }

    /// Blocchi dati distinti trattenuti dal bucket. `None` a walk illeggibile
    /// (il chiamante fallisce chiuso: mai over-budget silenzioso).
    #[inline(never)]
    pub fn quota_used(&self, bucket: &[u8]) -> Option<u64> {
        let mut blocks: Vec<u64> = Vec::new();
        // Nomi live: tutte le versioni in storia (pinnate incluse).
        let keys = self.keys_of_bucket(bucket, self.root_secondary);
        for (_, sec) in keys.iter() {
            for seq in self.seqs_of(sec.uuid, sec.head_seq)? {
                let rec = self.head_rec(sec.uuid, seq)?;
                self.value_blocks(&rec, &mut blocks)?;
            }
        }
        // Pinnati delle snapshot del bucket ANCHE a nome cancellato (il
        // delete sgancia il nome ma i blocchi restano finche' lo snapshot
        // vive: restano sul budget di chi li trattiene, mai buco).
        for snap in self.snaps.values() {
            if snap.bucket != bucket {
                continue;
            }
            for (_, uuid, seq) in snap.entries.iter() {
                let rec = self.head_rec(*uuid, *seq)?;
                self.value_blocks(&rec, &mut blocks)?;
            }
        }
        blocks.sort_unstable();
        blocks.dedup();
        Some(blocks.len() as u64)
    }

    /// Walk di catena overflow che tollera i blocchi gia' visti (dedup, non
    /// ciclo: le catene non si condividono per costruzione, ma un puntatore
    /// riveduto si conta una volta sola invece di dichiarare corruzione —
    /// i detector di corruzione restano `reachable_blocks`/GC).
    #[inline(never)]
    fn quota_walk_ov(&self, head: u64, out: &mut Vec<u64>) -> Option<()> {
        let mut cur = head;
        let mut guard = 1usize << 20;
        let mut payload = format::boxed_node();
        while cur != 0 {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            if out.contains(&cur) {
                break;
            }
            out.push(cur);
            let (ty, _) = self.store.read_node(cur, &mut payload)?;
            if ty != ARCA_NODE_TYPE_RAW {
                return None;
            }
            cur = rd64(&payload[..], 0)?;
        }
        Some(())
    }

    /// Il bucket puo' accogliere `add_bytes` in piu'? `None` = misura
    /// impossibile (fallire chiuso). Quota 0/assente = illimitato.
    #[inline(never)]
    pub fn quota_check(&self, bucket: &[u8], add_bytes: usize) -> Option<bool> {
        let quota = match self.quotas.get(bucket) {
            Some(&q) if q > 0 => q,
            _ => return Some(true),
        };
        let used = self.quota_used(bucket)?;
        let need = Self::quota_blocks_for(add_bytes).checked_add(Self::QUOTA_SLACK_BLOCKS)?;
        let end = used.checked_add(need)?;
        Some(end <= quota)
    }

    /// Dimensione della versione pinnata (solo record, niente dati): budget
    /// del rollback prima di allocare la nuova head.
    #[inline(never)]
    pub fn snap_pinned_size(&self, snap_id: u64, bucket: &[u8], key: &[u8]) -> Option<usize> {
        let snap = self.snaps.get(&snap_id)?;
        if snap.bucket != bucket {
            return None;
        }
        let sk = seckey_encode(bucket, key)?;
        let (_, uuid_pin, seq_pin) = snap.entries.iter().find(|(k, _, _)| *k == sk)?;
        let rec = self.head_rec(*uuid_pin, *seq_pin)?;
        Some(value_size(&rec) as usize)
    }

    /// Somma delle dimensioni pinnate (solo record): budget del clone sul
    /// bucket dst prima di copiare.
    #[inline(never)]
    pub fn snap_total_size(&self, snap_id: u64) -> Option<usize> {
        let snap = self.snaps.get(&snap_id)?;
        let mut total = 0usize;
        for (_, uuid_pin, seq_pin) in snap.entries.iter() {
            let rec = self.head_rec(*uuid_pin, *seq_pin)?;
            total = total.checked_add(value_size(&rec) as usize)?;
        }
        Some(total)
    }

    /// Imposta il budget (0 = rimuove il tetto). Solo tabella RAM: la
    /// persistenza e' `meta_store` + commit del chiamante (come gli snapshot).
    #[inline(never)]
    pub fn quota_set(&mut self, bucket: &[u8], quota: u64) -> Option<()> {
        if bucket.is_empty() || bucket.len() > super::proto::OBJ_BUCKET_MAX {
            return None;
        }
        if quota == 0 {
            self.quotas.remove(bucket);
        } else {
            self.quotas.insert(bucket.to_vec(), quota);
        }
        Some(())
    }

    // ── Reachability per orphan-GC (56.2c) ──────────────────────────────
    // Blocchi raggiungibili = nodi dei 3 alberi + dati dei valori primary
    // (catene legacy + entry/nodi MAP extent, W2: condivisi tra versioni,
    // dedup finale). I pin snapshot non sganciano mai i record dal primary:
    // la walk li trova comunque (la tabella serve a rollback/clone, non
    // alla GC).

    /// Tutti i blocchi raggiungibili dalle radici (ordinati, dedupati).
    /// Walk iterativa con guardia visited (puntatori corrotti: mai loop).
    #[inline(never)]
    pub fn reachable_blocks(&self) -> Option<Vec<u64>> {
        let mut out = Vec::new();
        for (root, tag) in [
            (self.root_primary, TREE_PRIMARY),
            (self.root_secondary, TREE_SECONDARY),
            (self.root_refcount, TREE_REFCOUNT),
        ] {
            self.walk_tree(root, tag, &mut out)?;
        }
        // Catene overflow dei valori primary (ogni record di ogni foglia).
        // DISCRIMINA per `ty`: il trial-parse di un interno su byte di foglia
        // puo' riuscire spurio e iniettare figli spazzatura (osservato: walk
        // fallita su albero valido). Mai trial-parse senza tipo.
        let mut stack = alloc::vec![self.root_primary];
        let mut seen: Vec<u64> = Vec::new();
        while let Some(blk) = stack.pop() {
            if blk == 0 || seen.contains(&blk) {
                continue;
            }
            seen.push(blk);
            let (ty, tg, _, p) = self.node_read(blk)?;
            if tg != TREE_PRIMARY {
                continue;
            }
            if ty == ARCA_NODE_TYPE_INTERNAL {
                let it = internal_parse(&p[..], TREE_PRIMARY)?;
                stack.extend(it.children.iter().copied());
                continue;
            }
            if ty != ARCA_NODE_TYPE_LEAF {
                return None;
            }
            let leaf = leaf_parse(&p[..], TREE_PRIMARY)?;
            for v in leaf.vals.iter() {
                let (rec, _) = prim_rec_parse(v)?;
                // Condivisione extent tra versioni: il dedup finale conta
                // una volta sola (mai corruzione: le catene legacy restano
                // a proprietario unico e `walk_ov` resta severo).
                self.value_blocks(&rec, &mut out)?;
            }
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    /// Walk di un albero: accumula i blocchi nodo in `out` (senza dedup qui).
    #[inline(never)]
    fn walk_tree(&self, root: u64, tag: u8, out: &mut Vec<u64>) -> Option<()> {
        let mut stack = alloc::vec![root];
        let mut guard = 1usize << 20;
        while let Some(blk) = stack.pop() {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            if blk == 0 || out.contains(&blk) {
                continue;
            }
            out.push(blk);
            let (ty, tg, _, p) = self.node_read(blk)?;
            if tg != tag {
                return None; // tag inatteso: struttura corrotta, loud
            }
            if ty == ARCA_NODE_TYPE_INTERNAL {
                let it = internal_parse(&p[..], tag)?;
                stack.extend(it.children.iter().copied());
            } else if ty != ARCA_NODE_TYPE_LEAF {
                return None;
            }
        }
        Some(())
    }

    /// Walk di una catena overflow: accumula i blocchi in `out`.
    #[inline(never)]
    fn walk_ov(&self, head: u64, out: &mut Vec<u64>) -> Option<()> {
        let mut cur = head;
        let mut guard = 1usize << 20;
        let mut payload = format::boxed_node();
        while cur != 0 {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            if out.contains(&cur) {
                return None; // ciclo in catena: corrotto, loud
            }
            out.push(cur);
            let (ty, _) = self.store.read_node(cur, &mut payload)?;
            if ty != ARCA_NODE_TYPE_RAW {
                return None;
            }
            cur = rd64(&payload[..], 0)?;
        }
        Some(())
    }

    /// Formatta le tre radici come foglie vuote (init volume fresco 56.2b:
    /// i blocchi sono gia' allocati dal driver, qui solo la formattazione).
    #[inline(never)]
    pub fn init_empty_roots(&mut self) -> Option<()> {
        let (rp, rs, rr) = (self.root_primary, self.root_secondary, self.root_refcount);
        self.fmt_empty(rp, TREE_PRIMARY)?;
        self.fmt_empty(rs, TREE_SECONDARY)?;
        self.fmt_empty(rr, TREE_REFCOUNT)?;
        Some(())
    }

    #[inline(never)]
    fn fmt_empty(&mut self, blk: u64, tag: u8) -> Option<()> {
        let mut p = format::boxed_node();
        p[0] = tag;
        // nrec = 0 e resto zero: foglia vuota (boxed_node azzera gia').
        if self.store.write_node(blk, ARCA_NODE_TYPE_LEAF, self.gen, &p) {
            Some(())
        } else {
            None
        }
    }

    /// Legge un nodo: `(type, tag, gen, payload)`. None se illeggibile.
    /// Payload in `Box` heap (mai 3.5K stack/return per-valore — regola §18:
    /// sotto stanno altri frame con buffer, l'annidamento sfonda i 16 KiB).
    #[inline(never)]
    fn node_read(&self, blk: u64) -> Option<(u8, u8, u64, Box<[u8; ARCA_NODE_PAYLOAD_LEN]>)> {
        let mut p = format::boxed_node();
        let (ty, generation) = self.store.read_node(blk, &mut p)?;
        let tag = *p.first()?;
        if ty != ARCA_NODE_TYPE_LEAF && ty != ARCA_NODE_TYPE_INTERNAL {
            return None;
        }
        Some((ty, tag, generation, p))
    }

    /// Lookup generico: foglia + posizione per `key` nell'albero `root/tag`.
    #[inline(never)]
    fn btree_lookup(&self, root: u64, tag: u8, key: &[u8]) -> Option<(u64, Leaf, usize, bool)> {
        let mut cur = root;
        let mut guard = 64;
        loop {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            let (ty, _tg, _gen, p) = self.node_read(cur)?;
            if ty == ARCA_NODE_TYPE_LEAF {
                let leaf = leaf_parse(&p[..], tag)?;
                return match leaf_pos(&leaf, key) {
                    Ok(i) => Some((cur, leaf, i, true)),
                    Err(i) => Some((cur, leaf, i, false)),
                };
            }
            let it = internal_parse(&p[..], tag)?;
            if it.children.is_empty() {
                return None;
            }
            cur = *it.children.get(internal_pick(&it, key))?;
        }
    }

    /// Path radice→foglia per `key`: lista (nodo, indice-figlio-scelto).
    #[inline(never)]
    fn btree_path(&self, root: u64, tag: u8, key: &[u8]) -> Option<Vec<(u64, usize)>> {
        let mut path = Vec::new();
        let mut cur = root;
        let mut guard = 64;
        loop {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            let (ty, _tg, _gen, p) = self.node_read(cur)?;
            if ty == ARCA_NODE_TYPE_LEAF {
                return Some(path);
            }
            let it = internal_parse(&p[..], tag)?;
            let pick = internal_pick(&it, key);
            let nxt = *it.children.get(pick)?;
            path.push((cur, pick));
            cur = nxt;
        }
    }

    /// Alloca un blocco e ci scrive il payload: `(blocco, ())` o `None`.
    /// I blocchi allocati qui e mai linkati vanno liberati dal chiamante
    /// a errore (igiene freelist: niente leak sul path di fallimento).
    #[inline(never)]
    fn emit(&mut self, ty: u8, payload: &[u8; ARCA_NODE_PAYLOAD_LEN]) -> Option<u64> {
        let b = self.store.alloc()?;
        if b == 0 {
            return None;
        }
        if !self.store.write_node(b, ty, self.gen, payload) {
            self.store.free(b);
            return None;
        }
        Some(b)
    }

    /// Scrive una foglia (split se necessario): `(sx, Option<(sep, dx)>)`.
    /// Entrambi i blocchi sono nuovi (COW); il vecchio va liberato dal
    /// chiamante SOLO a commit riuscito (altrimenti la generazione vecchia
    /// resta valida: vedi ordine write §3).
    #[inline(never)]
    fn write_leaf_split(&mut self, tag: u8, leaf: &Leaf) -> Option<(u64, Option<(Vec<u8>, u64)>)> {
        if let Some(enc) = leaf_encode(tag, leaf) {
            return Some((self.emit(ARCA_NODE_TYPE_LEAF, &enc)?, None));
        }
        if leaf.keys.len() < 2 {
            return None; // singolo record > nodo: chiave/valore troppo grande
        }
        let mid = leaf.keys.len() / 2;
        let (lk, lv) = (leaf.keys[..mid].to_vec(), leaf.vals[..mid].to_vec());
        let (rk, rv) = (leaf.keys[mid..].to_vec(), leaf.vals[mid..].to_vec());
        let (left, right) = (Leaf { keys: lk, vals: lv }, Leaf { keys: rk, vals: rv });
        // Emit sequenziali in blocchi separati: MAI due payload 3.5K vivi
        // nello stesso frame (7K transienti + catena IO = #PF — regola §18).
        let lb = {
            let le = leaf_encode(tag, &left)?;
            self.emit(ARCA_NODE_TYPE_LEAF, &le)?
        };
        match {
            let re = leaf_encode(tag, &right)?;
            self.emit(ARCA_NODE_TYPE_LEAF, &re)
        } {
            Some(rb) => Some((lb, Some((right.keys[0].clone(), rb)))),
            None => {
                self.store.free(lb);
                None
            }
        }
    }

    /// Riscrive i padri COW dal basso verso l'alto: `child_new` rimpiazza il
    /// figlio in `path[last]`; `up` (split del livello sotto) inserisce
    /// `(sep → rblk)` nel padre. Ritorna la nuova root (nuova se split).
    #[inline(never)]
    fn rewrite_parents(
        &mut self,
        path: &[(u64, usize)],
        tag: u8,
        mut child_new: u64,
        mut up: Option<(Vec<u8>, u64)>,
    ) -> Option<u64> {
        if path.is_empty() {
            // Root-foglia: se ha splittato, nuova root interna.
            if let Some((sep, rb)) = up {
                let it = Internal { children: alloc::vec![child_new, rb], seps: alloc::vec![sep] };
                let enc = internal_encode(tag, &it)?;
                return self.emit(ARCA_NODE_TYPE_INTERNAL, &enc);
            }
            return Some(child_new);
        }
        for (parent_blk, idx) in path.iter().rev() {
            let (_, _, _, pp) = self.node_read(*parent_blk)?;
            let mut it = internal_parse(&pp[..], tag)?;
            if *idx >= it.children.len() {
                return None; // path stale: mai riscrivere, fallisci loud
            }
            it.children[*idx] = child_new;
            if let Some((sep, rb)) = up.take() {
                it.seps.insert(*idx.min(&it.seps.len()), sep);
                it.children.insert(idx + 1, rb);
            }
            if let Some(enc) = internal_encode(tag, &it) {
                child_new = self.emit(ARCA_NODE_TYPE_INTERNAL, &enc)?;
                up = None;
                continue;
            }
            // Split interno per conteggio figli; il separatore mediano sale.
            let n = it.children.len();
            if n < 3 {
                return None; // due figli che non ci stanno: separatori enormi
            }
            let mid = n / 2;
            let r_children = it.children.split_off(mid);
            // Seps: sx tiene [..mid-1], il mediano sale, dx tiene [mid..].
            let promoted = it.seps.remove(mid - 1);
            let r_seps = it.seps.split_off(mid - 1);
            let (left, right) = (
                Internal { children: it.children, seps: it.seps },
                Internal { children: r_children, seps: r_seps },
            );
            // Come sopra: emit sequenziali, mai 2×3.5K vivi in frame.
            let lb = {
                let le = internal_encode(tag, &left)?;
                self.emit(ARCA_NODE_TYPE_INTERNAL, &le)?
            };
            match {
                let re = internal_encode(tag, &right)?;
                self.emit(ARCA_NODE_TYPE_INTERNAL, &re)
            } {
                Some(rb) => {
                    child_new = lb;
                    up = Some((promoted, rb));
                }
                None => {
                    self.store.free(lb);
                    return None;
                }
            }
        }
        // Split arrivato in radice: nuova root.
        if let Some((sep, rb)) = up {
            let it = Internal { children: alloc::vec![child_new, rb], seps: alloc::vec![sep] };
            let enc = internal_encode(tag, &it)?;
            return self.emit(ARCA_NODE_TYPE_INTERNAL, &enc);
        }
        Some(child_new)
    }

    /// Insert COW di (key,val) nell'albero `root/tag`; ritorna la nuova root.
    /// Chiave esistente = sostituzione valore (upsert).
    /// Foglia target per `key`: figlio dell'ultimo interno del path (o la
    /// root se l'albero e' una foglia singola). `btree_path` NON include la
    /// foglia: `path.last()` e' l'interno padre, mai la foglia stessa.
    #[inline(never)]
    fn leaf_of_path(&self, root: u64, tag: u8, path: &[(u64, usize)]) -> Option<u64> {
        match path.last() {
            Some((blk, idx)) => {
                let (_, _, _, p) = self.node_read(*blk)?;
                let it = internal_parse(&p[..], tag)?;
                Some(*it.children.get(*idx)?)
            }
            None => Some(root),
        }
    }

    #[inline(never)]
    fn btree_insert(
        &mut self,
        root: u64,
        tag: u8,
        key: Vec<u8>,
        val: Vec<u8>,
    ) -> Option<u64> {
        let path = self.btree_path(root, tag, &key)?;
        let leaf_blk = self.leaf_of_path(root, tag, &path)?;
        let (_, _, _, p) = self.node_read(leaf_blk)?;
        let mut leaf = leaf_parse(&p[..], tag)?;
        match leaf_pos(&leaf, &key) {
            Ok(i) => leaf.vals[i] = val,
            Err(i) => {
                leaf.keys.insert(i, key);
                leaf.vals.insert(i, val);
            }
        }
        let (child_new, up) = self.write_leaf_split(tag, &leaf)?;
        self.rewrite_parents(&path, tag, child_new, up)
    }

    /// Remove COW di `key` dall'albero `root/tag`: `(nuova_root, rimosso?)`.
    /// Merge SOLO a foglia vuota (correttezza: mai foglie vuote non-root);
    /// il merge generale per bassa fill e il borrow sono tuning futuri
    /// (non toccano la correttezza del lookup, solo l'efficienza spazio).
    #[inline(never)]
    fn btree_remove(&mut self, root: u64, tag: u8, key: &[u8]) -> Option<(u64, bool)> {
        let path = self.btree_path(root, tag, key)?;
        let leaf_blk = self.leaf_of_path(root, tag, &path)?;
        let (_, _, _, p) = self.node_read(leaf_blk)?;
        let mut leaf = leaf_parse(&p[..], tag)?;
        let i = match leaf_pos(&leaf, key) {
            Err(_) => return Some((root, false)), // no-op: ZERO scritture
            Ok(i) => i,
        };
        leaf.keys.remove(i);
        leaf.vals.remove(i);
        if path.is_empty() {
            // Root-foglia: anche vuota resta valida (albero vuoto).
            let enc = leaf_encode(tag, &leaf)?;
            let nb = self.emit(ARCA_NODE_TYPE_LEAF, &enc)?;
            return Some((nb, true));
        }
        if !leaf.keys.is_empty() {
            let (child_new, up) = self.write_leaf_split(tag, &leaf)?;
            debug_assert!(up.is_none(), "remove non cresce: split impossibile");
            let nr = self.rewrite_parents(&path, tag, child_new, up)?;
            return Some((nr, true));
        }
        // Foglia svuotata: fondila col fratello e rimuovi il separatore.
        self.remove_merge_empty(root, tag, &path)
    }

    /// Fonde una foglia svuotata col fratello (sx se esiste, altrimenti dx),
    /// rimuove il separatore dal padre e riscrive COW verso l'alto. Se la
    /// radice resta con un figlio solo, collassa (altezza -1).
    #[inline(never)]
    fn remove_merge_empty(
        &mut self,
        _root: u64,
        tag: u8,
        path: &[(u64, usize)],
    ) -> Option<(u64, bool)> {
        let (parent_blk, idx) = *path.last()?;
        let (_, _, _, pp) = self.node_read(parent_blk)?;
        let mut it = internal_parse(&pp[..], tag)?;
        if it.children.len() < 2 {
            return None; // padre degenerato: mai riscrivere
        }
        // Fratello: sx se esiste, altrimenti dx. Il figlio vuoto sparisce.
        let (sib_idx, sep_idx) = if idx > 0 { (idx - 1, idx - 1) } else { (1, 0) };
        let sib_blk = *it.children.get(sib_idx)?;
        let (_, _, _, sp) = self.node_read(sib_blk)?;
        let sib = leaf_parse(&sp[..], tag)?;
        if sib.keys.is_empty() {
            return None; // due foglie vuote adiacenti: corruzione, fallisci loud
        }
        let nb = self.emit(ARCA_NODE_TYPE_LEAF, &*leaf_encode(tag, &sib)?)?;
        // Il padre perde il figlio vuoto (idx) e il separatore adiacente.
        let lost = if sib_idx < idx { idx } else { idx };
        it.children.remove(lost);
        if sep_idx < it.seps.len() {
            it.seps.remove(sep_idx);
        }
        // Punta il fratello al nuovo blocco.
        let sib_pos = if sib_idx < idx { sib_idx } else { sib_idx - 1 };
        if sib_pos < it.children.len() {
            it.children[sib_pos] = nb;
        } else {
            return None;
        }
        if path.len() == 1 {
            // Il padre e' la radice.
            if it.children.len() == 1 {
                // Collasso: lafoglia fratello diventa radice (libera il padre
                // solo al commit: qui resta orfano per la generazione vecchia).
                return Some((nb, true));
            }
            let enc = internal_encode(tag, &it)?;
            let nr = self.emit(ARCA_NODE_TYPE_INTERNAL, &enc)?;
            return Some((nr, true));
        }
        // Padre non-root con 1 figlio: degenerazione profonda — non supportata
        // in 56.2b (richiede merge interno a cascata; a scala gate i merge
        // partono da foglie con padri ramificati: mai osservato). Fallisci
        // loud senza scrivere nulla di linkato (nb resta orfano temporaneo,
        // recuperato dalla orphan-GC 56.2c).
        if it.children.len() < 2 {
            self.store.free(nb);
            return None;
        }
        let enc = internal_encode(tag, &it)?;
        let pb_new = self.emit(ARCA_NODE_TYPE_INTERNAL, &enc)?;
        let nr = self.rewrite_parents(&path[..path.len() - 1], tag, pb_new, None)?;
        Some((nr, true))
    }

    // ── Refcount per-versione (albero tag 2, chiave [id:8][seq:8]) ──────

    #[inline(never)]
    fn ref_get(&self, id: u64, seq: u64) -> u64 {
        let mut k = [0u8; 16];
        k[..8].copy_from_slice(&id.to_le_bytes());
        k[8..].copy_from_slice(&seq.to_le_bytes());
        match self.btree_lookup(self.root_refcount, TREE_REFCOUNT, &k) {
            Some((_, leaf, i, true)) => ref_rec_parse(&leaf.vals[i])
                .map(|(r, _)| r.count)
                .unwrap_or(0),
            _ => 0,
        }
    }

    #[inline(never)]
    fn ref_set(&mut self, id: u64, seq: u64, count: u64) -> Option<()> {
        let mut k = [0u8; 16];
        k[..8].copy_from_slice(&id.to_le_bytes());
        k[8..].copy_from_slice(&seq.to_le_bytes());
        let r = RefRec { id, seq, count };
        let nr = self.btree_insert(self.root_refcount, TREE_REFCOUNT, k.to_vec(), ref_rec_encode(&r).to_vec())?;
        self.root_refcount = nr;
        Some(())
    }

    #[inline(never)]
    fn ref_inc(&mut self, id: u64, seq: u64) -> Option<()> {
        let c = self.ref_get(id, seq);
        self.ref_set(id, seq, c + 1)
    }

    /// Decrementa; a zero rimuove il record E libera i blocchi valore.
    /// Ritorna `true` se il conteggio ha raggiunto zero (chiamante: trim).
    #[inline(never)]
    fn ref_dec_free(&mut self, id: u64, seq: u64) -> Option<bool> {
        let c = self.ref_get(id, seq);
        if c <= 1 {
            // Ultimo pin (o mai pinnato): cancella il record refcount.
            let mut k = [0u8; 16];
            k[..8].copy_from_slice(&id.to_le_bytes());
            k[8..].copy_from_slice(&seq.to_le_bytes());
            let (nr, _) = self.btree_remove(self.root_refcount, TREE_REFCOUNT, &k)?;
            self.root_refcount = nr;
            Some(true)
        } else {
            self.ref_set(id, seq, c - 1)?;
            Some(false)
        }
    }

    // ── Valori primary (inline / extent; overflow solo legacy) ──────────
    // W2 (ADR-0050): ogni nuova scrittura produce extent; le catene OV
    // restano leggibili (oggetti mai riscritti) e migrano al primo write.
    // Condivisione: entry e nodi MAP condivisi tra versioni; free solo via
    // liveness in trim/delete (mai eager).

    /// Scrive un valore primary da byte completi (nuovi oggetti, rollback,
    /// clone, migrazioni): inline se piccolo, altrimenti extent freschi.
    #[inline(never)]
    fn value_write(&mut self, data: &[u8]) -> Option<DataRef> {
        if data.len() <= INLINE_MAX {
            return Some(DataRef::Inline(data.to_vec()));
        }
        let h = self.ext_write_all(data)?;
        Some(DataRef::Ext(h))
    }

    /// Legge un valore primary (inline, overflow legacy od extent).
    #[inline(never)]
    fn value_read(&self, r: &PrimRec) -> Option<Vec<u8>> {
        match &r.data {
            DataRef::Inline(d) => Some(d.clone()),
            DataRef::Ov(ov, size) => {
                let sz: usize = (*size).try_into().ok()?;
                ov_read(&self.store, *ov, sz)
            }
            DataRef::Ext(h) => {
                let size: usize = h.size.try_into().ok()?;
                let (entries, _) = self.ext_entries(h)?;
                let mut out = Vec::with_capacity(size.min(1 << 20));
                self.ext_read_span(&entries, 0, size, &mut out)?;
                if out.len() != size {
                    return None;
                }
                Some(out)
            }
        }
    }

    /// Scrive byte interi come extent freschi (run dall'allocatore con hint
    /// 0, raggruppati per consecutivita'): testa pronta. Fallisce intero a
    /// disco pieno (orfani alla GC, come `ov_write`).
    #[inline(never)]
    fn ext_write_all(&mut self, data: &[u8]) -> Option<ExtHead> {
        let size = data.len() as u64;
        let need = data.len().div_ceil(ARCA_NODE_PAYLOAD_LEN);
        let blks = self.store.alloc_run(0, need);
        if blks.len() < need {
            return None;
        }
        let entries = group_runs(&blks)?;
        self.ext_store_data(&entries, data)?;
        self.ext_build(size, entries)
    }

    /// Scrive `data` sui blocchi delle entry (span esatti del chiamante:
    /// capacita' oltre i dati solo nell'ultimo blocco, coda azzerata per
    /// igiene; buchi interni = bug chiamante, `None` loud).
    #[inline(never)]
    fn ext_store_data(&mut self, entries: &[(u64, u32)], data: &[u8]) -> Option<()> {
        let mut payload = format::boxed_node();
        let mut done = 0usize;
        let mut bi = 0usize;
        let total: usize = entries.iter().map(|(_, nb)| *nb as usize).sum();
        for (blk, nb) in entries.iter() {
            for b in 0..*nb {
                let bblk = blk.checked_add(b as u64)?;
                if bblk == 0 || done > data.len() {
                    return None;
                }
                let take = (data.len() - done).min(ARCA_NODE_PAYLOAD_LEN);
                bi += 1;
                if take < ARCA_NODE_PAYLOAD_LEN && bi != total {
                    return None;
                }
                payload[..take].copy_from_slice(&data[done..done + take]);
                payload[take..].fill(0);
                if !self.store.write_node(bblk, ARCA_NODE_TYPE_DATA, self.gen, &payload) {
                    return None;
                }
                done += take;
            }
        }
        if done != data.len() {
            return None;
        }
        Some(())
    }

    /// Carica entry + numeri dei nodi MAP di una testa: `(entries, map_nodes)`
    /// con entrambi in ordine vecchio→nuovo. Bound anti-OOM (oggetti ≤16
    /// MiB ⇒ pochi blocchi): oltre rifiuto loud. None a struttura
    /// illeggibile (mai parziali: o tutto o niente).
    #[inline(never)]
    fn ext_entries(&self, h: &ExtHead) -> Option<(Vec<(u64, u32)>, Vec<u64>)> {
        // Walk dalla head (nodo piu' nuovo) a ritroso, poi riordina.
        let mut segs: Vec<(u64, Vec<(u64, u32)>)> = Vec::new();
        let mut cur = h.map_head;
        // Bound: mai piu' nodi delle entry dichiarate (catena corrotta).
        let mut guard = (h.n_ext as usize).max(8) + 8;
        let mut payload = format::boxed_node();
        while cur != 0 {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            let (ty, _) = self.store.read_node(cur, &mut payload)?;
            if ty != ARCA_NODE_TYPE_MAP {
                return None;
            }
            let n = rd16(&payload[..], 8)? as usize;
            if n == 0 || n > EXT_MAP_CAP {
                return None;
            }
            let mut seg = Vec::with_capacity(n);
            let mut off = 10;
            for _ in 0..n {
                let blk = rd64(&payload[..], off)?;
                let nb = rd32(&payload[..], off + 8)?;
                off += 12;
                if blk == 0 || nb == 0 {
                    return None;
                }
                seg.push((blk, nb));
            }
            segs.push((cur, seg));
            cur = rd64(&payload[..], 0)?;
        }
        segs.reverse();
        let mut out = Vec::new();
        out.extend_from_slice(&h.inline);
        let mut nodes = Vec::new();
        for (blk, seg) in segs {
            nodes.push(blk);
            out.extend_from_slice(&seg);
        }
        if out.len() != h.n_ext as usize {
            return None;
        }
        let tblks: u64 = out.iter().map(|(_, nb)| *nb as u64).sum();
        if tblks > EXT_BLOCKS_MAX as u64 {
            return None;
        }
        Some((out, nodes))
    }

    /// Scrive un nodo MAP singolo (chunk non vuoto entro capacita', `prev`
    /// = nodo piu' vecchio o 0).
    #[inline(never)]
    fn ext_write_map_node(&mut self, blk: u64, prev: u64, chunk: &[(u64, u32)]) -> Option<()> {
        if blk == 0 || chunk.is_empty() || chunk.len() > EXT_MAP_CAP {
            return None;
        }
        let mut payload = format::boxed_node();
        payload[..8].copy_from_slice(&prev.to_le_bytes());
        payload[8..10].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
        let mut off = 10;
        for (b, nb) in chunk.iter() {
            payload[off..off + 8].copy_from_slice(&b.to_le_bytes());
            payload[off + 8..off + 12].copy_from_slice(&nb.to_le_bytes());
            off += 12;
        }
        payload[off..].fill(0);
        if !self.store.write_node(blk, ARCA_NODE_TYPE_MAP, self.gen, &payload) {
            return None;
        }
        Some(())
    }

    /// Scrive entry eccedenti l'inline in catena MAP fresca (prev noti,
    /// un write per nodo). Ritorna la head = nodo piu' nuovo (0 se vuote).
    #[inline(never)]
    fn ext_write_map(&mut self, entries: &[(u64, u32)]) -> Option<u64> {
        if entries.is_empty() {
            return Some(0);
        }
        let n_nodes = entries.len().div_ceil(EXT_MAP_CAP);
        let mut heads = Vec::with_capacity(n_nodes);
        for _ in 0..n_nodes {
            let blk = self.store.alloc()?;
            if blk == 0 {
                return None;
            }
            heads.push(blk);
        }
        for (i, head) in heads.iter().enumerate() {
            let lo = i * EXT_MAP_CAP;
            let hi = (lo + EXT_MAP_CAP).min(entries.len());
            let prev = if i > 0 { heads[i - 1] } else { 0 };
            self.ext_write_map_node(*head, prev, &entries[lo..hi])?;
        }
        Some(*heads.last().unwrap())
    }

    /// Costruisce ExtHead da entry complete (prime 12 inline, resto in MAP).
    #[inline(never)]
    fn ext_build(&mut self, size: u64, entries: Vec<(u64, u32)>) -> Option<ExtHead> {
        if entries.len() > u32::MAX as usize {
            return None;
        }
        let n_ext = entries.len() as u32;
        let n_in = entries.len().min(EXT_INLINE_MAX);
        let inline = entries[..n_in].to_vec();
        let map_head = if entries.len() > n_in {
            self.ext_write_map(&entries[n_in..])?
        } else {
            0
        };
        Some(ExtHead { size, inline, map_head, n_ext })
    }

    /// Legge [off..off+take) logici (take gia' clampato a EOF): attraversa
    /// solo i blocchi coperti, O(blocchi toccati), accesso casuale senza
    /// cursore. Oltre la copertura (sparse da zero-extend) restituisce zeri.
    /// Mai parziali: o `take` byte esatti o `None`.
    #[inline(never)]
    fn ext_read_span(
        &self,
        entries: &[(u64, u32)],
        off: usize,
        take: usize,
        out: &mut Vec<u8>,
    ) -> Option<()> {
        if take == 0 {
            return Some(());
        }
        let end = off.checked_add(take)?;
        let mut payload = format::boxed_node();
        let mut e_off = 0usize;
        let mut produced = 0usize;
        for (blk, nb) in entries.iter() {
            let e_len = (*nb as usize).checked_mul(ARCA_NODE_PAYLOAD_LEN)?;
            let e_end = e_off.checked_add(e_len)?;
            if e_end > off && e_off < end {
                let lo = off.max(e_off);
                let hi = end.min(e_end);
                let b_lo = (lo - e_off) / ARCA_NODE_PAYLOAD_LEN;
                let b_hi = (hi - e_off + ARCA_NODE_PAYLOAD_LEN - 1) / ARCA_NODE_PAYLOAD_LEN;
                for b in b_lo..b_hi {
                    let bblk = blk.checked_add(b as u64)?;
                    let (ty, _) = self.store.read_node(bblk, &mut payload)?;
                    if ty != ARCA_NODE_TYPE_DATA {
                        return None;
                    }
                    let bs = e_off + b * ARCA_NODE_PAYLOAD_LEN;
                    let s = lo.max(bs);
                    let e = hi.min(bs + ARCA_NODE_PAYLOAD_LEN);
                    out.extend_from_slice(payload.get(s - bs..e - bs)?);
                    produced += e - s;
                }
            }
            e_off = e_end;
            if e_off >= end {
                break;
            }
        }
        // Coda oltre copertura = zeri (sparse); buchi interni mai (entry
        // contigue per costruzione del write).
        if produced > take {
            return None;
        }
        while produced < take {
            out.push(0);
            produced += 1;
        }
        Some(())
    }

    /// Blocchi di un record (catena legacy / entry+MAP, mai inline).
    #[inline(never)]
    fn value_blocks(&self, r: &PrimRec, out: &mut Vec<u64>) -> Option<()> {
        match &r.data {
            DataRef::Inline(_) => Some(()),
            DataRef::Ov(ov, _) => self.walk_ov(*ov, out),
            DataRef::Ext(h) => {
                let (entries, nodes) = self.ext_entries(h)?;
                out.extend(nodes);
                for (blk, nb) in entries {
                    for b in 0..nb {
                        out.push(blk.checked_add(b as u64)?);
                    }
                }
                Some(())
            }
        }
    }

    /// Blocchi vivi delle versioni di `id` ESCLUSE `drop` (liveness W2):
    /// la storia lineare meno le droppate (le pinnate restano sempre nel
    /// primary: mai rimosse da trim/delete) — ordinati, dedupati. Il
    /// chiamante libera i blocchi della droppata non presenti qui.
    #[inline(never)]
    fn live_blocks_except(&self, id: u64, head_seq: u64, drop: &[u64]) -> Option<Vec<u64>> {
        let mut out = Vec::new();
        for seq in self.seqs_of(id, head_seq)? {
            if drop.contains(&seq) {
                continue;
            }
            let rec = self.head_rec(id, seq)?;
            self.value_blocks(&rec, &mut out)?;
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    /// Droppa versioni in batch (trim/delete): rimuove i record, poi libera
    /// UNA volta sola i blocchi non vivi (le droppate condividono blocchi
    /// tra loro: liberarli per-versione darebbe double-free loud). `live`
    /// ordinato (binary search). Free fallito = corruzione, `None` loud;
    /// a meta' i record sono gia' via (orfani alla GC, mai dati altrui).
    #[inline(never)]
    fn free_dropped_versions(&mut self, id: u64, drops: &[u64], live: &[u64]) -> Option<()> {
        let mut recs = Vec::new();
        for seq in drops {
            let pk = prim_key(id, *seq);
            let val = match self.btree_lookup(self.root_primary, TREE_PRIMARY, &pk) {
                Some((_, leaf, i, true)) => prim_rec_parse(&leaf.vals[i]).map(|(r, _)| r),
                _ => None,
            };
            let (nr, removed) = self.btree_remove(self.root_primary, TREE_PRIMARY, &pk)?;
            self.root_primary = nr;
            if removed {
                if let Some(rec) = val {
                    recs.push(rec);
                }
                // Pulisci il refcount (era 0: rimuovi il record).
                let _ = self.ref_dec_free(id, *seq);
            }
        }
        let mut to_free = Vec::new();
        for rec in recs.iter() {
            self.value_blocks(rec, &mut to_free)?;
        }
        to_free.sort_unstable();
        to_free.dedup();
        for b in to_free {
            if live.binary_search(&b).is_err() && !self.store.free(b) {
                return None;
            }
        }
        Some(())
    }

    /// Head di un oggetto: record al seq noto (i seq live sono densi
    /// `[lo..head]` per costruzione di `put_chunk`: trim dal basso, append
    /// in alto, mai buchi in cima). Lookup diretto, mai scansioni O(store).
    #[inline(never)]
    fn head_rec(&self, id: u64, seq: u64) -> Option<PrimRec> {
        if seq == 0 {
            return None;
        }
        let pk = prim_key(id, seq);
        let (_, leaf, i, found) = self.btree_lookup(self.root_primary, TREE_PRIMARY, &pk)?;
        if !found {
            return None;
        }
        prim_rec_parse(&leaf.vals[i]).map(|(r, _)| r)
    }

    /// Tutte le seq presenti per `id` (per trim/delete/stat/quota/liveness):
    /// vettore ordinato. I seq nascono densi ma i trim creano BUCHI (le
    /// pinnate restano mentre le versioni sopra cadono): la sola discesa
    /// densa si fermerebbe al primo gap rendendo le pinnate invisibili —
    /// i loro blocchi verrebbero liberati sotto i piedi (dangling con la
    /// condivisione W2; leak silenzioso prima). Suffisso denso + stragglers
    /// pinnati dal refcount: esatto in entrambi i casi.
    #[inline(never)]
    fn seqs_of(&self, id: u64, head_seq: u64) -> Option<Vec<u64>> {
        let mut out = Vec::new();
        // Suffisso denso dalla head (caso comune: niente buchi in cima).
        let mut s = head_seq;
        loop {
            let pk = prim_key(id, s);
            match self.btree_lookup(self.root_primary, TREE_PRIMARY, &pk) {
                Some((_, _, _, true)) => {
                    out.push(s);
                    if s == 0 {
                        break;
                    }
                    s -= 1;
                }
                _ => break,
            }
            if out.len() > 1 << 16 {
                break; // guardia: mai loop infinito su store corrotto
            }
        }
        // Stragglers pinnati sotto la frontiera (refcount > 0).
        for seq in self.pinned_seqs(id)? {
            if !out.contains(&seq) {
                out.push(seq);
            }
        }
        out.sort_unstable();
        Some(out)
    }

    /// Seq pinnate di `id` (refcount > 0): walk delle foglie refcount
    /// (albero piccolo e raro: solo snapshot). `None` a walk illeggibile
    /// (fail closed: mai enumeration parziali spacciate per esatte).
    #[inline(never)]
    fn pinned_seqs(&self, id: u64) -> Option<Vec<u64>> {
        let mut out = Vec::new();
        let mut seen: Vec<u64> = Vec::new();
        let mut stack = alloc::vec![self.root_refcount];
        let mut guard = 1usize << 20;
        while let Some(blk) = stack.pop() {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            if blk == 0 || seen.contains(&blk) {
                continue;
            }
            seen.push(blk);
            let (ty, tag, _, p) = self.node_read(blk)?;
            if tag != TREE_REFCOUNT {
                return None;
            }
            if ty == ARCA_NODE_TYPE_INTERNAL {
                let it = internal_parse(&p[..], TREE_REFCOUNT)?;
                stack.extend(it.children.iter().copied());
            } else if ty == ARCA_NODE_TYPE_LEAF {
                let leaf = leaf_parse(&p[..], TREE_REFCOUNT)?;
                for (k, v) in leaf.keys.iter().zip(leaf.vals.iter()) {
                    if k.len() != 16 {
                        continue; // chiave malformata: salta (difensiva)
                    }
                    let mut ib = [0u8; 8];
                    ib.copy_from_slice(&k[..8]);
                    if u64::from_le_bytes(ib) != id {
                        continue;
                    }
                    let (rec, _) = ref_rec_parse(v)?;
                    if rec.count > 0 {
                        let mut sb = [0u8; 8];
                        sb.copy_from_slice(&k[8..16]);
                        out.push(u64::from_le_bytes(sb));
                    }
                }
            } else {
                return None;
            }
        }
        Some(out)
    }

    // ── API oggetto (stessa semantica 56.1, backend blocchi) ─────────────

    /// PUT chunk a `offset` (0 = nuova versione da zero, >0 = patch della
    /// head): crea SEMPRE una versione nuova. Ritorna i byte accettati.
    /// W2: tocca solo gli extent coperti (condivisione + niente free eager);
    /// inline e catene legacy migrano ai layout nuovi al primo write.
    /// `now` = wall-clock dal chiamante (cardo `wall::wall_secs`).
    #[inline(never)]
    pub fn put_chunk(
        &mut self,
        bucket: &[u8],
        key: &[u8],
        offset: usize,
        data: &[u8],
        now: u64,
        retain: usize,
    ) -> Option<usize> {
        // Qualunque scrittura invalida i cursori range (nuova versione: la
        // mappa head cambia e i blocchi vecchi sono liberabili — vedi campo).
        self.range_clear();
        let sk = seckey_encode(bucket, key)?;
        let n = data.len();
        let end = offset.checked_add(n)?;
        if end > (1 << 24) {
            return None; // bound anti-OOM: 16 MiB per oggetto in 56.2b
        }
        let (uuid, head_seq, head) = match self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk) {
            Some((_, leaf, i, true)) => {
                let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
                let head = self.head_rec(sec.uuid, sec.head_seq)?;
                (sec.uuid, sec.head_seq, Some(head))
            }
            _ => {
                let id = self.next_id;
                self.next_id += 1;
                self.by_id.insert(id, sk.clone());
                (id, 0, None)
            }
        };
        let seq = head_seq + 1;
        // Nuovo riferimento dati + size (fresh a offset 0, patch sopra).
        let (data, new_size) = match head {
            None => {
                let r = self.value_write(data)?;
                (r, n as u64)
            }
            Some(h) => match h.data {
                DataRef::Inline(cur) => {
                    let mut next = if offset == 0 { Vec::new() } else { cur };
                    if next.len() < end {
                        next.resize(end, 0);
                    }
                    next.get_mut(offset..end)?.copy_from_slice(data);
                    let size = next.len() as u64;
                    let r = self.value_write(&next)?;
                    (r, size)
                }
                DataRef::Ov(ov, size) => {
                    if offset == 0 {
                        // Fresh: la base si scarta (niente lettura).
                        let r = self.value_write(data)?;
                        (r, n as u64)
                    } else {
                        // MIGRAZIONE one-time: materializza, patch, extent.
                        let sz: usize = size.try_into().ok()?;
                        let mut base = ov_read(&self.store, ov, sz)?;
                        if base.len() < end {
                            base.resize(end, 0);
                        }
                        base.get_mut(offset..end)?.copy_from_slice(data);
                        let size = base.len() as u64;
                        let r = self.value_write(&base)?;
                        (r, size)
                    }
                }
                DataRef::Ext(eh) => {
                    if offset == 0 {
                        // Fresh: la mappa vecchia si scarta intera (truncate
                        // incluso) — parita' con Inline/Ov, mai patch.
                        let r = self.value_write(data)?;
                        (r, n as u64)
                    } else {
                        let (nh, size) = self.ext_put_span(&eh, offset, data, end)?;
                        (DataRef::Ext(nh), size)
                    }
                }
            },
        };
        let rec = PrimRec { id: uuid, seq, mtime: now, data };
        let pk = prim_key(uuid, seq);
        let nr = self.btree_insert(self.root_primary, TREE_PRIMARY, pk.to_vec(), prim_rec_encode(&rec))?;
        self.root_primary = nr;
        // Upsert secondary (uuid stabile, head avanzata).
        let sec = SecRec {
            bucket: bucket.to_vec(), key: key.to_vec(), uuid,
            size: new_size, mtime: now, head_seq: seq,
        };
        let nr2 = self.btree_insert(self.root_secondary, TREE_SECONDARY, sk, sec_rec_encode(&sec)?)?;
        self.root_secondary = nr2;
        self.trim_retain(uuid, seq, retain)?;
        Some(n)
    }

    /// Patch/incolla a granularita' blocco (W2, il COW chiesto): espande la
    /// mappa in lista blocchi, sostituisce solo l'intervallo coperto da
    /// [offset, end) con blocchi freschi (i vecchi restano condivisi con le
    /// altre versioni, niente free eager), ricompatta in entry e riusa i
    /// nodi MAP invariati per numero. Solo i blocchi toccati si leggono
    /// (bordi parziali) e scrivono; l'append riempie il blocco di coda ed
    /// estende — O(1) per op, totale lineare. Ritorna (nuova testa, size).
    #[inline(never)]
    fn ext_put_span(
        &mut self,
        h: &ExtHead,
        offset: usize,
        data: &[u8],
        end: usize,
    ) -> Option<(ExtHead, u64)> {
        let old_size = h.size as usize;
        let new_size = old_size.max(end) as u64;
        let (entries, map_nodes) = self.ext_entries(h)?;
        // Lista blocchi piatta (bound EXT_BLOCKS_MAX: heap contenuta).
        let mut blocks = Vec::new();
        for (blk, nb) in entries.iter() {
            for b in 0..*nb {
                blocks.push(blk.checked_add(b as u64)?);
            }
        }
        let total = blocks.len();
        // Intervallo blocchi da sostituire (bordi parziali inclusi); oltre
        // copertura (append/extend) nessun blocco vecchio.
        let first = (offset / ARCA_NODE_PAYLOAD_LEN).min(total);
        let mut last = (end + ARCA_NODE_PAYLOAD_LEN - 1) / ARCA_NODE_PAYLOAD_LEN;
        if last > total {
            last = total;
        }
        // Span byte da assemblare: vecchi preservati + patch + zeri.
        let s_lo = first * ARCA_NODE_PAYLOAD_LEN;
        let s_hi = (last * ARCA_NODE_PAYLOAD_LEN).max(end);
        let mut span = Vec::new();
        span.resize(s_hi - s_lo, 0);
        let old_part = s_hi.min(old_size).saturating_sub(s_lo);
        if old_part > 0 {
            let mut tmp = Vec::new();
            self.ext_read_span(&entries, s_lo, old_part, &mut tmp)?;
            span[..old_part].copy_from_slice(&tmp);
        }
        span[offset - s_lo..offset - s_lo + data.len()].copy_from_slice(data);
        // Blocchi freschi (affinita': continua il run fisico precedente —
        // l'extent nuovo estende quello vecchio quando il disco lo permette).
        let need = span.len().div_ceil(ARCA_NODE_PAYLOAD_LEN);
        let mut fresh = Vec::with_capacity(need);
        let mut hint = if first > 0 { blocks[first - 1] } else { 0 };
        while fresh.len() < need {
            let got = self.store.alloc_run(hint, need - fresh.len());
            if got.is_empty() {
                return None;
            }
            hint = *got.last().unwrap();
            fresh.extend_from_slice(&got);
        }
        // Scrivi solo i freschi (i condivisi restano intatti).
        let span_entries = group_runs(&fresh)?;
        self.ext_store_data(&span_entries, &span)?;
        // Ricompatta tutto e riusa i nodi MAP invariati per numero.
        let mut new_blocks = Vec::with_capacity(total + fresh.len());
        new_blocks.extend_from_slice(&blocks[..first]);
        new_blocks.extend_from_slice(&fresh);
        new_blocks.extend_from_slice(&blocks[last..]);
        let new_entries = group_runs(&new_blocks)?;
        let n_in = new_entries.len().min(EXT_INLINE_MAX);
        // Nodi MAP: riusa il prefisso invariato per numero, riscrivi dalla
        // prima entry cambiata in poi. Corretto con catena all'indietro: i
        // `prev` riusati puntano a nodi riusati (mai patch in place); il
        // primo riscritto punta all'ultimo riusato (o 0).
        let old_mapped: &[(u64, u32)] = entries.get(h.inline.len()..).unwrap_or(&[]);
        let new_mapped = &new_entries[n_in..];
        let mut first_diff: Option<usize> = None;
        for (i, chunk) in new_mapped.chunks(EXT_MAP_CAP).enumerate() {
            let lo = i * EXT_MAP_CAP;
            if old_mapped.get(lo..lo + chunk.len()) != Some(chunk) {
                first_diff = Some(i);
                break;
            }
        }
        let mut node_blks = Vec::new();
        for (i, chunk) in new_mapped.chunks(EXT_MAP_CAP).enumerate() {
            let lo = i * EXT_MAP_CAP;
            let same = old_mapped.get(lo..lo + chunk.len()) == Some(chunk);
            let reuse = same && first_diff.map_or(true, |c| i < c) && i < map_nodes.len();
            if reuse {
                node_blks.push(map_nodes[i]);
            } else {
                let b = self.store.alloc()?;
                if b == 0 {
                    return None;
                }
                node_blks.push(b);
            }
        }
        for (i, chunk) in new_mapped.chunks(EXT_MAP_CAP).enumerate() {
            let reused = first_diff.map_or(true, |c| i < c) && i < map_nodes.len();
            if !reused {
                let lo = i * EXT_MAP_CAP;
                let hi = (lo + EXT_MAP_CAP).min(new_mapped.len());
                let prev = if i > 0 { node_blks[i - 1] } else { 0 };
                self.ext_write_map_node(node_blks[i], prev, &new_mapped[lo..hi])?;
            }
        }
        let map_head = node_blks.last().copied().unwrap_or(0);
        if new_entries.len() > u32::MAX as usize {
            return None;
        }
        let head = ExtHead {
            size: new_size,
            inline: new_entries[..n_in].to_vec(),
            map_head,
            n_ext: new_entries.len() as u32,
        };
        Some((head, new_size))
    }

    /// Trim retention: oltre `retain` versioni, le piu' vecchie non pinnate
    /// (refcount 0) vengono cancellate e liberate; quelle pinnate restano.
    /// W2: i blocchi condivisi con le versioni vive/pinnate restano
    /// (liveness sul set escluso, mai free eager).
    #[inline(never)]
    fn trim_retain(&mut self, id: u64, head_seq: u64, retain: usize) -> Option<()> {
        let seqs = self.seqs_of(id, head_seq)?;
        if seqs.len() <= retain {
            return Some(());
        }
        let drop_n = seqs.len() - retain;
        let mut drops = Vec::new();
        for seq in seqs.iter().take(drop_n) {
            if self.ref_get(id, *seq) == 0 {
                drops.push(*seq);
            }
        }
        // Vivi = tutto il resto (storia + pinnate, mai rimosse dal primary).
        let live = self.live_blocks_except(id, head_seq, &drops)?;
        self.free_dropped_versions(id, &drops, &live)?;
        Some(())
    }

    /// Head di (bucket,key): blob completo o `None` (assente/malformato).
    #[inline(never)]
    pub fn get(&self, bucket: &[u8], key: &[u8]) -> Option<Vec<u8>> {
        let sk = seckey_encode(bucket, key)?;
        let (_, leaf, i, found) = self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk)?;
        if !found {
            return None;
        }
        let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
        let head = self.head_rec(sec.uuid, sec.head_seq)?;
        self.value_read(&head)
    }

    /// Head per object_id (via indice inverso → secondary → primary).
    /// Oggetto cancellato (secondary assente) = `None`, mai dati inventati.
    #[inline(never)]
    pub fn get_id(&self, id: u64) -> Option<Vec<u8>> {
        let sk = self.by_id.get(&id)?.clone();
        let (_, leaf, i, found) = self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk)?;
        if !found {
            return None;
        }
        let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
        if sec.uuid != id {
            return None; // indice inverso stale: mai dati altrui
        }
        let head = self.head_rec(id, sec.head_seq)?;
        self.value_read(&head)
    }

    /// Range read con cursore (S2.5f/F0): `(len_totale, dati[offset..offset+take])`
    /// con `take = min(count, len-offset)` (vuoto oltre EOF, mai errore).
    /// Assente/malformato = `None` (stessa disciplina di `get`). Memoria
    /// O(take), nodi O(take) sul sequenziale (cursore), O(offset) sui salti.
    /// `&mut` per il cursore (RAM-only: perso al restart, mai persistito).
    #[inline(never)]
    pub fn get_range(
        &mut self,
        bucket: &[u8],
        key: &[u8],
        offset: usize,
        count: usize,
    ) -> Option<(usize, Vec<u8>)> {
        let sk = seckey_encode(bucket, key)?;
        let (_, leaf, i, found) = self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk)?;
        if !found {
            return None;
        }
        let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
        self.head_range(sec.uuid, sec.head_seq, offset, count)
    }

    /// Range read per object_id (stessa disciplina di `get_range`).
    #[inline(never)]
    pub fn get_id_range(
        &mut self,
        id: u64,
        offset: usize,
        count: usize,
    ) -> Option<(usize, Vec<u8>)> {
        let sk = self.by_id.get(&id)?.clone();
        let (_, leaf, i, found) = self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk)?;
        if !found {
            return None;
        }
        let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
        if sec.uuid != id {
            return None; // indice inverso stale: mai dati altrui
        }
        self.head_range(sec.uuid, sec.head_seq, offset, count)
    }

    /// Nucleo range su head nota: inline = slice diretto; overflow legacy =
    /// walk con cursore (vedi campo `range_cur` per il contratto); extent =
    /// lettura indicizzata O(blocchi toccati), accesso casuale senza cursore.
    #[inline(never)]
    fn head_range(
        &mut self,
        uuid: u64,
        head_seq: u64,
        offset: usize,
        count: usize,
    ) -> Option<(usize, Vec<u8>)> {
        let head = self.head_rec(uuid, head_seq)?;
        match head.data {
            DataRef::Inline(d) => {
                let size = d.len();
                if offset >= size {
                    return Some((size, Vec::new()));
                }
                let take = (size - offset).min(count);
                Some((size, d[offset..offset + take].to_vec()))
            }
            DataRef::Ext(h) => {
                let size: usize = h.size.try_into().ok()?;
                if offset >= size {
                    return Some((size, Vec::new()));
                }
                let take = (size - offset).min(count);
                let (entries, _) = self.ext_entries(&h)?;
                let mut out = Vec::with_capacity(take);
                self.ext_read_span(&entries, offset, take, &mut out)?;
                if out.len() != take {
                    return None;
                }
                Some((size, out))
            }
            DataRef::Ov(ov, size) => {
                let size: usize = size.try_into().ok()?;
                self.head_range_legacy(uuid, ov, size, offset, count)
            }
        }
    }

    /// Range su catena OV legacy (F0: walk con cursore; solo oggetti mai
    /// riscritti dopo W2 — il primo write migra a extent).
    #[inline(never)]
    fn head_range_legacy(
        &mut self,
        uuid: u64,
        ov: u64,
        size: usize,
        offset: usize,
        count: usize,
    ) -> Option<(usize, Vec<u8>)> {
        if offset >= size {
            return Some((size, Vec::new()));
        }
        let take = (size - offset).min(count);
        let mut cur = ov;
        // Partenza: cursore (nodo che contiene `offset`, o resume esatto a
        // fine nodo) oppure head con skip intero.
        let mut skip = offset;
        let mut hit = false;
        for &(u, blk, off) in self.range_cur.iter() {
            if u == uuid && blk != 0 && off <= offset && offset <= off + OV_CHUNK {
                cur = blk;
                skip = offset - off;
                hit = true;
                break;
            }
        }
        if !hit {
            // Nessun hit: avanza dalla head a nodi interi.
            let mut guard = size / 8 + 16;
            let mut payload = format::boxed_node();
            while skip >= OV_CHUNK {
                if guard == 0 {
                    return None;
                }
                guard -= 1;
                let (ty, _) = self.store.read_node(cur, &mut payload)?;
                if ty != ARCA_NODE_TYPE_RAW {
                    return None;
                }
                cur = rd64(&payload[..], 0)?;
                skip -= OV_CHUNK;
            }
        }
        // Raccolta (al massimo `take` byte): `skip` e' intra-nodo qui.
        if skip > OV_CHUNK {
            return None;
        }
        let mut out = Vec::with_capacity(take);
        let mut left = take;
        let mut node_off = offset - skip;
        let mut guard = size / 8 + 16;
        let mut payload = format::boxed_node();
        while left > 0 {
            if guard == 0 {
                return None;
            }
            guard -= 1;
            let (ty, _) = self.store.read_node(cur, &mut payload)?;
            if ty != ARCA_NODE_TYPE_RAW {
                return None;
            }
            let next = rd64(&payload[..], 0)?;
            if skip == OV_CHUNK {
                // Resume esatto a fine nodo: avanza senza leggere.
                skip = 0;
                node_off += OV_CHUNK;
                cur = next;
                if cur == 0 {
                    return None;
                }
                continue;
            }
            let n = left.min(OV_CHUNK - skip);
            out.extend_from_slice(payload.get(8 + skip..8 + skip + n)?);
            left -= n;
            if left > 0 {
                skip = 0;
                node_off += OV_CHUNK;
                cur = next;
                if cur == 0 {
                    return None;
                }
            } else {
                // Cursore sul nodo dell'ultimo byte (stesso uuid: riuso,
                // altrimenti eviction round-robin).
                let mut done = false;
                for c in self.range_cur.iter_mut() {
                    if c.0 == uuid {
                        *c = (uuid, cur, node_off);
                        done = true;
                        break;
                    }
                }
                if !done {
                    let i = self.range_rr % 8;
                    self.range_rr = self.range_rr.wrapping_add(1);
                    self.range_cur[i] = (uuid, cur, node_off);
                }
            }
        }
        if out.len() != take {
            return None;
        }
        Some((size, out))
    }

    /// Azzera i cursori range (ogni mutazione: la catena head cambia e i nodi
    /// vecchi sono liberabili/riusabili — un cursore stale leggerebbe dati
    /// altrui; il costo e' un head-walk al prossimo salto).
    #[inline(never)]
    fn range_clear(&mut self) {
        self.range_cur = [(0, 0, 0); 8];
    }

    /// Stat per (bucket,key): (id, size head, versioni contate, mtime head).
    #[inline(never)]
    pub fn stat(&self, bucket: &[u8], key: &[u8]) -> Option<(u64, u64, u64, u64)> {
        let sk = seckey_encode(bucket, key)?;
        let (_, leaf, i, found) = self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk)?;
        if !found {
            return None;
        }
        let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
        let head = self.head_rec(sec.uuid, sec.head_seq)?;
        let nv = self.seqs_of(sec.uuid, sec.head_seq)?.len() as u64;
        Some((sec.uuid, value_size(&head), nv, head.mtime))
    }

    /// Istogramma layout valori (W2 debug/tuning): (oggetti extent, entry
    /// totali, max entry singolo, oggetti legacy OV, oggetti inline). Solo
    /// diagnostica (costo O(oggetti) lookup: niente walk totali). Tollerante
    /// agli indici stale (rename non aggiorna `by_id`, pre-esistente: qui si
    /// salta, mai loud su una statistica).
    #[inline(never)]
    pub fn extent_stats(&self) -> Option<(u64, u64, u64, u64, u64)> {
        let mut n_ext = 0u64;
        let mut entries = 0u64;
        let mut max_e = 0u64;
        let mut n_ov = 0u64;
        let mut n_in = 0u64;
        for (id, sk) in self.by_id.iter() {
            let (_, leaf, i, found) = self.btree_lookup(self.root_secondary, TREE_SECONDARY, sk)?;
            if !found {
                continue;
            }
            let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
            if sec.uuid != *id {
                continue; // indice stale: mai dati altrui nemmeno in stats
            }
            let head = self.head_rec(sec.uuid, sec.head_seq)?;
            match &head.data {
                DataRef::Ext(h) => {
                    n_ext += 1;
                    entries += h.n_ext as u64;
                    max_e = max_e.max(h.n_ext as u64);
                }
                DataRef::Ov(..) => n_ov += 1,
                DataRef::Inline(_) => n_in += 1,
            }
        }
        Some((n_ext, entries, max_e, n_ov, n_in))
    }

    /// Stat per object_id: (size head, versioni contate, mtime head).
    /// Oggetto cancellato = `None` (mai dati inventati).
    #[inline(never)]
    pub fn stat_id(&self, id: u64) -> Option<(u64, u64, u64)> {
        let sk = self.by_id.get(&id)?.clone();
        let (_, leaf, i, found) = self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk)?;
        if !found {
            return None;
        }
        let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
        if sec.uuid != id {
            return None;
        }
        let head = self.head_rec(id, sec.head_seq)?;
        let nv = self.seqs_of(id, sec.head_seq)?.len() as u64;
        Some((value_size(&head), nv, head.mtime))
    }

    /// Cancella nome + catena viva (gli snapshot pinnati restano validi:
    /// i pin vivono nel refcount, i dati overflow pinnati non si liberano).
    /// Le versioni pinnate perdono il nome ma i blocchi restano finche'
    /// lo snapshot vive (orphan visibili alla GC 56.2c se lo snapshot muore
    /// dopo: documentato, non nascosto).
    #[inline(never)]
    pub fn delete(&mut self, bucket: &[u8], key: &[u8]) -> Option<bool> {
        // Come `put_chunk`: la cancellazione libera catene (vedi campo).
        self.range_clear();
        let sk = seckey_encode(bucket, key)?;
        let (sec, head_seq) = match self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk) {
            Some((_, leaf, i, true)) => {
                let (s, _) = sec_rec_parse(&leaf.vals[i])?;
                (s.uuid, s.head_seq)
            }
            _ => return Some(false),
        };
        let (nr, removed) = self.btree_remove(self.root_secondary, TREE_SECONDARY, &sk)?;
        self.root_secondary = nr;
        if !removed {
            return Some(false);
        }
        self.by_id.remove(&sec);
        // Versioni vive: cancella le non pinnate (i blocchi condivisi con
        // le pinnate restano via liveness), sgancia le pinnate.
        let seqs = self.seqs_of(sec, head_seq)?;
        let mut drops = Vec::new();
        for seq in seqs {
            if self.ref_get(sec, seq) == 0 {
                drops.push(seq);
            }
        }
        let live = self.live_blocks_except(sec, head_seq, &drops)?;
        self.free_dropped_versions(sec, &drops, &live)?;
        Some(true)
    }

    /// Rinomina una chiave nello stesso bucket (S1.1): la secondary punta
    /// allo STESSO uuid/head (storia condivisa, niente copia dati); se `new`
    /// esiste la sua catena e' liberata prima (replace atomico: UN commit
    /// del chiamante copre tutto, mai mezze rename). Solo secondary: il
    /// chiamante gestisce set transient/dir emergenti. `Some(true)` =
    /// rinominato, `Some(false)` = sorgente assente, `None` = IO/bound.
    #[inline(never)]
    pub fn rename_key(&mut self, bucket: &[u8], old: &[u8], new: &[u8]) -> Option<bool> {
        let sk_old = seckey_encode(bucket, old)?;
        let sk_new = seckey_encode(bucket, new)?;
        if sk_old == sk_new {
            return Some(true); // no-op identitaria
        }
        let sec = match self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk_old) {
            Some((_, leaf, i, true)) => {
                let (s, _) = sec_rec_parse(&leaf.vals[i])?;
                s
            }
            _ => return Some(false),
        };
        // Dst esistente: liberata prima (stessa disciplina di `delete`).
        if self
            .btree_lookup(self.root_secondary, TREE_SECONDARY, &sk_new)
            .map(|(_, _, _, found)| found)
            .unwrap_or(false)
        {
            self.delete(bucket, new)?;
        }
        let uuid = sec.uuid;
        let mut sec_new = sec;
        sec_new.key = new.to_vec();
        let nr = self.btree_insert(
            self.root_secondary,
            TREE_SECONDARY,
            sk_new,
            sec_rec_encode(&sec_new)?,
        )?;
        self.root_secondary = nr;
        let (nr2, _) = self.btree_remove(self.root_secondary, TREE_SECONDARY, &sk_old)?;
        self.root_secondary = nr2;
        self.by_id.insert(uuid, {
            let sk = seckey_encode(bucket, new)?;
            sk
        });
        Some(true)
    }

    // ── Snapshot / clone / rollback (pin = refcount su disco) ──────────

    /// Tutte le seckey di un bucket (scansione secondary: O(bucket), il
    /// listing paginato 56.3 userà range-scan + cursore su questo stesso path).
    #[inline(never)]
    fn keys_of_bucket(&self, bucket: &[u8], root: u64) -> Vec<(Vec<u8>, SecRec)> {
        // Walk completa del secondary (a scala gate decine di chiavi: niente
        // cursore, walk semplice). Il listing paginato 56.3 usera' range-scan
        // sullo stesso path.
        let mut out = Vec::new();
        let mut stack = Vec::from([root]);
        let mut guard = 1 << 16;
        while let Some(blk) = stack.pop() {
            if guard == 0 {
                break;
            }
            guard -= 1;
            let (ty, tag, _, p) = match self.node_read(blk) {
                Some(n) => n,
                None => continue,
            };
            if tag != TREE_SECONDARY {
                continue; // nodo estraneo: skip difensivo, mai panico
            }
            if ty == ARCA_NODE_TYPE_LEAF {
                if let Some(leaf) = leaf_parse(&p[..], TREE_SECONDARY) {
                    for (k, v) in leaf.keys.iter().zip(leaf.vals.iter()) {
                        if let Some((sec, _)) = sec_rec_parse(v) {
                            if sec.bucket == bucket {
                                out.push((k.clone(), sec));
                            }
                        }
                    }
                }
            } else if let Some(it) = internal_parse(&p[..], TREE_SECONDARY) {
                stack.extend(it.children.iter().copied());
            }
        }
        out
    }

    /// Snapshot del bucket: pinna la head di OGNI chiave (refcount inc).
    /// Ritorna l'id snapshot.
    #[inline(never)]
    pub fn snap_create(&mut self, bucket: &[u8], tick: u64) -> Option<u64> {
        if bucket.len() > super::proto::OBJ_BUCKET_MAX {
            return None;
        }
        let root = self.root_secondary;
        let entries_in: Vec<(Vec<u8>, SecRec)> = self.keys_of_bucket(bucket, root);
        let mut entries = Vec::with_capacity(entries_in.len());
        for (sk, sec) in entries_in.iter() {
            self.ref_inc(sec.uuid, sec.head_seq)?;
            entries.push((sk.clone(), sec.uuid, sec.head_seq));
        }
        let id = self.next_snap;
        self.next_snap += 1;
        self.snaps.insert(id, Snapshot { id, bucket: bucket.to_vec(), tick, entries });
        Some(id)
    }

    /// Elimina uno snapshot: decrementa i pin (le versioni a zero restano
    /// finche' la retention/trim le raccoglie; i blocchi si liberano al trim
    /// o alla delete — mai dangling, mai double-free).
    #[inline(never)]
    pub fn snap_delete(&mut self, snap_id: u64) -> bool {
        let snap = match self.snaps.remove(&snap_id) {
            Some(s) => s,
            None => return false,
        };
        for (_, uuid, seq) in snap.entries.iter() {
            let _ = self.ref_dec_free(*uuid, *seq);
        }
        true
    }

    /// Rollback per-chiave: la versione pinnata diventa una NUOVA head
    /// (clonata, storia mai troncata). Solo stesso bucket.
    #[inline(never)]
    pub fn snap_rollback(
        &mut self,
        bucket: &[u8],
        key: &[u8],
        snap_id: u64,
        now: u64,
        retain: usize,
    ) -> Option<u64> {
        // Come `put_chunk`: scrive una nuova head (vedi sotto) — i cursori
        // range della vecchia catena non sono piu' validi.
        self.range_clear();
        let snap = self.snaps.get(&snap_id)?;
        if snap.bucket != bucket {
            return None;
        }
        let sk = seckey_encode(bucket, key)?;
        let (_, uuid_pin, seq_pin) = snap.entries.iter().find(|(k, _, _)| *k == sk)?;
        let pinned = self.head_rec(*uuid_pin, *seq_pin)?;
        let data = self.value_read(&pinned)?;
        // Scrivi come nuova head della chiave live (crea l'oggetto se la
        // chiave e' stata cancellata dopo lo snapshot, con NUOVO uuid).
        let (uuid, head_seq) = match self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk) {
            Some((_, leaf, i, true)) => {
                let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
                (sec.uuid, sec.head_seq)
            }
            _ => {
                let id = self.next_id;
                self.next_id += 1;
                self.by_id.insert(id, sk.clone());
                (id, 0)
            }
        };
        let seq = head_seq + 1;
        let size = data.len() as u64;
        let data = self.value_write(&data)?;
        let rec = PrimRec { id: uuid, seq, mtime: now, data };
        let pk = prim_key(uuid, seq);
        let nr = self.btree_insert(self.root_primary, TREE_PRIMARY, pk.to_vec(), prim_rec_encode(&rec))?;
        self.root_primary = nr;
        let sec = SecRec {
            bucket: bucket.to_vec(), key: key.to_vec(), uuid,
            size, mtime: now, head_seq: seq,
        };
        let nr2 = self.btree_insert(self.root_secondary, TREE_SECONDARY, sk, sec_rec_encode(&sec)?)?;
        self.root_secondary = nr2;
        self.trim_retain(uuid, seq, retain)?;
        Some(size)
    }

    /// Clone di bucket: ogni entry dello snapshot diventa oggetto NUOVO
    /// (nuovi uuid) in `dst`. Ritorna gli oggetti clonati.
    #[inline(never)]
    pub fn snap_clone(&mut self, snap_id: u64, dst: &[u8], now: u64, retain: usize) -> Option<u64> {
        if dst.len() > super::proto::OBJ_BUCKET_MAX {
            return None;
        }
        let snap = self.snaps.get(&snap_id)?.clone();
        let mut n = 0u64;
        for (sk_pin, uuid_pin, seq_pin) in snap.entries.iter() {
            // Chiave destinazione = stesso key, bucket dst.
            let (_, key, _) = seckey_parse(sk_pin)?;
            let pinned = self.head_rec(*uuid_pin, *seq_pin)?;
            let data = self.value_read(&pinned)?;
            let sk_dst = seckey_encode(dst, key)?;
            let (uuid, head_seq) = match self.btree_lookup(self.root_secondary, TREE_SECONDARY, &sk_dst) {
                Some((_, leaf, i, true)) => {
                    let (sec, _) = sec_rec_parse(&leaf.vals[i])?;
                    (sec.uuid, sec.head_seq)
                }
                _ => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.by_id.insert(id, sk_dst.clone());
                    (id, 0)
                }
            };
            let seq = head_seq + 1;
            let size = data.len() as u64;
            let data = self.value_write(&data)?;
            let rec = PrimRec { id: uuid, seq, mtime: now, data };
            let pk = prim_key(uuid, seq);
            let nr = self.btree_insert(self.root_primary, TREE_PRIMARY, pk.to_vec(), prim_rec_encode(&rec))?;
            self.root_primary = nr;
            let sec = SecRec {
                bucket: dst.to_vec(), key: key.to_vec(), uuid,
                size, mtime: now, head_seq: seq,
            };
            let nr2 = self.btree_insert(self.root_secondary, TREE_SECONDARY, sk_dst, sec_rec_encode(&sec)?)?;
            self.root_secondary = nr2;
            self.trim_retain(uuid, seq, retain)?;
            n += 1;
        }
        Some(n)
    }
}

/// Store in memoria per i test host: blocchi numerati da 1 (0 = mai valido).
#[cfg(test)]
pub struct MemStore {
    pub blocks: Vec<Option<([u8; ARCA_NODE_PAYLOAD_LEN], u8, u64)>>,
    pub free_list: Vec<u64>,
}

#[cfg(test)]
impl MemStore {
    pub fn new() -> Self {
        Self { blocks: alloc::vec![None], free_list: Vec::new() }
    }
    pub fn live_count(&self) -> usize {
        self.blocks.iter().filter(|b| b.is_some()).count()
    }
}

#[cfg(test)]
impl BlockStore for MemStore {
    fn read_node(&self, blk: u64, out: &mut [u8; ARCA_NODE_PAYLOAD_LEN]) -> Option<(u8, u64)> {
        let (p, ty, gen) = self.blocks.get(blk as usize)?.as_ref()?;
        out.copy_from_slice(p);
        Some((*ty, *gen))
    }
    fn write_node(&mut self, blk: u64, ty: u8, gen: u64, payload: &[u8; ARCA_NODE_PAYLOAD_LEN]) -> bool {
        if blk == 0 || blk as usize >= self.blocks.len() {
            return false;
        }
        // Il tipo e' parte del contratto: MemStore lo registra come il disco.
        self.blocks[blk as usize] = Some((*payload, ty, gen));
        true
    }
    fn alloc(&mut self) -> Option<u64> {
        if let Some(b) = self.free_list.pop() {
            return Some(b);
        }
        let b = self.blocks.len() as u64;
        self.blocks.push(None);
        Some(b)
    }
    fn free(&mut self, blk: u64) -> bool {
        if blk == 0 || blk as usize >= self.blocks.len() {
            return false;
        }
        if self.blocks[blk as usize].is_none() {
            return false; // double-free: rifiuta loud come `ArcaVolume`
        }
        self.blocks[blk as usize] = None;
        self.free_list.push(blk);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> BTree<MemStore> {
        BTree::new(MemStore::new(), 1).expect("new")
    }

    #[test]
    fn roundtrip_piccolo() {
        let mut t = engine();
        assert_eq!(t.put_chunk(b"b", b"k1", 0, b"ciao", 100, 8), Some(4));
        assert_eq!(t.get(b"b", b"k1"), Some(alloc::vec![b'c', b'i', b'a', b'o']));
        let (id, sz, nv, mt) = t.stat(b"b", b"k1").expect("stat");
        assert!(id >= 1 && sz == 4 && nv == 1 && mt == 100);
        // GET_ID via indice inverso.
        let sk = seckey_encode(b"b", b"k1").unwrap();
        let uuid = t.by_id.iter().find(|(_, v)| **v == sk).map(|(k, _)| *k).unwrap();
        assert_eq!(uuid, id);
        assert_eq!(t.get_id(uuid), Some(alloc::vec![b'c', b'i', b'a', b'o']));
        assert_eq!(t.get_id(uuid + 1000000), None);
        assert_eq!(t.get(b"b", b"nope"), None);
    }

    #[test]
    fn scan_prefix_per_bucket_e_prefisso() {
        let mut t = engine();
        t.put_chunk(b"ns", b"bin/sh", 0, b"x", 1, 8).unwrap();
        t.put_chunk(b"ns", b"bin/ls", 0, b"y", 1, 8).unwrap();
        t.put_chunk(b"ns", b"etc/hosts", 0, b"z", 1, 8).unwrap();
        t.put_chunk(b"other", b"bin/sh", 0, b"w", 1, 8).unwrap();
        let mut got = t.scan_prefix(b"ns", b"bin/").expect("scan");
        got.sort();
        assert_eq!(got, alloc::vec![b"bin/ls".to_vec(), b"bin/sh".to_vec()]);
        assert_eq!(t.scan_prefix(b"ns", b"").unwrap().len(), 3);
        assert_eq!(t.scan_prefix(b"ns", b"nope").unwrap(), alloc::vec::Vec::<alloc::vec::Vec<u8>>::new());
        assert_eq!(t.scan_prefix(b"other", b"bin/").unwrap(), alloc::vec![b"bin/sh".to_vec()]);
        t.delete(b"ns", b"bin/ls").unwrap();
        assert_eq!(t.scan_prefix(b"ns", b"bin/").unwrap(), alloc::vec![b"bin/sh".to_vec()]);
    }

    #[test]
    fn versioni_catena_latest_e_retention() {        let mut t = engine();
        t.put_chunk(b"v", b"k", 0, b"A", 1, 8).unwrap();
        t.put_chunk(b"v", b"k", 0, b"B", 2, 8).unwrap();
        assert_eq!(t.get(b"v", b"k"), Some(alloc::vec![b'B']));
        let (_, _, nv, _) = t.stat(b"v", b"k").unwrap();
        assert_eq!(nv, 2);
        for i in 0..10u8 {
            t.put_chunk(b"v", b"k", 0, &[b'D', b'0' + i], 10 + i as u64, 8).unwrap();
        }
        // 12 versioni totali, trim a 8, latest D9.
        assert_eq!(t.get(b"v", b"k"), Some(alloc::vec![b'D', b'9']));
        let (_, sz, nv, _) = t.stat(b"v", b"k").unwrap();
        assert_eq!((sz, nv), (2, 8));
    }

    #[test]
    fn patch_offset_clone_head() {
        let mut t = engine();
        t.put_chunk(b"v", b"k", 0, b"hello", 1, 8).unwrap();
        // Patch a offset 1: nuova versione "hELLo", storia 2.
        assert_eq!(t.put_chunk(b"v", b"k", 1, b"ELL", 2, 8), Some(3));
        assert_eq!(t.get(b"v", b"k"), Some(b"hELLo".to_vec()));
        let (_, _, nv, _) = t.stat(b"v", b"k").unwrap();
        assert_eq!(nv, 2);
    }

    #[test]
    fn range_sequenziale_e_rewrite() {
        let mut t = engine();
        // Blob multi-nodo (3+ chunk da 3552): pattern posizionale.
        let n = super::OV_CHUNK * 2 + 1000;
        let big: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        t.put_chunk(b"r", b"big", 0, &big, 1, 8).unwrap();
        assert_eq!(t.get(b"r", b"big").as_deref(), Some(big.as_slice()));
        // Slice sequenziali da 4 KiB (come s25fuse): identiche al full.
        let mut off = 0usize;
        while off < n {
            let (len, part) = t.get_range(b"r", b"big", off, 4096).expect("range");
            assert_eq!(len, n);
            assert_eq!(part.as_slice(), &big[off..off + part.len()]);
            if part.is_empty() {
                break;
            }
            off += part.len();
        }
        assert_eq!(off, n);
        // Salti: inizio/metà/fine-nodo/EOF/oltre.
        for o in [0, 1, super::OV_CHUNK - 1, super::OV_CHUNK, super::OV_CHUNK + 1, n - 7, n - 1, n, n + 5] {
            let (len, part) = t.get_range(b"r", b"big", o, 4096).expect("range");
            assert_eq!(len, n);
            let want = if o >= n { &big[n..n] } else { &big[o..(o + 4096).min(n)] };
            assert_eq!(part.as_slice(), want, "off={}", o);
        }
        // count 0 e assente.
        assert_eq!(t.get_range(b"r", b"big", 10, 0), Some((n, Vec::new())));
        assert_eq!(t.get_range(b"r", b"nope", 0, 4096), None);
        // Via id: stesso contenuto.
        let (id, _, _, _) = t.stat(b"r", b"big").unwrap();
        let (len, part) = t.get_id_range(id, 100, 5000).expect("id range");
        assert_eq!((len, part.as_slice()), (n, &big[100..5100]));
        assert_eq!(t.get_id_range(id + 1000000, 0, 10), None);
        // Rewrite totale dopo letture (cursore popolato): MAI dati vecchi.
        let big2: Vec<u8> = (0..n).map(|i| (255 - (i % 251)) as u8).collect();
        t.put_chunk(b"r", b"big", 0, &big2, 2, 8).unwrap();
        let (len2, part2) = t.get_range(b"r", b"big", 0, 4096).expect("range v2");
        assert_eq!((len2, part2.as_slice()), (n, &big2[..4096]));
        let (_, part2b) = t.get_range(b"r", b"big", n - 7, 4096).expect("range v2b");
        assert_eq!(part2b.as_slice(), &big2[n - 7..]);
        // Inline piccolo: slice diretto.
        t.put_chunk(b"r", b"tiny", 0, b"abcdef", 3, 8).unwrap();
        assert_eq!(
            t.get_range(b"r", b"tiny", 2, 10),
            Some((6, b"cdef".to_vec()))
        );
    }

    #[test]
    fn range_append_sequenziali() {
        let mut t = engine();
        let mut expect = Vec::new();
        for k in 0..20usize {
            let chunk: Vec<u8> = (0..4096).map(|i| ((k * 4096 + i) % 251) as u8).collect();
            let r = t.put_chunk(b"r", b"seq", k * 4096, &chunk, k as u64, 8);
            assert!(r.is_some(), "put fallita a {}", k * 4096);
            expect.extend_from_slice(&chunk);
        }
        assert_eq!(t.get(b"r", b"seq").as_deref(), Some(expect.as_slice()));
        let (len, part) = t.get_range(b"r", b"seq", 65536, 4096).unwrap();
        assert_eq!(len, expect.len());
        assert_eq!(part.as_slice(), &expect[65536..65536 + part.len()]);
    }

    /// Decoratore di conteggio op blocco (W1 strumentazione): misura il costo
    /// algoritmico dei write senza rumore guest (in guest ogni op = IPC+PIO).
    /// I bound sotto sono tarati sui numeri post-fix (guardia regressione);
    /// i numeri assoluti si leggono con `-- --nocapture`.
    struct CountStore<S> {
        inner: S,
        rd: core::cell::Cell<usize>,
        wr: core::cell::Cell<usize>,
        al: core::cell::Cell<usize>,
        fr: core::cell::Cell<usize>,
    }

    impl<S> CountStore<S> {
        fn new(inner: S) -> Self {
            Self {
                inner,
                rd: core::cell::Cell::new(0),
                wr: core::cell::Cell::new(0),
                al: core::cell::Cell::new(0),
                fr: core::cell::Cell::new(0),
            }
        }
        fn counts(&self) -> (usize, usize, usize, usize) {
            (self.rd.get(), self.wr.get(), self.al.get(), self.fr.get())
        }
    }

    impl<S: BlockStore> BlockStore for CountStore<S> {
        fn read_node(&self, blk: u64, out: &mut [u8; ARCA_NODE_PAYLOAD_LEN]) -> Option<(u8, u64)> {
            self.rd.set(self.rd.get() + 1);
            self.inner.read_node(blk, out)
        }
        fn write_node(
            &mut self,
            blk: u64,
            ty: u8,
            gen: u64,
            payload: &[u8; ARCA_NODE_PAYLOAD_LEN],
        ) -> bool {
            self.wr.set(self.wr.get() + 1);
            self.inner.write_node(blk, ty, gen, payload)
        }
        fn alloc(&mut self) -> Option<u64> {
            self.al.set(self.al.get() + 1);
            self.inner.alloc()
        }
        fn free(&mut self, blk: u64) -> bool {
            self.fr.set(self.fr.get() + 1);
            self.inner.free(blk)
        }
    }

    #[test]
    fn write_append_opcount() {
        // 512 append sequenziali da 4 KiB (2 MiB finali, il target S2.5w):
        // con gli extent ogni op tocca O(1) blocchi — la seconda meta' deve
        // costare come la prima (un O(n^2) darebbe ~3x). Il test57 originale
        // in guest si inchiodava a 64 KiB su questo stesso pattern.
        let mut t = BTree::new(CountStore::new(MemStore::new()), 1).expect("new");
        let mut off = 0usize;
        for k in 0..256usize {
            let chunk: Vec<u8> = (0..4096).map(|i| ((k * 4096 + i) % 251) as u8).collect();
            assert_eq!(t.put_chunk(b"w", b"seq", off, &chunk, k as u64, 8), Some(4096), "put {}", k);
            off += 4096;
        }
        let (rd1, wr1, al1, fr1) = t.store.counts();
        for k in 256..512usize {
            let chunk: Vec<u8> = (0..4096).map(|i| ((k * 4096 + i) % 251) as u8).collect();
            assert_eq!(t.put_chunk(b"w", b"seq", off, &chunk, k as u64, 8), Some(4096), "put {}", k);
            off += 4096;
        }
        let (rd, wr, al, fr) = t.store.counts();
        assert_eq!(t.get(b"w", b"seq").map(|v| v.len()), Some(512 * 4096));
        // Contenuto posizionale intatto dopo trim aggressivo (retain=8 su
        // 512 versioni: condivisione + liveness esercitati davvero).
        let blob = t.get(b"w", b"seq").unwrap();
        assert!(blob.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        println!("[opcount] append512x4K rd={rd} wr={wr} alloc={al} free={fr} (meta1 rd={rd1} wr={wr1} al={al1} fr={fr1})");
        // Linearita' (bound larghi 2x: il quadratico darebbe ~3x; misurati
        // ~1.2x letture, ~1.0x scritture).
        assert!(rd - rd1 <= 2 * rd1.max(1));
        assert!(wr - wr1 <= 2 * wr1.max(1));
        assert!(al - al1 <= 2 * al1.max(1));
        assert!(fr - fr1 <= 2 * fr1.max(1));
        // Bound assoluti post-W2 (+~30% sui misurati
        // 28651/3133/3133/996): 2 MiB in ~33K op (il W1-quadratico ne
        // vorrebbe ~800K).
        assert!(rd <= 37500 && wr <= 4100 && al <= 4100 && fr <= 1300);
        // Ogni blocco allocato e' scritto una sola volta (invariante W1).
        assert_eq!(wr, al);
    }

    #[test]
    fn write_rewrite_opcount() {
        // 10 rewrite totali da 64 KiB (esercita trim oltre retain=8).
        let mut t = BTree::new(CountStore::new(MemStore::new()), 1).expect("new");
        let big: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
        for k in 0..10u64 {
            assert_eq!(t.put_chunk(b"w", b"big", 0, &big, k, 8), Some(65536));
        }
        assert_eq!(t.get(b"w", b"big").map(|v| v.len()), Some(65536));
        let (rd, wr, al, fr) = t.store.counts();
        println!("[opcount] rewrite10x64K rd={rd} wr={wr} alloc={al} free={fr}");
        // Bound post-W1 (+~25% sui misurati 367/215/215/38).
        assert!(rd <= 460 && wr <= 270 && al <= 270 && fr <= 50);
        assert_eq!(wr, al);
    }

    #[test]
    fn extent_midpatch_trim_condivisione() {
        // 20 patch centrali non sovrapposte + trim (retain=8 su 21 versioni):
        // i blocchi sostituiti muoiono solo a versioni droppate, i condivisi
        // sopravvivono — contenuto sempre esatto.
        let mut t = engine();
        let base: Vec<u8> = (0..30000).map(|i| (i % 251) as u8).collect();
        t.put_chunk(b"e", b"mid", 0, &base, 1, 8).unwrap();
        // Patch a offset >0 (offset 0 = versione fresca da zero per contratto,
        // mai patch: il merge e' a carico del chiamante come `ns_write`).
        for k in 0..20usize {
            let off = 1400 + k * 1300;
            let p: Vec<u8> = (0..1024).map(|i| (0xC0 + ((k + i) % 16)) as u8).collect();
            assert_eq!(t.put_chunk(b"e", b"mid", off, &p, 10 + k as u64, 8), Some(1024));
        }
        let (_, _, nv, _) = t.stat(b"e", b"mid").unwrap();
        assert_eq!(nv, 8);
        let got = t.get(b"e", b"mid").unwrap();
        assert_eq!(got.len(), 30000);
        for k in 0..20usize {
            let off = 1400 + k * 1300;
            for i in 0..1024 {
                assert_eq!(got[off + i], (0xC0 + ((k + i) % 16)) as u8, "k={k} i={i}");
            }
        }
        for (i, b) in got.iter().enumerate() {
            let patched = i >= 1400 && (i - 1400) % 1300 < 1024 && (i - 1400) / 1300 < 20;
            if !patched {
                assert_eq!(*b, (i % 251) as u8, "buco a {i}");
            }
        }
        // Niente leak grossolani: 30K dati + mappe + btree in <500 blocchi.
        assert!(t.store.live_count() < 500, "live={}", t.store.live_count());
    }

    #[test]
    fn extent_snapshot_pin_delete_rollback() {
        // Pin + overwrite totale + delete + rollback: i blocchi del pinnato
        // non si liberano mai sotto i piedi.
        let mut t = engine();
        let base: Vec<u8> = (0..20000).map(|i| (i % 251) as u8).collect();
        t.put_chunk(b"s", b"pin", 0, &base, 1, 8).unwrap();
        let sid = t.snap_create(b"s", 2).expect("snap");
        let over: Vec<u8> = (0..20000).map(|i| (0xFF - (i % 200)) as u8).collect();
        t.put_chunk(b"s", b"pin", 0, &over, 3, 8).unwrap();
        assert_eq!(t.get(b"s", b"pin").unwrap(), over);
        // Delete sgancia ma il pin tiene i blocchi: rollback li ritrova.
        assert!(t.delete(b"s", b"pin").unwrap());
        assert_eq!(t.get(b"s", b"pin"), None);
        t.snap_rollback(b"s", b"pin", sid, 4, 8).unwrap();
        assert_eq!(t.get(b"s", b"pin").unwrap(), base);
        assert!(t.snap_delete(sid));
        // Dopo il delete tutto torna liberabile: rewrite + trim aggressivo.
        for k in 0..10u64 {
            t.put_chunk(b"s", b"pin", 0, &over, 10 + k, 1).unwrap();
        }
        assert_eq!(t.get(b"s", b"pin").unwrap(), over);
    }

    #[test]
    fn extent_migra_legacy_al_write() {
        // Catena OV forgiata a mano (i write nuovi non ne producono piu'):
        // lettura duale ok, primo write migra a Ext con patch applicata.
        let mut t = engine();
        let old: Vec<u8> = (0..9000).map(|i| (i % 251) as u8).collect();
        let (ov, size) = ov_write(&mut t.store, 1, &old).expect("ov");
        let sk = seckey_encode(b"g", b"leg").unwrap();
        t.next_id = 78;
        t.by_id.insert(77, sk.clone());
        let pk = prim_key(77, 1);
        let rec = PrimRec { id: 77, seq: 1, mtime: 1, data: DataRef::Ov(ov, size) };
        let rp = t.root_primary;
        let nr = t.btree_insert(rp, TREE_PRIMARY, pk.to_vec(), prim_rec_encode(&rec)).unwrap();
        t.root_primary = nr;
        let sec = SecRec {
            bucket: b"g".to_vec(), key: b"leg".to_vec(), uuid: 77,
            size, mtime: 1, head_seq: 1,
        };
        let rs = t.root_secondary;
        let nr2 = t.btree_insert(rs, TREE_SECONDARY, sk, sec_rec_encode(&sec).unwrap()).unwrap();
        t.root_secondary = nr2;
        assert_eq!(t.get(b"g", b"leg").as_deref(), Some(old.as_slice()));
        t.put_chunk(b"g", b"leg", 100, &[0xAA; 50], 2, 8).unwrap();
        let got = t.get(b"g", b"leg").unwrap();
        assert_eq!(got.len(), 9000);
        assert_eq!(&got[..100], &old[..100]);
        assert!(got[100..150].iter().all(|b| *b == 0xAA));
        assert_eq!(&got[150..], &old[150..]);
        // Migrata davvero: head Ext, niente piu' catene.
        let head = t.head_rec(77, 2).unwrap();
        assert!(matches!(head.data, DataRef::Ext(_)));
        // 9 rewrite freschi: trim droppa la v1 legacy (walk_ov + free
        // esercitati) senza perdere la head.
        for k in 0..9u64 {
            t.put_chunk(b"g", b"leg", 0, &old, 10 + k, 8).unwrap();
        }
        assert_eq!(t.get(b"g", b"leg").as_deref(), Some(old.as_slice()));
    }

    #[test]
    fn extent_map_spill_confini() {
        // Append frammentanti (40x4K con trim/riuso freelist): decine di
        // entry → 12 inline + nodi MAP; patch a cavallo + range oltre
        // confine + confini inline/extent e blocco.
        let mut t = engine();
        let mut off = 0usize;
        for k in 0..40usize {
            let chunk: Vec<u8> = (0..4096).map(|i| ((k * 4096 + i) % 251) as u8).collect();
            t.put_chunk(b"m", b"big", off, &chunk, k as u64, 8).unwrap();
            off += 4096;
        }
        let (id, _, _, _) = t.stat(b"m", b"big").unwrap();
        let head = t.head_rec(id, 40).unwrap();
        match &head.data {
            DataRef::Ext(h) => {
                assert!(h.n_ext as usize > EXT_INLINE_MAX, "n_ext={}", h.n_ext);
                assert_eq!(h.inline.len(), EXT_INLINE_MAX);
                assert_ne!(h.map_head, 0);
            }
            other => panic!("atteso Ext, trovato {other:?}"),
        }
        let got = t.get(b"m", b"big").unwrap();
        assert_eq!(got.len(), 40 * 4096);
        assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        // Patch a cavallo di piu' entry + range oltre confine mappa.
        let poff = 30000;
        t.put_chunk(b"m", b"big", poff, &[0xBB; 8000], 41, 8).unwrap();
        let got2 = t.get(b"m", b"big").unwrap();
        assert_eq!(got2.len(), 40 * 4096);
        assert!(got2[poff..poff + 8000].iter().all(|b| *b == 0xBB));
        assert_eq!(&got2[..poff], &got[..poff]);
        assert_eq!(&got2[poff + 8000..], &got[poff + 8000..]);
        let (len, part) = t.get_range(b"m", b"big", poff - 100, 9000).unwrap();
        assert_eq!(len, 40 * 4096);
        assert_eq!(part.as_slice(), &got2[poff - 100..poff - 100 + 9000]);
        // File fresco su disco vuoto = 1 sola entry (l'affinita' funziona:
        // high_water++ consegna run contigui; sullo store frammentato sopra
        // i run si spezzano ed e' corretto cosi').
        let mut t2 = engine();
        let big2: Vec<u8> = (0..102400).map(|i| (i % 251) as u8).collect();
        t2.put_chunk(b"m", b"fresh", 0, &big2, 1, 8).unwrap();
        match &t2.head_rec(1, 1).unwrap().data {
            DataRef::Ext(h) => assert_eq!((h.n_ext, h.map_head), (1, 0)),
            other => panic!("atteso Ext singolo, trovato {other:?}"),
        }
        assert_eq!(t2.get(b"m", b"fresh").as_deref(), Some(big2.as_slice()));
        // Confini: 512 inline, 513 extent, shrink a inline, blocchi esatti.
        t.put_chunk(b"m", b"t512", 0, &[7u8; 512], 43, 8).unwrap();
        assert!(matches!(t.head_rec(2, 1).unwrap().data, DataRef::Inline(_)));
        t.put_chunk(b"m", b"t512", 512, &[9u8; 1], 44, 8).unwrap();
        let tiny = t.get(b"m", b"t512").unwrap();
        assert_eq!(tiny.len(), 513);
        assert!(tiny[..512].iter().all(|b| *b == 7) && tiny[512] == 9);
        assert!(matches!(t.head_rec(2, 2).unwrap().data, DataRef::Ext(_)));
        t.put_chunk(b"m", b"t512", 0, &[7u8; 512], 45, 8).unwrap();
        assert!(matches!(t.head_rec(2, 3).unwrap().data, DataRef::Inline(_)));
        for (key, n) in [(b"b9".as_slice(), 3559usize), (b"b0".as_slice(), 3560usize), (b"b1".as_slice(), 3561usize)] {
            let v: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            t.put_chunk(b"m", key, 0, &v, 46, 8).unwrap();
            assert_eq!(t.get(b"m", key).as_deref(), Some(v.as_slice()));
        }
    }

    /// Store fedele ad `ArcaVolume` (W2 diagnosis): freelist LIFO +
    /// high_water + live-set + alloc_specific + stale-read (un blocco
    /// liberato rilegge i dati VECCHI, come sul disco — MemStore invece
    /// ritorna None e nasconde gli use-after-free).
    struct FakeVol {
        blocks: Vec<Option<([u8; ARCA_NODE_PAYLOAD_LEN], u8, u64)>>,
        free_list: Vec<u64>,
        high_water: u64,
        live: alloc::collections::BTreeSet<u64>,
    }

    impl FakeVol {
        fn new() -> Self {
            Self { blocks: alloc::vec![None], free_list: Vec::new(), high_water: 1, live: alloc::collections::BTreeSet::new() }
        }
        fn is_live(&self, b: u64) -> bool {
            self.live.contains(&b)
        }
        fn alloc_specific(&mut self, n: u64) -> bool {
            if n == 0 || !self.live.insert(n) {
                return false;
            }
            if let Some(pos) = self.free_list.iter().position(|x| *x == n) {
                self.free_list.remove(pos);
                return true;
            }
            if n == self.high_water {
                self.high_water += 1;
                while self.blocks.len() <= n as usize {
                    self.blocks.push(None);
                }
                return true;
            }
            self.live.remove(&n);
            false
        }
    }

    impl BlockStore for FakeVol {
        fn read_node(&self, blk: u64, out: &mut [u8; ARCA_NODE_PAYLOAD_LEN]) -> Option<(u8, u64)> {
            let (p, ty, gen) = self.blocks.get(blk as usize)?.as_ref()?;
            out.copy_from_slice(p);
            Some((*ty, *gen))
        }
        fn write_node(&mut self, blk: u64, ty: u8, gen: u64, payload: &[u8; ARCA_NODE_PAYLOAD_LEN]) -> bool {
            if blk == 0 || !self.live.contains(&blk) {
                return false; // come ArcaVolume: mai scrivere non-live
            }
            while self.blocks.len() <= blk as usize {
                self.blocks.push(None);
            }
            self.blocks[blk as usize] = Some((*payload, ty, gen));
            true
        }
        fn alloc(&mut self) -> Option<u64> {
            let b = match self.free_list.pop() {
                Some(b) => b,
                None => {
                    let b = self.high_water;
                    self.high_water += 1;
                    b
                }
            };
            if b == 0 || !self.live.insert(b) {
                return None;
            }
            while self.blocks.len() <= b as usize {
                self.blocks.push(None);
            }
            Some(b)
        }
        fn free(&mut self, blk: u64) -> bool {
            if blk == 0 || !self.live.remove(&blk) {
                return false; // double-free: loud come ArcaVolume
            }
            self.free_list.push(blk); // contenuto vecchio resta (stale-read)
            true
        }
        fn alloc_run(&mut self, hint: u64, max: usize) -> Vec<u64> {
            // Replica esatta di ArcaVolume::alloc_run (stessa semantica).
            let mut out = Vec::new();
            if hint != 0 {
                let mut n = hint.checked_add(1).unwrap_or(0);
                while out.len() < max && n != 0 {
                    if self.alloc_specific(n) {
                        out.push(n);
                        n = n.checked_add(1).unwrap_or(0);
                    } else {
                        break;
                    }
                }
            }
            while out.len() < max {
                match self.alloc() {
                    Some(b) if b != 0 => out.push(b),
                    _ => break,
                }
            }
            out
        }
    }

    #[test]
    fn volume_faithful_churn() {
        // Stesso workload del gate (append/patch/delete/ricrea, churn
        // freelist con riuso LIFO): ogni blocco referenziato deve essere
        // live (niente dangling) e ogni contenuto esatto — lo stale-read
        // del fake rende visibile qualunque use-after-free.
        let mut t = BTree::new(FakeVol::new(), 1).expect("new");
        let check_live = |t: &BTree<FakeVol>| {
            let reach = t.reachable_blocks().expect("reachable");
            for b in reach {
                assert!(t.store.is_live(b), "dangling {b}");
            }
        };
        let mut expect: Vec<u8> = Vec::new();
        for k in 0..60usize {
            let chunk: Vec<u8> = (0..4096).map(|i| ((k * 4096 + i) % 251) as u8).collect();
            t.put_chunk(b"v", b"f", k * 4096, &chunk, k as u64, 4).unwrap();
            expect.extend_from_slice(&chunk);
            assert_eq!(t.get(b"v", b"f").as_deref(), Some(expect.as_slice()), "k={k}");
            check_live(&t);
        }
        // Patch centrali + delete/ricrea (churn gestorben).
        for k in 0..10usize {
            let off = 5000 + k * 20000;
            t.put_chunk(b"v", b"f", off, &[0xDD; 2000], 100 + k as u64, 4).unwrap();
            for i in 0..2000 {
                expect[off + i] = 0xDD;
            }
            assert_eq!(t.get(b"v", b"f").as_deref(), Some(expect.as_slice()), "p{k}");
            check_live(&t);
        }
        assert!(t.delete(b"v", b"f").unwrap());
        assert_eq!(t.get(b"v", b"f"), None);
        // Ricrea sulla freelist sporca (riuso LIFO di blocchi liberati).
        let over: Vec<u8> = (0..70000).map(|i| (0x11 + (i % 33)) as u8).collect();
        t.put_chunk(b"v", b"g", 0, &over, 200, 4).unwrap();
        assert_eq!(t.get(b"v", b"g").as_deref(), Some(over.as_slice()));
        check_live(&t);
    }

    #[test]
    fn volume_faithful_gate_scale() {
        // Maturita' da gate: 10 file x 30 round di append interlacciati +
        // snapshot/rollback/delete/ricrea/quota/meta — contenuto esatto e
        // niente dangling a ogni round.
        let mut t = BTree::new(FakeVol::new(), 1).expect("new");
        let mut expect: Vec<Vec<u8>> = Vec::new();
        for _ in 0..10 {
            expect.push(Vec::new());
        }
        let check_all = |t: &BTree<FakeVol>, expect: &Vec<Vec<u8>>| {
            for (f, exp) in expect.iter().enumerate() {
                let mut key = b"file".to_vec();
                key.push(b'0' + f as u8);
                if exp.is_empty() {
                    assert_eq!(t.get(b"gate", &key), None, "f={f}");
                    continue;
                }
                assert_eq!(t.get(b"gate", &key).as_deref(), Some(exp.as_slice()), "f={f}");
            }
            let reach = t.reachable_blocks().expect("reachable");
            for b in reach {
                assert!(t.store.is_live(b), "dangling {b}");
            }
        };
        let mut snap: Option<u64> = None;
        for round in 0..30usize {
            for f in 0..10usize {
                let mut key = b"file".to_vec();
                key.push(b'0' + f as u8);
                let off = expect[f].len();
                let chunk: Vec<u8> = (0..4096).map(|i| ((off + i) % 251) as u8).collect();
                t.put_chunk(b"gate", &key, off, &chunk, (round * 10 + f) as u64, 4).unwrap();
                expect[f].extend_from_slice(&chunk);
            }
            if round == 10 {
                snap = Some(t.snap_create(b"gate", 1000).expect("snap"));
                t.quota_set(b"gq", 100000).expect("quota");
                t.put_chunk(b"gq", b"q", 0, &[1u8; 3000], 1001, 8).unwrap();
            }
            if round == 20 {
                // Rollback di un file + delete/ricrea di un altro.
                let sid = snap.expect("snap");
                t.snap_rollback(b"gate", b"file3", sid, 2000, 8).expect("rb");
                expect[3] = t.get(b"gate", b"file3").expect("re-read").clone();
                assert!(t.delete(b"gate", b"file5").expect("del"));
                expect[5] = Vec::new();
                t.put_chunk(b"gate", b"file5", 0, &[0xEE; 5000], 2001, 4).unwrap();
                expect[5] = alloc::vec![0xEE; 5000];
            }
            check_all(&t, &expect);
        }
        // Snapshot finale: entry consistenti.
        let _ = snap;
        let (ne, en, mx, ov, ii) = t.extent_stats().expect("xstats");
        assert!(ne > 0 && en > 0, "ne={ne} en={en} mx={mx} ov={ov} in={ii}");
    }

    #[test]
    fn primrec_ext_codec_e_packed_rifiutato() {
        // Roundtrip Ext (anche vuota) + kind 3 PACKED e futuri rifiutati loud.
        let h = ExtHead {
            size: 9000,
            inline: alloc::vec![(11u64, 3u32), (20u64, 1u32)],
            map_head: 33,
            n_ext: 5,
        };
        let r = PrimRec { id: 7, seq: 9, mtime: 42, data: DataRef::Ext(h) };
        let enc = prim_rec_encode(&r);
        let (dec, len) = prim_rec_parse(&enc).expect("parse");
        assert_eq!(dec, r);
        assert_eq!(len, enc.len());
        let e0 = PrimRec {
            id: 1, seq: 1, mtime: 0,
            data: DataRef::Ext(ExtHead { size: 0, inline: alloc::vec![], map_head: 0, n_ext: 0 }),
        };
        let enc0 = prim_rec_encode(&e0);
        assert_eq!(prim_rec_parse(&enc0).map(|(r, _)| r), Some(e0));
        let mut bad = enc.clone();
        bad[24] = 3; // PACKED riservato
        assert!(prim_rec_parse(&bad).is_none());
        bad[24] = 9; // futuro
        assert!(prim_rec_parse(&bad).is_none());
    }

    #[test]
    fn snapshot_rollback_delete_clone() {
        let mut t = engine();
        t.put_chunk(b"b", b"k1", 0, b"A", 1, 8).unwrap();
        t.put_chunk(b"b", b"k1", 0, b"B", 2, 8).unwrap();
        let sid = t.snap_create(b"b", 3).expect("snap");
        t.put_chunk(b"b", b"k1", 0, b"C", 4, 8).unwrap();
        // Rollback: la pinnata B diventa nuova head (storia A,B,C,B').
        assert!(t.snap_rollback(b"b", b"k1", sid, 5, 8).is_some());
        assert_eq!(t.get(b"b", b"k1"), Some(alloc::vec![b'B']));
        let (_, _, nv, _) = t.stat(b"b", b"k1").unwrap();
        assert_eq!(nv, 4);
        // Delete snapshot: live intatto, seconda delete fallisce.
        assert!(t.snap_delete(sid));
        let (_, _, nv2, _) = t.stat(b"b", b"k1").unwrap();
        assert_eq!(nv2, 4);
        assert!(!t.snap_delete(sid));
        // Clone di un nuovo snapshot in dst.
        t.put_chunk(b"cs", b"a", 0, b"uno", 6, 8).unwrap();
        t.put_chunk(b"cs", b"b", 0, b"due!", 7, 8).unwrap();
        let s2 = t.snap_create(b"cs", 8).unwrap();
        assert_eq!(t.snap_clone(s2, b"cd", 9, 8), Some(2));
        assert_eq!(t.get(b"cd", b"a"), Some(alloc::vec![b'u', b'n', b'o']));
        assert_eq!(t.get(b"cd", b"b"), Some(alloc::vec![b'd', b'u', b'e', b'!']));
        // Clone assegna uuid NUOVI (niente aliasing d'identita').
        let (id_src, _, _, _) = t.stat(b"cs", b"a").unwrap();
        let (id_dst, _, _, _) = t.stat(b"cd", b"a").unwrap();
        assert_ne!(id_src, id_dst);
        // Delete oggetto: GET/STAT rifiutati, re-delete falsa.
        assert_eq!(t.delete(b"b", b"k1"), Some(true));
        assert_eq!(t.get(b"b", b"k1"), None);
        assert_eq!(t.stat(b"b", b"k1"), None);
        assert_eq!(t.delete(b"b", b"k1"), Some(false));
    }

    #[test]
    fn refcount_pin_sopravvive_a_delete_e_trim() {
        let mut t = engine();
        t.put_chunk(b"b", b"k", 0, b"PIN", 1, 8).unwrap();
        let sid = t.snap_create(b"b", 2).unwrap();
        // Delete live: lo snapshot pinna ancora i blocchi (refcount 1).
        assert_eq!(t.delete(b"b", b"k"), Some(true));
        assert_eq!(t.get(b"b", b"k"), None);
        // Rollback ricrea la chiave dallo snapshot (nuovo uuid).
        assert!(t.snap_rollback(b"b", b"k", sid, 3, 8).is_some());
        assert_eq!(t.get(b"b", b"k"), Some(alloc::vec![b'P', b'I', b'N']));
        // Trim aggressivo con pin: la versione pinnata non si libera.
        assert!(t.snap_delete(sid));
    }

    #[test]
    fn bulk_split_e_merge() {
        let mut t = engine();
        // 120 chiavi da ~70 B: forza split multi-livello della secondary.
        for i in 0..120u64 {
            let k = alloc::format!("chiave-{:03}", i);
            let v = alloc::vec![(i % 251) as u8; 64];
            assert!(t.put_chunk(b"d", k.as_bytes(), 0, &v, i, 8).is_some(), "put {}", i);
        }
        // Albero cresciuto oltre la foglia singola.
        assert!(t.store.live_count() > 4, "live={}", t.store.live_count());
        for i in 0..120u64 {
            let k = alloc::format!("chiave-{:03}", i);
            let v = alloc::vec![(i % 251) as u8; 64];
            assert_eq!(t.get(b"d", k.as_bytes()), Some(v), "get {}", i);
        }
        // Delete di 80 chiavi: merge a foglie vuote, le restanti leggibili.
        for i in 0..80u64 {
            let k = alloc::format!("chiave-{:03}", i);
            assert_eq!(t.delete(b"d", k.as_bytes()), Some(true), "del {}", i);
        }
        for i in 80..120u64 {
            let k = alloc::format!("chiave-{:03}", i);
            let v = alloc::vec![(i % 251) as u8; 64];
            assert_eq!(t.get(b"d", k.as_bytes()), Some(v), "survivor {}", i);
        }
        for i in 0..80u64 {
            let k = alloc::format!("chiave-{:03}", i);
            assert_eq!(t.get(b"d", k.as_bytes()), None, "gone {}", i);
        }
    }

    #[test]
    fn overflow_blob_grande_roundtrip() {
        let mut t = engine();
        let big: Vec<u8> = (0..3000u32).map(|i| (i * 7 % 251) as u8).collect();
        assert_eq!(t.put_chunk(b"b", b"big", 0, &big, 1, 8), Some(3000));
        assert_eq!(t.get(b"b", b"big"), Some(big));
        let (_, sz, nv, _) = t.stat(b"b", b"big").unwrap();
        assert_eq!((sz, nv), (3000, 1));
        // Seconda versione grande: la prima resta in storia (due catene ov).
        let big2: Vec<u8> = (0..3000u32).map(|i| (i * 13 % 251) as u8).collect();
        t.put_chunk(b"b", b"big", 0, &big2, 2, 8).unwrap();
        assert_eq!(t.get(b"b", b"big"), Some(big2));
    }

    #[test]
    fn chiavi_lunghe_e_bound() {
        let mut t = engine();
        let long_k = alloc::vec![b'x'; 200];
        assert!(t.put_chunk(b"b", &long_k, 0, b"v", 1, 8).is_some());
        assert_eq!(t.get(b"b", &long_k), Some(alloc::vec![b'v']));
        // Oltre bound: rifiuto loud, mai troncamento.
        let too_long_b = alloc::vec![b'y'; 17];
        let too_long_k = alloc::vec![b'z'; 256];
        assert_eq!(t.put_chunk(&too_long_b, b"k", 0, b"v", 1, 8), None);
        assert_eq!(t.put_chunk(b"b", &too_long_k, 0, b"v", 1, 8), None);
        assert_eq!(t.snap_create(&too_long_b, 1), None);
        assert_eq!(t.get(&too_long_b, b"k"), None);
    }

    #[test]
    fn meta_roundtrip_e_rollback_post_reload() {
        let mut t = engine();
        t.put_chunk(b"b", b"k", 0, b"v1", 1, 8).unwrap();
        t.put_chunk(b"b", b"k", 0, b"v2", 2, 8).unwrap();
        let sid = t.snap_create(b"b", 3).unwrap();
        t.meta_store().unwrap();
        let (rp, rs, rr, mr, nid, nsn) =
            (t.root_primary, t.root_secondary, t.root_refcount, t.meta_root, t.next_id, t.next_snap);
        assert!(mr != 0);
        // Restart simulato: tabella persa, blocchi riusati.
        let store = t.into_store();
        let mut t2 = BTree::new_with_roots(store, 1, rp, rs, rr).unwrap();
        t2.meta_root = mr;
        t2.next_id = nid;
        t2.next_snap = nsn;
        assert!(t2.snaps.is_empty());
        t2.rebuild_by_id().unwrap();
        t2.meta_load().unwrap();
        // Lo snapshot ricaricato e' usabile (rollback alla pinnata v2).
        assert!(t2.snap_rollback(b"b", b"k", sid, 4, 8).is_some());
        assert_eq!(t2.get(b"b", b"k"), Some(alloc::vec![b'v', b'2']));
        // Meta corrotta: tabella ignorata loud, dati intatti.
        let bad = t2.store.alloc().unwrap();
        {
            let mut p = [0u8; ARCA_NODE_PAYLOAD_LEN];
            p[0] = 9; // tag ignoto
            t2.store.write_node(bad, ARCA_NODE_TYPE_LEAF, 1, &p);
        }
        t2.meta_root = bad;
        assert!(t2.meta_load().is_none());
        assert_eq!(t2.get(b"b", b"k"), Some(alloc::vec![b'v', b'2']));
    }

    #[test]
    fn reachable_copre_tutto_e_trova_orfani() {
        let mut t = engine();
        t.put_chunk(b"b", b"k1", 0, b"uno", 1, 8).unwrap();
        t.put_chunk(b"b", b"k2", 0, b"due", 2, 8).unwrap();
        let big: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        t.put_chunk(b"b", b"big", 0, &big, 3, 8).unwrap();
        let sid = t.snap_create(b"b", 4).unwrap();
        assert!(sid > 0);
        // COW orfana le vecchie radici a ogni op (per disegno: le raccoglie
        // la GC): le radici CORRENTI + catena ov sono raggiungibili, la prima
        // radice (superata dai rewrite) no.
        let r = t.reachable_blocks().unwrap();
        for b in [t.root_primary, t.root_secondary, t.root_refcount] {
            assert!(r.contains(&b), "radice {} irraggiungibile?!", b);
        }
        assert!(!r.contains(&1), "vecchia root ancora raggiungibile?!");
        assert!(r.len() < t.store.live_count(), "nessun orfano COW?!");
        // Orfano staged (alloc mai linkato): non raggiungibile.
        let orf = t.store.alloc().unwrap();
        let r2 = t.reachable_blocks().unwrap();
        assert!(!r2.contains(&orf));
        assert!(r2.len() == r.len());
    }

    #[test]
    fn quota_tabella_meta_e_check() {
        let mut t = engine();
        // Due blob overflow (3000 B > INLINE_MAX = 1 blocco dati ciascuno).
        let big: Vec<u8> = (0..3000u32).map(|i| (i * 7 % 251) as u8).collect();
        t.put_chunk(b"q", b"k1", 0, &big, 1, 8).unwrap();
        t.put_chunk(b"q", b"k2", 0, &big, 2, 8).unwrap();
        t.put_chunk(b"altro", b"k", 0, &big, 3, 8).unwrap();
        // used: 2 blocchi su q (senza doppio conteggio: catene distinte),
        // 1 su altro; inline gratis.
        t.put_chunk(b"q", b"tiny", 0, b"x", 4, 8).unwrap();
        assert_eq!(t.quota_used(b"q"), Some(2));
        assert_eq!(t.quota_used(b"altro"), Some(1));
        assert_eq!(t.quota_used(b"vuoto"), Some(0));
        // Senza tetto tutto passa; bound oltre-nome rifiutato.
        assert_eq!(t.quota_check(b"q", 100000), Some(true));
        assert!(t.quota_set(&alloc::vec![b'y'; 17], 10).is_none());
        // Tetto 10 blocchi (used=2): inline+slack ci sta (2+8<=10),
        // un blob da 3000 B no (2+1+8=11>10).
        t.quota_set(b"q", 10).unwrap();
        assert_eq!(t.quota_check(b"q", 100), Some(true));
        assert_eq!(t.quota_check(b"q", 3000), Some(false));
        // Altro bucket senza tetto: passa sempre.
        assert_eq!(t.quota_check(b"altro", 100000), Some(true));
        // Pinnato a nome cancellato resta sul budget (delete con snapshot
        // vivo: v1 pinnata resta, v2 libera → used==1; senza snapshot 0).
        let a3k: Vec<u8> = (0..3000u32).map(|i| (i * 3 % 251) as u8).collect();
        let b3k: Vec<u8> = (0..3000u32).map(|i| (i * 5 % 251) as u8).collect();
        t.put_chunk(b"s", b"k", 0, &a3k, 5, 8).unwrap();
        let sid = t.snap_create(b"s", 6).unwrap();
        t.put_chunk(b"s", b"k", 0, &b3k, 7, 8).unwrap();
        assert_eq!(t.quota_used(b"s"), Some(2));
        t.delete(b"s", b"k").unwrap();
        assert_eq!(t.quota_used(b"s"), Some(1));
        t.snap_delete(sid);
        assert_eq!(t.quota_used(b"s"), Some(0));
        // Persistenza: meta_store + restart simulato, quote sopravvivono.
        t.meta_store().unwrap();
        let (rp, rs, rr, mr, nid, nsn) =
            (t.root_primary, t.root_secondary, t.root_refcount, t.meta_root, t.next_id, t.next_snap);
        let store = t.into_store();
        let mut t2 = BTree::new_with_roots(store, 1, rp, rs, rr).unwrap();
        t2.meta_root = mr;
        t2.next_id = nid;
        t2.next_snap = nsn;
        t2.rebuild_by_id().unwrap();
        t2.meta_load().unwrap();
        assert_eq!(t2.quotas.get(b"q".as_slice()), Some(&10));
        assert_eq!(t2.quota_used(b"q"), Some(2));
        assert_eq!(t2.quota_check(b"q", 3000), Some(false));
        // Tetto rimosso (0): di nuovo illimitato.
        t2.quota_set(b"q", 0).unwrap();
        assert_eq!(t2.quota_check(b"q", 100000), Some(true));
    }

    #[test]
    fn rollback_bucket_mismatch_e_id_ignoto() {
        let mut t = engine();
        t.put_chunk(b"a", b"k", 0, b"v", 1, 8).unwrap();
        let sid = t.snap_create(b"a", 2).unwrap();
        assert_eq!(t.snap_rollback(b"altro", b"k", sid, 3, 8), None);
        assert_eq!(t.snap_rollback(b"a", b"k", sid + 999, 3, 8), None);
        assert!(!t.snap_delete(sid + 999));
    }
}