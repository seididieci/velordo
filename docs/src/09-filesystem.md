# File System

## Panoramica

Il file system in un microkernel e' un **servizio userspace** che comunica
con gli altri processi tramite IPC. Il kernel non gestisce i file --
delega tutto al FS server.

```
Processi userspace (PID instabili per riuso: i peer si indirizzano per nome/canale):
  cardo    -- ArcaFS (root su volume, ADR-0059) + FAT32 (opt-in) + mount table
  vela -- /dev/null, /dev/zero
  block  -- driver ATA + nodi /dev/sdX + alias by-uuid/by-label (Fase 16)
  init      -- spawna disk/fs da embedded, il resto da ArcaFS (Fase 21/55)
  shell     -- usa civis wrappers per accedere ai file
```

> **ADR-0059: il FAT e' fuori dal boot.** Il filesystem di sistema e' **ArcaFS**
> (root su volume scelto per UUID dalla cmdline, `root=UUID=…`); il driver
> FAT32 resta in cardo come provider **opzionale** (riferimento
> prestazionale ArcaFS-vs-FAT e base per le chiavette USB) e la sua mount
> statica e' *iniettiva*: senza il disco non si attiva e non tocca nulla.

## Trasferimento dati: ring SPSC per-processo (Fase 10.2)

L'IPC e' register-based (4 x 64-bit = 32 byte). Per trasferire dati bulk
senza copie ne' race, ogni processo ha una coppia di **ring SPSC** dedicati.

### Meccanismo (Fase 10.2, sostituisce 9.6)

1. **Alloca i ring** (`sys_ring_alloc`, syscall 26): il kernel alloca due
   pagine fisiche, le mappa a `USER_FS_BUFFER` (request, = 0x4000_0020_0000)
   e `USER_RESP_RING` (response, = 0x4000_0021_0000) nello spazio del
   chiamante e ritorna i due indirizzi fisici via IpcResult.
2. **Registra presso cardo** (`FS_BUF_REG`, tag 0x31): il client invia i
   due phys a cardo (risolto per nome, servizio `Fs`). cardo memorizza
   `chan → (req, resp)` e mappa il ring del client nella propria finestra
   quando serve.
3. **Operazioni FS su ring**: ogni operazione = 1 frame nel request ring
   `[tag:4][w0:8][w1:8][payload]` + `send(FS_NOTIFY)` (tag 0x32). cardo
   consuma SEMPRE l'intero frame e scrive 1 response frame
   `[result:8][w1:8][payload]`. Il kernel NON e' nel percorso dati (le
   vecchie syscall 3-7/23-24 sono state rimosse).
4. **Device remoti** (`/dev/*`): cardo mappa entrambi i ring del client
   nello spazio del driver remoto (vela/gpu) tramite `map_in`
   (syscall 27, mapper generico cross-process) prima di inoltrare l'IPC.
   Il driver legge/scrive direttamente nei ring del client → **zero-copy**
   anche per `/dev/zero`.
5. **Chunking client-side**: la capacity reale di un ring e' 4087 B
   (dati `[0x0000..0xFF8)`, head a `0xFF8`, tail a `0xFFC`, free = CAP-1).
   I wrapper `read_fs`/`write_fs` di civis splittano payload > ~4000 B in
   piu' round trip (`RING_MAX_PAYLOAD`), cosi' /dev/zero legge 4096 B in
   2 round trip.

```
  cardo finestra:  mappa "ring del client corrente" a USER_FS_BUFFER/
                    USER_RESP_RING
  vela/gpu:       cardo mappa gli stessi ring via map_in
```

### Vantaggi rispetto al design precedente

- **SPSC by construction**: 1 producer + 1 consumer per direzione, niente
  lock e niente race tra client concorrenti
- **Zero-copy in ogni percorso**: anche `/dev/zero` e `/dev/null`
  scrivono/leggono direttamente nei ring del chiamante
- **1 IPC round trip per operazione**: client scrive il frame, notifica,
  il server risponde (prima servivano piu' messaggi con slot kernel)
- **Niente overhead kernel**: il kernel non copia dati e non instrada IPC

### Limiti

- Ring a pagina singola (~4087 B per frame): payload maggiori richiedono
  chunking (Fase 10.2) con round trip multipli
- Il server puo' leggere/scrivere arbitrariamente nei ring del client
  (trade-off del modello SPSC per-processo: il client e' l'unico producer e i
  ring sono mappati solo da server fidati che il client ha registrato)

## Architettura

```
         Client (shell, utente)
              |
              |  open() → civis: frame nel request ring
              |  IPC(FS_NOTIFY)  →  cardo
              v
         +------------------------------------------+
         |         cardo (servizio `Fs`, peer per nome)   |
         |  finestra: ring del client a              |
         |           USER_FS_BUFFER + USER_RESP_RING |
         |  Mount table (`Vec<FsMount>`, longest prefix, lazy):  |
         |    "fat" → FAT32 via UUID (Fase 16d, scrivibile Fase 20)  |
         |    + mount dinamici R_MOUNT/R_UMOUNT (Fase 16b)           |
         |  ramfs: BTreeMap<String, Node>            |
         |  FAT32: BPB + cluster chain (FileInfo per-fd + OPEN-once,
         |        Fase 21)                           |
         +------------------------------------------+
              |                    |
              |  ramfs/fat:        |  map_in + IPC(DEV_*)
              |  read/write       |  verso driver
              |  sui ring         v
              |  del client  +------------------------+
              |               |  driver per nome      |
              |               |  /dev/null → read=0   |
              |               |  /dev/zero → read=0s  |
              |               +------------------------+
              v
         Client: legge il response frame dal response ring
```

## Sub-fasi

> I "Checkpoint" sotto sono i risultati **all'epoca** di ciascuna sotto-fase
> (non il gate corrente). Gate corrente: `[testfs] PASS 5/5` + `[testfat] PASS
> 7/7` + `[testsarca] PASS 11/11` + `[usertests] PASS 57/57` + shell, zero
> FAIL/PANIC.

### 9.1 -- Shared buffer + ramfs server (originale, sostituita da 9.6)

La Fase 9.1 introdusse una shared buffer page unica (`USER_FS_BUFFER`)
e 5 syscall kernel (3-7) che instradavano le operazioni FS tramite IPC.
Questo design ha mostrato una **race condition**: due client che si
intercalano corrompono i dati nella pagina condivisa.

**Checkpoint originale:** ramfs funzionava via IPC (write + read
verification).

### 9.2 -- FAT32 read-only (poi scrivibile in Fase 20)

Driver ATA PIO e parser FAT32 nel processo userspace cardo, allora abilitato
alle porte 0x1F0-0x1F7 via **TSS per-processo** (ADR-0006).

**Checkpoint (all'epoca):** FAT32 read funziona (usertestfat PASS 6/6).

> Fase 16: il driver ATA e' migrato in `block` (entrambi i canali,
> enumerazione IDENTIFY, `/dev/sdX`, [ADR-0012](./adr/0012-userspace-disk-driver.md));
> cardo tiene solo il parser (generico su `BlockSource`) e non ha piu' porte
> ATA. Flusso `/fat/*` invariato per i client.
>
> > Fase 16c: la mappa nome→handle vive nel driver (`DISK_RESOLVE` 0x54 su
> > canale `Disk`); cardo risolve una volta a mount e per nome a ogni
> > riattivazione lazy, con drop d'epoca alla morte del driver
> > ([ADR-0013](./adr/0013-mount-syscall.md)). Raw `/dev/sdX` (`DEV_*`) intoccato.
> >
> > > Fase 16d: chiavi stabili `UUID=<hex8>`/`LABEL=<nome>` (seriale/label del
> > > volume FAT) al posto delle lettere instabili; nodi `/dev/disk/by-uuid/*`
> > > e `/dev/disk/by-label/*` registrati da block; listing dei padri
> > > sintetizzato dai prefix; registrazione multi-prefix atomica
> > > (`fs_register_multi`). Vedi ADR-0012/0013, t36 + `test-uuid-reorder.py`.

### 9.3 -- hub `/dev` separato + IPC routing (all’epoca: server devfs)

Device file server (`/dev/null`, `/dev/zero`) registrato presso cardo
tramite `FS_REGISTER`. Mount table dinamica con prefix-based resolution.

**Checkpoint (all'epoca):** /dev/null e /dev/zero funzionano (usertestfat PASS 6/6).

### 9.4 -- Shell integration

La shell (`usershell`) usa `civis` wrappers per leggere/scrivere file:
ls, cat, touch, mkdir, help, exit. Il server gpu gestiva la VGA e
(all'epoca) la tastiera; la shell opera sullo stesso fd del device
`/dev/input`. (Dalla Fase 15: gpu solo rendering `/dev/console`,
tastiera in `kbd`/`porta`; comandi estesi in Fase 18.)

**Checkpoint (all'epoca):** test-shell.py PASS 3/3.

### 9.5 -- Split layout + suite di regressione

Separazione `userland/` (servizi) e `testland/` (test/demo). Suite di
regressione 17 test con riga riepilogo `[usertests] PASS 17/17`.

### 9.6 -- Buffer per-processo + zero-copy IPC (sostituita da 10.2)

Rimozione della shared buffer page unica e delle syscall kernel 3-7/23-24.
Ogni processo alloca la propria pagina e la registra presso cardo
(`FS_BUF_REG`). Operazioni FS = IPC dirette client→cardo. Per i device
remoti, cardo mappa la pagina del client nel driver (`map_in`, syscall 27).
(Dalla 10.2: DUE ring SPSC per processo via `ring_alloc`, syscall 26.)

**Checkpoint (all'epoca):** testfs 5/5, testfat 6/6 (incl. /dev/null + /dev/zero),
usertests 17/17 (incl. churn vela concorrente), shell 3/3.

### 10.2 -- Ring SPSC per-processo (sostituisce 9.6)

La singola pagina FS e' sostituita da DUE ring SPSC per processo
(request a `USER_FS_BUFFER`, response a `USER_RESP_RING`), allocati da
`sys_ring_alloc` (syscall 26, riusa il vecchio slot). Ogni operazione FS
e' 1 frame nel request ring + `send(FS_NOTIFY)` (0x32); cardo consuma
l'intero frame e risponde con 1 response frame `[result][w1][payload]`
— eccezione: per i WRITE verso device remoti il frame NON viene consumato
da cardo (dedicato `handle_write_remote`: il payload resta nel request
ring e il driver lo legge direttamente, avanzando la tail).
Registrazione driver: `FS_BUF_REG` (0x31) per i ring + `FS_REGISTER` (0x30)
con frame `R_REGISTER` nel request ring. Device remoti: cardo inietta
entrambi i ring del client nel driver via `map_in` (27) — il driver
scrive/legge direttamente (zero copie). Libr splitta payload > ~4000 B
in piu' round trip (chunking multi-frame, Fase 10.2.4).

**Checkpoint (all'epoca):** testfs 5/5, testfat 6/6 (incl. /dev/null + /dev/zero),
usertests 17/17, shell 3/3.

## Ordine di implementazione

```
9.1  Shared buffer + ramfs server                                [x]
9.2  FAT32 read-only (TSS per-processo, ATA PIO)                [x]
9.3  hub `/dev` separato + IPC routing (server vela)              [x]
9.4  Shell integration (ls, cat, touch, mkdir, help, exit)       [x]
9.5  Split layout userland/testland + suite di regressione        [x]
9.6  Buffer per-processo + zero-copy IPC (rimozione shared buf)  [x]
10.2 Ring SPSC per-processo (sostituisce 9.6)                     [x]
16   Disk driver in userspace (block + cardo senza ATA)        [x]
16b  Mount/umount espliciti (tabella Vec<FsMount>, R_MOUNT/R_UMOUNT) [x]
16c  Resolve nome→handle lato driver (DISK_RESOLVE, single source)   [x]
16d  Identità stabile UUID/LABEL + listing + register multi-prefix     [x]
17   Diritti per-canale lato server ([ADR-0014](./adr/0014-channel-rights-serverside.md): tabella chan→{ops,subtree}, DROP solo-shrink + GET, fd capability pure) [x]
19.2 Metadati senza open (R_STAT 0x1B, risposta self-written `[size:8][kind:8]` + `[mtime:8]` dalla Fase 50: ramfs size reale, FAT mai readonly dalla Fase 20, device size 0, check RIGHTS_READDIR+subtree, `civis::stat`, t38) [x]
51   P2 vocabolario disco (relay topologia): `DISK_LIST/INFO` (0x56/0x57: entry `[sectors:8][flags:8]`, INFO + frame 76 B modello/seriale) + `R_DISK_LIST/INFO` (0x21/0x22, gate READDIR, self-written) + `IpcDisk::list/info` + `civis::disk_list/info` (`DiskDesc` con accessori flags); t32 esteso [x]
52   P3 durabilita' (contratto + barriera + sensori): `R_SYNC` (0x23, modi `SYNC_NONE/GROUP/PERWRITE`, ritorna prev umask-like, `GROUP` = FLUSH dei mount FAT via `DISK_FLUSH` 0x58, gate `RIGHTS_SYNC` 0x800) + `R_STATVFS` (0x24, `StatVfs` nel trait, FAT da FSInfo, ramfs illimitata) + `SYS_MEMINFO` 52 (frame free/total/used); t32/t37 estesi [x]
54   P5 integrita' + attrezzi: crate `blake2s` (BLAKE2s-256 proprio, no_std/no_alloc), `R_GET_HASH` 0x25 (content_hash compute-on-query, opzione A), superblock ArcaFS (single source `ARCA_*`), `negotiate()`→`arcafs` (stub `MountedFs::Arca`), tool host `arca create` (`tools/arca`), guest `arca list/stat` (`/bin/arca.bin`), `testsarca` 8/8 [x]

## ArcaFS P5 (Fase 54)

- **Superblock** (128 B a LBA0 + shadow LBA1): magic `ACFS`, versione, block
  size 3584, `volume_uuid`, `generation`, root/refcount/alloc, mountpoint,
  flags, checksum FNV-1a self-verifying. Offset costanti in `syscall-numbers`
  (`ARCA_OFF_*`): single source di cardo `negotiate()`, guest `arca` e tool
  host `create` (mai duplicati; il round-trip create→mount→stat lo verifica).
- **`negotiate()`**: legge LBA0 del nodo e prova ArcaFS PRIMA di vfat (match
  stretto: magic + versione + block size + checksum). Monta un'istanza
  `MountedFs::Arca(ArcaFs)`; la vista POSIX e' completa dalla 56.3
  (ADR-0042: dir emergenti + set RAM, bucket `ns`, motore globale a uuid
  combaciante), la vista nativa e' `R_OBJ_PUT`/`R_OBJ_GET` su store
  persistente (56.2: bucket `sys` seedato dal volume a ogni avvio).
  Mount ArcaFS anche in partizione
  MBR/GPT (superblock partition-relative, parser GPT per spec UEFI).
- **`R_GET_HASH`** (`civis::get_hash`, opzione A compute-on-query): cardo
  rilegge il file a chunk 4K e calcola BLAKE2s-256 (nessuno stato, nessuno
  store; il seal per-versione arriva con ArcaFS in A1). `BLAKE2s` proprio
  (crate `blake2s`, RFC 7693: vettori generati da due implementazioni
  indipendenti) usato anche dal seal futuro e dal tool host.

## Contratto di durabilita' (Fase 52, P3)

Cosa e' stabile, e quando (misurato, non presunto):

| Op | Stabile quando |
|----|----------------|
| ramfs write/create/mkdir | Mai su disco: visibile al `read` dopo la reply, perso a restart/reboot (ogni modo, per disegno) |
| FAT overwrite entro size | Al ritorno `n`: ogni chunk e' oltre `FLUSH CACHE` (PIO per-settore/per-run, DMA via `finish_dma`) |
| FAT grow/create/truncate | `size`/entry stabile a `patch_entry` flushato (commit point); crash prima = vecchia size + cluster orfani fsck-fixabili |
| ArcaFS put/delete namespace (56.3) | Al ritorno: commit shadow+flip (gen+1); crash prima = vecchia generazione intatta (COW + root-last) |
| ArcaFS mkdir/rmdir | Mai su disco per disegno (set RAM): visibili subito, perse le vuote a restart |
| `R_SYNC(GROUP)` | Barriera subito: FLUSH CACHE su ogni mount FAT attivo (write-cache del drive) + commit ArcaFS; ramfs intoccata |
| `R_SYNC(NONE/PERWRITE)` | Dichiarazioni registrate per-canale (prev ritornato); `PERWRITE` e' gia' il FAT, ramfs resta volatile |

Sensori: `R_STATVFS` (spazio mount: FAT blocchi=cluster da FSInfo con clamp, ramfs usati camminati + `MAX` illimitato) e `SYS_MEMINFO` (frame liberi/totali/usati del PMM; il kernel non decide mai: niente OOM-kill). Quota per bucket chiusa in A3 (ADR-0044: budget blocchi dati, `ERR_NOSPC`, tabella nella foglia meta); resta gancio per swap (B1).
50   P1 orologio (mtime veri): `usertime` (CMOS+Time), baseline lazy in cardo (`wall.rs`, niente IPC per-op), `mtime` su `FsNode`/decode DOS WrtTime/Date + stamp a create/grow/truncate, `Meta.mtime` via trait, `civis::Stat.mtime`; t38 esteso (Time monotono, mtime plausibile+crescente, FAT noto) [x]
20   FAT32 scrivibile ([ADR-0016](./adr/0016-fat-writable.md): DISK_WRITE + write PIO + overwrite/grow/alloc/OPEN_CREATE write-through, `testfat` 7/7, fsck pulito) [x]
21   Servizi da disco: cardo con cache FileInfo per-fd + generazione (bump a ogni mutazione FAT; stat sempre fresca) e `IpcDisk` con OPEN-once per connessione (re-OPEN solo a canale caduto) — dimezza i round-trip DISK dei load da disco [x]
46   Provider trait (`LocalFs` + `LocalFsDyn` + `DynHandle<T>`, enum `MountedFs::Local`) — scaffolding [x]
47   U1 wiring: handler cardo instradano via `LocalFs` per ramfs (open/read/write/readdir/stat/mkdir/delete), fix mkdir esiste→ERR_EXISTS, fix read oltre EOF→0; zero behavioral regression, gate 5/5+7/7+57/57 [x]
48   U2 wiring: handler cardo instradano via `LocalFsDyn` per FAT32 (read/write_local/open/readdir/stat), fix stat readonly FAT→false (Fase 20), `Fat32<B>` implementa `LocalFsDyn` (handle boxati); create_file/truncate restano FAT-specifici; zero behavioral regression, gate 5/5+7/7+57/57 [x]
49   T0 terreno pre-ArcaFS (un solo gate): handle unico `AnyHandle` by-value (niente Box/`*const ()`), mount-id `u64` stabili, `Source`+`negotiate()`+`fstype`, `R_MOUNT "ramfs"` (variante `Local` viva), create/truncate in `Fat32::open`; zero behavioral regression, gate 5/5+7/7+57/57 [x]
```

## File coinvolti

| File | Ruolo |
|------|-------|
| `kernel/src/arc/vmm_user.rs` | Ring per-processo (`RING_PHYS` multi-coppia, `alloc_ring_pages` a coppie fresche, `USER_FS_BUFFER`, `USER_RESP_RING`) |
| `kernel/src/syscall/` | Handler `sys_ring_alloc` (26), `sys_map_in` (27, generico) |
| `kernel/src/ordo/sched.rs` (esposto come `crate::ordo::sched`) | `process_cr3` (per map_in) |
| `libs/civis/src/lib.rs` | Wrappers FS su ring + `fs_init` lazy + chunking read/write + `map_in` + `ring_alloc_raw` (coppia senza handshake, Fase 16) |
| `userland/cardo/src/main.rs` | cardo: finestra ring, registro `chan→(req,resp)` + `ftable`/`next_fd` per canale, map_in per device remoti |
| `userland/cardo/src/ipc_disk.rs` | client `DISK_*` verso block (`BlockSource`, riconnessione lazy, Fase 16; resolve nome→handle + map di entrambi i ring, Fase 16c; OPEN-once per connessione, Fase 21) |
| `userland/cardo/src/provider.rs` | Trait `LocalFs` (presentazione POSIX), `LocalFsDyn` object-safe con `AnyHandle` by-value (Fase 49: niente piu' `*const ()`/`DynHandle`), `RamFs`+`Fat32` con match discriminato — Fase 46 scaffolding, Fase 47 wiring ramfs, Fase 48 `LocalFsDyn` FAT32, Fase 49 handle unico |
| `userland/block/src/main.rs` | block: detect+part, `/dev/sdX`, protocolli `DISK_*`+`DEV_*` (Fase 16; `DISK_RESOLVE` single-source-of-truth, Fase 16c) |
| `userland/vela/src/main.rs` | vela: `/dev/null`, `/dev/zero` |
| `userland/gpu/src/main.rs` | gpu: rendering `/dev/console` su VGA (tastiera in `kbd`/`porta` dalla Fase 15) |
| `userland/rector/src/main.rs` | rector: spawn servizi + test in sequenza |
| `syscall-numbers/src/lib.rs` | Costanti `SYS_RING_ALLOC=26`, `SYS_MAP_IN=27` |

## Riferimenti

- [OSDev Wiki - File Systems](https://wiki.osdev.org/File_Systems)
- [OSDev Wiki - FAT](https://wiki.osdev.org/FAT)
- [OSDev Wiki - ATA PIO](https://wiki.osdev.org/ATA_PIO_Mode)
- seL4 IPC bulk data transfer
- Redox OS schemes (filesystem come processi userspace)
