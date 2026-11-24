# ADR-0090: Ritiro di `/dev/console` — il testo è una sessione, non un device (D8c2)

## Status

**Accepted** (2026-11-24). **D8c2 implementata e verificata** (gate
`[gate] PASS`, 6 suite + shell nativa 33/33, zero `[munmap]`).

Dipende da ADR-0089 (sessioni `littera` e endpoint `SESS_*`), ADR-0060
(console server), ADR-0066 (danno/rect flush, T4 e mirror seriale con budget),
ADR-0073 (shm ownership/destroy), ADR-0086 (mirror seriale del testo).
Sostituisce il §4 di ADR-0089 (l'adattatore `/dev/console`).

## Context

D8c1 ha lasciato `/dev/console` come **adattatore** sulla sessione 0: la
console aveva ancora i gestori `DEV_OPEN/READ/WRITE/CLOSE` e registrava il
mount presso cardo, mentre login, shell e comandi continuavano a scrivere sul
device. Era il passo di bisezione giusto (gate invariato) ma è anche la forma
che il DE non può tenere:

1. **Un device per testo è un nome, non una capability.** Chi scrive deve
   *avere* la sessione (attestata sul pid), non indovinare un path. Con N
   sessioni il device diventa o `/dev/ttyN` (che abbiamo scartato) o un nome
   condiviso che non distingue le tile.
2. **cardo resta nel percorso caldo.** Ogni write di testo attraversa il
   filesystem single-threaded: è il ciclo che ADR-0066 documenta come
   deadlock potenziale (un driver che serve e chiama il FS).
3. **Doppia scrittura del mirror.** Ogni writer (login, `interpres`, shell
   POSIX, comandi) scrive **due volte**: sul device e sul seriale, ognuno con
   la propria copia del budget. Il mirror seriale è un contratto del gate: due
   implementazioni sono due modi di romperlo.
4. **La shell POSIX scriveva sulla tastiera.** `term_write_bytes` faceva
   `write_fs` sul fd di `/dev/input/keyboard`; `porta` accettava `DEV_WRITE` e
   inoltrava il payload a `/dev/console`. Tre driver per una riga di testo.

## Decision

### 1. La mailbox è dell'**attachment**, non della sessione

D8c1 aveva una mailbox **per sessione** (una pagina shm, `sess-proto`), e in
D8c1 era corretto perché l'unico client della sessione 0 era il test. Con
D8c2 il testo diventa l'output di tutti: due processi attaccati alla stessa
sessione condividono la metà OUT, e fra il "client copia" e il "server copia"
(dopo aver ricevuto il messaggio) la write dell'uno può essere sovrascritta
dall'altro — corruzione silenziosa, nessun errore.

Il percorso FS non aveva mai avuto il problema: il ring è **per processo**.
Ora la mailbox è per **canale**: `(canale, pid, shm, mappatura)`. Si crea alla
prima `SESS_CREATE`/`SESS_WHOAMI`/`SESS_ATTACH_DEFAULT` del canale e si
distrugge su `SESS_CLOSE` o `EXIT_NOTIFY` (per pid: alla notifica il canale è
già invalidato). La sessione resta una e il modello non cambia: cambia dove
passano i byte. Sparisce la mailbox della sessione 0 creata al boot — non
esiste più un mezzo condiviso.

Costo: una pagina per peer attaccato, la stessa cifra che il percorso del
testo spendeva già col ring FS. Limite dichiarato: una write è atomica fino a
`MAX_CHUNK` = 2048 B (il ring FS lo era fino a 4087); sopra si spezza in più
`SESS_WRITE` e un interleave con un altro scrittore è possibile. Se morde, la
mailbox passa a due pagine (OUT 4092): **parcheggiato**, non fatto.

### 2. `SESS_WRITE.w1` = hint di mirror

`SESS_WRITE` prende `w1`: `0` = la write entra nel **mirror seriale**, `1` =
solo schermo. Serve perché l'eco la fa il **lettore** (`identis-login`,
`interpres`, shell POSIX: ognuno ha il suo editor di linea, con cursore,
history e continuazioni ESC) e l'eco non deve finire nel log su cui il gate fa
regex byte-exact. Client: `Session::write` e `Session::write_screen`.

Con l'instradamento dei tasti nella sessione (D8d) l'eco si sposta in
`littera` e `SESS_ECHO` basta: allora `write_screen` resta per i ridisegni e i
test.

### 3. Il mirror seriale è del server

`feed_session` mirrora sul seriale la sessione **in focus**, in formato nudo
(nessun prefisso), con `print_string` raw — la **stessa** chiamata che facevano
i client, quindi gli stessi byte. `MIRROR_SESSION_MAX` (1 MiB per sessione) è
una rete di sicurezza: il budget *per comando* (256 KiB) resta nella shell,
che è l'unica a sapere cos'è un comando. Le write "solo schermo" non entrano
nel mirror, e il banner di boot non lo è: il log del gate resta quello di
prima.

Il mirror è anche il motivo per cui il marker di boot funziona: la write del
prompt di login ha reply **dopo** che il server ha alimentato e flushato la
sessione.

### 4. Il sink di stdout è uno, nel meccanismo

`civis::sess::out(bytes)` risolve la sessione **del processo**
(`SESS_WHOAMI`, attestata dal kernel), poi l'unico ingresso nominato
(`SESS_ATTACH_DEFAULT`, la sessione 0); cache per processo, invalidata su
`Ipc`/`ServiceDead` (restart della console) con un tentativo di riattacco;
se nessuna sessione risponde, l'output va sul **seriale**: non si perde mai.
`out_screen` è la variante che non entra nel mirror.

Su questo sink si appoggiano, senza sapere altro:

| Consumatore | Prima (D8c1) | Dopo (D8c2) |
|---|---|---|
| `identis-login`, `interpres`, `create-user`, `users`, `logout`, `dns-resolve` | `open("/dev/console")` + `write_fs` + `print_str`/`flush` (doppia scrittura) | `civis::sess::out` (una scrittura; il mirror è del server) |
| personalità POSIX (`libr::stdio::route_out`) | `open("/dev/console")` + mirror `SYS_WRITE` | `civis::sess::out`; redirect `>` invariato (e non mirrorato: il file non è il terminale) |
| shell POSIX (`term_write_bytes`, `term_echo`) | `write_fs` sul fd della **tastiera** → `porta` → `/dev/console` | `out` / `out_screen`; budget per comando invariato |
| `porta` | coda output + relay `DEV_WRITE` verso `/dev/console` | **nessun testo**: decodifica i tasti e basta |
| `usertests` t64 | `open("/dev/console")` + righe una per write | `SESS_ATTACH_DEFAULT` + `write_screen` (una per riga) |

### 5. Il ritiro è loud

`DEV_CONSOLE` sparisce dall'ABI (il numero 3 **non si riusa**), `dev_type`
non conosce più `"console"`, la console non registra il mount e non ha i
gestori `DEV_*`. Quindi: `open("/dev/console")` → `ERR_NOTFOUND`; `write` su
`/dev/input/keyboard` → `ERR` (il frame viene comunque consumato: cardo non lo
consuma per le write remote, e un frame appeso desincronizzerebbe il ring del
client). t74 lo prova.

Il marker di boot `[gate] boot: login-ready` (emesso da `identis-login` quando
la write del prompt ha avuto reply) sostituisce la stringa di UX
`[login] Nome utente:` nel contratto del gate: `run-tests.sh` e
`scripts/shell_harness.py` aspettano il marker.

## Conseguenze

- **Zero kernel, zero syscall, zero servizi**: l'unica modifica d'ABI è il
  `w1` di `SESS_WRITE` e la rimozione di `DEV_CONSOLE`. `SERVICE_COUNT` resta
  20, `SESS_*` restano 15 tag.
- **cardo esce dal percorso del testo** per sempre (niente mount, niente
  relay): con ADR-0089 era già vero per il *modello*, ora lo è anche per il
  trasporto.
- Sparisce la trappola di ADR-0086 (ri-registrare `/dev/console` su
  `EXIT_NOTIFY` del peer FS, con il deadlock cardo→console→cardo): non c'è più
  un mount da rifare. Il purge per pid di sessioni **e mailbox** resta.
- Il gate guadagna tre asserzioni strutturali: nessun sorgente cita
  `/dev/console` (eccezione: il test che ne prova il ritiro) né `DEV_CONSOLE`;
  t64 deve aver girato davvero (una suite verde con il test in SKIP sarebbe il
  modo silenzioso in cui questo ritiro fallisce).
- **Trappole pagate e annotate**:
  - mailbox condivisa = corruzione silenziosa (è il motivo del §1);
  - doppio mirror se il client non perde la sua copia *nello stesso passo* in
    cui nasce quella del server;
  - l'eco non entra nel mirror (`w1 = 1`), o cambia il log del gate;
  - `run-tests.sh` ha **due** punti che parlano del prompt (attesa e messaggio
    di timeout) e `shell_harness.py` ha `wait_login_prompt`: si cambiano
    insieme — e **mai** mentre il gate gira (bash rilegge il file per offset:
    una modifica a metà esecuzione produce un errore di sintassi a comando
    già passato);
  - la shell POSIX scriveva sul fd della **tastiera**: cercare
    `/dev/console` non bastava a trovarla;
  - `CONSOLE_OUT` **non è il terminale**: è il sink seriale del kernel (lo usa
    anche la PAL `std`). Resta.
- Fuori scope dichiarato: il sink `std::io::stdout` della PAL (scrive
  `SYS_WRITE`+`CONSOLE_OUT` = seriale); i tasti nella sessione e l'eco in
  `littera` (D8d); la mailbox a due pagine; il transcript su ArcaFS e telnet
  (parcheggiati, piano DE §6.6).
