# ADR-0079: Classi di privilegio e spawn attestato

## Status

**Accepted** (2026-11). Fasi 3–4. Dipende da ADR-0077 (bucket protetti) e
ADR-0078 (identità firmata).

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

## Consequences

### Positive

- Il privilegio poggia su autenticità, non su lineage: "figlio di init" resta
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
