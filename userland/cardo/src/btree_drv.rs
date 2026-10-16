//! Motore ArcaFS su disco (Fase 56.2b): B+tree COW + commit shadow+flip.
//!
//! `VolumeStore` adatta `ArcaVolume` al trait `arcafs::btree::BlockStore`
//! (POSSIEDE il volume: un solo handle, niente divergenze freelist/xh; I/O
//! diretto senza cache — vedi sotto). I nodi viaggiano in `Box` (mai array
//! KiB per-valore nel loop, regola §18). `bind` inizializza il volume fresco
//! o carica uno inizializzato (radici da superblock, id da header-ext,
//! `by_id` ricostruito); `commit` persiste header-ext + shadow + flip a OGNI
//! op mutante (ordine §3, niente journal). COW + root-last: a crash pre-flip
//! la generazione vecchia e' intatta (blocchi nuovi orfani per la GC 56.2c).
//!
//! Limiti dichiarati 56.2b (chiusura in 56.2c): la tabella snapshot vive in
//! RAM nel motore (dopo kill/restart i pin si perdono, i DATI no); lo
//! scaffold RAW resta usabile dopo il bind ma ALLOC/FREE post-bind sono
//! sconsigliati (freelist condivisa: corretti, ma sorprendenti).

use super::*;
use arcafs::btree::{BlockStore, BTree, TREE_PRIMARY, TREE_REFCOUNT, TREE_SECONDARY};
use arcafs::format::{
    self, ARCA_NODE_PAYLOAD_LEN, ARCA_NODE_TYPE_DATA, ARCA_NODE_TYPE_INTERNAL,
    ARCA_NODE_TYPE_LEAF, ARCA_NODE_TYPE_MAP, ARCA_NODE_TYPE_RAW, ARCA_XH_DIRTY,
};
use alloc::boxed::Box;

/// Blocco della secondary root al fresh-init (convenzione 56.2b:
/// deterministico a volume fresco; DOPO si sposta come le altre radici e
/// vive nel superblock `alloc_hint` — mai assumere il 2 a load).
pub const SECONDARY_ROOT: u64 = 2;
/// Versioni trattenute per oggetto (56.1: 8; A3 con la quota vera rivaluta).
const RETAIN: usize = 8;

/// Adattatore volume → `BlockStore`: possiede `ArcaVolume` (un solo handle)
/// + cache nodi write-through (W3, sotto: tuning coi numeri arrivato).
// ── Cache nodi write-through (W3, ADR-0051) ─────────────────────────────
// Un solo livello, qui (mai nel motore: niente double buffering). Chiave =
// numero blocco; classi inferite dal tipo nodo (LEAF/INTERNAL/MAP = metadati
// R0, RAW/DATA = dati R2); frequenza MFU a 2 bit con aging; LRU a contatore
// (niente clock). Write-through (mai dirty): l'evict e' un drop — reclaim
// banale, niente flush (il write-back ordinato e' lavoro delle epoche W4).
// Coerenza: i blocchi scritti non mutano mai (invariante W1; solo radici/
// xhdr/freelist mutano, via API settori che bypassa la cache); `free`
// sfratta (i numeri riciclati non devono mai servire stale).

/// Classe retain (il tipo decide la fascia, la frequenza promuove a caldo;
/// classe 2 PIN riservata al futuro hook `pin()`: mai sfrattata).
const CACHE_META: u8 = 0; // R0: nodi indice/mappe, sfrattati per ultimi
const CACHE_DATA: u8 = 1; // R2: dati, vittime prime

/// Fascia dal tipo nodo (politica, mai semantica: i futuri restano cold).
fn node_class(ty: u8) -> u8 {
    match ty {
        ARCA_NODE_TYPE_LEAF | ARCA_NODE_TYPE_INTERNAL | ARCA_NODE_TYPE_MAP => CACHE_META,
        ARCA_NODE_TYPE_RAW | ARCA_NODE_TYPE_DATA => CACHE_DATA, // legacy/scaffold + extent
        _ => CACHE_DATA, // tipi futuri: cold di default
    }
}

struct CacheEntry {
    ty: u8,
    generation: u64,
    payload: Box<[u8; ARCA_NODE_PAYLOAD_LEN]>,
    class: u8,
    freq: u8, // 0..3 saturante
    last: u64, // LRU logico
}

struct NodeCache {
    map: BTreeMap<u64, CacheEntry>,
    cap: usize,
    clock: u64,
    hits: u64,
    reads: u64,
}

impl NodeCache {
    /// Cap da `SYS_MEMINFO` (1/8 della libera in blocchi, [256..16384]):
    /// cresce da sola fino al tetto via LRU, mai oltre.
    fn new() -> Self {
        let (free, _, _) = civis::meminfo();
        let blocks = free.saturating_mul(4096) / 8 / arcafs::format::ARCA_BLOCK_SIZE as u64;
        Self { map: BTreeMap::new(), cap: (blocks as usize).clamp(256, 16384), clock: 0, hits: 0, reads: 0 }
    }

    fn get(&mut self, blk: u64, out: &mut [u8; ARCA_NODE_PAYLOAD_LEN]) -> Option<(u8, u64)> {
        self.reads += 1;
        self.clock += 1;
        let clock = self.clock;
        match self.map.get_mut(&blk) {
            Some(e) => {
                self.hits += 1;
                e.freq = (e.freq + 1).min(3);
                e.last = clock;
                out.copy_from_slice(&e.payload[..]);
                Some((e.ty, e.generation))
            }
            None => None,
        }
    }

    /// Write-through: aggiorna/inserisce (mai dirty). Sfratta prima se pieno.
    fn put(&mut self, blk: u64, ty: u8, generation: u64, payload: &[u8; ARCA_NODE_PAYLOAD_LEN]) {
        self.clock += 1;
        let clock = self.clock;
        match self.map.get_mut(&blk) {
            Some(e) => {
                e.ty = ty;
                e.generation = generation;
                e.payload.copy_from_slice(payload);
                e.class = node_class(ty);
                e.freq = (e.freq + 1).min(3);
                e.last = clock;
            }
            None => {
                if self.map.len() >= self.cap {
                    self.evict_one();
                }
                let mut p = format::boxed_node();
                p.copy_from_slice(payload);
                self.map.insert(
                    blk,
                    CacheEntry { ty, generation, payload: p, class: node_class(ty), freq: 1, last: clock },
                );
            }
        }
    }

    /// Vittima: classe max prima (DATA fuori, META resta), poi freq min,
    /// poi meno recente. Aging: dimezza freq a ogni scansione.
    fn evict_one(&mut self) {
        for e in self.map.values_mut() {
            e.freq /= 2;
        }
        let mut victim: Option<u64> = None;
        let mut b_class = 0u8;
        let mut b_freq = 0u8;
        let mut b_last = u64::MAX;
        let mut first = true;
        for (blk, e) in self.map.iter() {
            let worse = first
                || e.class > b_class
                || (e.class == b_class && (e.freq < b_freq || (e.freq == b_freq && e.last < b_last)));
            if worse {
                victim = Some(*blk);
                b_class = e.class;
                b_freq = e.freq;
                b_last = e.last;
                first = false;
            }
        }
        if let Some(b) = victim {
            self.map.remove(&b);
        }
    }

    fn evict(&mut self, blk: u64) {
        self.map.remove(&blk);
    }
}

/// Adattatore volume → `BlockStore`: possiede `ArcaVolume` (un solo handle,
/// freelist/xh mai divergenti) + cache nodi (sopra). Ogni accesso nodo
/// passa dalla cache (coerente per costruzione: write-through + sfratto
/// alla free); settori/xhdr/superblock la bypassano (sempre freschi).
pub struct VolumeStore {
    vol: crate::volume::ArcaVolume,
    cache: core::cell::RefCell<NodeCache>,
}

/// Motore disco: B+tree sopra `VolumeStore` (tutto owned, niente lifetime).
pub type DiskEngine = BTree<VolumeStore>;

impl VolumeStore {
    pub fn new(vol: crate::volume::ArcaVolume) -> Self {
        Self { vol, cache: core::cell::RefCell::new(NodeCache::new()) }
    }

    /// (hit, letture) cache nodi per XSTATS (W3 tuning).
    #[inline(never)]
    pub fn cache_stats(&self) -> (u64, u64) {
        let c = self.cache.borrow();
        (c.hits, c.reads)
    }

    /// Volume per le op RAW di scaffold e il commit (stesso handle del
    /// motore: freelist/xh mai divergenti).
    #[inline(never)]
    pub fn vol_mut(&mut self) -> &mut crate::volume::ArcaVolume {
        &mut self.vol
    }

    /// Volume in lettura (radici, id, stats).
    #[inline(never)]
    pub fn vol(&self) -> &crate::volume::ArcaVolume {
        &self.vol
    }

    /// Generation dal superblock (Fase A: diagnostica per lo stub del mount
    /// root, prima di legare il motore).
    #[inline(never)]
    pub fn generation(&self) -> Option<u64> {
        self.vol.generation()
    }
}

impl BlockStore for VolumeStore {
    #[inline(never)]
    fn read_node(
        &self,
        blk: u64,
        out: &mut [u8; ARCA_NODE_PAYLOAD_LEN],
    ) -> Option<(u8, u64)> {
        if let Some(v) = self.cache.borrow_mut().get(blk, out) {
            return Some(v);
        }
        let r = self.vol.read_node(blk, out)?;
        self.cache.borrow_mut().put(blk, r.0, r.1, out);
        Some(r)
    }

    #[inline(never)]
    fn write_node(
        &mut self,
        blk: u64,
        ty: u8,
        generation: u64,
        payload: &[u8; ARCA_NODE_PAYLOAD_LEN],
    ) -> bool {
        // Write-through: prima il disco (a errore niente cache), poi la entry.
        if !self.vol.write_node(blk, ty, generation, payload) {
            return false;
        }
        self.cache.borrow_mut().put(blk, ty, generation, payload);
        true
    }

    #[inline(never)]
    fn alloc(&mut self) -> Option<u64> {
        self.vol.alloc()
    }

    #[inline(never)]
    fn free(&mut self, blk: u64) -> bool {
        // Sfratto prima: il numero riciclato non deve mai servire stale.
        self.cache.borrow_mut().evict(blk);
        self.vol.free(blk)
    }

    fn alloc_run(&mut self, hint: u64, max: usize) -> alloc::vec::Vec<u64> {
        self.vol.alloc_run(hint, max)
    }
}

/// Lega il motore al volume (consuma `vol` dallo scaffold RAW — lazy: DOPO
/// i test 22-27, mai prima). Volume fresco (radici 0) = init con secondary
/// fissa al blocco 2; volume inizializzato = validazione tipo+tag e load
/// (id da header-ext, `by_id` ricostruito, snapshot persi: vedi sopra).
/// DIRTY acceso → log + prosegui (orphan-GC in 56.2c, mai blocco del mount).
#[inline(never)]
pub fn bind(mut store: VolumeStore) -> Option<DiskEngine> {
    let mut sec0 = [0u8; 512];
    if !store.vol().read_raw_sector(0, &mut sec0) {
        println!("[cardo] arca-disk: bind fallito (read superblock)");
        return None;
    }
    let (generation, _uuid, primary, secondary_sb, refcount) = match format::superblock_roots(&sec0) {
        Some(r) => r,
        None => {
            println!("[cardo] arca-disk: bind fallito (superblock invalido)");
            return None;
        }
    };
    if store.vol().xh_flags() & ARCA_XH_DIRTY != 0 {
        println!("[cardo] arca-disk: DIRTY acceso, orphan-GC rimandata (56.2c)");
    }
    // Freschezza: i volumi `arca create`/56.2a hanno ROOT=1 su un nodo RAW
    // vuoto (legacy) — NON sono inizializzati 56.2b. Solo radici
    // LEAF/INTERNAL col tag giusto contano come load (il tag si verifica
    // sotto); lettura fallita o tipo ignoto = loud, mai init sopra dati veri.
    let fresh = if primary == 0 {
        true
    } else {
        // Probe heap senza temp stack (regola §18: `Box::new([0u8; N])`
        // costruisce 3.5K sullo stack prima del move — qui e' bastato a
        // sfondare i 16 KiB: #PF osservato al primo bind).
        // NOTA: costanti SEMPRE fully-qualified qui — il nome bare
        // `ARCA_NODE_TYPE_RAW` in match-pattern risolveva male (osservato:
        // ty=2 scambiato per RAW, load mai raggiunto; con `arcafs::format::`
        // il load va ok). Mai bare in questo file.
        let mut probe = format::boxed_node();
        let (pty, _) = match store.read_node(primary, &mut probe) {
            Some(v) => v,
            None => return None,
        };
        if pty == arcafs::format::ARCA_NODE_TYPE_RAW {
            true
        } else if pty == arcafs::format::ARCA_NODE_TYPE_LEAF
            || pty == arcafs::format::ARCA_NODE_TYPE_INTERNAL
        {
            false
        } else {
            return None;
        }
    };
    if fresh {
        // Volume fresco: secondary fissa (sganciata dalla freelist o cima),
        // poi primary + refcount. DIRTY durante l'init (crash = re-init pulito
        // con leak orfani per la GC 56.2c, mai mezze radici linkate). Le
        // radici legacy nel superblock vengono sovrascritte al primo commit.
        let vol = store.vol_mut();
        let flags = vol.xh_flags();
        if !vol.set_xh_flags(flags | ARCA_XH_DIRTY) {
            return None;
        }
        if !vol.alloc_specific(SECONDARY_ROOT) {
            return None;
        }
        let rp = vol.alloc()?;
        let rr = vol.alloc()?;
        if rp == 0 || rr == 0 {
            return None;
        }
        let mut eng = BTree::new_with_roots(store, generation, rp, SECONDARY_ROOT, rr)?;
        eng.init_empty_roots()?;
        if !eng.store.vol_mut().sync_ids(1, 1) {
            return None;
        }
        if !commit(&mut eng) {
            return None;
        }
        println!("[cardo] arca-disk: init ok");
        Some(eng)
    } else {
        // Load: valida tipo + tag delle tre radici PERSISTITE prima di
        // fidarsi (la secondary si e' spostata a ogni split: il blocco 2 e'
        // solo il valore iniziale, mai quello corrente). Ogni ramo di
        // fallimento logga distinto (fail-loud: il bind e' sul percorso
        // di boot e i None silenti non sono diagnosticabili).
        if secondary_sb == 0 {
            println!("[cardo] arca-disk: bind load fallito (secondary 0)");
            return None;
        }
        for (blk, want) in
            [(primary, TREE_PRIMARY), (secondary_sb, TREE_SECONDARY), (refcount, TREE_REFCOUNT)]
        {
            let mut p = format::boxed_node();
            let (ty, _) = match store.read_node(blk, &mut p) {
                Some(v) => v,
                None => {
                    println!("[cardo] arca-disk: bind load fallito (read radice {})", blk);
                    return None;
                }
            };
            if ty != ARCA_NODE_TYPE_LEAF && ty != ARCA_NODE_TYPE_INTERNAL {
                println!("[cardo] arca-disk: bind load fallito (tipo radice {} = {})", blk, ty);
                return None;
            }
            if p[0] != want {
                println!("[cardo] arca-disk: bind load fallito (tag radice {})", blk);
                return None;
            }
        }
        let mut eng = match BTree::new_with_roots(store, generation, primary, secondary_sb, refcount) {
            Some(e) => e,
            None => {
                println!("[cardo] arca-disk: bind load fallito (new_with_roots)");
                return None;
            }
        };
        let (nid, nsn) = eng.store.vol().ids();
        if nid == 0 || nsn == 0 {
            println!("[cardo] arca-disk: bind load fallito (id header-ext degeneri)");
            return None; // header-ext corrotta: mai id degeneri (F2)
        }
        eng.next_id = nid;
        eng.next_snap = nsn;
        if eng.rebuild_by_id().is_none() {
            println!("[cardo] arca-disk: bind load fallito (rebuild_by_id)");
            return None;
        }
        // Tabella snapshot: meta assente (0) o illeggibile = tabella vuota,
        // loud ma mount avanti (disponibilita' prima di retention). Poi
        // next_snap oltre il max persistito (mai riuso sid, F2).
        eng.meta_root = format::superblock_meta(&sec0);
        if eng.meta_root != 0 && eng.meta_load().is_none() {
            println!("[cardo] arca-disk: tabella snapshot illeggibile, monto senza snapshot");
            eng.meta_root = 0;
            eng.snaps.clear();
        }
        let mut max_sid = 0u64;
        for &sid in eng.snaps.keys() {
            if sid > max_sid {
                max_sid = sid;
            }
        }
        if let Some(bump) = max_sid.checked_add(1) {
            if eng.next_snap < bump {
                eng.next_snap = bump;
            }
        }
        // Orphan-GC SEMPRE al load-bind (anche pulito: superset della spec,
        // deterministico senza dipendere dal timing del kill), poi commit
        // (chiude DIRTY e fissa la generazione di recovery). S1.2: il walk
        // e' CPU-only (niente I/O per blocco), scala ai GB senza costi.
        let n_orph = match gc_run(&mut eng) {
            Some(n) => n,
            None => {
                println!("[cardo] arca-disk: bind load fallito (orphan-GC)");
                return None;
            }
        };
        if !commit(&mut eng) {
            println!("[cardo] arca-disk: bind load fallito (commit recovery)");
            return None;
        }
        if n_orph > 0 {
            println!("[cardo] arca-disk: load ok, orfani recuperati");
        } else {
            println!("[cardo] arca-disk: load ok");
        }
        Some(eng)
    }
}

/// Orphan-GC (56.2c): raggiungibili dai 3 alberi (+ meta) meno freelist,
/// meno guardia live, meno blocco 0 → push in freelist. Ritorna gli orfani
/// recuperati. Un blocco illeggibile (torn write senza journal) conta come
/// orfano: irrecuperabile comunque, meglio riusabile che perso.
/// Mai dentro: blocchi live-guard (leftover RAW mai sganciati: leak sicuro,
/// mai double-push che corromperebbe la catena).
#[inline(never)]
fn gc_collect(eng: &mut DiskEngine) -> Option<Vec<u64>> {
    let mut reach = eng.reachable_blocks()?;
    if eng.meta_root != 0 {
        reach.push(eng.meta_root);
    }
    reach.sort_unstable();
    reach.dedup();
    let mut free = eng.store.vol().freelist_blocks();
    free.sort_unstable();
    let high = eng.store.vol().stats().0;
    // S1.2: niente probe di lettura per blocco (era I/O pura sprecata: la
    // decisione non dipende dalla leggibilita' — irraggiungibile = orfano
    // comunque). Il walk resta CPU-only (sort + binary search) e scala a
    // volumi da GB; la GC resta SEMPRE al load-bind come da 56.2c.
    let mut orphans = Vec::new();
    let mut n = 1u64;
    while n < high {
        let known = eng.store.vol().is_live(n)
            || reach.binary_search(&n).is_ok()
            || free.binary_search(&n).is_ok();
        if !known {
            orphans.push(n);
        }
        n += 1;
    }
    orphans.sort_unstable();
    orphans.dedup();
    Some(orphans)
}

/// Esegue la GC: colleziona e spinge in freelist. Ritorna il conteggio.
#[inline(never)]
fn gc_run(eng: &mut DiskEngine) -> Option<usize> {
    let orphans = gc_collect(eng)?;
    if orphans.is_empty() {
        return Some(0);
    }
    if !eng.store.vol_mut().gc_push_free_list(&orphans) {
        return None;
    }
    Some(orphans.len())
}

/// Commit (§3, 56.2b/c): header-ext (id + DIRTY) → shadow → flip superblock
/// (gen+1, radici + meta in UN colpo, mai generazioni diverse) → clear DIRTY.
/// Nodi + refcount sono gia' write-through a ogni op btree. Fallimento =
/// loud, mai mezze scritture linkate (COW + root-last: pre-flip la vecchia
/// generazione e' intatta).
#[inline(never)]
pub fn commit(eng: &mut DiskEngine) -> bool {
    let (nid, nsn, rp, rs, rr, meta) = (
        eng.next_id,
        eng.next_snap,
        eng.root_primary,
        eng.root_secondary,
        eng.root_refcount,
        eng.meta_root,
    );
    let vol = eng.store.vol_mut();
    let flags = vol.xh_flags();
    if !vol.store_meta(nid, nsn, flags | ARCA_XH_DIRTY) {
        return false;
    }
    let mut sec = [0u8; 512];
    if !vol.read_raw_sector(0, &mut sec) {
        return false;
    }
    let (generation, _, _, _, _) = match format::superblock_roots(&sec) {
        Some(g) => g,
        None => return false,
    };
    let gen1 = match generation.checked_add(1) {
        Some(g) => g,
        None => return false,
    };
    if format::superblock_set_roots(&mut sec, gen1, rp, rs, rr, meta).is_none() {
        return false;
    }
    if !vol.write_raw_sector(1, &sec) {
        return false; // shadow
    }
    if !vol.write_raw_sector(0, &sec) {
        return false; // flip
    }
    vol.store_meta(nid, nsn, flags & !ARCA_XH_DIRTY)
}

/// Lega il motore al volume (consuma `dbgvol`, lazy dopo lo scaffold RAW).
/// Ritorna `Some(true)` se legato ORA (il chiamante seedda `sys`), `Some(false)`
/// se era gia' legato (idempotente, niente re-seed), `None` a volume assente
/// o bind fallito (loud, mai meta' stato).
#[inline(never)]
pub fn set_backend(
    dbgvol: &mut Option<VolumeStore>,
    disk: &mut Option<DiskEngine>,
) -> Option<bool> {
    if disk.is_some() {
        return Some(false);
    }
    let store = dbgvol.take()?;
    *disk = Some(bind(store)?);
    Some(true)
}

/// Scrive un oggetto intero a offset 0 SENZA commit (seed al bind: il
/// chiamante fa UN commit a fine seed, non uno per file).
/// `#[inline(never)]`: vedi `seed_sys_disk` (firewall catena btree).
#[inline(never)]
pub fn seed_put(
    eng: &mut DiskEngine,
    bucket: &[u8],
    key: &[u8],
    data: &[u8],
) -> Option<usize> {
    if bucket.len() > civis::OBJ_BUCKET_MAX || key.len() > civis::OBJ_KEY_MAX {
        return None;
    }
    eng.put_chunk(bucket, key, 0, data, crate::wall::wall_secs(), RETAIN)
}

// ── Handler disco (stesso wire/reply del mem, backend blocchi + commit) ──
// `INVALID` oltre bound (mai troncamento), `NOTFOUND` a chiave assente,
// `ERR` a IO fallito — stesse sentinelle dei fratelli in `handlers.rs`.

#[inline(never)]
fn bounds_invalid(bucket: &[u8], key: &[u8]) -> bool {
    bucket.len() > civis::OBJ_BUCKET_MAX || key.len() > civis::OBJ_KEY_MAX
}

#[inline(never)]
fn commit_or(eng: &mut DiskEngine, v: u64) -> Result<u64, u64> {
    if commit(eng) {
        Ok(v)
    } else {
        Err(ERR)
    }
}

/// Cancello quota (A3, ADR-0044): il bucket puo' accogliere `add_bytes`?
/// Oltre budget → `ERR_NOSPC` PRIMA di allocare (mai mezze scritte);
/// misura impossibile → `ERR` (fallire chiuso, mai over-budget silenzioso).
#[inline(never)]
fn quota_gate(eng: &DiskEngine, bucket: &[u8], add_bytes: usize) -> Result<(), u64> {
    match eng.quota_check(bucket, add_bytes) {
        Some(true) => Ok(()),
        Some(false) => Err(ERR_NOSPC),
        None => Err(ERR),
    }
}

/// PUT chunk a `offset` (0 = nuova versione, >0 = patch): sempre commit.
/// Gated quota sul blob risultante (`offset+len` conservativo per le patch:
/// i blocchi vecchi sono ancora in `used`, il trim liberera' dopo).
#[inline(never)]
pub fn disk_put(eng: &mut DiskEngine, payload: &[u8], offset: usize) -> Result<u64, u64> {
    let (bucket, key, data) = arcafs::wire::parse_obj_prefix(payload).ok_or(ERR_INVALID)?;
    if bounds_invalid(bucket, key) {
        return Err(ERR_INVALID);
    }
    let total = offset.checked_add(data.len()).ok_or(ERR_INVALID)?;
    quota_gate(eng, bucket, total)?;
    let n = eng
        .put_chunk(bucket, key, offset, data, crate::wall::wall_secs(), RETAIN)
        .ok_or(ERR)?;
    commit_or(eng, n as u64)
}

/// GET stateless con chunking (stessa disciplina anti-desync del mem).
/// Scrive il frame SOLO a successo; a errore ritorna la sentinella senza
/// scrivere — il deny-frame lo scrive il dispatch (contratto "un solo frame
/// per op", test-6: prima lo scriveva qui e il dispatch non doveva
/// duplicarlo).
#[inline(never)]
pub fn disk_get(
    eng: &mut DiskEngine,
    payload: &[u8],
    offset: usize,
    count: usize,
) -> Result<u64, u64> {
    let (bucket, key, _) = arcafs::wire::parse_obj_prefix(payload).ok_or(ERR_INVALID)?;
    if bounds_invalid(bucket, key) {
        return Err(ERR_INVALID);
    }
    // S2.5f/F0: range read con cursore (mai il blob intero: su oggetti da
    // decine di MB il materializza-e-slice era quadratico per slice).
    // Stesso contratto di prima: w0 = len totale, payload = slice.
    match eng.get_range(bucket, key, offset, count) {
        Some((len, data)) => {
            rings::resp_ring_write(len as u64, 0, &data);
            Ok(len as u64)
        }
        None => Err(ERR_NOTFOUND),
    }
}

/// GET per object_id (stessa disciplina del GET).
#[inline(never)]
pub fn disk_get_id(
    eng: &mut DiskEngine,
    payload: &[u8],
    offset: usize,
    count: usize,
) -> Result<u64, u64> {
    let id = arcafs::wire::parse_u64(payload).ok_or(ERR_INVALID)?;
    // S2.5f/F0: vedi disk_get (range, mai blob intero).
    match eng.get_id_range(id, offset, count) {
        Some((len, data)) => {
            rings::resp_ring_write(len as u64, 0, &data);
            Ok(len as u64)
        }
        None => Err(ERR_NOTFOUND),
    }
}

/// Stat per (bucket,key): (id, size, frame [nv, mtime]).
#[inline(never)]
pub fn disk_stat(eng: &mut DiskEngine, payload: &[u8]) -> Result<(u64, u64, [u8; 16]), u64> {
    let (bucket, key, rest) = arcafs::wire::parse_obj_prefix(payload).ok_or(ERR_INVALID)?;
    if !rest.is_empty() || bounds_invalid(bucket, key) {
        return Err(ERR_INVALID);
    }
    let (id, size, nv, mtime) = eng.stat(bucket, key).ok_or(ERR_NOTFOUND)?;
    let mut frame = [0u8; 16];
    frame[..8].copy_from_slice(&nv.to_le_bytes());
    frame[8..].copy_from_slice(&mtime.to_le_bytes());
    Ok((id, size, frame))
}

/// Stat per object_id: (size, nv, frame [mtime]).
#[inline(never)]
pub fn disk_stat_id(eng: &mut DiskEngine, payload: &[u8]) -> Result<(u64, u64, [u8; 8]), u64> {
    let id = arcafs::wire::parse_u64(payload).ok_or(ERR_INVALID)?;
    let (size, nv, mtime) = match eng.stat_id(id) {
        Some(v) => v,
        None => return Err(ERR_NOTFOUND),
    };
    Ok((size, nv, mtime.to_le_bytes()))
}

/// DELETE nome + catena viva (pin snapshot salvi): sempre commit.
#[inline(never)]
pub fn disk_delete(eng: &mut DiskEngine, payload: &[u8]) -> Result<u64, u64> {
    let (bucket, key, rest) = arcafs::wire::parse_obj_prefix(payload).ok_or(ERR_INVALID)?;
    if !rest.is_empty() || bounds_invalid(bucket, key) {
        return Err(ERR_INVALID);
    }
    match eng.delete(bucket, key) {
        Some(true) => commit_or(eng, 0),
        Some(false) => Err(ERR_NOTFOUND),
        None => Err(ERR),
    }
}

// ── Vista POSIX del namespace (56.3) ─────────────────────────────────────
// Stesse sentinelle dei fratelli (`INVALID` oltre bound, `NOTFOUND` assente,
// `ERR` a IO fallito). Il bucket e' sempre `NS_BUCKET` (chiavi = path
// relativi, opachi per il motore). Mutanti con commit per-op come `disk_*`.

/// Bucket del namespace POSIX (56.3): chiavi = path relativi (`a/b/c`),
/// separato da `sys`/dati. Bound nomi da `seckey_encode` (mai troncamenti).
pub const NS_BUCKET: &[u8] = b"ns";

/// UUID del volume legato al motore (56.3: la vista POSIX serve solo questo
/// volume — mount di altri volumi = errore loud in `arca_with`, mai dati
/// altrui sullo stesso motore).
#[inline(never)]
pub fn volume_uuid(eng: &DiskEngine) -> u64 {
    eng.store.vol().uuid()
}

/// Apre un volume ArcaFS sul nodo dato e verifica che l'uuid combaci (Fase 2:
/// root su volume scelto per UUID, mai primo trovato). Ritorna `Some(store)`
/// se il superblock esiste E l'uuid combacia, `None` altrimenti.
pub fn open_with_uuid(handle: u32, expected: u64) -> Option<VolumeStore> {
    let vol = crate::volume::ArcaVolume::open(handle);
    if let Some(ref v) = vol {
        println!("[cardo] open_with_uuid handle={} uuid={:016X} expected={:016X}", handle, v.uuid(), expected);
    } else {
        println!("[cardo] open_with_uuid handle={} -> OPEN FAILED", handle);
    }
    if let Some(v) = vol {
        if v.uuid() == expected {
            Some(VolumeStore::new(v))
        } else {
            None
        }
    } else {
        None
    }
}

/// PUT namespace a `offset` (stessa semantica `put_chunk`: 0 = fresco,
/// >0 = patch con zero-fill): sempre commit. Ritorna i byte scritti.
#[inline(never)]
pub fn ns_put(eng: &mut DiskEngine, key: &[u8], offset: usize, data: &[u8]) -> Result<usize, u64> {
    if key.len() > civis::OBJ_KEY_MAX {
        return Err(ERR_INVALID);
    }
    let total = offset.checked_add(data.len()).ok_or(ERR_INVALID)?;
    quota_gate(eng, NS_BUCKET, total)?;
    let n = eng
        .put_chunk(NS_BUCKET, key, offset, data, crate::wall::wall_secs(), RETAIN)
        .ok_or(ERR)?;
    commit_or(eng, n as u64).map(|v| v as usize)
}

/// GET namespace intero (il chiamante affetta a `offset`: i file di boot
/// stanno sotto il bound 16 MiB per oggetto; heap, mai stack — regola §18).
#[inline(never)]
pub fn ns_get(eng: &DiskEngine, key: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    if key.len() > civis::OBJ_KEY_MAX {
        return None;
    }
    eng.get(NS_BUCKET, key)
}

/// Range read namespace (S2.5f/F0): `(len_totale, slice)` — il path read dei
/// file (ns_read) non materializza piu' l'intero oggetto per slice.
#[inline(never)]
pub fn ns_get_range(
    eng: &mut DiskEngine,
    key: &[u8],
    off: usize,
    count: usize,
) -> Option<(u64, alloc::vec::Vec<u8>)> {
    if key.len() > civis::OBJ_KEY_MAX {
        return None;
    }
    eng.get_range(NS_BUCKET, key, off, count)
        .map(|(len, v)| (len as u64, v))
}

/// XSTATS (W2 istogramma + W3 cache): (n_extent, entries, max_entry,
/// n_legacy_ov, n_inline, cache_hit, cache_reads). Solo motore disco.
#[inline(never)]
pub fn disk_xstats(eng: &DiskEngine) -> Option<(u64, u64, u64, u64, u64, u64, u64)> {
    let (ne, en, mx, ov, ii) = eng.extent_stats()?;
    let (hits, reads) = eng.store.cache_stats();
    Some((ne, en, mx, ov, ii, hits, reads))
}

/// STAT namespace: (size head, mtime head). Assente = `None`.
#[inline(never)]
pub fn ns_stat(eng: &DiskEngine, key: &[u8]) -> Option<(u64, u64)> {
    if key.len() > civis::OBJ_KEY_MAX {
        return None;
    }
    eng.stat(NS_BUCKET, key).map(|(_, size, _, mtime)| (size, mtime))
}

/// RENAME namespace (S1.1): sposta la chiave (stesso uuid/storia, replace
/// atomico del dst) con UN commit. Quota-neutral (stessi blocchi, nuova
/// chiave: niente gate, lo slack nodi lo assorbe lo spazio libero — a volume
/// pieno l'allocatore fallisce loud). Assente = `Err(ERR_NOTFOUND)`.
#[inline(never)]
pub fn ns_rename(eng: &mut DiskEngine, old: &[u8], new: &[u8]) -> Result<(), u64> {
    if old.len() > civis::OBJ_KEY_MAX || new.len() > civis::OBJ_KEY_MAX {
        return Err(ERR_INVALID);
    }
    match eng.rename_key(NS_BUCKET, old, new) {
        Some(true) => commit_or(eng, 0).map(|_| ()),
        Some(false) => Err(ERR_NOTFOUND),
        None => Err(ERR),
    }
}

/// DELETE namespace: sempre commit. Assente = `Err(ERR_NOTFOUND)`.
#[inline(never)]
pub fn ns_delete(eng: &mut DiskEngine, key: &[u8]) -> Result<(), u64> {
    if key.len() > civis::OBJ_KEY_MAX {
        return Err(ERR_INVALID);
    }
    match eng.delete(NS_BUCKET, key) {
        Some(true) => commit_or(eng, 0).map(|_| ()),
        Some(false) => Err(ERR_NOTFOUND),
        None => Err(ERR),
    }
}

/// Scansione chiavi per prefisso (56.3, readdir/stat-dir emergenti): chiavi
/// del bucket namespace che iniziano per `prefix` (heap, ordinate dal
/// chiamante per risposte deterministiche).
#[inline(never)]
pub fn ns_scan(eng: &DiskEngine, prefix: &[u8]) -> Option<alloc::vec::Vec<alloc::vec::Vec<u8>>> {
    eng.scan_prefix(NS_BUCKET, prefix)
}

/// SNAP_CREATE bucket → id: persiste la tabella + commit (refcount e meta
/// cambiano le radici; il vecchio blocco meta resta orfano per la GC).
#[inline(never)]
pub fn disk_snap_create(eng: &mut DiskEngine, payload: &[u8]) -> Result<u64, u64> {
    let bucket = arcafs::wire::parse_bucket_only(payload).ok_or(ERR_INVALID)?;
    let id = eng.snap_create(bucket, crate::wall::wall_secs()).ok_or(ERR)?;
    eng.meta_store().ok_or(ERR)?;
    commit_or(eng, id)
}

/// SNAP_DELETE: sgancia i pin, persiste la tabella + commit.
#[inline(never)]
pub fn disk_snap_delete(eng: &mut DiskEngine, payload: &[u8]) -> Result<u64, u64> {
    let id = arcafs::wire::parse_u64(payload).ok_or(ERR_INVALID)?;
    if eng.snap_delete(id) {
        eng.meta_store().ok_or(ERR)?;
        commit_or(eng, 0)
    } else {
        Err(ERR_NOTFOUND)
    }
}

/// SNAP_ROLLBACK `[sid:8][obj-prefix]` → nuova head size, sempre commit.
/// `INVALID` a bucket mismatch, `NOTFOUND` a snapshot/chiave assente.
#[inline(never)]
pub fn disk_snap_rollback(eng: &mut DiskEngine, payload: &[u8]) -> Result<u64, u64> {
    let (id, rest0) = arcafs::wire::split_id_rest(payload).ok_or(ERR_INVALID)?;
    let (bucket, key, rest) = arcafs::wire::parse_obj_prefix(rest0).ok_or(ERR_INVALID)?;
    if !rest.is_empty() || bounds_invalid(bucket, key) {
        return Err(ERR_INVALID);
    }
    let snap = eng.snaps.get(&id).ok_or(ERR_NOTFOUND)?;
    if snap.bucket != bucket {
        return Err(ERR_INVALID);
    }
    // Gated sul pinnato (nuova head = nuovi blocchi): prima di allocare.
    let need = eng.snap_pinned_size(id, bucket, key).ok_or(ERR)?;
    quota_gate(eng, bucket, need)?;
    let size = eng
        .snap_rollback(bucket, key, id, crate::wall::wall_secs(), RETAIN)
        .ok_or(ERR)?;
    commit_or(eng, size)
}

/// SNAP_CLONE `[sid:8][dstbucket]` → oggetti clonati, sempre commit.
#[inline(never)]
pub fn disk_snap_clone(eng: &mut DiskEngine, payload: &[u8]) -> Result<u64, u64> {
    let (id, rest0) = arcafs::wire::split_id_rest(payload).ok_or(ERR_INVALID)?;
    let dst = arcafs::wire::parse_bucket_only(rest0).ok_or(ERR_INVALID)?;
    if !eng.snaps.contains_key(&id) {
        return Err(ERR_NOTFOUND);
    }
    // Gated sul dst (il clone COPIA i dati: somma pinnati conservativa,
    // le chiavi dst esistenti restano in `used` fino al trim).
    let need = eng.snap_total_size(id).ok_or(ERR)?;
    quota_gate(eng, dst, need)?;
    let n = eng.snap_clone(id, dst, crate::wall::wall_secs(), RETAIN).ok_or(ERR)?;
    commit_or(eng, n)
}

/// QUOTA_SET `[quota:8][blen:1][bucket]`: budget blocchi dati (0 = toglie il
/// tetto). Persiste via meta+commit (come gli snapshot); il seed non e' mai
/// gated (il boot non deve fallire su quota).
#[inline(never)]
pub fn disk_quota_set(eng: &mut DiskEngine, payload: &[u8]) -> Result<u64, u64> {
    let (quota, tail) = arcafs::wire::split_id_rest(payload).ok_or(ERR_INVALID)?;
    let bucket = arcafs::wire::parse_bucket_only(tail).ok_or(ERR_INVALID)?;
    eng.quota_set(bucket, quota).ok_or(ERR_INVALID)?;
    eng.meta_store().ok_or(ERR)?;
    commit_or(eng, 0)
}

/// QUOTA_GET `[blen:1][bucket]` → (quota, used misurata ora). Solo motore
/// disco: il backend RAM resta senza quota (A3).
#[inline(never)]
pub fn disk_quota_get(eng: &DiskEngine, payload: &[u8]) -> Result<(u64, u64), u64> {
    let bucket = arcafs::wire::parse_bucket_only(payload).ok_or(ERR_INVALID)?;
    let quota = eng.quotas.get(bucket).copied().unwrap_or(0);
    let used = eng.quota_used(bucket).ok_or(ERR)?;
    Ok((quota, used))
}

// ── Scaffold RAW dopo il bind (stesso handle: via `eng.store`) ──────────
// Le write passano da `BlockStore::write_node` (write-through: cache
// coerente); le free invalidano. ALLOC/FREE post-bind corretti ma
// sconsigliati (freelist condivisa col motore: vedi nota in testa).

/// Lega lo scaffold RAW a una source (`/dev/sdc1`, resolve come i mount).
/// Il blocco viaggia solo nel payload (w0 e' la lunghezza, mai semantica nei
/// registri oltre l'expect — stessa convenzione dello scaffold 56.2a).
#[inline(never)]
pub fn raw_open(path: &str) -> Option<VolumeStore> {
    println!("[cardo] raw_open path={}...", path);
    let handle = mount::resolve_mount_source(path)?;
    println!("[cardo] raw_open path={} -> handle={}", path, handle);
    let vol = crate::volume::ArcaVolume::open(handle)?;
    Some(VolumeStore::new(vol))
}

/// Scansiona i device per un superblock ArcaFS (whole + partizioni 1-4,
/// sda-sdh come `find_arca` in testsarca): primo volume valido vinto.
/// Best-effort per l'auto-bind all'avvio (56.2c): assenza = None silenzioso
/// (niente volume, niente motore — init ripiega su FAT); resolve bounded,
/// mai wedge su nomi ignoti.
/// `#[inline(never)]`: chiamato una volta da real_main, resta fuori dal suo
/// frame (stessa ragione dei firewall btree con LTO).
#[inline(never)]
pub fn scan_and_open() -> Option<VolumeStore> {
    let mut path = [0u8; 16];
    for disk in 0..8u8 {
        let letter = b'a' + disk;
        // Whole-disk: /dev/sdX
        path[..5].copy_from_slice(b"/dev/");
        path[5..8].copy_from_slice(&[b's', b'd', letter]);
        if let Some(p) = core::str::from_utf8(&path[..8]).ok() {
            if let Some(v) = raw_open(p) {
                return Some(v);
            }
        }
        // Partizioni: /dev/sdXn
        for part in 1..=4u8 {
            path[..5].copy_from_slice(b"/dev/");
            path[5..9].copy_from_slice(&[b's', b'd', letter, b'0' + part]);
            if let Some(p) = core::str::from_utf8(&path[..9]).ok() {
                if let Some(v) = raw_open(p) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Esito RAW: scalare (reply generica) o con frame dedicato (il chiamante
/// scrive reply a due registri e fa `continue`, pattern PIPE). Blocco in
/// `Box` (heap): mai 3.5K sullo stack di questa funzione (regola §18).
pub enum ArcaDebugOut {
    Scalar(u64),
    Read(u64, Box<[u8; arcafs::format::ARCA_NODE_PAYLOAD_LEN]>),
    Stats(u64, u64, u64),
    /// QUOTA_GET: (quota_blocks, used_blocks misurata).
    Quota(u64, u64),
}

/// Unico handler RAW pre/post bind (stesso store del motore dopo USEDISK:
/// niente doppi handle, niente divergenze). OPEN qui non arriva mai
/// (dispatch: `raw_open` prima del bind, re-open dopo = ERR loud).
#[inline(never)]
pub fn handle_raw_debug(
    store: &mut VolumeStore,
    payload: &[u8],
) -> Result<ArcaDebugOut, u64> {
    use arcafs::proto::*;
    let sub = *payload.first().ok_or(ERR_INVALID)?;
    let rest = payload.get(1..).ok_or(ERR_INVALID)?;
    if sub == ARCA_SUB_OPEN {
        return Err(ERR_INVALID); // re-open dopo bind: non supportato (loud)
    }
    match sub {
        ARCA_SUB_ALLOC => {
            if !rest.is_empty() {
                return Err(ERR_INVALID);
            }
            Ok(ArcaDebugOut::Scalar(store.alloc().ok_or(ERR)?))
        }
        ARCA_SUB_FREE => {
            let n = arcafs::wire::parse_u64(rest).ok_or(ERR_INVALID)?;
            if store.free(n) {
                Ok(ArcaDebugOut::Scalar(0))
            } else {
                Err(ERR)
            }
        }
        ARCA_SUB_READ => {
            let n = arcafs::wire::parse_u64(rest).ok_or(ERR_INVALID)?;
            let mut data = format::boxed_node();
            match store.read_node(n, &mut data) {
                Some(_) => Ok(ArcaDebugOut::Read(n, data)),
                None => Err(ERR),
            }
        }
        ARCA_SUB_WRITE => {
            let (n, data) = arcafs::wire::split_id_rest(rest).ok_or(ERR_INVALID)?;
            if data.len() != arcafs::format::ARCA_NODE_PAYLOAD_LEN {
                return Err(ERR_INVALID);
            }
            let mut buf = format::boxed_node();
            buf.copy_from_slice(data);
            if store.write_node(n, arcafs::format::ARCA_NODE_TYPE_RAW, 0, &buf) {
                Ok(ArcaDebugOut::Scalar(0))
            } else {
                Err(ERR)
            }
        }
        ARCA_SUB_STAT => {
            if !rest.is_empty() {
                return Err(ERR_INVALID);
            }
            let (high, live, free) =
                (store.vol().stats().0, store.vol().stats().1, store.vol().stats().2);
            Ok(ArcaDebugOut::Stats(high, live, free))
        }
        _ => Err(ERR_INVALID),
    }
}
