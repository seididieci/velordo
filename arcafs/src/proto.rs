//! Tag di protocollo ArcaFS (mossi da `syscall-numbers` in 56.2a: il kernel
//! non li usa — verificato — e la casa del sottosistema e' questa).
//!
//! I tag stabili (`R_OBJ_*`, `R_SNAP_*`) sono API permanente multi-client.
//! Il debug formato/allocatore viaggia su UN solo tag (`R_ARCA_DEBUG`) con
//! sub-opcode nel primo byte del payload: lo scaffold si gatta/rimuove in
//! un punto solo (A7).

/// Object store nativo (Fase 55, A1): PUT (w0=size, payload=bucket\0key\0[data])
/// e GET (w0=offset, w1=count, payload=bucket\0key\0).
pub const R_OBJ_PUT: u32 = 0x26;
pub const R_OBJ_GET: u32 = 0x27;
/// Versioni + snapshot (Fase 56.1, A2 in RAM): ogni PUT crea una versione
/// (mai overwrite); snapshot per-bucket con pin delle versioni, clone di
/// bucket, rollback per-chiave (nuova versione clonata, mai truncate).
/// GET_ID/STAT_ID parlano per object_id; STAT/DELETE per (bucket,key).
/// Formati payload: CREATE `[blen:1][bucket]` → reply snap_id;
/// DELETE `[snap_id:8]`; ROLLBACK `[snap_id:8][obj-prefix]` → nuova size;
/// CLONE `[snap_id:8][dblen:1][dstbucket]` → oggetti clonati;
/// GET_ID `[id:8]` (w1=offset, come GET); STAT_ID `[id:8]` → (size, nv);
/// DELETE `[obj-prefix]`; STAT `[obj-prefix]` → (id, size, frame [nv,mtime]).
pub const R_SNAP_CREATE: u32 = 0x28;
pub const R_SNAP_DELETE: u32 = 0x29;
pub const R_SNAP_ROLLBACK: u32 = 0x2A;
pub const R_SNAP_CLONE: u32 = 0x2B;
pub const R_OBJ_GET_ID: u32 = 0x2C;
pub const R_OBJ_STAT_ID: u32 = 0x2D;
pub const R_OBJ_DELETE: u32 = 0x2E;
pub const R_OBJ_STAT: u32 = 0x2F;
/// Bound nomi object store (Fase 55, hygiene): bucket ≤ 16 B, chiave ≤ 255 B
/// (1 byte di lunghezza nel frame: oltre e' inesprimibile sul wire).
/// Entrambi i lati rifiutano loud oltre il bound (mai troncamento `as u8`).
pub const OBJ_BUCKET_MAX: usize = 16;
pub const OBJ_KEY_MAX: usize = 255;
/// Debug formato/allocatore (Fase 56.2a, scaffold: volume di scratch nel
/// gate; gating di policy in A7). UN solo tag globale; il primo byte del
/// payload e' il sub-op qui sotto. OPEN: path device → bind; ALLOC: → nuovo blocco; FREE: `[sub][block:8]`; READ: `[sub][block:8]`
/// → frame ring (prefisso); WRITE: `[sub][block:8][<= RING_MAX_PAYLOAD-9]`; STAT: `[sub]` →
/// (high_water, live) + frame `[free_head:8]`. Blocco 0 mai toccato.
pub const R_ARCA_DEBUG: u32 = 0x40;
/// Sub-opcode di `R_ARCA_DEBUG` (primo byte payload).
pub const ARCA_SUB_OPEN: u8 = 1;
pub const ARCA_SUB_ALLOC: u8 = 2;
pub const ARCA_SUB_FREE: u8 = 3;
pub const ARCA_SUB_READ: u8 = 4;
pub const ARCA_SUB_WRITE: u8 = 5;
pub const ARCA_SUB_STAT: u8 = 6;
/// USEDISK (Fase 56.2b): `R_ARCA_DEBUG` con payload `[7][on:1]` lega il
/// motore B+tree su disco al volume aperto e instrada `R_OBJ_*`/`R_SNAP_*`
/// sul backend disco (`on=1`) o su quello in-RAM (`on=0`, default).
/// Flag secco server-side: un backend alla volta, mai merge silenziosi.
pub const ARCA_SUB_USEDISK: u8 = 7;
/// QUOTA_SET (A3, ADR-0044): payload `[8][quota:8][blen:1][bucket]` → budget
/// blocchi dati del bucket (0 = illimitato); persiste via meta+commit.
/// QUOTA_GET (A3): payload `[9][blen:1][bucket]` → reply (quota, used).
/// Solo motore disco (pre-bind = ERR): il backend RAM resta senza quota.
pub const ARCA_SUB_QUOTA_SET: u8 = 8;
pub const ARCA_SUB_QUOTA_GET: u8 = 9;
/// XSTATS (W2): payload `[10]` (vuoto oltre il sub) → reply registri
/// (n_extent, entries_totali) + frame `[max_entry:8][n_legacy_ov:8]`
/// `[n_inline:8]`. Solo diagnostica/tuning (costo O(oggetti)).
pub const ARCA_SUB_XSTATS: u8 = 10;
/// CACHE_FLUSH (bench A/B): payload `[11]` → svuota la cache nodi del
/// `VolumeStore` legato (write-through, nessun effetto sul disco). Serve alla
/// misura "fredda" deterministica di ArcaFS: lo spoiler di block evince la
/// cache settoriale, ma non quella in cardo.
pub const ARCA_SUB_CACHE_FLUSH: u8 = 11;
