# ADR-0043: Bucket oggetti `sys`/`vela`/`usr` + tree `/bin`, `/bin/posix`, `/usr/bin`

## Status

Accepted — la parte `usr` (chiavi `usr/bin/*.bin`, bucket reserved senza
reader) e' superata da [ADR-0064](0064-bucket-binari-piatti.md): `usr` e' il
bucket `binary` piatto (`runhello`, `arca`, `cuuser`). `sys`/`vela`/`tst`
restano come qui descritto.

## Context

Init carica i servizi per `object_id` da un unico bucket `sys` (Fase 55, N0):
driver che toccano hardware (gpu, kbd, time) e servizi puri condividono lo
stesso namespace oggetti. Per dare un giorno permessi di scrittura diversi ai
bin driver serve prima separarli nei dati. In più la vista POSIX teneva tutti
i bin sotto `/bin` piatto (servizi, personalità POSIX e programmi utente
mescolati) e la shell risolveva i bare word su `/fat/bin`.

## Decision

Tre bucket oggetti su ArcaFS, con regola mirror (la chiave rispecchia il path
VFS — un solo layout da ricordare):

- `vela` (dal nome della crate driver condivisa `libs/vela`): bin che toccano
  hardware — criterio oggettivo `io_ranges` non vuoti (gpu/VGA, kbd/PS2,
  time/CMOS). Chiavi `bin/<nome>.bin`.
- `sys`: servizi senza HW (shell, posix, vela-svc, porta, uptime, vestigia).
  I bin POSIX vivono nella cartella `bin/posix/` (chiavi
  `bin/posix/shell.bin`, `bin/posix/posix.bin`).
- `usr` (nuovo, reserved): programmi utente lanciabili (`usr/bin/runhello.bin`,
  `usr/bin/arca.bin`). In D1 nessun reader via obj — la shell li lancia via
  path; il bucket esiste come reservation del namespace per il futuro
  exec verificato.
- `tst` (D2): suite di test (`tst:test/*.bin` per testfs/testfat/testarca/
  posixtst/tests/bench). Criterio: bin usati solo dalla suite, caricati da
  init per object_id come i servizi (stesso hash-pin). Gli helper di test
  (testcli/testspin/…) NON hanno oggetto: si caricano via path `/test/…`
  (dogfood del VFS root — ogni spawn helper è un open/read sul volume).

Vista POSIX (`ns:`): `/bin` (servizi), `/bin/posix/` (personalità),
`/usr/bin` (programmi) — solo su ArcaFS (FAT resta piatta fino a E). Dir
emergenti automatiche, niente mkdir (ADR-0042 invariato). PATH di default
della shell: `/usr/bin:/bin` (programmi prima dei servizi; `shell`/`posix`
non risolvono bare — niente li lancia così, init usa obj).

Hook di enforcement futuro (NON implementato in D1): `rights.rs::op_bit`
oggi non mappa `R_OBJ_*` (sempre consentito); il punto di aggancio è lì +
bit `RIGHTS_*` o ceiling per-bucket in `policy.rs` sul `R_OBJ_PUT` verso
`vela`/`usr`. Embedded (block/cardo/vestigia) = futuri membri se mai
un-embedded.

## Consequences

### Positive

- Classi di scrittura separabili per bucket senza migrazioni dati future.
- Tree leggibile: servizi, personalità e programmi ai loro posti.
- Bare word della shell risolti sul volume root (niente FAT nel percorso).

### Negative

- Omonimia `vela`: il servizio `vela` (/dev/null, no HW) resta in `sys`
  mentre il bucket `vela` è la classe driver — solo confusione di lettura,
  nessun clash tecnico (bucket ≠ key; nei log sempre qualificato).
- Divergenza temporanea FAT/ArcaFS (`/fat/bin/shell.bin` vs
  `/bin/posix/shell.bin`) fino alla fase E.
- `usr:` senza reader in D1 (~120KB seedati a vuoto come reservation).

### Neutral

- `SYS_SEED` cambia forma (path, bucket, key); `verify_image` invariato
  (pinning per `bin`, bucket-indipendente).
