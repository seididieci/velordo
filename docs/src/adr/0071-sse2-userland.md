# ADR-0071: SSE2 nello userland (target separato) e crypto di sistema

## Status

Accepted (2026-11). Prerequisito di N6.2 (TLS/DoT, ADR-0072).

## Context

Lo userland Velordo è compilato con `x86_64-unknown-none`, il cui target
spec disabilita esplicitamente SSE/SSE2 e impone `+soft-float`:

```
"features": "-mmx,-sse,-sse2,-sse3,-ssse3,-sse4.1,-sse4.2,-avx,-avx2,+soft-float"
```

Con questa configurazione LLVM (nightly 1.101, LLVM 23.1-rust) **non sa
legalizzare i tipi vettoriali a 128 bit** e muore in codegen:

- `poly1305` e `polyval`: `rustc-LLVM ERROR: Do not know how to split the
  result of this operator!`
- `curve25519-dalek`: SIGILL in `SelectionDAGISel`;
- di conseguenza `chacha20poly1305`, `aes-gcm`, `x25519-dalek`,
  `embedded-tls` non compilano.

Non è un difetto delle crate: `sha2`, `chacha20`, `p256` (ECDH+ECDSA), `rsa`,
`x509-cert`, `der`, `rustls-webpki` compilano già oggi. Il problema è solo la
mancanza di SSE2 nel target.

Il kernel, però, **gestisce già lo stato SSE**:

- `kernel/src/cpu.rs` abilita `CR4.OSFXSR` + `CR4.OSXMMEXCPT` (S2.5b, FPU
  eager);
- `kernel/src/ordo/context.rs` mantiene un'area `fxsave` di 512 B per pid e
  fa `fxsave`/`fxrstor` a ogni context switch (l'area FXSAVE copre XMM0–15
  quando OSFXSR è attivo).

Quindi il vincolo è nel target spec, non nell'OS. La scelta "niente SSE" era
ragionevole quando non c'era FPU eager; oggi non lo è più.

## Decision

1. **Nuovo target per lo userland**: `targets/x86_64-unknown-velordo-user.json`,
   clone di `x86_64-unknown-none` con
   `features: "-mmx,+sse,+sse2,+fxsr,-avx,-avx2"` e senza `+soft-float`.
   Il kernel resta su `x86_64-unknown-none` (soft-float, niente SSE): la
   modifica non lo tocca.
2. **Build**: `scripts/build_common.sh::build_one` passa esplicitamente
   `--target <json> -Zjson-target-spec`. La variabile rustflags diventa
   `CARGO_TARGET_X86_64_UNKNOWN_VELORDO_USER_RUSTFLAGS`; i `.cargo/config.toml`
   dei crate user puntano allo stesso JSON (`json-target-spec = true`).
3. **I simboli `mem*` sono nostri, in `libs/civis/src/rt.rs`**: con SSE2 LLVM
   non espande più `memcpy`/`memmove`/`memset`/`memcmp` inline e emette
   chiamate alla libc, quindi i simboli vanno forniti. *Non* si usa
   `-Zbuild-std-features=compiler-builtins-mem`: la sua implementazione x86_64
   spezza ogni copia in `rep movsb` + `rep movsq` + `rep movsb` e sotto QEMU
   TCG ogni `rep` è un helper (misurato **+46%** su `testsarca` append
   512 KiB: 8738 tick contro 5971 del target senza SSE2). `civis::rt` replica
   invece la forma che LLVM usava inline senza SSE: `rep movsb` per
   `memcpy`/`memmove` e `rep stosq` (8 B per iterazione) per `memset`, con
   testa/coda a byte per l'allineamento. Misura: **5631–5663 tick** su run
   diverse, meglio del target senza SSE2.
   La lezione sui numeri: contano i **byte per iterazione** dell'helper TCG,
   non il numero di istruzioni. Un `memset` a `rep stosb` (1 B) costa 8× uno
   `stosq`; un loop `movdqu` esplicito, che sembrerebbe il più veloce, è
   risultato *più lento* del `rep movsb` (helper in C, banda di memoria).
   Tabella misurata su `testsarca` append 512 KiB (host TCG, limite 6000):

   | variante `mem*` | `dt` |
   |---|---|
   | target vecchio (LLVM inline, riferimento) | 5971 |
   | `compiler-builtins-mem` (`movsb`+`movsq`+`movsb`) | 8738 |
   | loop `movdqu` a 64 B per tutto | 6854 |
   | ibrido (`rep movsb` bulk + `movdqu` < 64 B) | 6659 |
   | `rep movsb` + `rep stosq` (scelta finale) | **5663** (gate finale 5631) |
4. **Vale per tutto lo userland**, non solo per `libs/tls`: servizi nativi,
   test suite, flavour POSIX. Non c'è ABI condivisa tra binari (sono tutti
   statici e ricompilati insieme), quindi il cambio di convenzione float
   resta interno.
5. **Il target della PAL std** (`targets/x86_64-unknown-velordo.json`) resta
   invariato: se servirà SSE anche lì sarà una decisione separata.

## Consequences

### Positive

- Si sblocca l'ecosistema RustCrypto completo (`chacha20poly1305`,
  `aes-gcm`, `x25519-dalek`) e `embedded-tls` (ADR-0072): N6.2 non richiede
  di scrivere primitive crypto da zero.
- Nessuna modifica **strutturale** al kernel: il supporto hardware/OS
  (OSFXSR + `fxsave`/`fxrstor` per pid) c'era già. Un bug però è emerso, ed è
  stato corretto (sotto).
- `testsarca` non peggiora: 5631–5663 tick contro 5971 del target vecchio.

### Bug trovati (allineamento dell'RSP iniziale)

Con SSE2 il codice user contiene `movaps`/`movdqa` su slot 16-allineati, che
assumono la parità dell'ABI SysV (**RSP ≡ 8 mod 16 all'ingresso di una
funzione**). Il soft-float non toccava XMM, quindi tre calcoli di RSP iniziale
sbagliati erano rimasti invisibili; SSE2 li ha fatti emergere tutti come `#GP`
(code 139, silenzioso per il gate: `#GP` non è `FAIL`/`PANIC`/`FAULT`).

1. **Thread** (`SYS_THREAD_CREATE`): ricevevano l'RSP grezzo del chiamante —
   il TOP di un `mmap`, 16-allineato — e il primo `movaps [rsp+0x30]` del
   thread andava in `#GP`. Fix in
   `kernel/src/ordo/sched/spawn.rs::spawn_thread`: `rsp = (stack & !0xF) - 8`,
   con validazione della qword che il primo push scrive. Emerso in
   `threadtest` t9–t12.
2. **Processi con `argv`** (`layout_argv`): l'array veniva appoggiato a
   `(sp & !15) - 8*(argc+1) - 8*(envc+1)` e l'RSP era `arr - 8`: con `argc`
   **dispari** l'RSP risultava ≡ 0 invece di ≡ 8. Il primo `movaps` della
   catena (in `usershell`, appena spawnato dalla shell nativa) andava in
   `#GP`. Fix: `arr` arrotondato a 16 (`& !15`) prima di `rsp = arr - 8`, così
   la parità è corretta per ogni `argc`/`envc`. Vale anche per `exec`.
3. **Main thread senza `argv`**: già corretto (`setup_user_stack` restituisce
   `USER_STACK_TOP - 24`, con `USER_STACK_TOP` 16-allineato).

La lezione: l'RSP iniziale è un contratto dell'ABI, non un dettaglio del
chiamante — ogni punto che lo calcola (thread, argv, `spawn_copy`, `exec`)
va verificato per parità.

### Negative

- Tutto lo userland va ricompilato con un target JSON e `-Zjson-target-spec`
  (nightly già richiesto dal repo).
- I simboli `mem*` vanno forniti da noi (`civis::rt`): una dipendenza in più
  dal dettaglio del codegen (LLVM emette libcall solo con SSE attivo),
  documentata come pitfall. Chi aggiunge un crate user **deve** linkare
  `civis` (tutti lo fanno già).
- Un eventuale bug in un percorso SSE diventa un fault user-mode silenzioso
  (guardia stack/heap) invece di un errore di compilazione: il gate QEMU è
  l'unica rete.

### Neutral

- Le crate che già compilavano continuano a compilare; il comportamento
  osservabile del sistema non cambia.

## Alternatives Considered

- **Scrivere Poly1305/AEAD a mano e restare senza SSE2**: possibile
  (sha2/chacha20/p256/rsa/x509 compilano), ma lascia ~1700 righe di codice
  di sicurezza nostro (AEAD + handshake + policy X.509) e non sblocca
  `embedded-tls`.
- **`-Zbuild-std-features=compiler-builtins-mem`** (prima scelta, scartata):
  fornisce i `mem*` senza scriverli, ma la sua implementazione x86_64
  (`rep movsb` + `rep movsq` + `rep movsb`, con split per allineamento) costa
  **+46%** su `testsarca` append 512 KiB sotto TCG (8738 vs 5971 tick).
  `civis::rt` fa la stessa cosa nella forma che LLVM usava inline senza SSE.
- **`-C target-feature=+sse2` via RUSTFLAGS**: rompe `build-std`
  (`compiler_builtins`: *"Do not know how to soften this operator's operand"*),
  perché le feature si applicano anche a core/alloc.
- **Abilitare SSE solo per `libs/tls`**: le rustflags sono additive e
  per-target, non per-crate; servirebbe un crate precompilato, cioè un
  vendor con patch. Peggio.
- **Restare su `x86_64-unknown-velordo.json`** (target PAL, esiste già): ha
  `dynamic-linking: true`, `code-model: kernel` e SSE disabilitato; non è
  pensato per i servizi e mescolare i due scopi confonderebbe il confine
  PAL/userland.

## References

- `kernel/src/cpu.rs` (CR4.OSFXSR/OSXMMEXCPT), `kernel/src/ordo/context.rs`
  (fxsave/fxrstor per pid, S2.5b).
- ADR-0069 (stack di rete), ADR-0072 (TLS/DoT).
- Verifica eseguita: `userland/dns` linka con il nuovo target (62 KiB ELF);
  `embedded-tls` + `sha2` + `aes-gcm` + `p256` + `webpki` + `webpki-roots`
  linkano (89 KiB).
- Gate completo verde su entrambe le modalità QEMU: TCG (`append512`
  `dt=5631`, `threadtest` 12/12) e KVM (`KVM=1`, knob opt-in in `run.sh`:
  `dt=4926`, stesso 6/6). Il knob non cambia il default (TCG): il gate deve
  girare su host senza `/dev/kvm`.
