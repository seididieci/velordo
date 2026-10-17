# ROADMAP Velordo

Sorgente unica di **stato** (fasi completate) e **futuro** (Pianificate +
Parcheggiate). La storia dettagliata per fase (decisioni, bug trovati,
lezioni, validazioni) vive in `docs/src/14-cronologia-fasi.md`; i gate
citati li' sono snapshot storici — il gate corrente e' in
`docs/src/11-testing.md`.

Orizzonte (dichiarato, non roadmap): self-hosting — un Velordo capace di
ricompilare se stesso. Ordina le priorita' (storage veloce prima, servizi
fuori dal kernel poi, personalita' per software reale dopo) senza pianificare
nulla da solo.

## Stato (completate 1-55)

Gate: `[testfs] PASS 5/5` + `[testfat] PASS 7/7` + `[testsarca] PASS 61/61` +
`[posixtests] PASS 4/4` + `[usertests] PASS 58/58` + `[threadtest] PASS 12/12`, zero FAIL/PANIC/FAULT.

| Fase | Descrizione | Stato |
|------|-------------|-------|
| 1 | Bare metal Hello World (VGA) + boot PVH | ✅ Completata |
| 2 | Memory Map (PVH) + GDT/IDT | ✅ Completata |
| 3 | Interrupt hardware (PIC/PIT/kbd) | ✅ Completata |
| 4 | Frame allocator + heap kernel (dinamico; direct map 64G da Fase 27) | ✅ Completata |
| 5 | Processi + scheduler preemptive | ✅ Completata |
| 6 | User mode (ring 3) + syscall | ✅ Completata |
| 7 | IPC sincrona send/recv ⭐ | ✅ Completata |
| 8 | init + console server | ✅ Completata |
| 9 | File system server via IPC (ramfs/FAT32/devfs/shell, Fase 9.1-9.6) | ✅ Completata |
| 10 | IPC optimizations (ring msg_queue, bitmask pick_next, SPSC ring FS) | ✅ Completata |
| 11 | Scheduler RT a 32 priorita' + CBS (bandwidth reservation) | ✅ Completata |
| 12 | IPC per nome — registry + channel nel kernel (ADR-0008) | ✅ Completata |
| 13 | IPC asincrono: send/recv non bloccanti, request-id (ADR-0009) | ✅ Completata |
| 14 | Cleanup processi: exit/kill, notifica al parent, slot a generazioni (ADR-0010) | ✅ Completata |
| 15 | Keyboard + Terminal server in userspace (sgancio tastiera/VGA) | ✅ Completata |
| 16 | Disk/ATA driver server in userspace (sgancio ATA/FS) | ✅ Completata |
| 17 | Diritti per-canale lato server (capability su IPC) | ✅ Completata |
| 18 | Shell + utility utente | ✅ Completata |
| 19 | Introspezione (`ps`) + metadati (`stat`) | ✅ Completata |
| 20 | FAT32 scrivibile (persistenza, ADR-0016) | ✅ Completata |
| 21 | Servizi caricati da disco via `spawn_image` (ADR-0017) | ✅ Completata |
| 22 | Detach dalla cascata di morte (emendamento ADR-0010 §6) | ✅ Completata |
| 23/24/25 | Baseline + ottimizzazioni throughput (PIO multi-settore, cache settoriale write-through) | ✅ Completata |
| 26 | `async`/`await` in `libr` sopra IPC asincrona (ADR-0019, 4 passi) | ✅ Completata |
| 27 | Higher-half kernel + direct map (ADR-0020: 27.1/27.2/27.3) | ✅ Completata |
| 28 | `mmap` anonimo nel basso canonico (payoff higher-half) | ✅ Completata |
| 29 | Protezioni di memoria (`mprotect`/NX, fault→kill del processo) | ✅ Completata |
| 30 | Memoria condivisa tra processi (`shm_create`/`shm_map`) | ✅ Completata |
| 31 | Loader ELF per-segmento (W^X del binario, ADR-0021) | ✅ Completata |
| 32 | Shared text ELF (segmenti immutabili condivisi, ADR-0022) | ✅ Completata |
| 33 | Infrastruttura COW (frame refcount + COW fault, ADR-0023) | ✅ Completata |
| 34 | `fork` — COW dell'address space (ADR-0024) | ✅ Completata |
| 35 | Hardening (threat model + cancelli kernel, ADR-0025/0026) | ✅ Completata |
| 36 | Identità misurata (hash nel PCB + manifest + policy su identità, ADR-0027) | ✅ Completata |
| 37 | `exec` in-place + shell che lancia programmi (run/jobs/wait, ADR-0028) | ✅ Completata |
| 38 | ATA DMA + IRQ (ADR-0029) | ✅ Completata |
| 39 | Fondamenta posix: registry 8→16 + `Service::Posix`, `libr::posix`, harness t53 (ADR-0030) | ✅ Completata |
| 40 | fd virtuali + redirect file (`> >> < 2> 2>&1`, `R_LSEEK`, `O_TRUNC`/`O_APPEND`, `R_DUP_*` modello B, errori tipati, t54) | ✅ Completata |
| 41 | Parser shell (quoting, `$VAR`, `; && \|\|`, glob) | ✅ Completata |
| 42 | Pipe + waitpid | ✅ Completata |
| 43 | Env/PATH/script (shebang, history; 43a ADR-0033 + 43b ADR-0034) | ✅ Completata |
| 44 | Job control + segnali (44a suspend/resume + fg/bg/Ctrl-Z, ADR-0035; 44b Ctrl-C selettivo + catch, ADR-0036) | ✅ Completata |
| 45 | Indurimento + chiusura posix (policy su identità + sandbox build, ADR-0037) | ✅ Completata |
| 46 | Provider trait filesystem (`LocalFs` + `LocalFsDyn`, `MountedFs::Local`, ADR-0038) | ✅ Completata |
| 47 | Wiring trait per ramfs (U1: open/read/write/readdir/stat/mkdir/delete) | ✅ Completata |
| 48 | Wiring trait per FAT32 (U2: `LocalFsDyn`, fix stat readonly) | ✅ Completata |
| 49 | Terreno pre-ArcaFS (T0: `AnyHandle`, mount-id stabili, `Source`+`fstype`, `Local` vivo, create/truncate nel trait) | ✅ Completata |
| 50 | Orologio P1 (`Service::Time`, `usertime` CMOS+`TIME_NOW`, `mtime` veri ramfs+FAT via trait, t38 esteso) | ✅ Completata |
| 51 | Vocabolario disco P2 (`DISK/R_DISK_LIST+INFO`, IDENTIFY estesa, relay cardo, `libr::disk_*`, t32 esteso, dati S1/S2) | ✅ Completata |
| 52 | Durabilita' P3 (`R_SYNC` modi+barriera, `RIGHTS_SYNC`, `R_STATVFS` nel trait, `SYS_MEMINFO`, contratto, t32/t37 estesi) | ✅ Completata |
| 53 | Misura bulk P4 (audit CAP, sweep 4K/16K/64K ramfs+FAT hot/cold in userbench, tabella costo-vs-dimensione) | ✅ Completata |
| 54 | Integrita' + attrezzi P5 (crate `blake2s`, `R_GET_HASH`, superblock ArcaFS, `negotiate()`→arcafs stub, `arca create` host, `arca list/stat` guest, testsarca 8/8) | ✅ Completata |
| 55 | ArcaFS A1+N0 (`R_OBJ_PUT/GET` + store in-memory, parser GPT per spec, mount MBR/GPT in partizione, `sys` seedato, init dual-mode + doppio pinning FNV/BLAKE2s, fallback FAT provato, testsarca 13/13, t58) | ✅ Completata |

## Pianificate

Voci con scope e vittoria dichiarati (non date).

- [ ] **ArcaFS/object-store** (dopo T0/Fase 49): filesystem avanzato stile
      btrfs/zfs — snapshot, niente partizionamento, layout adattivo per tipo
      di device (hdd/ssd/emmc), raid avanzato, monitoring (SMART), COW su
      disco; hook futuro per distribuzione nativa (ceph/glusterfs-like).
      Sessione A0 completata: bozza in `arcafs.md` (modello, indice,
      formato, API, mapping, diritti, quota, tool, multi-device, swap,
      vector, logging §15) + piano OS-first P1–P5 (§13). P1/50 chiusa
      (orologio), P2/51 chiusa (vocabolario disco + dati S1/S2), P3/52 chiusa
      (durabilita': contratto + barriera + sensori), P4/53 chiusa (misura
      bulk: tabella costo-vs-dimensione in `13-performance.md`), P5/54 chiusa
       (integrita' + attrezzi: BLAKE2s, content_hash, `arca create`/`list`,
       `negotiate()`→arcafs stub, testsarca 8/8). A1+N0/55 chiusa
       (object store in-memory, mount MBR/GPT in partizione, `sys`
       seedato, init dual-mode + pinning FNV/BLAKE2s, testsarca 13/13).
       Prossimo: stesura di dettaglio + ADR per A2, poi 56.
       Vittoria: spec scritta + primo mount.
- [x] **56 (A2)** (completata: 56.1 versioni in RAM 21/21; 56.2a casa `arcafs/`
       + formato on-disk + allocatore + `R_ARCA_DEBUG` 27/27; 56.2b B+tree COW +
       commit shadow+flip su disco, backend unico 32/32; 56.2c recovery/orphan-GC
       + tabella snapshot persistente + sys-dal-volume all'avvio 33/33): COW +
       snapshot/clone + GC + rollback/retention persistenti. Packing S1/S2 e
       `R_OBJ_MGET` (se 53 lo chiede) restano rinviati (i marker dir non
       servono piu': 56.3, emergenti + set RAM).
       Vittoria conseguita: rollback vero (persistente, sopravvive al kill);
       retention log implementabile (sblocco 57).
- [x] **56.3 (namespace POSIX su ArcaFS)** (completata, ADR-0042):
  dir emergenti (esistono ⟺ chiavi col prefisso nel bucket `ns`) + set
  transient in RAM per le `mkdir` (`mkdir` mai su disco, `rmdir`
  mai-esistita = errore, vuote perse al restart); vista POSIX in
  `userland/cardo` via motore globale a uuid combaciante (`ArcaWith`,
  mai dati altrui); `scan_prefix` nel B+tree; `R_SYNC GROUP` = commit;
  suite 40/40 → 50/50.
  Vittoria conseguita: file/dir su volume ArcaFS + intatti dopo kill.
- [x] **57 (L1) logging** (chiusa, ADR-0039; L0 cancellato senza implementarlo:
      la ragione — ArcaFS non esisteva — e' estinta): `vestigia`
      (`Service::Vestigia` = 10, embedded boot-TCB, spawn parallelo a block prima
      di fs) RAM-first con `LOG_FLUSH` di init (handshake FS + backdate/re-key
      pre-Time + riversamento incrementale + dual-write), bucket per identita'
      (`peer_info`, mai dichiarato), `!idx` per la latest post-restart, seal
      via snapshot, client `libr::log` su anelli condivisi (cancello leggero,
      mai handshake FS), regola non-POSIX. testsarca 33→40 (registrato,
      append+latest own-bucket, 1024B, rifiuti, seal+delete, stats con
      flush-proof, bounce+rewarm).
      Vittoria conseguita: log persistenti nativi con retention via snapshot+GC.
- [x] **58 Personalità POSIX separata** (58.1–58.6, ADR-0041): meccanismo
      neutro in `libs/civis` (ex `libs/libr`), personalità in
      `flavours/posix/{libr,server,shell,cli,tests}`. 58.1 (docs/ADR), 58.2
      (rename `libr`→`civis`), 58.3 (estrazione crate + inversione hook
      `entry!`/`route_out`/redir-magic), 58.4 (spostamento package in
      `flavours/posix/{server,shell,cli}`, output in `flavours/posix/build`),
      58.5 (suite `userposixtests` in `flavours/posix/tests`, gate
      `[posixtests] 4/4`, usertests 54/54) e 58.6 (chiusura docs) chiusi.
       Vittoria: `rg "posix::" libs/civis/src` = zero; una seconda personalità
       riusa `civis` invariato.
- [x] **Root su volume (Fase 2: C/A/D1/D2/E)** (chiusa, ADR-0043): ArcaFS
      come `/` scelto per UUID (`root=UUID=`, terzo drive sempre attaccato),
      ramfs solo su `/tmp`, `/fat` montato per dati (niente fallback).
      C: seed build-time (volume 32MiB, 90 voci, fail-loud; derivate MBR/GPT
      adeguate). A: mount root catch-all (`target ""`, `/dev` escluso),
      motore legato al volume root, parità errori IsDir/Exists su Arca.
      D1: init carica i servizi per object_id (bucket `vela` = driver per
      `io_ranges`, `sys` = servizi, `usr` reserved; tree `/bin`, `/bin/posix`,
      `/usr/bin`; PATH `/usr/bin:/bin`), fallback FAT rimosso. D2: suite via
      oggetti `tst`, helper via path `/test` (dogfood VFS). E: shell suite
      verde (harness col drive root + overlay arca per `--jobs`), testsarca
      50/50 pieni (drive GPT per 12-13, 22-32 su `sdc` root in bucket isolati,
      41-50 su mount root esplicito; parser MBR fuori dal gate, vedi
      `11-testing.md`).
      Vittoria conseguita: gate 5/5+7/7+50/50+4/4+54/54 + shell 10/10, zero
      fallback FAT in init e nella shell.
- [x] **59 (A3 quota + subvolumi)** (chiusa, ADR-0044): subvolume = bucket
       con budget blocchi dati (`quota_blocks`, 0 = illimitato; `used`
       misurato esatto: catene overflow distinte di tutte le versioni +
       pinnati snapshot anche a nome cancellato; nodi indice/inline/meta
       fuori quota per disegno); enforcement pre-allocazione su put/patch/
       rollback/clone (`ERR_NOSPC` → `Error::NoSpace` → ENOSPC); tabella
       nella foglia meta (`quota/<bucket>`, persiste via meta+commit);
       sub-op `QUOTA_SET`/`QUOTA_GET` su `R_ARCA_DEBUG` (solo motore disco);
       testsarca 50→54 (over-budget tipizzato, indipendenza, persistenza
       dopo bounce, used con snapshot/delete) + unit host su `MemStore`.
       Vittoria conseguita: quota che scatta davvero + gate invariato.
- [ ] **59+ (A4–A8, V1, B1)** (dopo A3, come da `arcafs.md` §13): ABAC
       engine, device-awareness, RAID, tool completo, rete; servizio
       vettoriale e backend VM/block fuori dal FS, mai dentro.
       Vittoria: una fase alla volta, ciascuna col suo gate.
- [x] **S0 survey self-hosting** (chiusa, ADR-0045): strategia cross-build
       su host + ultimo miglio nativo; nodo thread deciso con dati (rustc
       spawna 32 thread anche per hello → S-T prerequisito duro, tcc gira
       senza); linker = rust-lld dal sysroot, tcc come `cc` di bootstrap;
       gap PAL in tabella (MUST: rename, chdir/getcwd, symlink, chmod,
       thread/TLS/futex; DEFER: net, backtrace, dynamic-lib); sizing
       (hello ~85 MB RSS, sysroot ~200–400 MB → volumi/RAM da crescere).
       Prossimo: S1 (MUST senza thread + PAL single-thread).
- [x] **S1.3 PAL std Velordo + hello nativo** (chiusa, ADR-0047):
       `target_os = "velordo"` (`singlethread`, liste `no_threads`),
       alloc bump su sbrk, console su SYS_WRITE, Instant su ticks, RDRAND,
       `_start`+`lang_start`, sysroot assemblato (rlib+rmeta), hello std
       (println/Vec/Mutex/HashMap/Instant) verde via test-shell-std.py
       (6/6, fuori gate default con SKIP). Rinviati a S2: thread/fs/net/
       SystemTime/test-harness, `singlethread = false`.
       Vittoria conseguita: std gira nativo. Prossimo: S2 (sysroot nativo
       + rustc).
- [x] **Bonifica POSIX negli interni + ADR-0048 scope S2** (chiusa):
       `SYS_FORK`→`SYS_SPAWN_COPY` (45), `O_*`→`OPEN_*`,
       `SEEK_*`→`SEEK_START/CURRENT/END`, maniglie `CONSOLE_OUT/ERR`
       (numeri 1/2 invariati), `OsStr` proprio, costanti FS morte (3-7)
       eliminate; wrapper `fork()` resta nel flavour. Principio ADR-0048:
       niente POSIX dentro (solo personalita'), gate `rg` verificabili.
       Survey+L0+L1: strada A percorribile (Os::Other by-design, std
       compila + 1 riga build.rs, arm foglia per-crate, linker nostro con
       trigger, due target, panic abort, pipe riusata). Vittoria S2:
       rustc nativo che ricompila un programma sostanzioso. Prossimo:
       S2.0 (argv/env).
- [x] **S2.0 argv+env PAL** (chiusa): `sys/args` + `sys/env` (modello
       hermit, `OsStringExt` nostro) + `sys/paths` (getcwd/chdir veri su
       S1.1, temp `/tmp`; exe/home unsupported) + envp derivato in
       `pal::init`; `_start` naked (fix: il prologo falsava RSP con
       argc>0). Kernel/shell invariati. test-shell-std.py 6→10
       (argv/env/set_var/cwd), fuori gate con SKIP. Prossimo: S2.1
       (mmap/munmap/mprotect + free).
- [x] **S2.1 allocatore segregated-fit** (chiusa): `libs/valloc` (no_std,
       Provider astratto) + `tools/alloc-fuzz` (differenziale host:
       81k op single+8-thread, zero leak/corruzioni); classi O(1),
       slab a classe pura con ritorno munmap, large diretti, spinlock,
       stats; PAL via COPY_MAP + `alloc_stats`. Kernel invariato
       (file-backed rinviato a S2.4). test-shell-std.py 10→13
       (churn/overalign/stats). Prossimo: S2.2 (`std::fs`).
- [x] **S2.2 `std::fs` sopra cardo** (chiusa): `libs/vfscli` condivisa
       (estratto `civis`, commit checkpoint + gate) + PAL `sys/fs`
       (sagoma hermit, provider raw, honest-Err); generatore numeri ABI
       in `apply.py` (zero duplicati) + VA in `syscall-numbers`;
       test-shell-std.py 13→17 (fs roundtrip/len/readdir su `/tmp`).
       set_readonly no-op (firma () di std).
- [x] **S2.3 thread + sync + TLS** (chiusa): PAL `sys/thread` (TCB in cima
       allo stack, entry naked + jmp, join su futex, Dekker al detach,
       sleep/yield su deadline, parallelism=1) + `sys/sync/futex`
       (Mutex/RwLock/Condvar/Once/Parker gratis) + TLS a chiavi modello
       xous (tabella via RSP, registro 32 slot, dtor dal trampolino) +
       target `singlethread=false`; `Command` slitta in S2.4 (vuole IPC).
       test-shell-std.py 17→24 (sum=1000, condvar, once, tls main/child).
- [x] **S2.4 process/`Command` + pipe** (chiusa): `sys::pipe` (riuso
       `vfscli`, `std::io::pipe` stabile; TEST_POLICY per i diritti
       Fase 45) + `sys::process` (fork+exec, wait EXIT_NOTIFY, `output()`
       con capture, stdin echo) + stdio fd-override (`VELORDO_FD*`) +
       lock pipe globale. Lezioni: run_out vs run lunghi (run_until su
       DONE); notify solo al leader (wait non-leader muto); no setenv nel
       figlio (append env nel blocco). `net` resta stub (deciso).
       test-shell-std.py 24→30. Prossimo: S2.5 (staticone + linker).
- [x] **S-T thread kernel 1:1 + TLS + futex** (chiusa, ADR-0046): thread =
       PCB con gruppo (cr3/canali/fd/heap/VMA condivisi col leader, stack/
       TSS/FS/coda propri); pool 32→128 (bitmask u128, TSS pool, VMA 64);
       FSGSBASE per TLS user (`+fsgsbase` in run.sh); futex WAIT/WAKE +
       deadline a tick (walk presenza con bit U, sweep in on_tick);
       lifecycle di gruppo (kill/suspend/resume/exec/fork) + IPC di gruppo
       (sender leader, wake preciso per waiting_req); suite `threadtest`
       11/11 (create/stack/TLS/joins/timeout/wake/IPC/fd/stress-32).
       Vittoria conseguita: programmi multithread nativi + gate invariato.
       Prossimo: S1.
- [ ] **S2.5 rustc nativo** (aperta, in corso): S2.5a spike+decisioni
       (ADR-0048: staticone Cranelift, target singolo, scala graduata) e
       S2.5b FPU eager (ADR-0049, threadtest 12/12) chiuse; S2.5c layout VA
       (stack +512M, FS +1T, MAX_PAGES 64K, W+X a coppie) + exec streaming
       (niente staging in heap; fix: PCB cr3 aggiornato, reclamo col vecchio
       corrompeva l'allocator) chiusa, gate 6/6; staticone cross-compilato
       (99 MB static-PIE, `--version`+hello ok). S2.5d spawn streaming
       (SPAWN_IMAGE_MAX 128 MiB, BIGSTACK 8 MiB) + run in guest chiusa:
       staticone gira (Usage + driver thread, exit puliti), sysroot da
       risolvere; fix `is_user_range` al leader, `panic_output` visibile,
       RDRAND-fallback; mistero `table[1]=3` risolto (lock stdio →
       `current_id()` pre-init: mai println prima di `init.init()`).
       S2.5e spawn argv (`SpawnMeta` 56 B, layout dual-base via direct map)
       + `--version` in guest (`rustc 1.100.0-nightly`, exit 0) chiusa,
        gate 6/6 (60/60 testsarca con range + prefix-overwrite + append
        test). S2.5f/F0 range read
       ArcaFS chiusa (cursore LRU, invalidazione su mutazione; append
       guest-side O(n^2) documentato: file grandi solo da seed host).
        Prossimo: S2.5w (W1–W5, voce sotto) poi S2.5f/F1 (staticone su
        ArcaFS) + F2 (mini-sysroot).
- [x] **S2.5w scrittura ArcaFS** (extent variabili + cache cardo + epoche
      minime; prima di S2.5f/F1: il write path O(n^2) non regge sysroot e
      output rustc in guest): W1 L0-lite + invariante immutabilita' CHIUSA
      (stat-vs-get in `ns_write` con merge esplicito nel prefix-overwrite,
      `ov_write` con link all'indietro; baseline op-count host 68K→50K op
      (-26%), bound regressione in test; quota-skip non serve: `quota_check`
      salta gia' il walk a bucket senza quota; test58 prefix-overwrite,
      gate 6/6); W2 extent dinamici CHIUSA (ADR-0050: PrimRec kind EXTENT
      12 inline + nodi MAP in catena all'indietro, kind PACKED riservato,
      ARCA_VERSION 2; write a granularita' blocco con condivisione +
      liveness al trim/delete in batch; migrazione legacy al primo write;
      alloc_run con affinita' ultimo-run; host 24/24 con opcount lineare
      2 MiB ~33K op + FakeVol fedele al volume; quota_blocks_for con MAP
      oltre inline; XSTATS via ARCA_DEBUG; test59 append 512K bound ~1100
      tick; retry throttled ring cardo→block contro backpressure burst;
      bug trovati: riuso nodi MAP in avanti insano, double-free su drop
      multipli, seqs_of cieca ai buchi pinnati, fresh-truncate perso nel
      ramo Ext, stima quota; gate 6/6); W3 cache in cardo CHIUSA (ADR-0051:
      write-through a un livello nel `VolumeStore`, classi dal tipo nodo,
      MFU 2-bit + LRU, cap da `SYS_MEMINFO`, evict-on-free, hit/reads in
      XSTATS: hit 99.8% in gate, test59 1100→~850 tick; niente readahead
      sincrono (beneficio zero); stack hash in heap + firewall dopo caccia
      con `-Z emit-stack-sizes`; gate 6/6); W4 epoche minime CHIUSA
      (ADR-0052: due contratti — fd-write a epoca con close/SYNC/soglia
      256K/eta' 500 tick, API senza fd sincrona; epoca GLOBALE per volume,
      non per-fd: un commit salva tutte le write dirty insieme; crash =
      ultimo commit durevole, epoca aperta persa in blocco, orfani
      recuperati dalla GC al bind; choke unico `commit_or` con invariante
      commit⟹pulita; test60 kill-mid recovery: B/C durevoli, A persa,
      allocatore sano; fix test: rimontare /arca esplicito dopo il bounce —
      il root mount catch-all falsa la chiave `arca/...`; diagnostica
      `reachable=` sul fallimento GC; gate 6/6); W5 packing sub-blocco
      stesso-bucket (chiusa, ADR-0053: tipo nodo PACKED, header
      `[used:u16][refs:u16][blen:u8][bucket]`, bump-alloc per bucket con
      `pack_cur` ricostruito al bind, free eager per refcount + sweep GC al
      bind; oltre il blocco → extent, solo verso l'alto; misura spazio
      sysroot via XSTATS esteso). Vittoria: gate 6/6 a ogni sottofase +
      append 512K lineare bound (test59 ~1100 tick, opcount host 2 MiB ~33K
      op) + N file piccoli in M<N blocchi dati.

## Parcheggiate

Idee senza trigger (non date, solo su pressione reale).

- [ ] audio AC97+CBS (primo client servizio PCI, chiude ADR-0007 davvero)
- [ ] server-run async block (quando l'overlap DMA lo richiede)
- [ ] Strato 3 credenziali
- [ ] ext2/ATAPI/write-back/read-ahead/`DISK_STATS`/generazioni PID/thread
      (solo su pressione reale)
- [ ] OOM-kill a load (negativa ADR-0028)
