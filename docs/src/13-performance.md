# Performance (Fasi 23–25 — baseline + ottimizzazioni; 38 — DMA/IRQ; A/B ArcaFS/FAT)

> **ADR-0058**: il confronto A/B ArcaFS-vs-FAT resta valido (e' il motivo
> per cui il provider FAT32 e' ancora nel tree), ma il **FAT e' fuori dal
> boot**: il bench lo attacca con `FAT_IMG=1` (run opt-in, non nel gate).

> Piattaforma di riferimento: **KVM** (`-accel kvm -cpu host`). I tempi TCG
> sono emulati e NON di riferimento. Orologio: TSC in ring 3 (CR4.TSD mai
> impostato), calibrato sul PIT via `civis::tsc_calibrate` (~4.45 GHz sul
> riferimento 23/24, ~1.6 GHz sull'host della campagna 25: confrontare solo
> misure dello STESSO host). Nessuna cache nel percorso dati fino a 24
> (25 aggiunge la cache settoriale, vedi sotto).

## Harness

- `testland/bench` (`userbench`, `/test/bench.bin`): op end-to-end con
  warmup parametrico (`run(name, iters, warm, …)`), stampa righe
  `[bench] <nome> iters=<n> warm=<w> cyc_op=<c> max_cyc=<m> kb_s=<k>`.
- `scripts/bench.sh`: N boot KVM, fail-loud se il bench non completa
  (`DONE ok=1`); al termine esegue `scripts/bench-compare.sh` (due tabelle,
  rapporti e spread tra run). Esecuzione: `RUN_BENCH=1` compila init con
  feature `bench` (ortogonale a `skip_tests`): il bench gira dopo l'eventuale
  suite, prima della shell — **mai nel gate** di regressione.
- `scripts/bench-split.sh`: modalita' **per-size** (consigliata su host lenti
  e per evitare un timeout unico): un boot per gruppo (`small`, `4k`, `16k`,
  `64k`, `256k`, `1m`, `4m`, `native`), con timeout per-gruppo (1800 s;
  2400 s per `1m`) e log separati `/tmp/bench-<gruppo>.log`. Ogni run seeda
  `/test/bench-group.txt`; il guest esegue solo le righe del gruppo e stampa
  il suo `DONE ok=1`. Alla fine i log vengono passati insieme a
  `bench-compare.py` (nessuna riga duplicata, quindi niente spread tra
  gruppi). Si lancia anche come `BENCH_PROFILE=split ./scripts/bench.sh`;
  `BENCH_GROUPS="1m 4m native" ./scripts/bench-split.sh` limita i gruppi.
- **Profili** (`BENCH_PROFILE`, default `full`): `full` = campagna di
  riferimento (`RUNS=3`, `TIMEOUT_S=5400`, `ARCA_SIZE_MIB=4096`); `reduced` =
  validazione rapida su host lenti (`RUNS=1`, `TIMEOUT_S=900`,
  `ARCA_SIZE_MIB=512`, feature cargo `reduced` che salta 1M/4M e nativo 1M).
  Ogni default e' sovrascrivibile via env (`RUNS`, `TIMEOUT_S`,
  `ARCA_SIZE_MIB`, `BENCH_REDUCED`).
- **Early-stop**: `bench.sh` avvia QEMU in un process group (`setsid`) e
  chiude appena compare `DONE ok=` nel log; il `TIMEOUT_S` resta la rete di
  sicurezza per build/boot bloccati (QEMU resta sulla shell dopo il bench).
  `DONE ok=0`, timeout o uscita senza `DONE` sono fail-loud.
- **Fixture grandi pre-seedate** (`RUN_BENCH=1`, solo full): `arca-tool.sh`
  scrive host-side SOLO i file `read-only` ArcaFS (4M hot/cold, 1M cold-read);
  sul FAT (che ora richiede il run opt-in `FAT_IMG=1`, ADR-0058) i fixture
  1M cold-read e 4M li crea il bench stesso (`inject-bins.sh` non esiste
  piu': i binari sono nel volume ArcaFS). `ensure_fixture`
  nel bench salta la creazione se `stat` riporta la size attesa. Evita
  1024/262 chunk di creazione prima della misura cold e, sul FAT, la crescita
  guest-side del 4M (walk della catena a ogni chunk: patologicamente lenta).
  I fixture scrivibili (1M hot, 1M cold-write, `bch:k1m`) restano creati dal
  guest: un write su un file ArcaFS seedato da host fallisce dopo ~28 KiB
  (`ns_put` patch) mentre il file creato guest-side funziona — bug del motore
  annotato, fuori dallo scope della campagna. Il gate non imposta
  `RUN_BENCH`, quindi non se ne accorge.
- **Parser**: `bench-compare.py` ignora il prefisso seriale `[<sec>]` del
  kernel; le righe `[bench]` restano la single-source dei campi.

## Baseline 23 (KVM, media 3 run, TSC ~4.45 GHz) → 24 (stessa base)

| Op | 23 cyc/op | 24 cyc/op | 24 latenza | 24 throughput |
|----|-----------|-----------|------------|---------------|
| `zero_1B` (2000× read 1 B `/dev/zero`) | ~8.2 K | ~8.3 K (invariato) | ~1.9 µs | — |
| `sda_512B_seq` (200× read 512 B `/dev/sda`) | ~5.43 M | ~5.45 M (invariato) | ~1.2 ms | ~409 KiB/s |
| `fat_small_orc` (500× open+read+close 25 B) | ~92.6 M | ~10.9 M (**8.5x**) | ~2.4 ms | ~10 KiB/s |
| `ramfs_4K_write` (100× write 4 KiB) | ~110 K | ~109 K (invariato) | ~25 µs | ~161 MiB/s |
| `ramfs_4K_read` (100× read 4 KiB) | ~62 K | ~61 K (invariato) | ~14 µs | ~289 MiB/s |
| `fat_4K_oow` (50× open+overwrite+close 4 KiB) | ~285 M | ~129 M (**2.2x**) | ~30 ms | ~135 KiB/s |

## 24 — cosa ha funzionato

- **24.1 (solo cardo, nessun cambio di protocollo)**: memo dell'ultimo
  settore FAT (invalidata a `set_fat_entry`, azzerata a ogni epoca) +
  letture a settori mirati (`read_file` per span, `read_dir` con stop al
  terminatore 0x00 invece dell'intero cluster). Da sola: small FAT 8.5x.
- **24.2 (protocollo DISK v2, stessi tag)**: frame con count (≤7/IPC, bound
  del ring), 1 comando PIO per run (`AtaDisk::read/write_sectors`), 1
  FLUSH CACHE per write (prima: comando+flush a settore). Da sola sopra
  24.1: overwrite 4K 36 ms → 30 ms. `BlockSource::{read,write}_sectors`
  (default a loop, `IpcDisk` in vero multi); DEV relay intatto.

## Lezione 24.2 — heap dei server: niente `Vec` temporanei per-op

A metà 24.2 i bench crollavano progressivamente (write ramfs 25 µs →
2 ms) in proporzione alle op FAT precedenti — con gate verde e risultati
corretti. Diagnosi (strumento temporaneo `civis::heap::heap_stats`,
mantenuto): la free-list first-fit di cardo cresceva di ~1 blocco a op
FAT (temp `Vec` 4K/512 B liberati tra blocchi vivi, mai coalescibili) e
ogni allocazione paga O(n) + O(n²) di `coalesce()` su tutte le op
successive, anche ramfs. Cura: hot path FAT zero-alloc — parse dir
incrementale con offset aritmetici, run in buffer stack a chunk ≤8,
risposta read in stack (count ≤ 4096 già garantito). Regola: **nei server,
mai allocazioni heap nel percorso per-op** (solo a setup/mount).

## Lettura

- Lo stack FS+IPC senza disco vola (14–25 µs): **il collo di bottiglia è
  il percorso disco**, non l'IPC in sé (floor ~2 µs/op).
- Un settore da disco costa ~1.2 ms: 2 round-trip IPC + handoff scheduler +
  ~266 uscite KVM (polling PIO porta per porta).
- `fat_small_orc` (~21 ms per 25 B) e `fat_4K_oow` (~64 ms) mostrano il
  moltiplicatore: ogni op logica = MANY settori (walk catena FAT con re-read
  del settore FAT a ogni cluster, find per open, read-modify-write +
  FLUSH CACHE dedicato per settore in scrittura).
- Le latenze a singola op interagiscono col quanto scheduler (20 ms):
  il riferimento per le ottimizzazioni (24) è il throughput, non la
  latenza minima.

## Soglia di non-regressione

Peggioramento > 10% su una qualunque riga (stesso host KVM, media 3 run)
= fail. Rivalutare la baseline solo a parità di hardware e versione QEMU.

## 25 — cache settoriale write-through in block (ADR-0018)

Un solo strato di cache a blocchi nel driver (`userland/disk/src/cache.rs`:
256 entry, chiave fisica `(disco, lba)`, CLOCK, write-through, zero heap nel
per-op); `fat_memo` (24.1) rimosso da `cardo`. Protocollo `DISK_*` invariato.

Confronto A/B **sullo stesso host** (KVM, media 3 run, TSC ~1.6 GHz;
la tabella 24 sopra e' di un altro host e NON e' confrontabile):

| Op | 24 stesso host (cyc/op) | 25 (cyc/op) | Effetto |
|----|-------------------------|-------------|---------|
| `zero_1B` | ~5.9 K | ~6.9 K | invariato (no disco) |
| `sda_512B_seq` | ~3.04 M | ~3.75 M | invariato entro il rumore¹ |
| `fat_small_orc` | ~6.14 M (~6 KiB/s) | ~49 K (~838 KiB/s) | **~126x** |
| `ramfs_4K_write` | ~93 K | ~117 K | invariato entro il rumore¹ |
| `ramfs_4K_read` | ~64 K | ~76 K | invariato entro il rumore¹ |
| `fat_4K_oow` | ~86 M (~81 KiB/s) | ~46 M (~146 KiB/s) | **~1.9x** |

¹ Rumore misurato: su questo host run identici dello stesso binario variano
±20–40% sulle op brevi (DVFS: tsc 1.62–1.68 GHz tra run; `fat_4K_oow`
baseline 57–97 KiB/s). Si dichiara solo cio' che supera di molto il rumore:
le re-read degli stessi settori (small FAT) non pagano piu' PIO; gli
overwrite pagano ancora PIO+FLUSH per settore (write-through) ma le re-read
di FAT/dir vanno in cache; il sequenziale freddo resta PIO (niente
read-ahead in 25, volutamente).

Hit rate: bench 74% (1514 hit / 534 settori via PIO), suite 91%.
Gate invariato (5/5 + 7/7 + 44/44 + shell 30/30).

## 38 — ATA DMA + IRQ (ADR-0029)

Motore Bus-Master PIIX in `block` (staging 1 pagina via `SYS_DMA_ALLOC`,
PRD split 64K, `READ/WRITE DMA EXT` UDMA2, fallback PIO per-op; protocollo
`DISK_*` invariato, DEV relay resta PIO) + attesa event-driven del
completamento (IRQ14/15 → notify, wakeup-preemption centrale in `notify_irq`,
guardie reply in `pop_msg` per notify/EXIT).

Confronto A/B **sullo stesso host** (KVM, media 3 run, TSC ~4.42 GHz; le
tabelle 24/25 sopra sono di altri host e NON confrontabili — poll = tree
38.1c, event = 38.2 con preemption):

| Op | poll 38.1c (cyc/op) | event 38.2 (cyc/op) | Effetto |
|----|---------------------|---------------------|---------|
| `zero_1B` | ~8.7 K | ~8.8 K | parità (no disco) |
| `sda_512B_seq` | ~5.47 M | ~5.55 M | parità (DEV relay = PIO in entrambi) |
| `fat_small_orc` | ~1.25 M | ~1.22 M | parità entro il rumore |
| `ramfs_4K_write` | ~112 K | ~116 K | parità (no disco) |
| `ramfs_4K_read` | ~61 K | ~61 K | parità (no disco) |
| `fat_4K_oow` | ~18.9 M | ~18.3 M | parità entro il rumore |

Tutte le righe entro la banda ±10% (soglia repo): nessuna regressione,
nessun miracolo — le op sono device-bound e il guadagno è altrove:

- **CPU non più bruciata in poll** (per costruzione): il poll 38.1c spinnava
  ~device-time a transfer a priorità Normal; l'event-driven dorme in `recv`
  e si sveglia via IRQ con switch diretto. Misura diretta (`ticks_used` di
  block ogni 512 xfers): su questo host (IO cached, device ~50 µs) le due
  versioni sono indistinguibili (+17–19 tick/512 in entrambi — il costo
  dominante resta memcpy/handling/ring, identico); il risparmio scala col
  tempo-device e conta sotto carico o su device lenti. Dichiarato il bounds,
  non gonfiato il numero.
- **Latenza IRQ→processo sub-tick per tutti i driver** (tasti inclusi) +
  prerequisito per audio CBS e server-run async (parcheggiati).

Cosa NON ha funzionato (tenuto a lezione, come 24.2-heap):

- Event-driven senza preemption: ogni wait pagava ~1 tick di wake differito
  (firme: `cyc_op` identici tra run = multipli di tick) — fat_small
  1.3M→180M cyc (~140x), fat_4K_oow 20M→810M (~40x). L'IPC sync fa handoff
  diretto, l'IRQ era l'unico wakeup differito: la preemption lo chiude.
- EXIT altrui in `wait_dma` clobberava la reply (canale reale) → wedge
  permanente a fine suite (morte usertests durante shell-load), visto in
  `test-shell.py`. Fix alla radice (`pop_msg` salta anche gli EXIT: rispondere
  a un morto è impossibile per disegno) — da userland la reply non si può
  ri-armare (niente `reply_to`).

Gate invariato (5/5 + 7/7 + 52/52 + shell verde); `ev_wait`≈transfer,
`fb=0`, `abort=0` su 4000+ transfer; `irq_drained` conta i re-fire
level-triggered (deterministici: identici tra run).

## 53 — Misura bulk P4 (round-trip vs dimensione)

Solo misura (Fase 53, P1–P5 OS-first verso ArcaFS): sweep 4K/16K/64K su
ramfs + FAT caldo/freddo, zero cambi di formato, zero pagine extra. Righe
`bulk_*` in `testland/bench` (stesso harness/`bench.sh`, mai nel gate).

Metodologia (lezioni apprese incluse):

- Freddo = **file distinti + spoiler unico**: ogni iter tocca settori mai
  visti (dati + catene FAT freddi; la dir padre va hot dopo iter 1, caveat
  documentato). Il primo tentativo (spoiler prima di OGNI iter) misurava
  spoiler+op con varianza dello spoiler (±25%) superiore al segnale —
  scartato dopo una campagna che lo ha provato: i cold venivano flat ~1.1G
  e persino inferiori agli hot. I numeri sotto sono op puri.
- `bulk_spoil_300sec` resta come riferimento metodologico (300 settori ≈
  costo eviction, ~1.1G cyc su questo host).
- Metodologia uniforme open/op/close per iter a offset 0 (steady state);
  b4/b5 restano gli anchor storici (grow-walk / oow). ramfs: serie singola
  (hot=cold in RAM, niente DISK/cache). Iter decrescenti con la size.
- Audit CAP single-source: `RING_DATA_CAP=4088` + `RING_MAX_PAYLOAD=4000`
  restano l'unica sorgente in `civis` (unico straggler trovato e fissato:
  `porta` clippava a letterale `4000`). Bound distinti intoccati: server
  `expect` 4096 (scratch per-op), `DISK_MAX_SECTORS` 7×512=3584 (fit ring),
  DEV relay 4096 (pre-esistente).

Campagna sullo stesso host (KVM, media 3 run, TSC ~1.66 GHz; tabelle
precedenti di altri host NON confrontabili; spread = max−min sui 3 run):

| Op | cyc/op medio | KiB/s medi | Spread | Lettura |
|----|--------------|------------|--------|---------|
| `bulk_ramfs_16K_write` | ~340 K | ~79 K | 10% | lineare da b4 |
| `bulk_ramfs_16K_read` | ~348 K | ~78 K | 15% | come write |
| `bulk_ramfs_64K_write` | ~1.30 M | ~83 K | 16% | ~20 cyc/B costanti |
| `bulk_ramfs_64K_read` | ~1.29 M | ~84 K | 11% | zero-copy al floor memcpy |
| `bulk_fat_4K_write_hot` | ~19.2 M | ~355 | 29% | ~4.7K cyc/B |
| `bulk_fat_4K_read_hot` | ~2.05 M | ~3.3 K | 45% | metadata ~2M fissi |
| `bulk_fat_16K_write_hot` | ~63.7 M | ~381 | 37% | ~3.9K cyc/B |
| `bulk_fat_16K_read_hot` | ~12.8 M | ~2.2 K | 62% | costo/B che cresce |
| `bulk_fat_64K_write_hot` | ~275 M | ~419 | 63% | ~4.2K cyc/B, lineare |
| `bulk_fat_64K_read_hot` | ~83 M | ~1.3 K | 18% | superlineare (v. sotto) |
| `bulk_fat_4K_write_cold` | ~20.8 M | ~328 | 31% | ≈ hot (write-through) |
| `bulk_fat_4K_read_cold` | ~3.76 M | ~2.1 K | 95% | 1.8x hot (metadata) |
| `bulk_fat_16K_write_cold` | ~75 M | ~365 | 30% | ≈ hot |
| `bulk_fat_16K_read_cold` | ~11.9 M | ~2.4 K | 54% | ≈ hot entro rumore |
| `bulk_fat_64K_write_cold` | ~253 M | ~425 | 11% | ≈ hot |
| `bulk_fat_64K_read_cold` | ~67.8 M | ~1.6 K | 8% | ≈ hot entro rumore |

Lettura per A2 (decisione rinviata ai numeri — eccoli):

- **Costo ~lineare nei chunk**: FS a 4000 B/chunk + DISK a 7 settori/run
  dominano; frame più grandi = meno round-trip (write ~4.2K cyc/B costanti,
  read con quota fissa metadata ~2M cyc ≈ 1.3 ms per op).
- **Read superlineare** (501 → 783 → 1269 cyc/B da 4K a 64K): walk di
  catena + run per cluster, un IPC DISK per settore — il fan-out
  (`R_OBJ_MGET`: un IPC, N blob) attacca esattamente questo.
- **Cold ≈ hot sul bulk dati** (write identiche per write-through; read
  grandi entro rumore): la cache salva i metadati, non i dati — A2 non può
  contarci per il bulk.
- ramfs al floor (~20 cyc/B): lo zero-copy c'è già; il multi-frame serve
  al FAT, non alla RAM.

Gate invariato (5/5 + 7/7 + 57/57 + shell verde); bench mai nel gate.

<<<<<<< HEAD
## A/B ArcaFS vs FAT + nativo vs POSIX (estensione Fase 23/53)

Contesto: da ADR-0043 la root `/` e' ArcaFS; `/tmp` e' la ramfs vera e
`/fat` il FAT32. Le righe `bulk_ramfs_*` della campagna 53 battevano quindi
in realta' la root ArcaFS (label sbagliata, non numeri sbagliati). Il fix
sposta la ramfs su `/tmp` e aggiunge il confronto che mancava: **stesso
client POSIX/VFS** su ArcaFS (`/`) e FAT (`/fat`), piu' l'API nativa
`R_OBJ_*` contro il percorso POSIX **sullo stesso volume ArcaFS**.

Metodologia (uniforme a 53: open/op/close per iter, offset 0):

- **Hot**: fixture pre-cresciuto, `warm` iterazioni di riscaldamento prima
  della misura (per le size grandi il "caldo" e' limitato: block ha 256
  entry × 512 B ≈ 128 KiB, la `NodeCache` di cardo 256–16384 nodi da 64 KiB).
- **Cold**: `iters + warm` file/oggetti **distinti** + UN solo flush prima
  della misura: `spoil_cache()` (300 settori, eviction della cache
  settoriale) e, solo per ArcaFS, `arca_cache_flush()` (nuovo sub-op debug
  `ARCA_SUB_CACHE_FLUSH = 11` su `R_ARCA_DEBUG`, wrapper
  `civis::arca_cache_flush()`). I nodi indice tornano caldi dopo la prima
  iterazione: caveat dichiarato, non "cold per op".
- **FAT**: niente `mkdir`/`remove`/`rename`, nomi 8.3; i fixture restano
  fino alla rigenerazione dell'immagine a ogni `run.sh` (filesystem
  limitato, non simmetria artificiale). Anche il bench non fa cleanup su
  nessuno dei due FS: il volume e' rigenerato a ogni run e su ArcaFS un
  `remove` e' un'altra mutazione B+tree (commit + orfani), che falserebbe
  la campagna.
- **Spazio/GC ArcaFS**: la orphan-GC gira solo al bind (56.2c); in una
  sessione i nodi COW sostituiti restano allocati fino al remount. Ogni
  commit di un piccolo overwrite lascia ~192 KiB di orfani (foglia primaria
  + secondaria riscritte, piu' il remove del trim). Con i 128 MiB di default
  la campagna esauriva i blocchi dopo ~650 commit (`ERR` a meta'
  `small_ow`); il profilo `reduced` usa `ARCA_SIZE_MIB=512`, il `full` 4096
  (stima ~1 GiB di orfani dopo il pre-seed dei fixture grandi). L'immagine e'
  rigenerata a ogni `run.sh`. Non e' un target di performance: e' headroom
  per misurare la matrice, non un test di NOSPC.
- **`small_ow` Arca**: 200 iterazioni misurate (FAT resta 1000). Il costo/op
  Arca e' dominato dal commit a close (W4); 1000 commit sono un test di
  durabilita'/spazio, non un campione di throughput piccolo-file. Il
  rapporto con FAT resta su `cyc/op` (gli `iters` possono differire).
- **Readdir**: confronto tra radici (`/` ArcaFS, `/fat` FAT) — misura il
  costo del listing, non l'uguaglianza dei contenuti (caveat).
- **Nativo vs POSIX**: oggetti nel bucket dedicato `bch`. `obj_put` senza fd
  committa **a ogni chunk** (4000 B): 1 MiB = ~262 commit, durevolezza
  esplicita per chiamata; il path POSIX con fd usa epoche (commit a close/
  soglia 256 KiB/eta'), quindi il confronto sul PUT grande va letto come
  "costo della durevolezza per chiamata", non come regressione.
  `obj_get`/`obj_stat` sono il floor del motore (niente VFS/namespace).

Matrice delle righe `[bench]` (tutte fuori dal gate):

| Famiglia | Righe | Size |
|----------|-------|------|
| Metadati A/B | `{fat,arca}_small_orc`, `_small_ow`, `_stat`, `_readdir` | 25 B / listing |
| Bulk hot A/B | `bulk_{fat,arca}_<S>_{write,read}_hot` | 4K/16K/64K/256K/1M |
| Bulk cold A/B | `bulk_{fat,arca}_<S>_{write,read}_cold` | 4K/16K/64K/1M |
| Scaling 4M | `bulk_{fat,arca}_4M_read_{hot,cold}` | 4M (solo read) |
| Nativo | `arca_*_native`, `bulk_arca_<S>_*_native` | small/4K/1M (+1M cold) |
| Anchor | `zero_1B`, `sda_512B_seq`, `fat_4K_oow`, `ramfs_4K_*`, `bulk_ramfs_*` | storici (ramfs ora su `/tmp`) |

Gruppi di `bench-split.sh`: `small` = metadati + anchor + ramfs;
`4k`/`16k`/`64k`/`256k` = solo le righe bulk di quella size (hot+cold);
`1m` = hot+cold 1M; `4m` = sole read hot/cold 4M; `native` = setup oggetti
`bch` e righe `R_OBJ_*`.

## Campagna split 2026-10 (1 run, KVM)

Log per-gruppo `/tmp/bench-{small,4k,16k,64k,256k,1m,4m,native}.log`,
TSC medio ~1.93 GHz, `spread=0/0` (un solo run: i numeri assoluti sono una
fotografia, non una media; i rapporti A/B sono la parte utile). `A/B > 1` =
ArcaFS piu' lento di FAT; per le read un A/B < 1 significa ArcaFS piu' veloce.
Il gruppo `native` e' stato rilanciato una volta dopo il fix del wrapper: il
PUT nativo 1M varia sensibilmente tra run (vedi caveat sotto).

| Caso | A/B | arca cyc/op | fat cyc/op | Lettura |
|------|-----|-------------|------------|---------|
| `small_ow` (25 B, commit) | **35.76x** | 383.83M | 10.73M | ogni close ArcaFS committa un'epoca |
| `stat` | **3.49x** | 2.59M | 742.3K | metadati ArcaFS piu' costosi |
| `small_orc` | 0.48x | 712.5K | 1.50M | ArcaFS avvantaggiata su open+read |
| `readdir` | 0.17x | 132.0K | 789.0K | listing ArcaFS molto piu' economico |
| `bulk_4K_write_hot` | **36.40x** | 884.38M | 24.30M | commit per close + chunk |
| `bulk_4K_read_hot` | 0.65x | 1.98M | 3.04M | ArcaFS piu' veloce |
| `bulk_4K_read_cold` | 1.10x | 5.44M | 4.95M | quasi pari |
| `bulk_16K_write_hot` | **29.28x** | 2605.93M | 89.00M | |
| `bulk_16K_read_hot` | 0.22x | 2.60M | 11.99M | |
| `bulk_16K_read_cold` | 1.46x | 23.40M | 16.00M | cold ArcaFS piu' caro |
| `bulk_64K_write_hot` | **28.10x** | 10069.00M | 358.31M | |
| `bulk_64K_read_hot` | 0.99x | 36.89M | 37.22M | pari |
| `bulk_64K_read_cold` | 1.08x | 59.75M | 55.48M | pari entro rumore |
| `bulk_256K_write_hot` | **15.88x** | 45306.25M | 2853.22M | |
| `bulk_256K_read_hot` | 0.84x | 137.37M | 164.40M | |
| `bulk_1M_write_hot` | 5.66x | 188532.94M | 33288.33M | |
| `bulk_1M_write_cold` | 5.70x | 202193.66M | 35446.80M | |
| `bulk_1M_read_hot` | 0.82x | 549.68M | 671.61M | |
| `bulk_1M_read_cold` | 0.56x | 517.96M | 923.07M | |
| `bulk_4M_read_hot` | **0.05x** | 167.60M | 3266.72M | ArcaFS ~19x piu' veloce |
| `bulk_4M_read_cold` | 0.34x | 1286.04M | 3787.59M | |

Nativo `R_OBJ_*` (A) vs POSIX ArcaFS (B), stesso volume:

| Caso | A/B | native cyc/op | posix cyc/op |
|------|-----|---------------|--------------|
| `small_orc` | 0.39x | 281.1K | 712.5K |
| `stat` | 0.30x | 767.5K | 2.59M |
| `4K_read_hot` | 0.31x | 618.2K | 1.98M |
| `4K_write_hot` | 1.21x | 1070.26M | 884.38M |
| `1M_read_hot` | 0.72x | 397.15M | 549.68M |
| `1M_read_cold` | 0.69x | 356.60M | 517.96M |
| `1M_write_hot` | 0.88x | 166075.25M | 188532.94M |

Lettura: il costo ArcaFS resta dominato dalle **mutazioni/commit** (write
4K–1M ~5.7–36x FAT; `small_ow` ~36x), mentre le **read** sono competitive e
sulle size grandi spesso migliori (4M hot ~19x FAT). Le read native sono al
floor del motore e battono il percorso POSIX; il PUT nativo 1M e' dominato
dai **262 commit** (uno per chunk) e la misura varia molto tra run
(166G–231G cyc/op, rapporto 0.88x–1.23x): va letta come costo della
durabilita' per chiamata, non come throughput stabile. Il 4M FAT hot/cold e'
misurato su fixture host-seedati: senza pre-seed la crescita guest-side
(catena ripercorsa a ogni chunk) rendeva il gruppo impraticabile.


Campagna e lettura dei risultati:

```bash
# campagna di riferimento (KVM; log /tmp/bench-run{1,2,3}.log):
BENCH_PROFILE=full RUNS=3 ./scripts/bench.sh
# validazione rapida su host lenti (~5 min, un run):
BENCH_PROFILE=reduced ./scripts/bench.sh
# matrice full divisa per size (un boot per gruppo; log /tmp/bench-<g>.log):
./scripts/bench-split.sh
BENCH_PROFILE=split ./scripts/bench.sh
BENCH_GROUPS="1m 4m native" ./scripts/bench-split.sh
# solo confronto su log gia' raccolti (accetta anche il prefisso seriale):
./scripts/bench-compare.sh /tmp/bench-run1.log /tmp/bench-run2.log /tmp/bench-run3.log
```

Il profilo `split` e' il modo robusto di misurare la matrice su un host
lento: ogni gruppo ha un timeout proprio e un `DONE ok=1` indipendente, e i
log parziali restano analizzabili anche se un gruppo fallisce. I gruppi
`small`/`4k`/`16k`/`64k`/`256k` usano un'immagine ArcaFS da 512 MiB; `1m`,
`4m` e `native` usano 4096 MiB per gli orfani COW e il PUT nativo 1M. Il
profilo `full` monolitico resta disponibile per una singola fotografia, ma
su host WSL lenti la matrice completa puo' superare i 90 minuti.

Il comparator stampa due tabelle: **ArcaFS (A) vs FAT (B)** e **nativo (A)
vs POSIX (B)**. `A/B > 1` = A piu' lento (piu' cicli per op); `spread%` =
`(max−min)/media` sui run. La soglia di non-regressione resta quella del
repo (>10% peggioramento a parita' di host KVM); i numeri assoluti delle
campagne 23/25/38/53 restano snapshot di altri host e NON sono
confrontabili con questa matrice (stesso host, stessa campagna).

Il profilo `full` su un host WSL lento (ArcaFS ~31 s per una write 256K,
~2 min per 1M, piu' la creazione guest-side dei fixture 1M) richiede
~55-65 min, build inclusa; `reduced` chiude in ~5 min e
serve solo a validare l'harness, non a produrre numeri di riferimento.
`bench-split.sh` spezza la matrice per size e resta la via consigliata quando
la build/boot e' lenta o il 4M FAT rischia il timeout monolitico.

Le righe 4M sono solo lettura per contenere la durata; il PUT nativo 1M ha
`warm=0, iters=1` (un PUT misurato, ~262 commit da 4000 B) e va riportato
come finding sulla durabilita' per chiamata, non come riga di throughput
generale. La stessa asimmetria vale per il PUT nativo 4K (2 commit per
chiamata) contro il fd-write POSIX (commit a close/soglia/eta').
## S2.5f/F1 — 100 MB su ArcaFS (lettura sequenziale, QEMU TCG)

Misura del primo payload grande su ArcaFS: lo staticone rustc
(100 135 752 B) letto da `s25fuse` a slice da 4 KiB con range read F0
(`get_range` con cursore LRU) in un run 1 GiB (`RUN_S25=1 RUN_MEM=1G`).

| Voce | Valore |
|------|--------|
| Byte letti | 100 135 752 |
| Tempo load | ~47.5 s (~2.1 MB/s) |
| Slice | 4 KiB (cap ring FS: `RING_MAX_PAYLOAD` 4000) |
| Round trip stimati | ~56 K (25.6 K IPC a cardo + ~19 IPC DMA per blocco 64 KiB) |

Lettura: il tetto **non** e' ArcaFS (la cache nodi fa hit sui blocchi da
64 KiB: 1 miss ogni 16 slice) ma il numero di round trip seriali. Ogni
slice = 1 IPC a cardo; ogni blocco da 64 KiB = 128 settori letti in chunk
da 7 settori (`DISK_MAX_SECTORS`, finestra PRD) = ~19 IPC al driver. Con
~0.9 ms per round trip il conto torna. Candidato W7 (fuori dal gate):
finestra DMA multi-pagina + payload FS piu' grandi (64 KiB per op), che
taglierebbe i round trip di un ordine di grandezza.

Nota: nessun cambiamento al gate (seed condizionale `RUN_S25=1`, volume
root 384 MiB in quel run soltanto).

## S2.5w/P4 — write-path ArcaFS: Fase 0/1 (2026-10)

Misura del costo reale delle scritture e primi tagli a basso rischio, senza
cambiare formato, cache o contratto di commit. Numeri KVM (`-accel kvm
-cpu host`), un run per gruppo (`4k`, `native`, `1m`), valori assoluti non
mediati e non confrontabili con le tabelle 23/25/38/53 di altri host.

### Fase 0 — strumentazione

- Contatori in `BTreeStats` (`put_chunks/bytes`, `ext_spans/bytes`,
  `trim_drops`, `freed_versions`, `commits`), `VolumeIoStats` (nodi per tipo,
  alloc/free, header-ext) e `DiskIoStats` (richieste IPC e settori).
- Sub-op debug `ARCA_SUB_IO_STATS = 12` su `R_ARCA_DEBUG`, frame di 27
  `u64`; wrapper client `civis::arca_io_stats()`.
- `testland/bench` stampa per ogni workload una riga `[bench-io]` col delta
  dei contatori (es. `nodeW=... freeB=... xh=... diskWr=.../...s`), fuori
  dal gate (`RUN_BENCH` non attivo).

### Baseline: dove va il tempo

Totali sulle iterazioni misurate (hot 3 write, cold 2, nativo 1, 4K hot 25,
cold 15): `cyc/op` e' la media, le altre colonne sono i totali della run.

| Caso | cyc/op | nodeW | freeB | xh | diskWr | settori |
|------|--------|-------|-------|----|--------|---------|
| POSIX 1M hot | 153.65G | 3408 | 65.1 MiB | 4541 | 89.167 | 574.108 |
| POSIX 1M cold | 156.38G | 2272 | 43.2 MiB | 3026 | 59.406 | 382.482 |
| Nativo 1M | 166.87G | 1151 | 21.3 MiB | 1997 | 30.871 | 193.499 |
| POSIX 4K hot | 805.10M | 251 | 192 KiB | 207 | 5.083 | 32.769 |
| POSIX 4K cold | 640.65M | 92 | 0 | 92 | 1.870 | 11.898 |

Lettura: una write POSIX da 1 MiB muoveva ~98 MB di settori. Il costo non
era il payload (4 KiB per chunk) ma l'amplificazione del blocco da 64 KiB:
ogni nodo riscritto intero, ogni `free` riscriveva 64 KiB, ogni alloc/free
aggiornava l'header-ext, e ogni blocco veniva spezzato in ~19 IPC/DMA da
7 settori (`DISK_MAX_SECTORS`). Il 4K era gia' dominato dai nodi (251 write
per 25 op), non dai free.

### Fase 1 — tagli applicati

- **1a, free a un settore**: il push in freelist scrive solo il primo settore
  (next-pointer); il resto del blocco non e' raggiungibile finche' non viene
  riallocato, e `write_node` riscrive sempre il blocco intero. `freeB` passa
  da 65 MiB a centinaia di KiB nel 1M.
- **1b, header-ext differita su free/GC**: la testa della freelist si
  persiste al commit (o alla successiva allocazione), non a ogni free/orfano.
  Gli `alloc` restano sincroni: un blocco preso dalla freelist e' ancora
  nella catena persistita, e sovrascriverne il primo settore senza avanzare
  prima l'header puo' corrompere la freelist dopo un crash. Il contratto W4
  resta invariato (ultimo commit durevole; epoca aperta persa; orfani
  recuperati al bind).

La 1a/1b non toccano il bound di `append 512KiB`: il test e' tornato sotto
il limite (`6571 -> 5697` tick su KVM, limite `6000`) e il gate resta 62/62.

| Caso | cyc/op (prima -> dopo) | diskWr | settori | freeB | xh |
|------|------------------------|--------|---------|-------|----|
| POSIX 1M hot | 153.65G -> **130.75G** (-14.9%) | 89.167 -> **69.364** | 574.108 -> **440.836** | 65.1 MiB -> **520 KiB** | 4541 -> **3488** |
| POSIX 1M cold | 156.38G -> **138.25G** (-11.6%) | 59.406 -> **46.246** | 382.482 -> **293.894** | 43.2 MiB -> **346 KiB** | 3026 -> **2328** |
| Nativo 1M | 166.87G -> **129.62G** (-22.3%) | 30.871 -> **24.392** | 193.499 -> **149.851** | 21.3 MiB -> **170.5 KiB** | 1997 -> **1656** |
| POSIX 4K hot | 805.10M -> 890.97M | 5.083 -> 5.026 | 32.769 -> 32.385 | 192 KiB -> 1.5 KiB | 207 -> 204 |
| POSIX 4K cold | 640.65M -> 727.14M | 1.870 -> 1.870 | 11.898 -> 11.898 | 0 -> 0 | 92 -> 92 |

Il 1M e il nativo mostrano il guadagno atteso: **-21/-23% di IPC e settori**
e **-15/-22% di cicli**. Il 4K non mostra il guadagno (`freeB` scende ma i
cicli peggiorano); con una sola run e con FAT hot variata nello stesso
intervallo, il dato non basta per attribuire una regressione al free. Il 4K
resta dominato dalle riscritture dei nodi e va rimisurato con `RUNS=3`
prima di farne un finding.

### Cosa resta

Il collo di bottiglia residuo e' la **riscrittura dei nodi da 64 KiB** e il
loro trasporto a 7 settori per IPC (1M hot: 3408 nodi, ~69K richieste disco).
I prossimi candidati sono W7 (finestra DMA multi-pagina / payload piu'
grandi) e il write-back a epoche con flush unico per commit; non sono parte
di questa fase. Il `free` a un settore lascia payload vecchi nei blocchi
liberi: e' un tradeoff di performance accettato qui, non una promessa di
secure erase.
