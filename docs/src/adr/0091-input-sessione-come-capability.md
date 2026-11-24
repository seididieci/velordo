# ADR-0091: I tasti sono della sessione — input come capability (D8d)

## Status

**Accepted** (2026-11-24). **D8d implementata e verificata** (gate
`[gate] PASS`, 6 suite + shell nativa 33/33, usertests 71/71 con t75).

Dipende da ADR-0082 (input a eventi), ADR-0083/0084 (attesa con deadline e
hint coalescente + stash), ADR-0089 (sessioni `littera`), ADR-0090 (ritiro di
`/dev/console`). Prepara il DE (D8e): il DE diventa il focus owner.

## Context

In D8c2 il **testo** ha smesso di essere un file: è una sessione, e la
capability è la mailbox. L'**input** era rimasto l'ultimo device del terminale:

- `porta` decodifica gli scancode e li serve su `/dev/input/keyboard`; ogni
  programma che vuole i tasti **apre un path** (login, `interpres`,
  `create-user`, la shell POSIX con un fd suo);
- `INPUT_ATTACH` non aveva policy: chiunque poteva sottoscriversi e leggere i
  tasti di tutti — con il DE non c'è più "la tastiera di tutti", c'è quella
  della tile in focus;
- il percorso tornava attraverso cardo (`read_fs`), cioè il filesystem
  single-threaded che il testo aveva appena lasciato;
- l'eco era al lettore (giusto: ogni lettore è un editor di linea), ma il
  modello non sapeva nulla dei tasti che riceveva.

## Decision

### 1. La tastiera non è un file

`porta` serve i byte con **`INPUT_READ`** (0x49): diretto, sincrono, payload
nei **registri** (8 byte in `w0`, `w1` = quanti) — niente ring, niente cardo.
La sottoscrizione (`INPUT_ATTACH`) è la **capability** e richiede classe ≥
`ThirdParty`: `Untrusted` non legge i tasti di nessuno. Il device
`/dev/input/keyboard` si **ritira** (mount via, `DEV_KEYBOARD` fuori dall'ABI,
`dev_type("keyboard")` via): la tastiera decodificata non è più un nome che
chiunque apre. Resta `/dev/kbd` (scancode raw), che è l'ingresso del driver e
lo legge solo `porta`.

### 2. Il focus owner inietta

Chi possiede il focus — la **console** in D8d (possiede `Sessions::focus`), il
**DE** in D8e — si sottoscrive a `porta` (E0: `INPUT_ATTACH` +
`INPUT_NOTIFY`), a ogni hint **drena** e spinge i byte nella coda della
sessione in focus. In D8e il DE prende il posto della console e usa
`SESS_INPUT` (tag riservato) per iniettare: il canale della sessione è del
server, l'iniezione no.

Il percorso dei tasti diventa: `kbd` → `porta` → (IPC diretta) → console →
coda della sessione → lettore. **cardo non c'è.**

### 3. Hint coalescente, coda è la verità

`SESS_INPUT_EVENT` (0xAA) è il notify: fire-and-forget verso il **lettore
dichiarato** (`SESS_READ` lo registra; `detach`/`close`/morte lo tolgono;
hint fallito → ruolo libero). Un hint perso non perde un tasto — chi si
sveglia drena finché `SESS_READ` non torna 0 — e uno in più fa un drain a
vuoto. Il pattern è quello di E0/E2 (ADR-0082/0084): **hint**, mai dato.

Conseguenza sui client: gli **hint non sono risposte**. `ipc::wait_reply`
ora mette da parte (`stash`) i tag-hint invece di fallire con
`UnexpectedMsg`: senza, un tasto premuto mentre si parla con un altro servizio
farebbe fallire quella richiesta — e il tasto andrebbe perso. È la stessa
lezione di E2, applicata al percorso sincrono.

### 4. L'eco è del lettore; `SESS_ECHO` è l'opt-in

Chi legge **ha un editor di linea** (login, `interpres`, shell POSIX: cursore,
history, hint, continuazioni ESC) e scrive lui i caratteri sulla sessione
(`sess::out_screen`, mai nel mirror seriale). Un'eco di sessione
*raddoppierebbe* ogni carattere; e il modello il backspace lo conosce solo
come "cursore a sinistra" (non cancella), quindi un'eco di servizio non
saprebbe nemmeno fare la cosa più semplice.

Quindi: `input::attach` **spegne** l'eco del modello (il lettore si dichiara
tale) e `SESS_ECHO` resta il flag che il server onora sui byte che inietta —
per un consumatore **senza** editor (D8e: prompt di chrome, o un "getty" nudo).
t75 copre i due versi.

### 5. `civis::input` non cambia forma

`attach`/`read`/`wait`/`wait_until`/`detach` restano: cambiano *dentro*
(sessione invece del device). Login, `interpres` e `create-user` non cambiano
di una riga; la shell POSIX migra le sue tre funzioni di lettura e perde
`TERM_FD` (l'ultimo fd del vecchio percorso). La cache della sessione è una
sola (`sess::current`), condivisa da stdout e input: un processo parla con
**una** sessione, sempre la stessa.

Una sola eccezione alla regola del "device": il **focus owner** usa
`civis::input::device` (attach + drain), che è la parte riusabile dal DE.

## Conseguenze

- **Isolamento**: un programma legge i tasti **della sua sessione**; la
  tastiera non è più un file apribile, e la capability si nega a `Untrusted`.
- **Zero kernel, zero servizi nuovi**: un tag per lato (`INPUT_READ` 0x49,
  `SESS_INPUT_EVENT` 0xAA), un tipo di device ritirato (`DEV_KEYBOARD`).
- **cardo esce dal percorso dell'input** come era uscito da quello del testo:
  la tastiera è un servizio, non un filesystem.
- Il DE (D8e) riceve i keybinding globali gratis: è lui il focus owner, quindi
  vede i tasti **prima** di decidere cosa iniettare nella sessione.
- `INPUT_MAX` (1024) a coda piena scarta il byte **nuovo**, non il più vecchio:
  il lettore sta per leggere la testa, e i tasti già arrivati sono quelli
  battuti prima (un `pop_front` perdeva il tasto che stava per essere letto).
- **Trappole pagate e annotate**:
  - la **reply va prima dell'hint**: chi inietta è spesso anche il lettore
    (`SESS_TEST_INPUT`), e un hint accodato davanti alla reply farebbe fallire
    la sua `send` sincrona (`UnexpectedMsg`) — il test lo ha mostrato al primo
    giro;
  - l'`EXIT_NOTIFY` **della console** va riconosciuto in `wait_until`
    (riattaccata una volta): consumato e ignorato, il lettore dormirebbe per
    sempre su una console morta (stesso pattern che `input.rs` aveva per
    `porta`);
  - i messaggi estranei in `wait_until` vanno nello **stash** (E2), non persi;
  - la console parte **prima** di `porta`: l'attach è bounded a boot e, dopo la
    morte di `porta`, il riattacco è un tentativo per volta nel loop (`recv_timeout`
    con la scadenza del retry) — mai un'attesa bloccante dentro un server;
  - **la cache di sessione va invalidata dalla `close`**: chi risolve la
    sessione (`sess::out`/`sess::current`) e poi la chiude lasciava la cache
    stantia, e ogni operazione successiva tornava `Denied` (t75 l'ha trovato:
    "info fallita" su una sessione chiusa dal test precedente). Ora
    `Session::close` dimentica la cache se era la sua e `input::attach` la butta
    se `SESS_ECHO` viene negata (cache stantia dopo un restart);
  - `send` è `Ok` **anche quando il server rifiuta** (`w0 = !0`): il gate della
    capability va guardato sul payload della reply, non sul `Result` dell'IPC
    (t31 leggeva un rifiuto come successo);
  - `t31`/`t74` citavano il device: riscritti, o il ritiro sarebbe passato con
    i test spenti (lezione di D8c2).
- Fuori scope dichiarato: i keybinding di split e il routing verso gli ospiti
  GUI (D8e/D8f — servono il DE); il puntatore; il grab selettivo (il DE legge
  tutto e decide); il power management della tastiera (Pianificate).
