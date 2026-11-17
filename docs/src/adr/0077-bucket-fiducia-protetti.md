# ADR-0077: Bucket di fiducia protetti in cardo

## Status

**Accepted** (2026-11). Fase 0 **implementata**: `policy::write_allowed` nel
choke point di cardo, classificazione del peer all'handshake
(`policy::classify`), test t57 esteso con l'attore ignoto `foreign` (esiti
`0b11111111`), gate 6/6 e net tests PASS. Prerequisito di ADR-0078 (firma dei
binari): senza questa protezione il materiale di fiducia sarebbe scrivibile da
chiunque.

**Fase 2 (ADR-0078)**: i bucket protetti si allargano a **`sig`** (i sidecar
delle firme: solo init li scrive) e **`vela`** (i driver: protegge le immagini
firmate dalla sostituzione). Oggi nessun client scrive `vela`: l'unico writer
runtime è il seed interno di cardo (`seed_sys_disk`, che non passa dal choke
point), e le immagini che ne uscirebbero sono comunque non firmate.

**Bug trovato implementando**: `fs_reply_check` (client FS) non mappava
`ERR_DENIED`, che quindi cadeva nel ramo `v => Ok(v)` e veniva letto come una
**lunghezza**. Il diniego era invisibile ai client (il put "riusciva"). Ora
`ERR_DENIED => Err(Error::Denied)`; `rights_execute` continua a riportare
`Ok(false)` per i chiamanti.

## Context

Il choke point di cardo applica il tetto di policy per identità
(`policy::ceiling_for`, Fase 45) e poi lascia passare l'operazione. Il default
per hash ignoto è:

```rust
pub const DEFAULT_UNKNOWN_OPS: u32 = 0x19F;   // ALL senza MOUNT/UMOUNT/GRANT/PIPE
```

`0x19F` **include `RIGHTS_WRITE`**, e `btree_drv::disk_put` non ha alcun
controllo di bucket: parsa `bucket\0key\0[data]` e scrive. Ne segue che un
processo qualsiasi (hash ignoto, nessun privilegio) può:

```c
obj_put(b"sys",  b"ca.der",   <DER arbitrario>)   // anchor di fiducia DoT
obj_put(b"usr",  b"netecho",  <ELF arbitrario>)   // programma in usr/bin
obj_put(b"tst",  b"test/tests.bin", <ELF>)        // suite di test
obj_put(b"ns",   b"bin/x",    ...)                // namespace stesso
```

e, via namespace (`R_OPEN` + `R_WRITE`, che passano da `ns_put`), riscrivere
`bin/vestigia.bin`, `usr/bin/runhello.bin`, `identis:config`.

Questo è un buco **già presente**: l'anchor DoT (`sys:ca.der`) è
manomettibile oggi. Ed è bloccante per il resto del progetto: firmare i binari
e mettere anchor/manifest/policy in `sys:` sarebbe teatro finché il bucket che
li contiene è scrivibile.

## Decision

Un guard centralizzato nel choke point di cardo, dopo il tetto di policy e
prima del dispatch. Tre regole, in ordine:

1. **Operazioni amministrative** (`R_SNAP_CREATE`, `R_SNAP_ROLLBACK`,
   `R_SNAP_CLONE`, `R_SNAP_DELETE`): richiedono `ceiling == RIGHTS_ALL`.
   Un rollback può riportare il volume a uno stato precedente: è un'operazione
   di amministrazione, non un'opzione di un programma.
2. **Target protetti**: scrittura/creazione/cancellazione/rename su
   - path con prefisso `bin/`, `usr/bin/`, `test/` (binari e suite);
   - bucket `sys`, `usr`, `tst`, `ns` (oggetti e namespace);
   - **`sig` e `vela`** (aggiunti in ADR-0078 fase 2: i sidecar delle firme e
     i driver. Un `vela` scrivibile da chiunque permetteva di sostituire un
     driver **firmato** — la firma senza write-protection del bucket che la
     contiene è teatro, esattamente come per `sys` in fase 0);
   richiede `ceiling == RIGHTS_ALL` **oppure** peer `pid == 1` (init).
3. **Carve-out per owner di servizio**: `identis:` (path) → solo Identis;
   bucket `log` → solo Vestigia. L'owner è determinato **una volta
   all'handshake** confrontando `peer_pid(chan)` con `service_pid(service)`, e
   la classificazione è in cache come il tetto (purgata su `EXIT_NOTIFY`).

La regola "`ceiling == RIGHTS_ALL`" non è un espediente: è la classificazione
che già esiste. `RIGHTS_ALL` lo hanno init, i servizi TCB (`SERVICE_POLICY`) e
i binari della suite (`TEST_POLICY`). Questo tiene verdi i test che usano
snapshot (`testsarca`, t56) e la suite che scrive nei bucket di prova
(`test`, `d56`, ...), che **non** sono protetti.

Il guard vale per: `R_OBJ_PUT`, `R_OBJ_DELETE`, `R_OPEN` con
`CREATE|TRUNC|APPEND`, `R_WRITE` (sul path dell'fd, via `ftable.get`),
`R_DELETE`, `R_MKDIR`, `R_RENAME` (entrambi i path), `R_SYMLINK` (path del
link), `R_CHMOD`.

Il diniego consuma il frame e risponde `ERR_DENIED`, con la stessa disciplina
del check di tetto (mai desincronizzare il request ring).

## Consequences

### Positive

- Anchor, manifest, policy, binari di sistema e `identis:config` smettono di
  essere scrivibili da un programma qualunque.
- Chiude in un colpo S3.3(a) (`identis:config` protetto) e la parte
  "subtree" di ADR-0070.
- Il guard è **una funzione** (`policy::guard_write`), non una tabella
  sparsa: un solo posto da leggere e testare.
- Prerequisito di ADR-0078: il materiale di fiducia ha una casa protetta.

### Negative

- I servizi TCB (`RIGHTS_ALL`) possono ancora scrivere ovunque: accettato,
  sono TCB. Un cardo compromesso resta un problema di sistema.
- La suite di test ha `RIGHTS_ALL` e quindi può scrivere i bucket protetti:
  accettato, è una build di test. Il caso negativo si prova con l'attore
  `foreign`, che è fuori da ogni tabella.
- Un `R_WRITE` ora costa una lookup in `ftable` per il path: trascurabile
  (la lookup avviene comunque nell'handler).

### Neutral

- Nessuna nuova syscall, nessun nuovo tag, kernel intatto.
- Il formato del payload non cambia: il guard legge il bucket dal prefisso
  già parsato da `arcafs::wire`.

## Alternatives Considered

- **Tabella ACL esplicita per bucket/key**: stesso effetto, più stato da
  gestire e da tenere sincronizzato con i seed. Il tetto di policy esiste già
  e distingue TCB da non-TCB: riusarlo è meno codice e meno superficie.
- **Bucket `sys` montato read-only**: richiederebbe semantica di mount
  read-only per bucket (non esiste) e non coprirebbe `usr/bin` né `ns`.
- **Solo firme (ADR-0078), senza guard**: le firme dicono *chi* ha scritto un
  binario, non impediscono di **sovrascrivere** l'anchor o il manifest. La
  firma senza questa protezione è teatro.

## References

- ADR-0026 (threat model, Strato 1+2), ADR-0041 (meccanismo/personalità),
  ADR-0058 (debiti utenti, S3.3(a)), ADR-0070 (policy subtree `sys:*`),
  ADR-0078 (firma dei binari)
- `userland/cardo/src/policy.rs` (`ceiling_for`, `DEFAULT_UNKNOWN_OPS`)
- `userland/cardo/src/server.rs` (choke point, `FS_BUF_REG`)
- `userland/cardo/src/btree_drv.rs` (`disk_put`, `ns_put`)
