# ADR-0079: Classi di privilegio e spawn attestato

## Status

**Accepted** (2026-11). Fasi 3–4. Dipende da ADR-0077 (bucket protetti) e
ADR-0078 (identità firmata).

**Stato**: **fase 3 implementata** (2026-11): trust table nel kernel (32 voci),
`SYS_TRUST_IMAGE = 64` registrabile solo da cardo, `ImageClass` nel PCB
(`image_class`), classe calcolata dal kernel ricalcolando l'`ImageId` sui byte
caricati, gate su `service_register`/`dma_alloc` device/`map_mmio`/`io_ranges`,
`peer_info` riporta la classe in `rdx`, t67 nel gate. Fase 4 (`io_ranges` dal
manifest) da fare. Vedi §Implementazione.

## Context

Oggi il privilegio è distribuito in punti diversi, ciascuno con la sua chiave:

| Privilegio | Gate attuale |
|---|---|
| slot `Service` (tranne `Test`) | figlio di init (`service_register`) |
| grant DMA device (slot ≥ 1) | owner di `Service::Console` |
| finestra MMIO | owner di `Service::Gpu` |
| porte I/O (TSS bitmap) | `io_ranges` **compilati** in `NAMED_BINARIES` |
| tetto ops FS | policy per hash (cardo) |
| porte ben note di rete | stub per hash (`net`) |

Funziona, ma non risponde alla domanda "questo codice è autorizzato a essere un
driver?". La risposta oggi è "è figlio di init", che è un fatto di *lineage*,
non di *autenticità*: chiunque riesca a farsi spawanre da init (o a occupare
uno slot dopo un restart) ha il privilegio. E aggiungere un driver richiede di
ricompilare il kernel per le sue `io_ranges`.

## Decision

### 1. Tre classi, nel PCB

```rust
enum Class { System, ThirdParty, Untrusted }
```

- **System**: firmato con la chiave di sistema, oppure binario **embedded nel
  kernel** (`rector`, `cardo`, `block`, `vestigia` — spawn dal kernel, non dal
  loader).
- **ThirdParty**: firmato con una chiave autorizzata dall'admin.
- **Untrusted**: tutto il resto (codice compilato dall'utente).

La classe è determinata dal **digest** dell'immagine caricata, non da ciò che
il chiamante dichiara.

### 2. Trust table nel kernel, registrata da cardo

```rust
struct TrustEntry {
    id_lo: u64, id_hi: u64,
    class: Class,
    io_ranges: [Option<(u16, u16)>; 4],
}
```

- massimo 32 voci, `.bss` statico;
- registrabile **solo** dall'owner di `Service::Fs` (cardo), via
  `SYS_TRUST_IMAGE = 64` (primo numero libero);
- `spawn_image`/`exec`: dopo il digest, lookup → classe (default
  `Untrusted`); `exec` **ricalcola** (mai eredita la classe).

Il kernel non verifica firme: si fida della registrazione di cardo, che è il
punto dove i byte entrano e dove la firma è già stata verificata (ADR-0078).
Limite dichiarato: un cardo compromesso può mentire.

### 3. Gate strutturali

| Punto | Regola |
|---|---|
| `service_register` | slot privilegiati → `System` **e** figlio di init; `Service::Test` resta esente (escape di test, documentato) |
| `dma_alloc` slot device (≥ 1) | `System` |
| `map_mmio` | `System` |
| `io_ranges` | `System`; per gli altri il campo di `SpawnMeta` è **ignorato** |

Il tetto FS e la policy di rete restano in userspace (ADR-0080): il kernel non
diventa un motore di policy, solo un cancello con pochi bit.

### 4. `io_ranges` dal manifest

Le porte I/O di un driver passano dal manifest firmato alla trust entry
(`io_ranges[≤4]`), e spariscono da `NAMED_BINARIES`. Aggiungere un driver
diventa: build, firma, seed, riga di manifest — **zero rebuild del kernel**.

## Implementazione (fase 3)

### Trust table e `SYS_TRUST_IMAGE`

`kernel/src/ordo/sched.rs`: tabella statica di 32 entry `{ImageId, ImageClass,
used}`. `trust_lookup` (chiamata sotto `SCHED.lock` da `create_user` /
`exec_current`) è O(32) e non prende lock. `trust_register` (da
`sys_trust_image`) autorizza **solo** init o l'owner di `Service::Cardo`: il
writer della tabella deve essere uno, il servizio che verifica le firme. Un
figlio di init qualsiasi non basta — sarebbe un buco (chiunque spawato da init
promuoverebbe immagini arbitrarie).

`SYS_TRUST_IMAGE = 64` (argomenti `id_lo`, `id_hi`, `class`; 0 o -1). È l'unica
syscall nuova della fase.

### La classe segue i byte caricati (TOCTOU chiuso)

Il kernel **non** si fida della classe dichiarata: la ricalcola. `create_user`
e `exec_current` calcolano già `ImageId` sui byte che stanno per mappare; il
PCB riceve `image_class = trust_lookup(image_id)`. Se l'oggetto cambia tra la
verifica di cardo e il load, l'`ImageId` è diverso, la trust entry non
combacia e la classe cade su `Untrusted` — mai una promozione.

Nessuna crypto nel kernel: si confronta un `[u64; 2]` già calcolato (ADR-0078
§Alternatives), non si verifica una firma.

### Flusso per path (POSIX) e per bucket (nativo)

- **POSIX** (`libr::exec`/`verify_exec`, shell `load_prog`): cardo riceve
  `R_RIGHTS_EXECUTE` con `bucket = "ns"` e `key` = path senza `/` iniziale. Il
  **VFS** detiene la verità sui path: cardo mappa path→oggetto e decide
  `mandatory` anche per prefisso (`bin/`, `usr/bin/`), non solo per bucket. La
  verifica registra l'identità nella trust table; il kernel assegna la classe
  all'exec. Un diniego esplicito ferma l'esecuzione; un file fuori dal
  namespace (es. FAT) prosegue `Untrusted`.
- **Nativo** (`interpres`): bucket/key piatti, come in fase 2.
- **init**: prima di ogni `spawn_image` da disco chiama `R_RIGHTS_EXECUTE`
  (fail-closed su diniego), così la trust entry esiste prima dello spawn.
- **Embedded** (`rector`, `cardo`, `block`, `vestigia`) e **init**: `System` per
  costruzione (`set_image_class` dal kernel), non passano dal volume.

### Gate strutturali

| Punto | Regola |
|---|---|
| `service_register` | slot ≠ `Test`: figlio di init **e** classe `System` (lineage + autenticità) |
| `dma_alloc` slot ≥ 1 | classe `System` (lo slot 0 staging resta aperto) |
| `map_mmio` | owner del device **e** classe `System` |
| `io_ranges` in `spawn_image` | classe `System` (chi non lo è e le chiede → -1) |

### Osservabilità e test

`SYS_PEER_INFO` riporta la classe in `rdx` (`civis::peer_info_class`): un
client vede la classe **autorevole** del peer. **t67** verifica: (1) helper
firmato `system` spedito in `tst` → `System` (la firma vince sul bucket);
(2) helper non firmato → `Untrusted`; (3) TOCTOU: verificato il contenuto su
disco, si spawna una copia con un bit diverso → `Untrusted`; (4) gate: usertests
(figlio di init, `Untrusted`) non registra `Service::Net`.

### Limiti dichiarati

- **Fase 4 non inclusa**: le `io_ranges` dei driver restano in
  `NAMED_BINARIES` (compilate); arrivare dal manifest è il lavoro successivo.
- **TOCTOU residuo per init**: init è TCB e può sostituire un oggetto tra
  verifica e load; il risultato è comunque `Untrusted`, mai una promozione.
- **`libr::exec` su file fuori dal namespace** (FAT) non ha verifica di firma:
  esegue `Untrusted` (il gate `service_register`/device resta chiuso).

## Consequences

### Positive- Il privilegio poggia su autenticità, non su lineage: "figlio di init" resta
  necessario ma non sufficiente.
- Aggiungere un driver non tocca il kernel.
- La classe è un dato del PCB: leggibile da `ps`/log per diagnosi.
- I test di classe non richiedono rete: stanno nel gate.

### Negative

- Il kernel guadagna 32 voci di tabella e un po' di logica di lookup: piccolo,
  ma è TCB e va trattato come tale (fail-closed su tabella piena: le nuove
  registrazioni sono rifiutate, mai sovrascritte).
- Dipendenza da cardo per la correttezza della tabella (limite dichiarato).
- Un binario embedded è `System` per costruzione: se il kernel embedda un
  binario sbagliato, nessuna firma lo salva. Accettato: il set embedded è
  minimo (4 binari) e cambiarlo richiede una modifica al kernel.

### Neutral

- `SYS_TRUST_IMAGE` è l'unica syscall nuova; nessun tag IPC nuovo.
- La classe non cambia l'ABI di spawn/exec: resta `spawn_image(bytes)`.
- `peer_info` guadagna `rdx` (la classe): i client che ignorano il registro
  restano validi (`civis::peer_info` invariata, `peer_info_class` nuova).

## Alternatives Considered

- **Spawn per handle verificato** (`spawn_object(handle)`): più pulito
  concettualmente, ma cambia l'ABI di spawn/exec, la shell e tutti i
  chiamanti. Il digest dà la stessa garanzia (la classe viene dall'immagine,
  non dalla dichiarazione) con meno churn. Rinviato se servirà.
- **Verifica della firma nel kernel**: costo e superficie (ADR-0078).
- **Policy object per processo nel kernel**: il kernel diventerebbe un motore
  di policy; il modello microkernel (ADR-0005) lo vuole fuori. Solo i bit
  strutturali restano dentro.
- **`io_ranges` in `SpawnMeta` fidandosi del chiamante**: chiunque si
  dichiarerebbe driver. Scartato; il campo resta ma è ignorato per
  `class != System`.

## References

- ADR-0005 (microkernel), ADR-0026 (hardening), ADR-0077, ADR-0078, ADR-0080
- `kernel/src/syscall/service.rs` (`service_register`), `kernel/src/syscall/mem.rs`
  (`dma_alloc`, `map_mmio`), `kernel/src/user_binary.rs` (`NAMED_BINARIES`),
  `kernel/src/ordo/process.rs` (PCB)
- Fase 3: `kernel/src/ordo/sched.rs` (trust table), `kernel/src/syscall/misc.rs`
  (`sys_trust_image`), `kernel/src/ordo/sched/exec.rs` (`exec_current`),
  `libs/civis/src/spawn.rs` (`trust_image`, `peer_info_class`),
  `userland/cardo/src/{verify,rights}.rs`, `flavours/posix/libr/src/exec.rs`
  (`verify_exec`), `testland/usertests/src/t_sig.rs` (t67)
