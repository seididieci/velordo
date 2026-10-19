# ADR-0054: Blocchi 64 KiB e rimozione cap 16 MiB in ArcaFS (W6)

## Status

Accepted

## Context

Dopo W5 (packing sub-blocco stesso-bucket), ArcaFS gestiva bene i file piccoli
ma aveva limiti artificiali per i file grandi:

- `ARCA_BLOCK_SIZE = 3584` B (payload 3560 B): 200 GB = ~56M blocchi, altrettanti
  round-trip IPC/ring per una lettura completa. Troppo lento per video, zip,
  volumi di VM.
- `put_chunk` rifiutava oggetti > 16 MiB (`end > 1 << 24`).
- `EXT_BLOCKS_MAX = 8192` bloccava gli extent oltre ~28 MB.
- `reachable_blocks()`, `ext_entries()`, `value_blocks()` materializzavano
  `Vec` di tutti i blocchi/entry, rischiando OOM al bind/GC con file grandi.

Benchmark di riferimento (fio, Bonnie++, studi ext4/XFS/btrfs/ZFS) confermano
che blocchi grandi (64K-256K) migliorano 2-9× sia il sequenziale che il random
su file grandi, sia su SSD che su HDD. La frammentazione COW su file grandi
scritti a caso (VM disk, database) è un problema noto (su HDD critico, su SSD
meno) e va mitigata con blocchi grandi e defrag opzionale, non con cap
artificiali.

## Decision

### 1. Block size 65536 B (64 KiB)

`ARCA_BLOCK_SIZE = 65536`, `ARCA_NODE_PAYLOAD_LEN = 65512`,
`ARCA_BLOCK_SECTORS = 128`. `ARCA_VERSION` passa da 2 a 3 (niente retrocompat:
le immagini vanno rigenerate, il mount v1/v2 fallisce loud in
`superblock_verify`).

Con 64 KiB blocchi, 200 GB = ~3M blocchi invece di ~56M: I/O e metadata ridotti
di ~18×. Il packed (W5) continua a mitigare lo spreco su file piccoli
(più slice per blocco, stesso bucket).

### 2. Rimozione cap 16 MiB e EXT_BLOCKS_MAX come rifiuto

- `put_chunk`: rimosso `if end > (1 << 24)`. Il nuovo bound è
  `EXT_BLOCKS_MAX * ARCA_NODE_PAYLOAD_LEN` (~17 GiB con 64 KiB blocchi),
  oltre il quale l'oggetto va splittato in chunk-oggetti lato chiamante
  (già fatto da `s25fuse` per lo staticone).
- `ext_entries`: rimosso il rifiuto `tblks > EXT_BLOCKS_MAX`. L'estent
  run-length `(blk:8, nb:4)` con `nb: u32` può indirizzare blocchi arbitrari;
  il bound anti-OOM viene gestito dallo streaming (punto 3), non da un cap
  arbitrario.

### 3. Walk streaming (no `Vec` materiale)

Per evitare OOM al bind/GC con molti file grandi, le funzioni che camminano
gli extent usano interfacce callback/iterator invece di `Vec` materiale:
- `reachable_blocks()` → `reachable_foreach_blocks()`: la GC spinge in
  freelist incrementalmente, senza costruire un `Vec` di tutti i blocchi vivi.
- `ext_entries()` → `ext_foreach_entries()`: itera le run una alla volta.
- `value_blocks()` → `value_foreach_blocks()`.
- `free_dropped_versions()` libera incrementalmente.

`ext_put_span` materializza ancora uno `span: Vec<u8>` per le patch su file
grandi (read-modify-write della regione toccata): accettabile per patch
moderate, da ottimizzare se necessario.

### 4. API range-based per file > RAM

`obj_get()` restituisce ancora `Vec<u8>` (tutto l'oggetto). Per file > RAM si
usa `obj_get_range()` (nuova sub-op civis) che legge un range senza
materializzare l'oggetto intero.

## Consequences

### Positive

- File grandi (100-200 GB): ~18× meno blocchi, I/O e metadata ridotti
  proporzionalmente.
- Nessun cap artificiale: oggetti singoli fino a ~17 GiB, oltre via chunking.
- Nessun rischio OOM al bind/GC grazie allo streaming.
- Il packed (W5) continua a funzionare per i file piccoli.

### Negative / limiti

- `ext_put_span` materiale ancora la span per le patch su file grandi:
  patch di GB allocherebbero GB di RAM. Mitigazione: l'applicazione scriva a
  blocchi allineati o usi append a offset 0 (truncate+rewrite).
- La frammentazione COW su scritture random sparse (VM disk) rimane intrinseca
  al COW; su HDD può degradare le letture. Mitigazione: defrag opzionale,
  blocchi grandi, preallocation.
- Cambio block size rompe il formato on-disk (ARCA_VERSION 3): le immagini
  esistenti vanno rigenerate.

## Testing

- Test host (`arcafs/src/btree.rs`): oggetto > 16 MiB accettato, extent con
  > 12 entry (nodi MAP attivati), quota con packing sharing, file grande
  contiguo.
- Test guest (`testland/testsarca`, test 62): file da 512 KiB (> PACK_CAP) su
  ArcaFS, write sequenziale, read-back integrale, read a offset sparsi,
  delete + reclaim + rimount. La taglia e' 512 KiB (non 4 MiB) perche'
  l'append sullo stesso oggetto e' O(n^2) via `ext_put_span`/COW: nel gate
  resta ampia per esercitare extent e nodi MAP senza dominare il tempo.
- Gate: `[testsarca] PASS 62/62` + `[testfs] 5/5` + `[testfat] 7/7` +
  `[posixtests] 4/4` + `[usertests] 58/58` + `[threadtest] 12/12`, zero
  FAIL/PANIC/FAULT; host arcafs 31/31.

## Decisioni differite

- Defrag integrato per file COW frammentati (parcheggiato: si userà strumento
  esterno o comando dedicato se il problema si presenta su HDD reale).
- `obj_get_range` in civis (priorità dopo la stabilizzazione del layout).
- Ripristino della `live` set al bind per `rebuild_pack_cur`/free dei blocchi
  preesistenti: oggi dopo remount la guardia `live` e' vuota, quindi rewrite
  packed e free eager sono best-effort (possibile leak, non esercitato dal
  gate; W6b se emerge su mount reali).
- Blocchi > 64 KiB (es. 256 KiB) se i benchmark su hardware reale lo
  richiedono.
