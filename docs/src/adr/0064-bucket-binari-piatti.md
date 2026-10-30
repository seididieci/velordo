# ADR-0064: Bucket `binary` — namespace piatto di eseguibili

## Status

Accepted (supera la parte `usr` di [ADR-0043](0043-object-buckets-sys-drv-usr.md))

## Context

S3.2b aveva introdotto i tipi di bucket e `BIN_LIST`, ma il tipo veniva
**derivato** dal seed statico `SYS_SEED` (ogni bucket che compariva con una
chiave `.bin` diventava eseguibile). Il risultato non corrispondeva
all'intenzione:

- `sys`, `vela` e `tst` risultavano eseguibili, `usr` no — proprio il bucket
  dei programmi utente restava fuori;
- la shell cercava `<bucket>/<programma>`, quindi per lanciare `runhello`
  serviva un nome di percorso (`usr/bin/runhello.bin`) e un `PATH` per sapere
  dove cercare: una variabile d'ambiente per una decisione che il filesystem
  puo' gia' esprimere;
- il seed metteva i programmi sotto `usr/bin/...`: una gerarchia inutile per
  un bucket che contiene **solo** eseguibili.

In piu' il percorso di esecuzione era rotto in tre punti indipendenti:

1. `handle_rights_execute` scriveva **due** frame di risposta (uno suo, uno
   del dispatch generico): il secondo restava stale nel response ring e la
   successiva op FS leggeva spazzatura;
2. `R_RIGHTS_EXECUTE` era numerato `0x26`, lo stesso valore di `R_OBJ_PUT`:
   il primo match vinceva e l'handler EXECUTE era codice morto;
3. `RIGHTS_ALL` valeva `0xFFF` e **non conteneva** `RIGHTS_EXECUTE` (`0x1000`):
   nemmeno i servizi TCB potevano ottenere il permesso, quindi ogni spawn
   verificato sarebbe stato negato;
4. l'handler `BIN_LIST` di cardo consumava il frame ma **non rispondeva**:
   la shell restava bloccata in `BlockedOnReply` per sempre.

## Decision

Il tipo di bucket **`binary`** e' un namespace **piatto** di soli eseguibili:

- **niente sottocartelle**: la chiave e' il nome del programma (`runhello`),
  non un percorso;
- **solo eseguibili**: il bucket non mescola dati e binari;
- la shell non ha bisogno di `PATH`: chiede i bucket `binary` con `BIN_LIST`
  e cerca il nome digitato **cosi' com'e'**.

Decisioni collegate:

- **Tabella esplicita**, non derivata dal seed: oggi `bucket_types` contiene
  solo `usr → binary`; `sys`, `vela` e `tst` restano `object` (i loro path
  strutturati non sono piatti). Il tipo si dichiarera' alla creazione con un
  futuro `R_BUCKET_CREATE`, quando i bucket saranno dinamici.
- **Chiavi `usr` senza suffisso**: `runhello`, `arca`, `cuuser` (il tool di
  seed non aggiunge `.bin`). Il mirror `ns` resta invariato
  (`usr/bin/runhello.bin`): la vista POSIX non cambia, il path VFS e' un'altra
  cosa dal nome nel bucket.
- **`RIGHTS_EXECUTE` in `ALL`**: `RIGHTS_ALL = 0x1FFF`, e le mask di policy
  dei servizi/test seguono (`0x1FFF`). Il default per hash ignoto resta
  `0x19F` **senza** EXECUTE (fail-closed).
- **`R_RIGHTS_EXECUTE = 0x37`**: numero libero, nessuna collisione con i tag
  `arcafs` (`R_OBJ_PUT` 0x26).
- **Un solo frame per op**: l'handler EXECUTE valida e ritorna il codice, il
  dispatch generico scrive l'unica risposta.
- **Reply obbligatoria**: ogni ramo che chiude un'op FS risponde, anche
  `BIN_LIST` (il client usa `send` sincrona: senza reply resta bloccato).
- **Payload EXECUTE col formato condiviso** `arcafs::wire::obj_prefix`
  (`[bucket_len][bucket]\0[key_len][key]\0`): la lunghezza viaggia in `w0`,
  il server consuma esattamente il dichiarato.

## Consequences

### Positive

- `runhello` gira end-to-end dalla shell nativa: `BIN_LIST` → `obj_get` →
  `RIGHTS_EXECUTE` → `spawn_image` con `argv` → exit notificato al padre.
- Nessun `PATH`: aggiungere un programma significa metterlo in un bucket
  `binary`, senza toccare l'ambiente della shell.
- Un bucket piatto e' verificabile: "solo eseguibili, nessuna sottocartella"
  e' un invariante controllabile, non una convenzione.
- Il mirror `ns` resta la vista POSIX: i due namespace (nome piatto vs path)
  non si confondono.

### Negative

- `sys`/`vela`/`tst` non sono `binary`: i loro path (`bin/...`,
  `bin/posix/...`, `test/...`) restano `object`. Diventeranno `binary` solo
  con un appiattimento esplicito delle chiavi (e dei seed di init).
- Il nome nel bucket e il path VFS possono divergere (`runhello` vs
  `usr/bin/runhello.bin`): e' voluto, ma va ricordato quando si aggiunge un
  programma (due seed, un solo file).
- `usr` non contiene ancora un `create-user` "ufficiale" con lo stesso nome
  del binario (`cuuser`): il rename e' rimandato.

### Neutral

- `BIN_LIST` continua a viaggiare sul tag `0x41` e a restituire i nomi
  separati da `\n`: cambia solo il tipo filtrato (`binary`).
- Lo spawn resta `spawn_image` (immagine in RAM): nessuna nuova syscall.

## Alternatives Considered

- **`PATH` nell'ambiente della shell**: una variabile per ogni ricerca,
  duplicata tra shell e programmi, e comunque un'informazione che il
  filesystem gia' possiede. Rejected.
- **Derivare il tipo dal seed `SYS_SEED`** (stato precedente): marcava come
  eseguibili servizi e test e lasciava fuori `usr`; il tipo di un bucket e'
  una decisione, non un effetto collaterale di dove vive un file. Rejected.
- **Chiavi con suffisso `.bin`** dentro `usr`, con la shell che prova
  entrambe: nasconde un dettaglio implementativo nel nome del comando.
  Rejected.
- **Appiattire subito anche `sys`/`vela`/`tst`**: richiede di cambiare i
  seed di init e i path POSIX in un colpo solo; lavoro separato. Rimandato.
- **Reply scritta dall'handler** (stato precedente): due frame per op,
  desincronizzazione del ring. Rejected.

## References

- [ADR-0043](0043-object-buckets-sys-drv-usr.md) — bucket `sys`/`vela`/`usr`
  (la parte `usr` e' superata da questo ADR)
- [ADR-0037](0037-policy-identity-sandbox.md) — policy per hash e mask ops
- [ADR-0026](0026-threat-model-hardening.md) — threat model e hardening
- `userland/cardo/src/server.rs` — `bucket_types`, `BIN_LIST`, dispatch
- `userland/cardo/src/rights.rs` — `handle_rights_execute`
- `libs/civis/src/ipc.rs` — `bin_list`; `libs/civis/src/fs/rights.rs` —
  `rights_execute`
- `userland/shell/src/main.rs` — `try_execute_external`
- `scripts/arca-tool.sh` — seed del bucket `usr` piatto
