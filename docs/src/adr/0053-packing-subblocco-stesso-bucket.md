# ADR-0053: Packing sub-blocco stesso-bucket in ArcaFS (W5)

## Status

Accepted

## Context

Dopo W1–W4 ogni oggetto ArcaFS maggiore di `INLINE_MAX` (512 B) occupa
almeno un blocco intero da `ARCA_NODE_PAYLOAD_LEN` (3560 B), anche se ne
serve solo una frazione: un oggetto da 600 B spreca ~2960 B. Il mini-sysroot
di S2.5 è popolato da molti file piccoli (`.rmeta`, ticket, manifest), quindi
lo spreco è rilevante e va misurato/ridotto prima di `S2.5f` (staticone su
ArcaFS).

Vincoli di partenza:

- **Invariante W1**: un blocco scritto non viene mai mutato in place (COW);
  una slice packed è immutabile dopo la scrittura.
- **W2**: oltre `PACK_CAP` (≈3539 B) l'oggetto va a extent, "solo verso
  l'alto" (crescita promuove packed→extent, mai demote).
- **Quota per bucket (ADR-0044)**: `quota_used` si misura esatto con dedup;
  un blocco packed condiviso conta una volta sola per bucket.
- **Free transazionale (W4)**: i blocchi liberati eager durante un'epoca
  aperta diventano orfani recuperati dalla GC al bind; niente journal.
- **Stack cardo 16 KiB** (§18): niente array KiB letterali sullo stack.

## Decision

### 1. Nuovo tipo nodo `PACKED` e riferimento

`ARCA_NODE_TYPE_PACKED = 5`. Il payload di un blocco packed ha header
`[used:u16][refs:u16][blen:u8][bucket:blen]` (hdr ≤ 5+16=21 B) seguito da
slice contigue append-only. **1 oggetto = 1 slice** `(blk, off, len)`,
mai spill su due blocchi. `PACK_CAP = 3560 − 21 = 3539`; la banda alta
(3540..3560 B) va a extent (1 blocco, trascurabile).

`DataRef::Packed(PackRef { blk, off, len })`; codec prim-rec kind=3
`[blk:8][off:2][len:2]` (prima rifiutato loud, ora abilitato con validazione
strutturale: `blk != 0`, `0 < len ≤ PACK_CAP`, `off+len ≤ payload`).

### 2. Scrittura: bump-alloc per bucket

`value_write(bucket, data)`: `≤ INLINE_MAX` → Inline; `≤ PACK_CAP` → packed
(`packed_append`); oltre → extent. `pack_cur: HashMap<bucket, (blk, used)>`
tiene il blocco di bump corrente per bucket; se la coda non basta, si alloca
un nuovo blocco bump (header + slice in un'unica `write_node`). `refs` del
blocco cresce di 1 nella stessa write. Nessuna slice è mai modificata sul
posto (W1): una patch su oggetto packed legge la slice, applica, riscrive
una nuova slice (la vecchia muore per refcount).

Tutti i chiamanti (`put_chunk`, `put`/snap_restore, `snap_clone`) hanno già
il bucket; `value_write` prende ora `bucket` come parametro.

### 3. Lettura

`value_read`/`head_range` gestiscono `Packed`: leggono il blocco, validano
`hdr ≤ off`, `off+len ≤ payload`, `len` in banda, restituiscono la slice.

### 4. Free eager per refcount + sweep al bind

I blocchi packed sono **condivisi tra oggetti dello stesso bucket**, quindi
non sono liberabili eager per-versione (rischio double-free/dangling). In
`free_dropped_versions` i packed sono esclusi dal set `to_free`
(`value_blocks_freeable` li salta) e invece gestiti da refcount:

- al bind (`rebuild_pack_cur`), si ricontano i `refs` dai record primary
  live (walk O(oggetti)) e si riscrive l'header di ogni blocco packed;
- al trim/delete, per ogni blocco packed tra i droppati si decrementa
  `refs` del numero di record droppati che lo referenziano; a `refs == 0`
  il blocco va in freelist e si sgancia da `pack_cur` se era il bump
  corrente;
- i blocchi packed non raggiungibili (refusati da ogni record) sono orfani
  e recuperati dalla GC al bind (`reachable_blocks` include i packed via
  `value_blocks`).

Il `pack_cur` è RAM-only: al bind viene ricostruito scegliendo, per bucket,
il blocco packed raggiungibile con più spazio libero (header persistito:
`used`+`bucket`). Questo riusa le code libere tra i remount.

### 5. Quota e statistiche

`quota_used` conta i blocchi packed (dedup per bucket) — corretto perché
`value_blocks` li include. `quota_blocks_for` resta conservativo
(fail-closed); quota 0 = illimitata (sysroot).

`extent_stats`/`disk_xstats`/`arca_extent_stats` estesi con
`n_packed`, `packed_blocks`, `packed_used_bytes`, `packed_cap_bytes`; lo
spreco misurabile è `packed_cap − packed_used`. `ARCA_SUB_STAT`
(`high_water`, `live`, `free_head`) resta l'instrumento oggettivo di blocchi
usati.

## Consequences

### Positive

- Oggetti piccoli dello stesso bucket condividono blocchi: N oggetti da
  1000 B usano `ceil(N·1000/3539)` blocchi invece di N (es. 16 oggetti → 5
  blocchi, ~7× meno spazio).
- Free eager preciso (refcount): nessun leak di blocchi packed condivisi
  oltre la vita dell'ultimo referente; sweep al bind recupera orfani/crash.
- Nessuna nuova syscall né ZIL; si riusa la GC al bind esistente.

### Negative / limiti

- Tra un bind e l'altro le slice packed morte restano occupate (leak
  limitato alla sessione); il rebind le recupera.
- L'epoca globale di W4 si applica anche alle write packed: una write dirty
  può essere salvata da un commit successivo di altra operazione.
- `pack_cur` ricostruito per bucket richiede il bucket nell'header del
  blocco packed (≈21 B di hdr, PACK_CAP = 3539).

## Testing

- **test61 "packing sub-blocco stesso-bucket"** (`testland/testsarca`): 16
  oggetti da 1000 B in uno stesso bucket → `n_packed=16`,
  `packed_blocks < 16`, `packed_used = 16000`; delete tutti →
  `packed_blocks = 0` (refs→0); rimount → `rebuild_pack_cur` sano.
- Test host in `arcafs/src/btree.rs`: condivisione stesso-bucket, isolamento
  cross-bucket, read-back+patch, crescita→extent, delete-uno-sopravvive,
  delete-tutti-libera, codec kind3 roundtrip + rifiuti.
- Gate: `[testsarca] PASS 61/61` + `[testfs] 5/5` + `[testfat] 7/7` +
  `[posixtests] 4/4` + `[usertests] 58/58` + `[threadtest] 12/12`, zero
  FAIL/PANIC/FAULT; host arcafs 29/29.

## Decisioni differite (non promesse)

- Refcount già adottato (era l'alternativa allo sweep-puro): tiene.
- Resume del bump dopo bind già adottato (era l'alternativa RAM-only): tiene.
- Clustering chiavi, shrinker, defrag, hint allocsize: parcheggiati.
