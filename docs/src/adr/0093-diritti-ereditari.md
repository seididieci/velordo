# ADR-0093: Diritti ereditari all'avvio — il figlio non ha più diritti del padre (S3.4)

## Status

**Accepted** (2026-11-25). Implementata in due passi **S3.4a** (kernel: floor di
classe, anti-elevazione, `SpawnMeta.rights`, `peer_info` esteso, `t77`) e
**S3.4b** (cardo: ops ereditate nel tetto del canale; shell nativa: floor
`ThirdParty` ai figli; probe `usr/bin/floorprobe`). Gate invariato:
`[usertests] PASS 72/72` (t77), 6 suite + shell nativa 33/33.

Dipende da ADR-0026 (hardening), ADR-0077 (bucket protetti), ADR-0078 (firma e
`ImageId`), ADR-0079 (classi nel PCB, porte I/O dal manifest), ADR-0080
(Custos/policy). Chiude il debito di ADR-0055 §5 («i diritti si ereditano
attraverso `spawn`»), che fino a oggi non era vero.

## Contesto

Prima di S3.4 il **padre non veniva consultato** all'avvio:

- `handle_rights_execute` (cardo) guardava i diritti del **chiamante** sul suo
  canale FS (`READ|EXECUTE`) e il verdetto di firma del **target**. La
  discendenza non entrava: `ceiling_for` classificava il peer per **identità
  dell'immagine**, non per padre.
- La classe nel PCB è **del contenuto**, ricalcolata a ogni load
  (`trust_class(image_id)`): `fork` ereditava tutto, ma **`exec` poteva
  elevare**. `usershell` (`ThirdParty`) poteva eseguire `sys/bin/interpres.bin`
  (firmato `System`) e **diventare System**.
- Il controllo in `libr`/shell era **facoltativo**: `SYS_EXEC` e
  `SYS_SPAWN_IMAGE` non chiedevano nulla; chi chiamava `exec_image` nudo
  saltava tutto.

"Un server decide se un processo può partire" non è implementabile solo nel
server: l'avvio non passa da lui. La scelta è il **kernel che applica il floor
sulla classe che ha appena misurato** (la trust table è già popolata da cardo):
niente crypto nel kernel, niente TOCTOU.

## Decisione

**Vittoria**: un figlio non può avere più diritti del padre. Chi lancia un
processo può **limitarlo** (classe minima delle immagini che potrà eseguire +
maschera delle ops FS), e il limite è **ereditario e monotono**: il figlio può
solo restringere, mai riallargare.

### Il meccanismo

```
SpawnMeta.rights = packed { floor: ImageClass (bit 0..8), ops: u32 (bit 8..40) }
figlio.floor = max(padre.floor, richiesto.floor)     // solo verso l'alto
figlio.ops   = padre.ops & richiesto.ops             // 0 = eredita, solo verso il basso
create_user / exec_current:
    c = trust_class(image_id_misurato)
    ammesso sse  figlio.floor <= c <= classe_corrente_del_padre
cardo (handshake):
    ceiling_for(chan) = policy(identita') & peer.ops_ereditate
```

**Invariante di rappresentazione**: in `SpawnMeta.rights` e in `peer_info.r10`
lo **zero significa "eredita"**, mai "nessun diritto" né "tutti i diritti".
`syscall_numbers::{rights_pack, rights_floor, rights_ops, rights_reserved}`
sono l'unica codifica, condivisa da kernel e `civis`.

### Kernel (S3.4a)

- PCB: `class_floor: ImageClass` (default `Untrusted`) e `ops_ceiling: u32`
  (default `RIGHTS_ALL`).
- `Process::create_user`: calcola `floor`/`ops_ceiling` e **rifiuta** (prima di
  allocare stack/address space) se `class(immagine)` è fuori da
  `floor .. classe_del_padre`. Log loud.
- `exec_current`: stessa regola, **senza** "richiesto": floor e ops si
  **conservano** (monotoni); una nuova immagine fuori intervallo è rifiutata e
  il processo resta intatto.
- `create_spawn_copy` (fork) e `create_thread`: copiano/ereditano
  esplicitamente `class_floor` e `ops_ceiling` dal padre/leader.
- `SpawnMeta` cresce da 56 a 64 byte (`rights: u64`); bit riservati a 0
  (fail-loud). Kernel e `civis` si aggiornano nello stesso commit: il kernel
  rifiuta `meta_len` diverso.
- `SYS_PEER_INFO` (47) espone i diritti ereditari **packed in `r10`** (stesso
  formato); `rdx` resta la classe. `civis::peer_info_rights`/`peer_info_ops`.
- **Hardening dei gate di ruolo** (stesso passo): `SYS_TRUST_IMAGE` e
  `SYS_SPAWN` per nome richiedono, oltre al ruolo (init/cardo, pid 1), anche la
  **classe `System`** del chiamante. Se init esegue un'immagine `Untrusted`
  (downgrade), non può più promuovere immagini nella trust table né spawnare il
  TCB embedded: chiude il caso «init degradato che riapre la porta».
- **`spawn_copy`/`create_thread` copiano `image_class` dal padre/leader**
  (non la ricalcolano da `image_id`): il fork/thread di un processo `System`
  forzato (init, embedded) resta `System` — il contenuto è lo stesso e la
  classe forzata è del padre, non del binario embedded non firmato.

### Cardo e shell (S3.4b)

- `ceiling_for(chan) = ceiling_base_for(chan) & peer_info_ops(chan)`:
  il tetto della policy è attenuato dalla maschera ereditaria. `0` (canale di
  cui non si conoscono i diritti) è fail-closed.
- La shell nativa (`interpres`) lancia i programmi con
  `floor = ThirdParty`: un'immagine `Untrusted` viene **rifiutata dal kernel**
  (non da un check facoltativo), e un `exec` verso `System` non eleva.
- init e TCB restano com'erano: `System` con floor `Untrusted` e ops `ALL`;
  i binari embedded (spawn dal kernel, `parent = None`) non passano dal
  controllo.

### Subtree ereditario (path, oltre classe e ops)

La maschera ops non basta: il **prefisso di path** (`subtree`, `ChanRights` in
cardo) era per-canale e **self-restriction only**, non ereditato. Un padre che
restringeva il proprio canale a `/tmp` e spawnava un figlio lasciava al figlio
il subtree **root** (default della sua identità): il figlio riapriva ciò che il
padre aveva chiuso.

Ora cardo mantiene `pid → subtree` (aggiornata all'handshake e a
`R_RIGHTS_DROP`, purgata su `EXIT_NOTIFY`) e all'handshake di un nuovo canale
**risale i padri** con `ps_info(pid).parent` (bound 16) fino al primo pid con
un subtree noto: lo applica come default al canale del figlio. È
**attestazione** (la catena la dice il kernel), non trasferimento: un processo
non può fingersi figlio di un altro. `R_RIGHTS_DROP` continua a permettere solo
il restringimento (il figlio può scendere a `/tmp/x`, mai risalire a `/`). Il
subtree resta una stringa di cardo: il kernel non interpreta i path (ADR-0005).

**Trappola pagata**: aggiungere l'handshake al loop di cardo ha **sfondato lo
stack di 16 KiB** (LTO fonde i frame: `#PF` user-mode sotto la guardia). Gli
helper `register_client`/`drop_rights`/`inherited_subtree` sono `#[inline(never)]`
(firewall), come i livelli btree/volume.

### Vie di creazione: nessuna senza controllo

| Via | Prima | Dopo S3.4a |
|---|---|---|
| `SYS_SPAWN` per nome (embedded) | solo init, classe `System` dal kernel | invariato (TCB, non raggiungibile da user) |
| `SYS_SPAWN_IMAGE` | nessun controllo oltre ELF/meta | `floor ≤ trust_class(byte) ≤ classe_del_padre` |
| `SYS_EXEC` | nessun controllo, **poteva elevare** | stessa regola; floor e ops **conservati** |
| `SYS_SPAWN_COPY` (fork) | copia del processo | copia esplicita di floor e ops |
| `SYS_THREAD_CREATE` | stesso address space | eredita floor e ops dal leader |
| spawn di boot/kernel | kernel, `parent = None` | invariato |

**Cosa resta possibile di proposito**: un `Untrusted` può ancora spawnare ed
eseguire codice `Untrusted` (`0 ≤ 0`) — la suite `tst` gira così e i programmi
su FAT devono partire. Diventa impossibile **guadagnare**: niente `Untrusted →
ThirdParty` eseguendo un firmato, niente `ThirdParty → System`, niente figlio
con floor `ThirdParty` che esegue un non firmato. La firma resta di cardo (il
kernel guarda solo la trust table): S3.4 aggiunge il muro dei **privilegi**,
non quello dell'**autenticità**.

## Test

- **`t77`** (nel gate, gira come `tst`/`Untrusted`): spawn/exec nudi di
  un'immagine `System` → `-1`; fork + exec → `-1`; thread del gruppo con floor
  del leader → spawn `System` negato; `SYS_SPAWN` per nome da non-init → `-1`;
  attenuazione annidata di `ops` verificata con `peer_info`; **bit riservati**
  in `rights` → `-1`; **fork** che eredita floor/ops (`peer_info_rights`);
  **`ceiling_for` con ops ereditate** (`rightshelper` spawnato con
  `OPEN|READ`: read ok, write negata); **gate strutturali** da `Untrusted`
  (`trust_image`, `dma_alloc` slot device, `map_mmio`) → `Err`; init
  invariato (la suite stessa ne è la prova). Conteggio: `usertests 72/72`.
- **`t67`/`t68` adattati**: non possono più spawnare un'immagine `System` da
  `tst` (era proprio l'elevazione che S3.4 vieta). t67 verifica la classe
  `System` di un processo vivo (cardo) e il **rifiuto** dello spawn elevato;
  t68 verifica il rifiuto di `ioprobe` (`System`). La copertura delle porte dal
  manifest si sposta sul probe `usr` (fuori gate), dove il padre è
  `ThirdParty`.
- **`usr/bin/floorprobe`** (fuori gate, `scripts/run-rights-tests.sh`): la
  shell nativa lo lancia con floor `ThirdParty`; il probe verifica che
  `Untrusted` è rifiutato, `ThirdParty` ammesso, `exec` di `Untrusted` negato.
- **`subtreehelper` + `subtreechild`** (in `t77`): il padre crea
  `tmp/subtree.txt`, restringe il proprio canale a `tmp` con `R_RIGHTS_DROP`,
  spawna il figlio che **eredita** `tmp`: dentro legge, fuori (`hello.txt`) è
  negato. Prova l'eredità del prefisso di path (attestazione via `ps_info`).

## Trappole

1. **ABI `SpawnMeta`**: kernel e `civis` nello stesso commit; rebuild di tutti
   i binari. Un binario vecchio fallisce loud (`meta_len`).
2. **`exec` non azzera il floor**: la persona resta (stesso pid, canali,
   parent), cambia solo l'immagine.
3. **Firma pigra**: la trust table è popolata da cardo al primo verdetto;
   un floor `ThirdParty` rifiuta un'immagine firmata ma non ancora verificata.
   È fail-closed: i chiamanti cooperativi (shell, init) verificano prima.
4. **Ogni via di creazione va bonificata**: `create_user`, `exec_current`,
   `create_spawn_copy`, `create_thread` costruiscono il `Process` campo per
   campo; un campo dimenticato non dà errore di compilazione. `t77` punti 3 e 4
   sono la guardia.
5. **`peer_info` ABI**: `rdi+rsi` = `ImageId`, `rdx` = classe, `r10` =
   diritti packed. `peer_info_class` continua a leggere solo `rdx`.

## Fuori scope (dichiarato)

Capability di sessione (S3.3c), cifratura dei bucket, TTL/rotazione del token.
Il **subtree** dei path è ereditato (sezione sopra); resta fuori l'eredità del
subtree **attraverso mount/symlink** (il check è sul path normalizzato, non
risolve i link).
