# ADR-0048: Scope S2 — toolchain Rust nativa

## Status

Accepted

## Context

S1 ha chiuso: target JSON, FS MUST, sizing 1 GB, PAL std single-thread +
hello nativo (ADR-0047). S2 = far girare `rustc` nativo. Prima di scrivere
codice, tre sonde in `/tmp` (repo intoccato): survey strace di `rustc
hello.rs` (7171 syscall), L0 (censimento `rustc` al commit pinnato
`67eda617e`), L1 (`cargo check -p rustc_driver_impl --target velordo` contro
la nostra PAL). Dati grezzi: notebook dell'esperimento, non nel repo.

## Survey: cosa vuole rustc (misurato)

| # | Bisogno OS | Evidenza | Stato Velordo |
|---|------------|----------|---------------|
| 1 | Thread | 31 `clone3` + 3058 `futex` (70% tempo) su 32 CPU | S-T pronto, manca PAL `std::thread` |
| 2 | Memoria virtuale | 369 `mmap` + 93 `munmap` + 382 `mprotect` + 48 map PROT_EXEC | solo `sbrk`-bump senza free → gap grosso |
| 3 | File | 276 `openat`, 211 `stat`, `mkdir/unlink/rename/lseek/pread` | rename/symlink/chmod fatti (S1.1), resto da fare |
| 4 | Figli | 3 `wait4` + 16 `execve` (10 = ricerca PATH): lancia il linker e lo aspetta | spawn+exit_notify ok, manca `Command`+wait |
| 5 | Pipe | 3 `pipe2` + `socketpair` + `dup2` + `poll` (cattura output linker) | riuso `civis::pipe()` (Fase 42), niente di nuovo |
| 6 | argv/env | con `env -i` muore (`linker 'cc' not found`: cerca cc via PATH) | argc=0 stub, niente env → S2.0 |
| 7 | Exe-path | 1070 `readlink` fallite (canonicalizza per trovare il sysroot) | readlink ok, niente `/proc/self/exe` → `--sysroot` esplicito |
| 8 | Random | 19 `getrandom` (seed hash) | RDRAND gia' fatto (meglio di POSIX) |
| 9 | Segnali | SIGPIPE→IGN, handler SIGSEGV con alt-stack (overflow report) | niente segnali: degrado accettabile (morte secca) |
| 10 | Stack | rustc vuole MB | stack user 16 KiB → parametrizzare per binario |

Pesi: rustc = shim 647 KB + `librustc_driver` 156 MB + `libLLVM` 199 MB
(dinamici); loader nostro solo-statico → staticone ~400 MB+. RAM: 85-107 MB
per hello, GB per crate vere. Linker: rustc → `cc` → `collect2` →
`rust-lld` shippato nel toolchain (12 MB).

## L0: nessun muro in rustc per un OS nuovo

1. `Os::Other` esiste apposta per i JSON (`spec/mod.rs`, `consistency.rs`:
   vietato solo ai builtin). `os = "velordo"` = uso previsto. Precedenti:
   Hermit, Redox, Xous, Managarm.
2. 18 match su OS in tutto il codegen, tutti benigni (rami UEFI/Windows/
   Darwin, `ELFOSABI_NONE` di default, gate f16/f128 conservativi).
3. Triple LLVM tollerante (`x86_64-unknown-hermit` in-tree) → la nostra
   `x86_64-unknown-velordo`, ELF di default.
4. `linker`/`linker-flavor` overridabili dal JSON (default
   `Gnu(Cc::Yes, Lld::No)`).
5. I muri veri sono dal nostro lato (PAL, staticone, pipe, stack, RAM).

## L1: la work-list generata dal compilatore

`cargo check rustc_driver` per velordo (overlay toolchain + `apply.py` sul
sorgente pinnato): `core`/`alloc`/`std`/`proc_macro` COMPILANO con la PAL
S1.3 + **una riga** in `library/std/build.rs` (allowlist `+ velordo`,
altrimenti tutto `std` diventa `restricted_std` e l'ecosistema e'
inutilizzabile — rito d'iniziazione: hermit/redox/xous/uefi sono gia' in
lista). Poi emergono gli arm foglia (`getrandom`: `target is not
supported`; `memmap2`: fallback errato) = da aggiungere per-crate
(upstream o `[patch]` vendored), mai fingendo unix (hermit non dichiara
`family = "unix"`: precedente diretto).

Superficie `std` toccata dal compilatore (file in `compiler/`): path 161,
sync 139, io 80, fs 56, env 54, process 41, thread 23 (spawn/scope/Builder),
os::unix 14 (flock, filesearch, fs_util), time 13, **net 0**. L2 (link
statico) sequenziato dopo la PAL.

## Decision

- **Vittoria S2**: rustc nativo che ricompila un programma sostanzioso
  (candidato: cardo o shell; fissare in S2.5).
- **Due target**: user-minimal (attuale, softfloat senza SSE) + native-full
  (SSE, ABI host, stack grandi) per rustc.
- **Pipe**: riuso `civis::pipe()` (Fase 42); l'object code viaggia su file,
  in pipe solo diagnostica.
- **Panic del rustc nativo**: `abort` (ICE = morte, niente libunwind).
- **Linker nativo**: scritto nostro, scope "statico PIE x86_64, no LTO, no
  debug", lld stock come oracolo differenziale. **Trigger di
  rivalutazione**: se il subset esplode (eh_frame? TLS? split-debuginfo?)
  si rivaluta il port di lld. Tradurre lld dal C++ scartato: il ritaglio
  del subset costa comunque, piu' dipendenze LLVM-ADT, fork congelato e
  Rust non idiomatico (vedi discussione).
- **Famiglia**: niente `family = "unix"` (stile hermit); arm `velordo`
  per-crate; superficie unix-shaped nella PAL sopra meccanismi Velordo.
- **Patch toolchain**: `build.rs` allowlist + PAL, nel patch-set versionato
  con `apply.py` (fail-loud agli anchor).
- **Ordine**: S2.0 argv/env → S2.1 `mmap/munmap/mprotect` + free → S2.2
  `std::fs` → S2.3 thread + sync + TLS → S2.4 process/`Command` + pipe
  (chiusa: `output()` con capture su se' stesso) → S2.5 staticone + linker
  + `rustc` in QEMU. Sub-fasi con commit.

## Principio: niente POSIX dentro (solo personalita')

POSIX non entra nel kernel, nel meccanismo (`civis`, ABI syscall, PAL
`std`), ne' nei servizi nativi. Appare solo come **personalita'** in
`flavours/posix`, per compatibilita'. La direzione dell'adattamento e' a
senso unico: personalita' e PAL si adattano a Velordo, **mai il contrario**
— nessuna syscall nasce per soddisfare `std` o POSIX (estensione di
ADR-0025/ADR-0041).

Bonifiche eseguite (dove possibile e sensato): `SYS_FORK` →
`SYS_SPAWN_COPY` (45 invariato), `O_*` → `OPEN_*`, `SEEK_*` →
`SEEK_START/CURRENT/END`, `fd` → maniglie `CONSOLE_OUT/ERR` nel path
`SYS_WRITE` (numeri 1/2 invariati: coincidenza storica, nessun fd-table),
`OsStr` proprio in `std::os::velordo`, costanti FS kernel-side morte
(3-7) eliminate. Il wrapper `fork()` resta nel flavour (posto giusto).

Gate verificabili: `rg "posix::" libs/civis/src kernel/src` → zero;
`rg "os::unix" pal/` → zero; ogni nuova syscall passa il "noun test"
(avrebbe senso su un OS non-POSIX?).

Residui documentati (non semantica, non precedenti): numeri 1/2, ELF come
contenitore neutro, verbi open/read/write/close e fd-protocol `ftable`
(nomi pre-POSIX/neutri; rename totale = churn senza guadagno), `OsStr`
byte-string.
