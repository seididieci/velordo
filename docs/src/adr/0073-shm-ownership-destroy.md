# ADR-0073: Ownership e `destroy` della memoria condivisa

## Status

**Accepted** (2026-11). Fase 30.1 implementata: `shm_create` registra l'owner,
`shm_destroy` (63) e' owner-only, il teardown rilascia i riferimenti owner.
Test `t65` (suite 61/61). Nessun cambio di comportamento per i client che
mappano una regione creata da un altro processo.

## Context

La Fase 30 (memoria condivisa) ha esposto `shm_create` / `shm_map` senza una
chiusura esplicita: `shm_create` metteva `refs = 0` e solo il `shm_map`
dell'owner incrementava il contatore. Il contatore veniva decrementato da
`munmap` / teardown, quindi il ciclo di vita era interamente legato alla
**mappatura**, non alla creazione.

Tre buchi reali:

1. **`shm_create` senza `shm_map`** (o con `shm_map` fallita): frame contigui
   e uno dei 16 slot `SHM_TABLE` restano occupati fino al reboot. Il
   `free_contiguous` non viene mai chiamato.
2. **`shm_create` seguito da `shm_map` fallita** (OOM, VA piena, prot
   invalido): l'id e' gia' stato ritornato, il chiamante non ha nessuna VMA
   da smappare, quindi il ref non si puo' rilasciare. `net`/`dns`/`siit`
   hanno path di errore di questo tipo.
3. **Owner che muore senza `munmap`**: il teardown del processo rilascia le
   sue VMA (quindi il ref del `shm_map`), ma il ref di creazione non e' una
   VMA: il frame resta orfano se nessun altro lo mappa.

Il caso (3) e' il piu' grave: `SHM_MAX = 16` e ogni regione arriva a 256 KiB,
quindi un leak non e' "un po' di RAM in piu'", e' l'esaurimento della
facility per tutti i processi. In `net` il ring e' 128 KiB per endpoint: con
16 endpoint in churn, `shm_create` fallisce per tutti.

## Decision

**Owner + destroy differito.** `shm_create` registra l'owner e prende il
riferimento iniziale; `shm_destroy` e' owner-only e chiude la regione; i
frame restano vivi finche' l'ultimo mapper non fa `munmap`.

- **Tabella**: `SHM_TABLE` diventa `[ShmEntry; 16]` con
  `(phys, frames, refs, owner, destroyed)`; `refs = 1` alla creazione
  (riferimento dell'owner), `+1` per ogni `shm_map`.
- **`SYS_SHM_DESTROY = 63`**: `(id) -> 0 | -1`. Solo l'owner (pid registrato
  da `shm_create`) puo' chiudere. Marca `destroyed` e rilascia il ref
  dell'owner: se `refs` scende a 0 i frame vanno via subito, altrimenti
  restano finche' l'ultimo `munmap`/teardown non scende a 0. Idempotente
  (secondo destroy = -1), non-owner = -1.
- **`shm_map` su `destroyed`**: fallisce (`-1`). La regione non si "resuscita"
  dopo che l'owner l'ha chiusa; le mappature esistenti restano valide e
  leggibili/scrivibili (i frame non sono liberi finche' non scendono a 0).
- **Teardown**: `teardown_user_space` chiama `release_owner(pid)` dopo
  `vma_clear`: ogni regione con `owner == pid` e `!destroyed` viene marcata
  `destroyed` e il suo ref owner rilasciato. Chi crea e muore non lascia
  frame orfani. Le mappature di altri processi restano valide.
- **`civis::shm_destroy`**: wrapper `(id) -> Result<(), Error>`.
- **Consumatori**: `net`/`dns`/`siit` `free_endpoint` chiamano `shm_destroy`
  dopo il `munmap` del proprio ring. Il driver `netdev` non ha un exit
  esplicito: il teardown rilascia il ref owner. I client (tlsprobe, net3,
  netecho, dns, siit, …) non distruggono: il loro ref e' una mappatura, e
  `munmap`/teardown lo rilascia.

## Perche' non "free immediato"

`shm_destroy(id)` che libera subito i frame sarebbe un use-after-free: altri
processi possono avere la regione mappata (ring di `net` mappato da client
TLS, DNS, SIIT, netdev) e il kernel non puo' rimappare. Il modello "destroy
differito" e' l'unico che rende la chiusura deterministica senza rompere le
mappature vive.

## Conseguenze

- **Nessun leak**: creazione senza `shm_map` (o con `shm_map` fallita) e
  owner che muore senza `munmap` non lasciano frame. Il contatore `SHM_MAX`
  non si esaurisce.
- **ABI**: `shm_create` (42) e `shm_map` (43) invariate; nuova `63`. I
  chiamanti che non chiamano `shm_destroy` restano corretti (il teardown
  rilascia l'owner), ma il rilascio e' deterministico solo se il
  chiamante lo chiama.
- **Costo**: un campo in piu' in `SHM_TABLE`, un walk di 16 slot al
  teardown, un controllo `shm_destroyed` in `shm_map`. Nessun cambio nel
  percorso di fault.
- **Test**: `t65` (create+destroy senza map, destroy con map, non-owner
  rifiutato, riuso dello slot). `t46`/`t48` aggiornati a `shm_destroy` dopo
  `munmap`. La suite passa da 60/60 a 61/61.

## Riferimenti

- `docs/src/04-memory.md` (Memoria condivisa, Fase 30 + 30.1).
- `docs/src/06-syscalls.md` (riga 63, wrapper `shm_destroy`).
- `docs/src/11-testing.md` (t65).
- `kernel/src/arc/vmm_user/shm.rs` (ownership, refcount, destroy).
- `kernel/src/arc/vmm_user/teardown.rs` (`release_owner`).
- `kernel/src/syscall/mem.rs` (`sys_shm_destroy`, `shm_destroyed` in
  `sys_shm_map`).
