# ADR-0047: PAL std Velordo (S1.3) — hello std nativo

## Status

Accepted

## Context

S0 ha stabilito: cross-build su host + ultimo miglio nativo; thread OS
prerequisiti (S-T chiusa); linker = rust-lld. S1.3 porta la vittoria: un
programma **std** (println, Vec, Mutex, HashMap, Instant) compilato ed
eseguito nativamente. Precedente: Xous (std su microkernel a messaggi).

## Decision

`target_os = "velordo"` con PAL in `pal/velordo/` (sorgenti) applicata via
`pal/velordo/apply.py` (arm `target_os` con assert fail-loud, mai patch
mute) a una copia di `library/` da rust-src, build con cargo +
`RUSTC_BOOTSTRAP` (`scripts/build-pal.sh`; tutto fuori dal repo in
`$PAL_WORK`, solo `pal/build/hello-std.bin` rientra per il seed).

- **Thread**: `singlethread = true` nel target JSON + liste `no_threads`
  (TLS/statics, Mutex/Condvar Cell-based). Onesto solo per programmi
  single-thread (S1.3): LLVM abbassa gli atomics a non-atomici — con
  thread futuri va tolto insieme ai backend futex (S2). `std::thread::spawn`
  fallisce graceful (unsupported), `sleep` su ticks.
- **Allocatore**: bump-pointer sopra `sbrk` (mai free, allineato, atomico
  sul bump). `dealloc` no-op intenzionale.
- **Console**: stdout/stderr su `SYS_WRITE` (fd 1/2 → seriale), stdin EOF,
  `panic_output` default. `write_all` ricicla sui short-write.
- **Orologio**: `Instant` su ticks PIT (10 ms); `SystemTime` panics
  (wall-clock vera = servizio Time via IPC: S2).
- **Casualita'**: RDRAND hardware (mai zeri spacciati: se fallisce, abort).
- **TLS/statics**: `no_threads` (zero codice PAL).
- **Entry**: `_start` nella PAL (RIP=USER_CODE, argc da [rsp]) che chiama
  il wrapper `main` del compilatore (== lang_start). `panic=abort`
  (niente unwinding), `exit` via syscall.
- **Build**: sysroot crate `-p std` (il crate `test` vuole `restricted_std`:
  niente harness on-target in v1), `--no-default-features` +
  `compiler-builtins-mem` (il `-c` vuole compiler-rt assente),
  `-C panic=abort` solo per il target,
  `-Zforce-unstable-if-unmarked` OBBLIGATORIO (senza, i mark
  `rustc_const_stable_indirect` di hashbrown non vengono registrati e std
  fallisce con "cannot be exposed to stable" — stesso errore di Xous
  #133857), sysroot con `.rmeta` accanto agli `.rlib` (cargo compila con
  `-Z embed-metadata=no`: senza rmeta "only metadata stub found"), hello
  con `#![feature(restricted_std)]` (la std e' ristretta: niente fd/net).
- **Gate**: `scripts/test-shell-std.py` fuori dal gate default (6 check;
  SKIP exit-0 se il binario manca — il gate 10/10 non dipende dalla PAL);
  seed/inject condizionali al binario.

## Consequences

- Rinviati a S2 (rustc nativo): `std::thread` sopra S-T + TLS nativa,
  `std::fs` sopra le syscall file, `SystemTime` vera, net, crate `test`,
  `singlethread = false` + backend futex.
- La PAL segue il toolchain pinnato (1.100): a ogni bump si riapplica e si
  ricompila (apply.py fail-loud sugli anchor).
- `targets/x86_64-unknown-velordo.json` ora ha `singlethread` (S1.0 l'aveva
  senza).

## Aggiornamenti

- **S2.5f/F2 (rustc nativo in guest)**: due stub che abortivano il
  compilatore all'avvio sono stati implementati.
  `SystemTime::now()` legge il servizio `Time` (`TIME_NOW`: epoch UTC +
  centesimi) con IPC grezza — la PAL non dipende da `civis`; servizio
  assente/morto → `UNIX_EPOCH` (orologio non inventato, mai panic).
  `getpid()` risponde a `SYS_GETPID` (8) invece di panicare. Restano stub
  dichiarati: `accessed`/`created` (mtime non tracciato), `current_exe`,
  `home_dir`, `ftruncate`, `remove_dir_all`.
