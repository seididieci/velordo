# ADR-0052: Epoche transazionali minime in cardo (W4)

## Status

Accepted

## Context

Dopo W1–W3 ogni operazione mutante che passa dal motore ArcaFS fa un
commit completo (header-ext DIRTY → shadow → flip superblock → clear
DIRTY). Con W1 (un solo `put_chunk` per scrittura) e W2 (layout extent),
il costo per-op e' sceso, ma il commit resta il collo di bottiglia del
carico di scrittura: 512 append da 4 KiB = 512 commit di superblock, e in
generale ogni `write` di un file paga un flip di radici.

Il gate prima di W4 e' verde (6/6) e i test di crash (test32/v53)
presuppongono che un `obj_put` senza fd e una scrittura quota siano
durevoli al bounce. Serve quindi introdurre batching dei commit (epoche)
**senza** cambiare la semantica delle API sincrone.

Vincoli di partenza:

- Write-through (W3, ADR-0051): i blocchi nodo sono gia' su disco quando
  scritti; il commit flippa solo le radici. I nodi scritti durante
  un'epoca aperta diventano orfani se l'epoca non viene committata.
- Invariante W1: un blocco scritto non viene mai mutato in place (COW).
- COW + root-last: a crash pre-flip la generazione vecchia e' intatta.

## Decision

### 1. Due contratti, mai ambigui

- **fd-write** (`open` → `write` → `close`): commit **d'epoca**. La
  scrittura accumula byte dirty; il commit avviene a close, `SYNC`
  esplicita, soglia byte o eta'.
- **API senza fd** (`disk_put`/`disk_delete`, `ns_delete`, `ns_rename`,
  `ns_put_sync` per i symlink, snapshot/rollback/clone): commit
  **sincrono** (`commit_or`). Questi path non hanno una close che li
  flushi, quindi restano durevoli subito — preserva test32 (`obj_put`
  senza fd) e v53 (persistenza quota al bounce).

Il confine e' il tipo di chiamata, non il bucket: `ns_put` (fd) differisce,
`ns_put_sync` no.

### 2. Epoca GLOBALE, non per-fd

Lo stato dell'epoca e' un contatore di byte dirty + il tick di apertura,
unici per cardo (single-thread, precedente: `wall.rs`). L'epoca e' del
**volume**, non del singolo fd: qualunque commit (close, `SYNC`, soglia,
eta') chiude l'epoca **intera** — tutte le scritture dirty in quel
momento, di qualunque fd o processo.

Conseguenza semantica esplicita: una write dirty viene salvata anche da un
commit di un'altra operazione che capita dopo di essa. "Epoca aperta
persa al crash" significa *le scritture dall'ultimo commit sono perse in
blocco*, non *questa write e' isolata dalle altre*.

### 3. Choke unico e invariante

- `commit_or` e' l'unico choke: dopo ogni commit riuscito fa `epoch_reset`
  (dirty = 0). Invariante: **commit ⟹ epoca pulita**.
- `epoch_commit_or(eng, v, nbytes)`: `epoch_note` poi commit se
  `epoch_should_commit` (dirty ≥ `EPOCH_BYTES` = 256 KiB oppure eta' ≥
  `EPOCH_TICKS` = 500 tick ≈ 5 s), altrimenti `Ok` senza commit.
- `epoch_flush`: commit se dirty, no-op se pulita (close e `SYNC`).

### 4. Semantica di crash

- Ultimo commit = durevole (superblock flip + shadow, COW/root-last).
- Epoca aperta persa: i blocchi dei nodi scritti ma non committati restano
  orfani; la GC al bind (56.2c) li recupera in freelist.
- `SYNC` esplicita (`disk_sync(SYNC_GROUP)`) = flush immediato, per chi
  vuole durevolezza senza aspettare close/soglia/eta'.

### 5. Osservabilita' del fallimento

Se la GC al bind fallisce, il messaggio distingue `reachable` ok (push
freelist fallita = I/O in scrittura) da `reachable` fallita (struttura
illeggibile), per non confondere un guasto I/O transiente con corruzione.

## Consequences

### Positive

- Il carico di scrittura sostiene piu' append per commit invece di uno per
  op; il costo di commit si ammortizza su fino a 256 KiB / 5 s.
- Zero cambi alle API sincrone: i path fd-less restano durevoli subito,
  i test di crash esistenti (test32, v53) restano validi.
- Il crash recovery riusa la GC al bind gia' esistente: nessun journal.

### Negative / limiti

- L'epoca globale non isola le write tra loro: non esiste (ancora) il
  concetto di "transazione per-file". Chi volesse A persa ma B salvata
  deve ordinare le write rispetto ai commit.
- Niente ZIL/log-tree: la durabilita' dipende dal flip del superblock; il
  batching e' solo ammortamento, non una nuova garanzia.

## Testing

- **test60 "epoca kill-mid recovery"** (`testland/testsarca`): B (write +
  close) e C (write + `SYNC`) sopravvivono al bounce di cardo; A (write
  senza close, ultima scrittura dell'epoca) sparisce; dopo il recovery
  l'allocatore e' sano (si scrive e rilegge ancora). Il test rimonta
  `/arca` esplicitamente dopo il bounce (pattern v50) — il root mount
  catch-all falserebbe la lettura con la chiave `arca/...`.
- Gate: `[testsarca] PASS 60/60` + `[testfs] 5/5` + `[testfat] 7/7` +
  `[posixtests] 4/4` + `[usertests] 58/58` + `[threadtest] 12/12`, zero
  FAIL/PANIC/FAULT.
