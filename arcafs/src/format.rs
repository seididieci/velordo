//! Formato blocchi on-disk ArcaFS (Fase 56.2a). Funzioni pure di
//! encode/decode/verifica condivise tra guest e tool host: il layout e'
//! definito UNA volta sola qui.
//!
//! Blocco = 65536 B = 128 settori (chunkati a <=7 settori per IPC dal
//! `BlockSource`), partition-relative: blocco N = settori N*128..N*128+127.
//! Blocco 0 = superblock (LBA0) + shadow (LBA1) + header-estensione (settori
//! 2-6, i primi 1024 B restano intoccati per non sfiorare mai
//! superblock/shadow). Il blocco 0 non si alloca mai.

use alloc::boxed::Box;
use syscall_numbers::image_hash;

/// Magic superblock + versione formato.
pub const ARCA_MAGIC: &[u8; 4] = b"ACFS";
/// Versione formato (W2 extent: 2; W6 blocchi 64 KiB: 3; v4 identita' del
/// volume: 4 — niente retrocompat: le immagini si rigenerano (tool host e
/// guest condividono questo crate), un mount v1/v2/v3 fallisce loud in
/// `superblock_verify`).
pub const ARCA_VERSION: u32 = 4;
/// Block size negoziato = 64 KiB (128 settori da 512 B): riduce metadata e
/// I/O su file grandi (200 GB -> ~3M blocchi invece di ~56M con 3.5 KiB).
pub const ARCA_BLOCK_SIZE: u32 = 65536;
/// Superblock: 192 B a LBA0 (+ shadow LBA1); checksum FNV-1a di [0..184).
/// v4: il superblock e' cresciuto dentro il settore (512 B) per fare spazio
/// a `label` + `mountpoint` (l'header-extension resta a 1024: la geometria
/// del blocco non cambia).
pub const ARCA_SUPER_LEN: usize = 192;
pub const ARCA_OFF_MAGIC: usize = 0;
pub const ARCA_OFF_VERSION: usize = 4;
pub const ARCA_OFF_BLOCK_SIZE: usize = 8;
pub const ARCA_OFF_UUID: usize = 12;
pub const ARCA_OFF_GEN: usize = 20;
pub const ARCA_OFF_ROOT: usize = 28;
pub const ARCA_OFF_REFCOUNT: usize = 36;
pub const ARCA_OFF_ALLOC: usize = 44;
/// **Label** del volume (32 B, NUL-padded): il *nome* del volume ("TESTS").
/// Non e' ancora usata per la discovery by-label (quella e' roba da USB,
/// ADR-0058): oggi e' l'identita' leggibile e il nome che il log mostra.
pub const ARCA_OFF_LABEL: usize = 52;
/// Lunghezza massima del campo label (32 B: [52..84)).
pub const ARCA_LABEL_MAX: usize = 32;
/// **Mountpoint dichiarato** dal volume (56 B, NUL-padded): *dove* il volume
/// vuole essere montato, path **relativo** alla root ("tests", "media/usb";
/// la vista POSIX ci mette davanti la "/"). Vuoto = il volume non dichiara
/// niente e non viene auto-montato (modello ZFS: un dataset senza
/// `mountpoint` resta un dataset).
pub const ARCA_OFF_MOUNT: usize = 84;
/// Lunghezza massima del campo mountpoint (56 B: [84..140)).
pub const ARCA_MOUNT_MAX: usize = 56;
/// Offset del puntatore meta-root (56.2c): 8 B a [140..148).
pub const ARCA_OFF_META: usize = 140;
pub const ARCA_OFF_AUTO: usize = 148;
pub const ARCA_OFF_FLAGS: usize = 149;
pub const ARCA_OFF_CHECK: usize = 184;
/// Settori per blocco (65536 / 512 = 128).
pub const ARCA_BLOCK_SECTORS: usize = 128;
/// Byte offset dell'header-estensione nel blocco 0 (salta LBA0+LBA1).
pub const ARCA_XHDROFF: usize = 1024;
/// Magic header-estensione + versione.
pub const ARCA_XMAGIC: &[u8; 4] = b"AXHD";
pub const ARCA_XVER: u32 = 1;
/// Header-estensione (56 B): magic 0:4, ver 4:4, free_head 8:8, high_water
/// 16:8, next_id 24:8, next_snap 32:8, flags 40:8 (bit0 DIRTY, 56.2b),
/// check 48:8 (FNV-1a di [0..48]).
pub const ARCA_XHDRLEN: usize = 56;
pub const ARCA_XHOFF_FREE: usize = 8;
pub const ARCA_XHOFF_HIGH: usize = 16;
pub const ARCA_XHOFF_NEXTID: usize = 24;
pub const ARCA_XHOFF_NEXTSNAP: usize = 32;
pub const ARCA_XHOFF_FLAGS: usize = 40;
pub const ARCA_XHOFF_CHECK: usize = 48;
/// Bit 0 dei flags header-ext: DIRTY (mutazione in corso, commit non
/// flippato). Acceso a inizio mutazione, spento al flip superblock (56.2b);
/// a mount con DIRTY acceso serve orphan-GC (56.2c).
pub const ARCA_XH_DIRTY: u64 = 1;
/// Nodo blocco (65536 B): magic "ANOD" 0:4, type 4:1, gen 8:8, payload
/// 16:65512, check 65528:8 (FNV-1a di [0..65528]).
pub const ARCA_NMAGIC: &[u8; 4] = b"ANOD";
pub const ARCA_NODE_TYPE_RAW: u8 = 0;
pub const ARCA_NODE_TYPE_LEAF: u8 = 1;
pub const ARCA_NODE_TYPE_INTERNAL: u8 = 2;
/// Mappa extent (W2/W6): `[prev:8][n:2 LE][entries: n x (blk:8, nb:4)]`
/// (5458 entry/nodo MAP con payload 65512). Catena ALL'INDIETRO (ogni nodo
/// punta al piu' vecchio): le versioni riusano i nodi invariati e riscrivono
/// solo dalla prima entry cambiata in poi. Punta solo blocchi DATA.
pub const ARCA_NODE_TYPE_MAP: u8 = 3;
/// Blocco dati extent (W2): 65512 B di dati puri, niente header interno.
/// Immutabile dopo la scrittura (invariante W1 per la cache).
pub const ARCA_NODE_TYPE_DATA: u8 = 4;
/// Blocco packed (W5): header `[used:u16][refs:u16][blen:u8][bucket:blen]`,
/// poi slice contigue append-only (1 oggetto = 1 slice, immutabile dopo
/// la scrittura, condiviso tra oggetti dello stesso bucket).
pub const ARCA_NODE_TYPE_PACKED: u8 = 5;
/// Header di un nodo: 16 byte davanti al payload.
pub const ARCA_NODE_PAYLOAD: usize = 16;
pub const ARCA_NODE_PAYLOAD_LEN: usize = ARCA_BLOCK_SIZE as usize - ARCA_NODE_PAYLOAD - 8; // 65512
pub const ARCA_NODE_CHECK: usize = ARCA_BLOCK_SIZE as usize - 8; // 65528

/// Header-estensione parsata.
pub struct HeaderExt {
    pub free_head: u64,
    pub high_water: u64,
    pub next_id: u64,
    pub next_snap: u64,
    pub flags: u64,
}

fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Identita' del volume (v4): generazione, uuid, label e mountpoint
/// dichiarato. I due campi testuali sono NUL-padded nel superblock.
#[derive(Clone, Copy, Debug)]
pub struct Superblock {
    pub generation: u64,
    pub uuid: u64,
    pub label: [u8; ARCA_LABEL_MAX],
    pub label_len: usize,
    pub mountpoint: [u8; ARCA_MOUNT_MAX],
    pub mountpoint_len: usize,
}

/// Default manuale: `Default` non e' derivabile per array > 32 elementi.
impl Default for Superblock {
    fn default() -> Self {
        Superblock {
            generation: 0,
            uuid: 0,
            label: [0; ARCA_LABEL_MAX],
            label_len: 0,
            mountpoint: [0; ARCA_MOUNT_MAX],
            mountpoint_len: 0,
        }
    }
}

impl Superblock {
    /// Label come testo ("" se assente o non UTF-8).
    pub fn label_str(&self) -> &str {
        core::str::from_utf8(&self.label[..self.label_len]).unwrap_or("")
    }

    /// Mountpoint dichiarato come testo ("" = il volume non dichiara niente
    /// e non viene auto-montato).
    pub fn mountpoint_str(&self) -> &str {
        core::str::from_utf8(&self.mountpoint[..self.mountpoint_len]).unwrap_or("")
    }
}

/// Copia un testo in un campo NUL-padded (max `field.len()` byte) e
/// ritorna la lunghezza scritta. Il campo viene **azzzerato** prima: un
/// patch con un testo piu' corto non deve lasciare la coda del vecchio.
fn put_text(field: &mut [u8], text: &str) -> usize {
    field.fill(0);
    let b = text.as_bytes();
    let n = b.len().min(field.len());
    field[..n].copy_from_slice(&b[..n]);
    n
}

/// Legge un campo NUL-padded: ritorna i byte fino al primo NUL.
fn get_text(field: &[u8]) -> &[u8] {
    let n = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    &field[..n]
}

/// Verifica superblock (magic+versione+block-size+checksum): `(gen, uuid)`
/// o `None`. Stessi 4 check di `probe_arca` (il mount resta dov'e').
pub fn superblock_verify(sec: &[u8]) -> Option<(u64, u64)> {
    let sb = sec.get(..ARCA_SUPER_LEN)?;
    if sb[ARCA_OFF_MAGIC..ARCA_OFF_MAGIC + 4] != *ARCA_MAGIC {
        return None;
    }
    let u32le = |o: usize| {
        u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]])
    };
    if u32le(ARCA_OFF_VERSION) != ARCA_VERSION {
        return None;
    }
    if u32le(ARCA_OFF_BLOCK_SIZE) != ARCA_BLOCK_SIZE {
        return None;
    }
    if image_hash(&sb[..ARCA_OFF_CHECK]) != u64le(sb, ARCA_OFF_CHECK) {
        return None;
    }
    Some((u64le(sb, ARCA_OFF_GEN), u64le(sb, ARCA_OFF_UUID)))
}

/// Legge l'identita' completa del volume (stessi check di
/// `superblock_verify` + label e mountpoint dichiarato). `None` se il
/// superblock non verifica.
pub fn superblock_identity(sec: &[u8]) -> Option<Superblock> {
    let (generation, uuid) = superblock_verify(sec)?;
    let sb = sec.get(..ARCA_SUPER_LEN)?;
    let label = get_text(&sb[ARCA_OFF_LABEL..ARCA_OFF_LABEL + ARCA_LABEL_MAX]);
    let mount = get_text(&sb[ARCA_OFF_MOUNT..ARCA_OFF_MOUNT + ARCA_MOUNT_MAX]);
    let mut id = Superblock {
        generation,
        uuid,
        ..Default::default()
    };
    id.label[..label.len()].copy_from_slice(label);
    id.label_len = label.len();
    id.mountpoint[..mount.len()].copy_from_slice(mount);
    id.mountpoint_len = mount.len();
    Some(id)
}

/// Costruisce 192 B di superblock (LE esplicito, checksum FNV-1a).
/// Mossa dal tool host: stessa funzione per `create` e guest `format`.
/// `label` e `mountpoint` sono clippati ai rispettivi bound (32 B / 56 B) e
/// NUL-padded: i campi non si sovrappongono mai (`meta_root` sta a [140..148)
/// e il checksum copre [0..184)).
pub fn superblock_build(
    uuid: u64,
    generation: u64,
    label: &str,
    mountpoint: &str,
) -> [u8; ARCA_SUPER_LEN] {
    let mut sb = [0u8; ARCA_SUPER_LEN];
    sb[ARCA_OFF_MAGIC..ARCA_OFF_MAGIC + 4].copy_from_slice(ARCA_MAGIC);
    sb[ARCA_OFF_VERSION..ARCA_OFF_VERSION + 4].copy_from_slice(&ARCA_VERSION.to_le_bytes());
    sb[ARCA_OFF_BLOCK_SIZE..ARCA_OFF_BLOCK_SIZE + 4]
        .copy_from_slice(&ARCA_BLOCK_SIZE.to_le_bytes());
    sb[ARCA_OFF_UUID..ARCA_OFF_UUID + 8].copy_from_slice(&uuid.to_le_bytes());
    sb[ARCA_OFF_GEN..ARCA_OFF_GEN + 8].copy_from_slice(&generation.to_le_bytes());
    put_text(
        &mut sb[ARCA_OFF_LABEL..ARCA_OFF_LABEL + ARCA_LABEL_MAX],
        label,
    );
    put_text(
        &mut sb[ARCA_OFF_MOUNT..ARCA_OFF_MOUNT + ARCA_MOUNT_MAX],
        mountpoint,
    );
    sb[ARCA_OFF_AUTO] = 0;
    sb[ARCA_OFF_FLAGS] = 0;
    let checksum = image_hash(&sb[..ARCA_OFF_CHECK]);
    sb[ARCA_OFF_CHECK..ARCA_OFF_CHECK + 8].copy_from_slice(&checksum.to_le_bytes());
    sb
}

/// Patch dei campi testuali del superblock (label e/o mountpoint),
/// ricalcolando il checksum. `None` se il superblock non verifica.
pub fn superblock_set_identity(
    sec: &mut [u8; 512],
    label: Option<&str>,
    mountpoint: Option<&str>,
) -> Option<()> {
    superblock_verify(sec)?;
    if let Some(l) = label {
        put_text(&mut sec[ARCA_OFF_LABEL..ARCA_OFF_LABEL + ARCA_LABEL_MAX], l);
    }
    if let Some(m) = mountpoint {
        put_text(&mut sec[ARCA_OFF_MOUNT..ARCA_OFF_MOUNT + ARCA_MOUNT_MAX], m);
    }
    let checksum = image_hash(&sec[..ARCA_OFF_CHECK]);
    sec[ARCA_OFF_CHECK..ARCA_OFF_CHECK + 8].copy_from_slice(&checksum.to_le_bytes());
    Some(())
}

/// Legge `meta_root` dal superblock (0 = nessuna tabella snapshot).
/// Il campo meta sta a [140..148) (v4), mai dentro il testo dei campi
/// NUL-padded (label [52..84), mountpoint [84..140)).
pub fn superblock_meta(sec: &[u8]) -> u64 {
    match sec.get(ARCA_OFF_META..ARCA_OFF_META + 8) {
        Some(w) => u64::from_le_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]),
        None => 0,
    }
}

/// Imposta ROOT nel superblock e ricalcola il checksum (preserva il resto).
/// `None` se il settore non e' un superblock valido.
pub fn superblock_set_root(sec: &mut [u8; 512], root: u64) -> Option<()> {
    superblock_verify(sec)?;
    sec[ARCA_OFF_ROOT..ARCA_OFF_ROOT + 8].copy_from_slice(&root.to_le_bytes());
    let checksum = image_hash(&sec[..ARCA_OFF_CHECK]);
    sec[ARCA_OFF_CHECK..ARCA_OFF_CHECK + 8].copy_from_slice(&checksum.to_le_bytes());
    Some(())
}

/// Legge le radici B+tree dal superblock: `(gen, uuid, primary, secondary,
/// refcount)`. Tutte e tre le radici sono persistite: la secondary SI SPOSTA
/// a ogni split (come le altre — COW riscrive la root a ogni mutazione) e
/// rileggerla da un blocco fisso darebbe un albero stale (osservato: chiavi
/// post-split invisibili dopo il remount). `alloc_hint` (offset 44, hint
/// soft di placement mai usato in 56.2b) ospita la secondary root; A5
/// rivalutera' il placement con numeri reali. `None` se non valido.
pub fn superblock_roots(sec: &[u8]) -> Option<(u64, u64, u64, u64, u64)> {
    let (gen, uuid) = superblock_verify(sec)?;
    let sb = sec.get(..ARCA_SUPER_LEN)?;
    let r = |o: usize| {
        u64::from_le_bytes([
            sb[o], sb[o + 1], sb[o + 2], sb[o + 3], sb[o + 4], sb[o + 5], sb[o + 6],
            sb[o + 7],
        ])
    };
    Some((gen, uuid, r(ARCA_OFF_ROOT), r(ARCA_OFF_ALLOC), r(ARCA_OFF_REFCOUNT)))
}

/// Imposta generazione + radici B+tree + meta e ricalcola il checksum (il
/// commit: shadow + flip con tutto aggiornato in UN colpo — radici e meta
/// mai di generazioni diverse). `None` se non valido.
pub fn superblock_set_roots(
    sec: &mut [u8; 512],
    generation: u64,
    primary: u64,
    secondary: u64,
    refcount: u64,
    meta: u64,
) -> Option<()> {
    superblock_verify(sec)?;
    sec[ARCA_OFF_GEN..ARCA_OFF_GEN + 8].copy_from_slice(&generation.to_le_bytes());
    sec[ARCA_OFF_ROOT..ARCA_OFF_ROOT + 8].copy_from_slice(&primary.to_le_bytes());
    sec[ARCA_OFF_ALLOC..ARCA_OFF_ALLOC + 8].copy_from_slice(&secondary.to_le_bytes());
    sec[ARCA_OFF_REFCOUNT..ARCA_OFF_REFCOUNT + 8].copy_from_slice(&refcount.to_le_bytes());
    sec[ARCA_OFF_META..ARCA_OFF_META + 8].copy_from_slice(&meta.to_le_bytes());
    let checksum = image_hash(&sec[..ARCA_OFF_CHECK]);
    sec[ARCA_OFF_CHECK..ARCA_OFF_CHECK + 8].copy_from_slice(&checksum.to_le_bytes());
    Some(())
}

/// Codifica header-estensione (56 B).
pub fn xhdr_encode(xh: &HeaderExt) -> [u8; ARCA_XHDRLEN] {
    let mut b = [0u8; ARCA_XHDRLEN];
    b[..4].copy_from_slice(ARCA_XMAGIC);
    b[4..8].copy_from_slice(&ARCA_XVER.to_le_bytes());
    put64(&mut b, ARCA_XHOFF_FREE, xh.free_head);
    put64(&mut b, ARCA_XHOFF_HIGH, xh.high_water);
    put64(&mut b, ARCA_XHOFF_NEXTID, xh.next_id);
    put64(&mut b, ARCA_XHOFF_NEXTSNAP, xh.next_snap);
    put64(&mut b, ARCA_XHOFF_FLAGS, xh.flags);
    let check = image_hash(&b[..ARCA_XHOFF_CHECK]);
    put64(&mut b, ARCA_XHOFF_CHECK, check);
    b
}

/// Decodifica+verifica header-estensione (magic+ver+checksum).
pub fn xhdr_decode(b: &[u8]) -> Option<HeaderExt> {
    let b = b.get(..ARCA_XHDRLEN)?;
    if b[..4] != *ARCA_XMAGIC {
        return None;
    }
    if u32::from_le_bytes([b[4], b[5], b[6], b[7]]) != ARCA_XVER {
        return None;
    }
    if image_hash(&b[..ARCA_XHOFF_CHECK]) != u64le(b, ARCA_XHOFF_CHECK) {
        return None;
    }
    Some(HeaderExt {
        free_head: u64le(b, ARCA_XHOFF_FREE),
        high_water: u64le(b, ARCA_XHOFF_HIGH),
        next_id: u64le(b, ARCA_XHOFF_NEXTID),
        next_snap: u64le(b, ARCA_XHOFF_NEXTSNAP),
        flags: u64le(b, ARCA_XHOFF_FLAGS),
    })
}

/// Riempie un blocco-nodo (header + payload + checksum).
pub fn node_fill(blk: &mut [u8; ARCA_BLOCK_SIZE as usize], ty: u8, gen: u64, payload: &[u8; ARCA_NODE_PAYLOAD_LEN]) {
    blk[..4].copy_from_slice(ARCA_NMAGIC);
    blk[4] = ty;
    blk[8..16].copy_from_slice(&gen.to_le_bytes());
    blk[ARCA_NODE_PAYLOAD..ARCA_NODE_PAYLOAD + ARCA_NODE_PAYLOAD_LEN].copy_from_slice(payload);
    let check = image_hash(&blk[..ARCA_NODE_CHECK]);
    blk[ARCA_NODE_CHECK..].copy_from_slice(&check.to_le_bytes());
}

/// Verifica un blocco-nodo (magic + checksum): `(type, gen)` o `None`.
pub fn node_verify(blk: &[u8; ARCA_BLOCK_SIZE as usize]) -> Option<(u8, u64)> {
    if blk[..4] != *ARCA_NMAGIC {
        return None;
    }
    if image_hash(&blk[..ARCA_NODE_CHECK]) != u64le(blk, ARCA_NODE_CHECK) {
        return None;
    }
    Some((blk[4], u64le(blk, 8)))
}

/// Alloca un payload nodo azzerato sull'heap (regola stack §18).
/// Mai `Box::new([0u8; N])`: l'array letterale si costruisce sullo stack
/// prima del move nell'heap (3.5K transienti che, annidati sotto altri
/// array, sfondano i 16 KiB del loop cardo — osservato: #PF al bind).
/// `new_zeroed` azzera direttamente la memoria heap: niente temp, sound
/// perche' la memoria e' davvero inizializzata a zero.
pub fn boxed_node() -> Box<[u8; ARCA_NODE_PAYLOAD_LEN]> {
    unsafe { Box::new_zeroed().assume_init() }
}

/// Come sopra per il blocco intero (header + payload + checksum).
pub fn boxed_block() -> Box<[u8; ARCA_BLOCK_SIZE as usize]> {
    unsafe { Box::new_zeroed().assume_init() }
}
