# ADR-0058: Il FAT esce dal boot e dalla suite — ArcaFS unico filesystem di sistema

## Status

Accepted

## Context

Velordo è nato con due filesystem: una **ramfs** in memoria e un **FAT32**
su disco (la "root di boot"), con i binari dei servizi iniettati in
`/fat/bin` via `mcopy` a ogni build (`scripts/inject-bins.sh`). Con
l'arrivo di **ArcaFS** (Fase 54+) il quadro e' cambiato: il volume root e'
un volume ArcaFS (`root=UUID=…` dalla cmdline) e i binari possono vivere
come **oggetti** in bucket di classe (`sys`, `vela`, `usr`, `tst`) dentro
ArcaFS — il meccanismo `obj_get`/`spawn_image` (Fase 55, N0) e' gia' pronto
e init gia' carica da li' la maggior parte dei servizi.

Il FAT era pero' rimasto in tre ruoli:

1. **media di boot**: `init` caricava `identis` e `login` da `/fat/bin/…`
   (gli altri servizi gia' venivano da ArcaFS);
2. **piano di prova della suite**: `testfat` (7/7) nel gate, `usertests`
   t32/t33/t35/t36/t58 su path e device FAT, `foreign` (t57), la suite
   shell (10 fasi) su `/fat`, il PAL (`hello-std.bin` iniettato nel FAT);
3. **riferimento prestazionale**: il confronto A/B ArcaFS-vs-FAT nel bench
   (`scripts/bench-compare.py`), che misura il salto di I/O del passaggio
   a un filesystem a extent.

Con il filesystem unico, il FAT è un **terzo filesystem** che il boot deve
portare dietro: due dischi extra, un iniettore (`mcopy` con nomi 8.3), una
grammatica di path (`/fat/…`) e nomi di file che il VFS POSIX non sa
scrivere (`HELLO.TXT` maiuscolo, `SUB`, `MARKER.TXT`). E il suo driver
(FAT32 in `cardo`) resta comunque nel tree.

Il vincolo che rende la semplificazione non banale: **le lettere dei device
cambiano** quando sparisce un disco (`sda` era il FAT, ora e' il volume
root), e i test hardcodavano `/dev/sda`. La lezione di Fase 16d
("la suite cerca i volumi via magic/UUID, mai per lettera") era gia' scritta
nei commenti: il FAT l'aveva resa inapplicabile.

In parallelo, il volume ArcaFS non aveva **identita'**: il suo UUID stava
nel superblock ma nessuno lo leggeva (la discovery by-uuid/by-label del
driver disco era solo-BPB, cioe' solo-FAT), e il campo "mountpoint" del
superblock era **morto** (scritto, mai letto).

## Decision

### 1. ArcaFS è l'unico filesystem di sistema

Il boot non attacca più dischi FAT: `run.sh` monta solo il volume root
ArcaFS (+ le derivate opzionali `ARCA_IMG=1`). Tutti i binari — servizi,
driver, test, programmi utente — vivono in ArcaFS: nel namespace (`ns:`,
per i path VFS) e nei bucket di classe (per gli oggetti).

`init` non ha più due modi di caricare un servizio: `SvcMeta.path` (il
file su `/fat`) **sparisce**, e `spawn_entry` è "oggetto ArcaFS **o**
embedded (init/disk/fs/log)". Il doppio pinning degli hash (FNV-1a +
BLAKE2s) resta sulla via oggetto (`verify_image`), e `identis`/`login`
— che non erano pinnati (arrivavano dal FAT senza manifest) — **entrano
nel pinning**.

### 2. La suite FAT32 esce dal gate (ma resta nel repo)

La suite `testfat` non gira più nel gate: il gate è **6 righe** (testfs,
testsarca, posixtests, identistest, usertests, threadtest). Il crate
resta e gira in un **run dedicato** (`scripts/run-fat-tests.sh`) che
attacca un disco FAT e abilita il feature `fat_tests` di init. Motivo: il
driver FAT32 resta nel tree (riferimento prestazionale + futuro USB) e
una suite che lo copre costa poco; una suite *nel gate* che richiede un
disco che il boot non usa no.

### 3. I test migrano dal FAT ad ArcaFS

- **t32** (bounce del driver disco): il baseline non è più "settore 0 di
  `/dev/sda`" ma "il device che ha il superblock ACFS" — **scoperto**, mai
  per lettera; lo smoke è il fixture sulla root.
- **t33/t35** (mount/umount): il mount è sul volume root scoperto; il
  contenuto letto è il fixture (`hello.txt`).
- **t34** (diritti per canale): il subtree di prova è `/bin` (una dir reale
  sulla root) invece di `/fat`.
- **t36** (identità): riscritto sulla dimensione **UUID** dell'identità
  ArcaFS (mount per UUID a 16 hex, open raw del by-path, listing di
  `/dev/disk/by-uuid`). La dimensione **LABEL è rinviata** (è roba da USB,
  §6).
- **t58**: l'oggetto nativo è confrontato con il **path `ns`** sulla root
  (byte-identici per costruzione del seed) invece che con il file FAT.
- **`foreign`** (t57): il fd lecito è un file sulla root.
- **`testsarca`**: i device hardcoded (`/dev/sdc`, `/dev/sdd1`) diventano
  **scoperti per magic+UUID**: senza, i gruppi 22-32 e 41-50 (il motore su
  disco) venivano **saltati** (adaptive skip: verde ma non testati).
- **suite shell** (10 fasi): l'harness allega **un** disco invece di tre
  (niente overlay FAT); i path `/fat/…` diventano path della root; la
  "ls /" non elenca più `fat`.
- **PAL**: `hello-std.bin` è seedato in ArcaFS (`ns:test/stdhello.bin`).

### 4. Il volume ArcaFS impara a presentarsi (superblock v4)

`ARCA_VERSION` 3 → **4**: il superblock cresce da 128 a **192 B** (sta
dentro il settore da 512 B; l'header-extension resta a 1024 → la geometria
del blocco non cambia) e il layout dei campi diventa:

```
  0 magic "ACFS" · 4 version · 8 block_size · 12 uuid · 20 gen
 28 root · 36 refcount · 44 alloc
 52 label      (32 B, NUL-padded)  — il NOME del volume ("SYSTEM")
 84 mountpoint (56 B, NUL-padded)  — il posto dove dichiara di stare
140 meta_root · 148 auto · 149 flags · 184 checksum (FNV-1a di [0..184))
```

Il campo **mountpoint** esisteva già (a offset 52) ma era **morto**: scritto
da `superblock_build`, letto da nessuno. Ora è vivo (e il nome non è più
"mountpoint" per caso: è un campo di testo con due significati distinti).

Le due stringhe hanno significati diversi e non si somigliano:

- **label**: l'identità del volume (il suo nome) — la usa il log, e
  sarà la chiave della discovery by-label (fase USB);
- **mountpoint**: *dove* il volume vuole essere montato, path **relativo**
  alla root ("tests", "media/usb"); la vista POSIX ci mette davanti la "/".
  Vuoto = il volume non dichiara niente.

Il **tool host** impara a scriverli: `arca create --label L [--mountpoint M]`
(default: il mountpoint segue la label) e il nuovo `arca set <img>
[--uuid] [--label] [--mountpoint] [--lba N]`, che patcha il superblock
ricalcolando il checksum (gemello di `arca uuid`).

Il **driver disco** impara a leggerli: `sniff_arca_uuid` (magic + versione
+ block-size + checksum, casa `arcafs`) espone l'uuid a 64 bit del volume
come identità del nodo → `/dev/disk/by-uuid/<16hex>` (formato distinto dal
seriale FAT a 8 hex: nessuna ambiguità) e la chiave `UUID=<16hex>` nella
risoluzione delle source.

### 5. Il modello: dichiarativo per ArcaFS, imperativo per il FAT

- **ArcaFS** è *dichiarativo*: il volume dice come si chiama e (in futuro)
  dove vuole stare.
- **FAT** resta *imperativo*: il sistema decide (tabella dei mount).

La voce FAT in `STATIC_MOUNTS` resta, ma **iniettiva**: senza il disco il
mount non si attiva (la spec è lazy) e non tocca nulla. Serve ai run
opt-in (suite FAT, confronto prestazionale) e tornerà con le chiavette USB.

### 6. Cosa è rinviato (e perché)

- **L'auto-mount dei volumi dichiarati** (il probe che legge il mountpoint
  e monta il volume lì): **bloccato** dal limite "un engine per volume" di
  cardo — un secondo volume ArcaFS si monta, ma è uno **stub non
  leggibile** (l'unico `DiskEngine` globale è legato alla root). Montarlo
  sarebbe una promessa che il sistema non può mantenere (una directory
  vuota che sembra montata). L'auto-mount arriva con il **multi-engine**,
  che è anche il prerequisito dell'USB.
- **La discovery by-label** (e il mount per LABEL): è roba da USB (una
  chiavetta con la sua etichetta). Per ora il campo label c'è ed è letto,
  ma la by-label non esiste ancora.
- **Il bucket `usr:<nome>`** come volume: vedi ADR-0057 §7 (rinviato a
  S3.2/3.3 insieme all'ACL per utente).

## Consequences

### Positive

- **Un solo filesystem di sistema**: un disco in meno al boot, un
  iniettore in meno (`inject-bins.sh` è cancellato: i binari sono nel
  volume), una grammatica di path in meno (`/fat/…`), nessun vincolo 8.3
  sui nomi.
- **Il boot non ha più un disco "in più"**: meno I/O di enumerazione, meno
  mount, e il `blkdbg` più pulito.
- **Il pinning degli hash copre `identis` e `login`**: prima arrivavano
  dal FAT senza manifest (mai verificati); ora sono oggetti ArcaFS con
  doppio pinning FNV+BLAKE2s come tutti gli altri.
- **La suite non hardcode più le lettere**: i device si scoprono per
  magic/UUID → i test sopravvivono al cambio del numero di dischi (e il
  `SWAP_DRIVES` resta possibile).
- **Il volume ha un'identità**: uuid esposto (by-uuid, mount per UUID) e
  label leggibile → la discovery USB ha la sua base.
- **Il campo mountpoint smette di essere morto**: il formato lo porta, il
  tool lo scrive, il guest lo legge — manca solo chi lo onora (§6).

### Negative / limiti

- **Il gate perde la suite FAT32** (7/7): la copertura del driver FAT32
  passa dai test alla suite opt-in. Se il driver FAT32 si rompe, il gate
  non se ne accorge (è il prezzo del FAT fuori dal sistema).
- **Il provider FAT32 resta codice "senza casa"**: vive in cardo ma
  niente nel boot né nel gate lo esercita (resta il confronto A/B e la
  suite opt-in).
- **L'auto-mount richiede il multi-engine**: il campo c'è, ma la
  dichiarazione non viene ancora onorata (i volumi derivati restano
  montati a mano dai test).
- **`/fat` sparisce dai path**: le suite e i documenti che parlavano di
  `/fat` vanno migrati (fatto per la suite; i capitoli storici restano
  snapshot).
- **La suite shell ora fa il login** (root/root) prima di avere la shell:
  il login è l'entry point nativo (ADR-0055) e non è più un programma
  "al posto" della shell ma il suo predecessore.

### Neutral

- La versione del formato ArcaFS sale a 4: le immagini si rigenerano a
  ogni run (il volume è un artefatto, non un dato).
- Il superblock cresce a 192 B: la shadow a LBA1 e l'header-extension a
  1024 restano dove sono; solo il checksum si sposta.
- La suite `testfat` resta nel repo (feature `fat_tests`): il costo è un
  binario in più nel tree, non nel gate.

## Alternatives Considered

- **Tenere il FAT come media di boot** (solo i binari su FAT, il resto
  ArcaFS): scartato — due modi di caricare un servizio, due mount, e il
  FAT resta un prerequisito del boot (il requisito era eliminarlo).
- **Cancellare del tutto il provider FAT32**: scartato — è il riferimento
  prestazionale (l'A/B ArcaFS-vs-FAT) e la base dell'USB; cancellarlo
  significa riscriverlo quando servirà.
- **Cancellare la suite FAT32** invece di metterla opt-in: scartato — il
  driver resta nel tree e deve avere una suite che lo copre (meglio
  opt-in che nessuna).
- **Auto-mount subito, senza multi-engine**: scartato — un secondo volume
  ArcaFS montato è uno stub non leggibile: il mount prometterebbe dati che
  non arrivano. Meglio dichiarare il mountpoint e onorarlo quando
  l'engine è pronto.
- **Superblock v4 con label + mountpoint ridotti per non crescere**
  (es. 128 B con label 12 B): scartato — crescere a 192 B è gratis
  (il settore è 512 B) e toglie i vincoli arbitrari sui due nomi.
- **Mountpoint assoluto ("/tests")**: scartato per la decisione dell'utente
  — il path è relativo alla root e la vista POSIX aggiunge la "/"; il
  VFS di cardo ha solo target relativi (la radice è il volume root).

## Testing

- **Gate** (6 righe, zero FAIL/PANIC/FAULT): testfs 5/5, testsarca
  64/64 (i due casi nuovi `identita' volume v4` leggono label e
  mountpoint dal superblock), posixtests 4/4, identistest 11/11,
  usertests 58/58, threadtest 12/12.
- **Suite shell** (fuori dal gate kernel): 10/10 fasi, con il login
  all'avvio (root/root) e i path sulla root ArcaFS.
- **Suite FAT opt-in**: `scripts/run-fat-tests.sh` (disco FAT + feature
  `fat_tests`).
- **Tool host**: `arca create --label/--mountpoint` e `arca set` (patch
  del superblock con ricalcolo del checksum) verificati a mano; il test
  `seed_roundtrip_load` gira sul formato v4.

## Relazioni con altre decisioni

- **ADR-0054** (blocchi 64 KiB): il superblock v4 vive nello stesso
  blocco da 64 KiB, la cui geometria non cambia.
- **ADR-0055** (gestione utenti) e **ADR-0057** (token di sessione):
  `identis` e `login` passano da `/fat` ad ArcaFS; il bucket
  `identis:config` è una chiave del namespace del volume root (le sue
  ACL per utente restano il debito S3.3).
- **ADR-0037** (policy su identità): il tetto per canale resta; qui si
  aggiunge l'identità del *volume* (uuid a 64 bit) accanto a quella del
  *programma* (hash dell'immagine).
- **ADR-0005** (microkernel): il filesystem è in userspace; questo ADR
  sposta il *default* del filesystem di sistema da FAT32 a ArcaFS senza
  toccare il kernel.
