# ADR-0081: il gate end-to-end — la shell nativa nello stesso boot

## Status

**Accepted** (2026-11-19). Implementata. Dipende da ADR-0062 (login gate),
ADR-0067 (attese in futex), ADR-0079/0080 (classi e policy), ADR-0059 (FAT
fuori dal gate).

## Context

Il gate di regressione copriva la suite (testfs, testsarca, posixtests,
identistest, usertests, threadtest) ma **nessun percorso utente reale**:
login → shell nativa → `obj_get` → `R_RIGHTS_EXECUTE` → `spawn_image`.

Il bug chiuso in ADR-0080 fase 6 — la shell spawnata dal login senza
verifica dell'oggetto, quindi `Untrusted`, quindi senza `RIGHTS_EXECUTE` e
incapace di eseguire qualsiasi programma da `/usr/bin` — è passato
inosservato perché la fase `native` di `test-shell-all.sh` esisteva ma era
**fuori dal gate** e nessuno la eseguiva dopo i commit. La regressione si è
vista solo provando il sistema a mano.

Due ostacoli impedivano di metterla nel gate:

1. **Il boot del gate non era pilotabile**: `run.sh` usa `-serial stdio` e
   nessun monitor QEMU, quindi niente `sendkey` (la fase `native` è
   interattiva).
2. **`RUN_TESTS` è compile-time** (`skip_tests` in `userland/rector`): un
   secondo boot con lo stesso kernel rifarebbe tutta la suite prima del
   login, e l'harness va in timeout sul prompt.

## Decision

1. **Un solo boot.** `run-tests.sh` avvia `run.sh` con `SERIAL=file:<log>` e
   `MONITOR=unix:<sock>,server=on,wait=off` (due env di plumbing in
   `run.sh`; il default resta `stdio` e nessun monitor). L'output live è un
   `tail -F` sul file.
2. **Il prompt del login è il segnale di fine boot.** Compare solo dopo la
   suite (init spawa il login per ultimo), quindi il gate non si chiude più
   con un timeout esterno: si attende `[login] Nome utente:`, si controllano
   le righe della suite, e da lì si fa login (`root/root`, ADR-0062) via
   `sendkey` e girano i **31 check** della shell nativa.
3. **Stessi check del run autonomo.** `run_checks()` in
   `scripts/test-shell-native.py` è condiviso fra il boot autonomo
   (`test-shell-all.sh native`) e il modo `--attach-pid`, che usa
   `Shell.attached()`: nessuno spawn di QEMU e nessun seed (il volume è
   montato dal guest).
4. **Il gate è autoverificante**: 6 righe di suite + zero
   `FAIL|PANIC|#.* FAULT` + fase shell, riepilogo `[gate] PASS/FAIL` ed
   **exit code reale**. Valvole: `GATE_SHELL=0` (salta la fase shell),
   `GATE_TIMEOUT` (attesa del prompt, default 600 s), `SHELL_TIMEOUT`
   (default 900 s).
5. **Timing coerente con l'acceleratore.** `SHELL_KVM=auto|0|1` allinea gli
   sleep dell'harness a ciò che fa davvero `run.sh` (il gate passa `KVM`); il
   ramo TCG dell'harness usa `-cpu qemu64,+fsgsbase,+rdrand,+rdseed` come
   `run.sh`. Senza, `qemu64` liscio non espone `fsgsbase`/`rdrand`/`rdseed`
   (verificato con `query-cpu-model-expansion`) e lo userland (TLS +
   `getrandom`) non parte: bug latente mai visto perché l'harness usava sempre
   KVM quando `/dev/kvm` esisteva.

## Consequences

### Positive

- La regressione di ADR-0080 fase 6 ha un test che la coglie: verifica
  mancante → shell `Untrusted` → `users` fallisce (`EXECUTE negato`); verifica
  che nega → la shell non parte. Entrambi i casi fanno fallire il gate.
- Un boot, un log, un seed: nessun secondo boot, nessun token nuovo nel boot
  di init, nessuna ricompilazione. Costo misurato su KVM: suite ~104 s guest
  + fase shell ~145 s.
- Il gate non può più "passare in silenzio": esce `!= 0` appena manca una
  riga, un diniego o un panic.
- `run.sh` guadagna `SERIAL`/`MONITOR`: qualunque boot può essere pilotato
  (utile anche a mano, senza il gate).
- Il ramo TCG dell'harness ora è corretto: la fase nativa gira anche su una
  macchina senza `/dev/kvm`.

### Negative

- La fase shell gira **sul volume post-suite**, non su un seed fresco:
  `users` mostra anche gli utenti creati dai test (`alice`), i bucket possono
  avere residui. I check reggono (31/31 verificati), ma un test che degradasse
  un servizio senza fallire renderebbe ambigua una failure della shell (il log
  mostra comunque la prima causa).
- La fase shell eredita il kernel del gate (`sched_debug`): log più rumoroso
  e timing leggermente più lento del run autonomo.
- `run-tests.sh` dipende da `rg`, `tail`, dal monitor QEMU e da `sendkey`
  (già richiesti dai test shell interattivi).
- Dopo il gate il kernel su disco è quello con la suite: `test-shell-all.sh`
  da solo richiede ancora il rebuild di produzione (noto, documentato in
  `11-testing.md`).

### Neutral

- Nessun tag, syscall o ABI nuovo; nessuna modifica al kernel.
- Le altre 10 fasi POSIX restano fuori dal gate.

## Alternatives Considered

- **Secondo boot con un token `notests=1` in cmdline** (una seconda QEMU con
  seed fresco): hermetico, ma +15-20 s, un token nuovo nel boot di init e due
  boot da orchestrare. Il valore aggiunto non copre il costo; `--attach`
  resta riusabile se servisse davvero un volume pulito.
- **Ricompilare init produzione fra i due boot**: un rilenkaggio del kernel a
  ogni gate.
- **Gating runtime completo al posto della feature `skip_tests`**: più
  invasivo (tocca il boot di produzione e le build dedicate `bench`/FAT).
- **Sottoinsieme ridotto di check nel gate**: scartato — la fase completa
  costa ~145 s e copre anche interprete, history e la guardia CPU del prompt.

## References

- ADR-0062 (login come gate), ADR-0067 (attese in futex), ADR-0079/0080
  (classi e policy), ADR-0059 (FAT fuori dal gate).
- `run.sh`, `run-tests.sh`, `scripts/shell_harness.py`,
  `scripts/test-shell-native.py`, `scripts/test-shell-all.sh`.
