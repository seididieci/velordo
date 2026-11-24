# ADR-0089: Sessioni di terminale — `littera` e l'endpoint di sessione (D8c1)

## Status

**Accepted** (2026-11-24). **D8c1 implementata e verificata** (gate
`[gate] PASS`, `[usertests] PASS 70/70` con t74, zero `[munmap]`).

Dipende da ADR-0060 (console server), ADR-0066 (danno/rect flush e T4,
scroll a shift di pixel), ADR-0073 (shm ownership/destroy), ADR-0087/0088
(compositor e finestre del DE).

## Context

Il DE di Velordo è fatto di **tile**, e una tile *è* una sessione di
terminale: non una finestra che contiene un terminale (piano DE, torsione 1).
Finora esisteva **un** terminale, ed era lo stato della console: griglia,
decoder UTF-8, parser ANSI, cursore, input ed eco, tutti dentro `Term`
(`userland/console`), mescolati a surface, font, danno, compositor e IPC.

Tre problemi concreti di quella forma:

1. **N terminali, un solo stato.** Non c'è modo di dire "questo testo va nel
   terminale 3": `/dev/console` è uno stream unico e `SYS_WRITE(CONSOLE_OUT)`
   ha un solo bersaglio.
2. **Testo e display sono due mestieri.** Chi scrive ANSI non ha bisogno di
   sapere cos'è un font, un danno o uno shift di pixel; chi disegna ha bisogno
   di sapere *quando* il modello scroscia, non *come* si interpreta un `CSI`.
3. **Il testo deve poter viaggiare senza passare da cardo.** Il cardo è
   single-threaded e la console lo sta bloccando mentre serve
   `/dev/console`: un secondo canale di testo *attraverso il filesystem*
   sarebbe un deadlock, non una feature.

## Decision

### 1. `libs/littera`: la sessione senza display

`littera` è un crate `no_std` puro: `Session` = `term::Grid` + decoder UTF-8 +
parser ESC/CSI (colori truecolor/256, `CSI D`/`CSI C`/`CSI K`, `BS`, `CR`,
`LF`, `FF`) + coda di input + flag di eco. Nessun pixel, nessuna tile,
nessuna IPC: `littera` non conosce il DE, il DE (via `console`) conosce
`littera`. La divisione è deliberata:

- la **sessione** sa *cosa* è stato scritto (modello, scrollback, cursore);
- la **console** sa *dove* mostrarlo (surface, font, danno, shift dei pixel).

`Sessions` è la tabella: `MAX_SESSIONS = 8` (sessione 0 compresa), id = indice
di slot **stabile** (mai riusato finché il posto non torna libero), `focus`,
attestazioni per pid, attachment per canale. La sessione 0 esiste da subito ed
è lo **schermo**: scrollback 1000 righe (il terminale di sempre); le sessioni
nuove ne hanno 256 — N tile non devono costare 1000 righe l'una.

**Lo scroll appartiene a entrambi**, ed è l'unico punto in cui la divisione
morde: il modello deve scorrere *prima* che il cursore si sposti, e chi
disegna può accompagnare lo scroll con uno shift di pixel (T4). Per questo
`Session::put(cp)` non esegue lo scroll: ritorna `Step::NeedScroll`, il
chiamante fa il suo scroll (modello + pixel) e richiama `put` con lo **stesso**
codepoint; la sessione ricorda l'azione interrotta (`Newline`, `Home`,
`WideWrap`) e la completa. L'ordinamento osservabile è identico a quello del
vecchio `put` della console, e la console mantiene la sua politica T4 intatta
(`shift` solo se disegnata, non-bulk, sfondo riga-invariante, superficie
pixel).

### 2. L'endpoint di sessione: 15 tag, nessuna syscall, nessun servizio nuovo

La capability è una **mailbox**: una pagina shm (4096 B) che il server crea,
mappa RW e consegna alla creazione (`sess-proto`: OUT client→sessione
`0..2048`, IN sessione→client `2048..4096`, `MAX_CHUNK = 2048`). Il testo
passa solo da qui; `SESS_WRITE`/`SESS_READ`/`SESS_DUMP` sono chunk da al più
2048 B, con la mailbox già scritta/da leggere.

| Tag | Nome | Semantica |
|---|---|---|
| 0x9B | `SESS_CREATE` | `w0` = flag (0 = legata al chiamante, 1 = non legata) → `w0` = sess, `w1` = shm della mailbox |
| 0x9C | `SESS_WHOAMI` | nessun argomento → la sessione **del pid chiamante** (owner o binding) |
| 0x9D | `SESS_ATTACH_DEFAULT` | → la sessione 0: l'unico ingresso **nominato** |
| 0x9E | `SESS_BIND` | `w0` = sess, `w1` = pid: il DE lega un figlio appena nato |
| 0x9F | `SESS_WRITE` | `w0` = len (byte in OUT) → feed della sessione attaccata |
| 0xA0 | `SESS_READ` | `w0` = max → copia fino a `max` byte di input in IN |
| 0xA1 | `SESS_RESIZE` | `w0` = `cols<<16\|rows` → reflow (D4) |
| 0xA2 | `SESS_ECHO` | `w0` = on: eco dell'input sul modello (flag di **sessione**) |
| 0xA3 | `SESS_FOCUS` | `w0` = sess: la sessione **disegnata** |
| 0xA4 | `SESS_CLOSE` | `w0` = sess (mai la 0): smappa e distrugge la mailbox |
| 0xA5 | `SESS_INFO` | `w0` = sess → `w0` = `cols<<16\|rows`, `w1` = eco \| focus<<1 |
| 0xA6 | `SESS_STATE` | `w0` = sess, `w1` = limite → `(non vuote, CRC del testo)` |
| 0xA7 | `SESS_DUMP` | `w0` = sess, `w1` = max → coda del testo visibile in IN |
| 0xA8 | `SESS_STATS` | → `(sessioni vive, focus)` |
| 0xA9 | `SESS_TEST_INPUT` | `w0` = len (byte in OUT) — solo test: inietta input |

Zero syscall nuove, zero `Service` nuovi (`SERVICE_COUNT` resta 20): il
servizio che possiede le sessioni è `console`, che già possiede i pixel.
`civis::sess` è il client (`Session { id, shm, va }`, `Drop` che smappa a
pagina) e non conosce POSIX né il DE: la capability è la mailbox, non un
numero magico. La sessione 0 ha la sua mailbox **da subito** — nasce col
terminale di boot, prima di `signal_ready`: senza, l'unico ingresso nominato
consegnerebbe uno shm inesistente (trappola trovata dal primo giro di t74,
che rispondeva "attach_default negata"). I gestori rispondono `!0` se la
mailbox manca: mai un id che il client non può mappare.

### 3. Attestazione per pid, non trasferimento di handle

Chi è la sessione di un processo lo dice il **kernel**, non il processo: il
server usa `SYS_PEER_PID`/`SYS_PEER_INFO` sul canale del messaggio e risolve
per pid (`SESS_WHOAMI`). Nessun handle passato allo spawn, nessun
`VELORDO_SESSION` in ambiente, nessun argv da fidarsi, nessuna passeggiata
nell'albero dei parent. `SESS_BIND` esiste per il caso opposto (il DE crea una
sessione per un figlio appena nato: il padre la lega **al** figlio), ed è
gated a classe ≥ `ThirdParty`. `EXIT_NOTIFY` purga binding e sessioni di cui
il morto era owner — mai la 0 — smappando e distruggendo la mailbox
(`munmap` a pagina **prima** di `shm_destroy`, come in D8b).

Policy: `SESS_CREATE` è **aperta anche a `Untrusted`** (una sessione non è un
privilegio: è memoria bounded, 8 al massimo, e serve ai programmi che non
hanno ancora un'identità firmata — l'utente la vede nascere e morire);
l'ingresso nominato `SESS_ATTACH_DEFAULT` è aperto (serve a chi deve parlare
con il terminale principale); `SESS_BIND` e la creazione non legata no.

### 4. `/dev/console` diventa un adattatore sulla sessione 0

Il device non è un secondo canale di testo: è la sessione dello schermo vista
da chi non parla ancora il protocollo di sessione. `DEV_WRITE` →
`feed_session(0, …)`, `CONSOLE_*` → sessione in focus. Il gate non cambia:
login, shell, `runhello`, `hexdump` scrivono come prima, e il contratto
"terminale pieno → re-adattamento → coda del flusso" (t63/t64/t73) continua a
valere perché il modello, lo scrollback e la politica di danno sono gli
stessi — solo spostati di crate.

### 5. Limiti dichiarati di D8c1

- `SESS_FOCUS` accetta **solo la sessione 0**: disegnare N sessioni in N tile
  è D8e (serve la tile con la sua geometria, non solo il modello).
- `SESS_RESIZE` è **negato sulla sessione in focus**: la sua misura è quella
  del display e la decide il riadattamento (D4). Le altre sessioni si
  ridimensionano liberamente (e fanno reflow).
- `WIN_OPEN` con `w0 = 0` significa **"la tile della mia sessione"**, risolta
  per attestazione: in D8c1 è ammessa solo a chi è attaccato alla sessione in
  focus. L'id esplicito della tile resta accettato (t73).
- Il parser ANSI è quello che c'era: niente posizionamento del cursore
  (`CSI H`/`CSI J`), niente tab stops. Sono debiti dichiarati, non scoperte.

## Conseguenze

- `libs/littera` è unit-testabile **sull'host**: 14 test coprono testo,
  capo/scroll, wrap, backspace/`CSI K`, SGR truecolor/256, eco e lettura,
  dump (coda e confini UTF-8), resize, tabella (creazione/chiusura/focus,
  binding/attachment, purge, tetto). `libs/sess-proto` ne ha 3 (layout).
- Il test d'integrazione è **t74** in `usertests`: 13 passi che usano
  l'endpoint come lo userebbe un programma — stats, `whoami` prima e dopo la
  creazione, `WIN_OPEN(0)` negato senza sessione, feed/`state`/`dump`, resize
  con reflow, eco spento (si legge ma non appare) e acceso (appare), `bind`
  negato a `Untrusted`, focus su non-0 negato e `focus(0)` che non cambia un
  pixel (CRC del frame), `close` che riporta a 1/0, `close(0)` negata. Uscita
  a punto unico: il focus torna alla sessione 0 anche sui fallimenti, così il
  login non resta su una tile vuota.
- Prossimi passi dichiarati: il mirror seriale **per sessione** (D8c2: la
  sessione in focus è "nuda", senza prefisso; le altre si leggono con
  `SESS_DUMP`), poi le tile multiple (D8e). Transcript su ArcaFS e telnet
  restano parcheggiati (piano DE §9).
