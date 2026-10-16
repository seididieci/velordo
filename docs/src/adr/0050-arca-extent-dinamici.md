# ADR-0050: Layout extent ArcaFS (scrittura O(toccati))

## Status

Accepted

## Context

Il valore primary era un blob unico per versione (inline ≤512 B o catena
OV): ogni `put_chunk` rileggeva e riscriveva l'intero file (O(n) per op,
O(n²) per append sequenziali — 2 MiB via append da 4K = ~1 GB di I/O e
stallo oltre 64K in guest, S2.5w). Serviva COW a granularita' blocco senza
journaling (il FS e' gia' COW transazionale: shadow superblock + flip,
come ZFS/btrfs senza ZIL — vedi discussione S2.5w).

## Decision

1. **Extent variabili, niente tabella**: un extent e' il run contiguo che
   l'allocatore consegna (`alloc_run` con affinita' ultimo-run:
   `hint+1, hint+2, ...` via `alloc_specific`, poi `alloc` normale),
   raggruppato per consecutivita' e spezzato a `EXT_RUN_MAX` (256 blocchi,
   ~900 KiB). La taglia emerge dall'allocatore (dinamica vera); file freschi
   su disco vuoto = 1 entry.
2. **Formato** (`ARCA_VERSION` 1→2, senza retrocompat: immagini rigenerate,
   mount v1 rifiutato loud):
   - `PrimRec` kind 2 EXTENT: `[size:8][n_ext:4][n_in:1]`
     `[entries: n_in × (blk:8, nb:4)]` `[map_head:8 se n_ext > n_in]`;
     nb in blocchi, offset logici impliciti cumulativi, EOF da `size`.
     Prime 12 entry inline (158 B nel record), resto in nodi MAP.
   - Nodo `MAP` (tipo 3): `[prev:8][n:2 LE][entries: n×12]` (295/nodo).
     **Catena all'indietro** (ogni nodo punta al piu' vecchio): le versioni
     riusano i nodi invariati per numero e riscrivono solo dalla prima
     entry cambiata in poi — i `prev` riusati restano sempre validi senza
     patch in place (una catena in avanti renderebbe qualunque riuso
     parziale insano: i `next` resterebbero puntati alla catena vecchia).
   - Blocco `DATA` (tipo 4): 3560 B puri, mai header interno.
   - Kind 3 PACKED riservato (parse = `None` loud): il packing W5 non
     richiede cambi di codec, solo l'implementazione.
   - Kind 1 OV legacy: sola lettura (path duale) + migrazione al primo
     write (materializza una volta, mai nuove catene). Mai condivisione
     di catene legacy: rollback/clone da legacy copiano (come prima);
     `walk_ov` resta severo sui revisitati.
3. **Condivisione + liveness, mai free eager**: la nuova versione riusa i
   blocchi/entry intoccati per numero; al trim/delete i blocchi si liberano
   solo se non referenziati da alcuna versione viva (storia + pinnate),
   in batch dedupato (droppare N versioni condividendo blocchi e liberarli
   per-versione darebbe double-free loud). Snapshot e rollback/clone
   copiano come prima (nessuna condivisione cross-oggetto).
4. **Enumerazione versioni esatta**: `seqs_of` = suffisso denso + pinnate
   dal refcount (i trim creano buchi: le pinnate sotto la frontiera
   diventano invisibili alla sola discesa densa — i loro blocchi verrebbero
   liberati sotto i piedi; prima della condivisione era solo leak
   silenzioso). Vale per trim/delete/stat/quota/liveness.
5. **Invariante W1 confermata**: blocchi scritti una volta sola
   (`wr == alloc` asserito nei test); solo radici/xhdr/freelist mutano.
6. **Osservabilita'**: `extent_stats` + sub-op `ARCA_SUB_XSTATS` (registri
   + frame): oggetti extent/entry totali/max/legacy/inline per tarare il
   seed sysroot.

## Consequences

### Positive

- Append/patch = O(blocchi toccati) + O(nodi mappa toccati); 512 append da
  4K (2 MiB): ~800K op stimati col layout vecchio → ~33K misurati host,
  linearita' asserita (seconda meta' ~1.2x); in guest 512 KiB in ~1100 tick
  con bound (test59) dove prima ci si inchiodava a 64 KiB.
- Versioni a costo quasi-zero (condivisione): retention/snapshot economici.
- Letture a accesso casuale senza cursore (indicizzate); il cursore F0
  resta per le catene legacy in via di estinzione.
- Quota `used` ora esatta anche con pinnate sotto frontiera (prima
  sotto-contava); stima pre-check aggiornata (nodi MAP oltre l'inline).

### Negative

- Formato v2 senza retrocompat (accettato: immagini rigenerate ogni run).
- Mappe oltre 12 entry costano nodi MAP (piccoli) + load O(nodi) per put
  (assorbito dalla cache W3; mai piu' O(dati)).
- Frammentazione mappa per patch frequenti (entry +1/+2 a patch): nessun
  defrag in W2 (segnato per dopo, con clustering chiavi).
- `seqs_of` ora passeggia anche il refcount (albero piccolo: solo pin).

### Neutral

- Rollback/clone restano copie integrali (semplicita' > risparmio; i file
  grandi si seedano da host in un put).
- Zero-extend oltre EOF alloca zeri fisici (parita' col passato, niente
  sparse in W2).
- PACKED riservato ma non implementato (W5); ZIL/log-tree rinviati (solo
  se fsync S2.5 lo chiede).
