# ADR-0051: Cache nodi write-through in cardo

## Status

Accepted

## Context

Dopo W2 ogni put rilegge decine di nodi caldi (path btree, mappe extent,
liveness: ~5:1 letture/scritture per op) e ogni lettura e' PIO via IPC.
Serve una cache (lezione Kafka n.1: una sola, mai double buffering col
motore) dimensionata sulla RAM reale e reclamabile, senza introdurre
semantica di crash nuova prima delle epoche W4.

## Decision

1. **Un solo livello, nel `VolumeStore`** (mai nel motore). Chiave = numero
   blocco; classi inferite dal tipo nodo (LEAF/INTERNAL/MAP = metadati R0,
   RAW/DATA = dati R2 — il tipo E' la semantica, nessun tagging dal motore).
2. **Write-through, mai dirty**: l'evict e' un drop (reclaim banale, niente
   flush). Il write-back ordinato e' lavoro delle epoche W4.
3. **Coerenza per costruzione**: i blocchi scritti non mutano mai
   (invariante W1; solo radici/xhdr/freelist mutano, via API settori che
   bypassa la cache); ordine disco-prima-cache-poi (a errore niente entry);
   `free` sfratta (i numeri riciclati non servono mai stale). Un solo store
   per volume (nessuna aliasing di cache).
4. **Retain a fasce + MFU**: vittima = classe max prima (DATA fuori, META
   resta), poi freq min (2 bit saturanti con aging a scansione), poi LRU a
   contatore (niente clock). Classe PIN riservata al futuro hook `pin()`.
5. **Sizing da `SYS_MEMINFO`** (1/8 della libera in blocchi, [256..16384]):
   cresce da sola fino al tetto via LRU. Niente shrinker kernel (segnato
   futuro: il kernel non notifica pressione).
6. **Niente readahead**: con I/O sincrono il prefetch non riduce ne' il
   totale letture (ogni byte si legge una volta comunque) ne' il numero di
   op — beneficio zero senza async/pipelining. Si rivede con DMA async o
   epoche W4.
7. **Stack**: i buffer 4K del path hash in heap (`Box::new_zeroed`, mai
   letterali) + `#[inline(never)]` firewall — la catena
   real_main/handle_get_hash/hash_of_provider sfondava i 16 KiB (misurato
   con `-Z emit-stack-sizes`: frame 4.8K+4.5K+4.8K). `handle_read` resta
   stack per disegno documentato (churn heap).

## Consequences

### Positive

- Hit rate 99.8% in gate (34324/34385); test59 1100→~850 tick; tutte le
  riletture calde (path, mappe, stat-per-write) gratis.
- Zero cambi di semantica/durabilita' (write-through = commit di prima).
- Osservabilita': hit/reads in XSTATS.

### Negative

- Heap cardo fino al cap (57 MB max teorici, demand-paged; 1/8 della libera
  e' comportamento standard da page cache).
- `RefCell` per la cache (nessuna rientranza per costruzione: la cache non
  richiama mai lo store).
- Evict O(n) a scansione (n ≤ cap; RAM contro PIO: sempre conveniente).

### Neutral

- Le scritture pagano ancora PIO+flush pieni (W4).
- Letture oltre EOF/sequenziali lunghe rileggono i metadati per slice
  (pochi nodi, cached dopo il primo).
