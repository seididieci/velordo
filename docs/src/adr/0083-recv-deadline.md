# ADR-0083: `recv` con deadline — un messaggio *oppure* il tempo

## Status

**Accepted** (2026-11-19). **E1 implementata e nel gate** (usertests 68/68,
shell nativa 33/33). Segue E0 (ADR-0082) e ne realizza il secondo passo; E2
(eventi di rete) resta il piano dichiarato, non ancora realizzato.

Dipende da ADR-0008 (IPC per nome), ADR-0009 (IPC asincrona), ADR-0019
(executor), ADR-0067 (attese senza busy-wait), ADR-0081 (gate end-to-end) e
ADR-0082 (E0: input come eventi).

## Context

Dopo E0 la shell dorme su **una sola** attesa: `civis::input::wait()` →
`recv()` sul canale di `porta`. È il modello giusto (un `recv` per processo,
il notify è un hint, il drain è la verità), ma ha un buco: **quell'attesa non
ha una scadenza**. Chi dorme in `recv` non ha modo di sapere che è passato un
secondo, quindi l'orologio del prompt resta fermo finché non arriva un tasto.

Le due primitive disponibili erano entrambe monche:

| Primitive | Sveglia su IPC | Scadenza |
|---|---|---|
| `SYS_RECV` (17) | **sì** | **no** |
| `SYS_FUTEX_WAIT` (56) | no | sì |

Da qui i vincoli che il progetto già si era dato: niente `select`/`epoll`/
eventfd (superficie nuova, ADR-0082), niente busy-wait né risvegli a tick
(ADR-0067: CPU al prompt < 5%, misurata 1,0%), e l'executor `civis::task` può
attendere **solo** IPC (per questo è reply-only). Il difetto non è "manca
`async`": è che manca **una** attesa che sappia dire "un messaggio *oppure* il
tempo".

## Decision

### 1. Kernel: la deadline viaggia in `SYS_RECV`

`SYS_RECV` (17) prende `arg1` = **tick assoluto** di scadenza (`0` = mai, che è
esattamente il comportamento di tutti i chiamanti precedenti: nessuna
migrazione, nessun flag).

- **Timeout = `rax = -2`**, nessun messaggio consumato: il chiamante può
  riprovare senza ambiguità. `-1` resta "errore / nessun peer".
- **La coda vince sul timeout.** Un messaggio già arrivato si consegna anche
  se il tick è passato mentre lo schedulavamo. Il caso non è teorico: il
  sweep e l'arrivo di un tasto possono cadere nello stesso tick. Mai un
  evento perso per un timer.
- **La scadenza vive nel PCB** (`Process::recv_deadline: u64`), non in una
  tabella: muore col processo, nessuno slot orfano da ripulire, nessun tetto
  artificiale di waiter. Lo sweep (`tick_recv_deadlines`) gira in `on_tick`
  a lock tenuto, **dopo** quello dei futex, e tocca solo chi è *ancora*
  `Blocked` in `recv` con una deadline (chi è stato svegliato da un messaggio
  è già `Ready`: la coda vince anche lì).
- La deadline si azzera in **tre** punti: alla consegna del messaggio
  (`ipc_recv_until` e i due wake di `ipc_send`/`ipc_send_async`), al timeout,
  e quando la syscall riesamina il caso "nessun altro runnable".

Il meccanismo riusa l'anti-deadlock esistente di `pick_next()`: se non c'è
nessun altro runnable il processo resta `Ready` e la syscall ritorna `-1`
invece di dormire. Per questo `recv_timeout` **controlla l'orologio prima di
bloccarsi**: con il tempo già scaduto chiude subito con `Timeout` invece di
finire nel ramo d'errore (un timer non deve mai trasformarsi in un hang).

### 2. Meccanismo: `recv_timeout`, `Sleep`, deadline minima

- `civis::ipc::RecvError { Timeout, Dead }` e `recv_timeout(deadline)`:
  l'attesa **unica** del meccanismo. `Timeout` = la coda non è stata toccata;
  `Dead` = nessun peer/scheduler non pronto (come prima).
- `task::Receivable::deadline()` con default `0`: i task esistenti
  (`WaitReply`, `FsRead`, `Join`) non cambiano, e chi ha un timer lo dichiara.
- `task::Sleep` (`until`/`ticks`/`remaining`) è il timer: `Future` + un
  `deadline()` che vale `0` quando è scaduto (scaduto = pronto al prossimo
  poll, mai un `Pending` con deadline passata: sarebbe un busy-loop).
- `block_on`/`run` dormono su `recv_timeout(min(deadline dei task pending))`;
  `Join` propaga la **minima dei figli non finiti**. Un task può quindi
  attendere una reply *e* una scadenza con **una sola** attesa bloccante: il
  processo resta `Blocked` e lo sveglia il PIT, zero risvegli a vuoto.

### 3. Input: `wait_until` (evento *oppure* tempo)

`civis::input::wait_until(deadline) -> Wait { Event, Timeout, Dead }` è
`wait()` con la scadenza. `wait()` diventa `matches!(wait_until(0), Wait::Event)`:
un solo percorso, nessuna copia della logica di riattacco su morte di `porta`.
Su `RecvError::Dead` con il tempo già scaduto chiude con `Timeout` (stesso
motivo dell'anti-deadlock sopra): la shell non deve morire perché il timer è
scattato nel momento sbagliato.

### 4. Vittoria: l'orologio del prompt si muove mentre la shell dorme

Il prompt mostra `HH:MM:SS`. A ogni `read()` che ritorna `0` la shell arma
`wait_until(prossimo_confine_di_secondo)`; al `Timeout` ridisegna **solo le
8 cifre**, in-place:

```
ESC[D ×13  +  colore  +  HH:MM:SS  +  reset  +  ESC[C ×5
```

Tredici indietro (8 cifre dell'ora + 5 del suffisso) e cinque avanti: **saldo
zero** delle celle, il cursore torna esattamente dov'era. Il terminale deve
muoversi di una cella e lasciare intatto il suffisso; scrivere 8 cifre partendo
da 5 celle indietro (saldo +8) fa **crescere la riga di 8 celle al secondo e
cancella il suffisso** — l'errore del primo giro, visibile subito a schermo
(`10:40:2910:40:3010:40:31…`) e ora coperto dalla forma del check di gate.

Le cinque celle del suffisso sono `" " ✓|✗ " " ">" " "`, con entrambi i glifi
di stato larghi **una** cella (guardato da un unit test in `libs/term`). Il
console server muove di **una** cella per `ESC[D`/`ESC[C` (ignora i parametri
CSI), quindi il movimento è ripetuto, non `ESC[13D`.

Vincoli scelti (tutti per non rompere ciò che il gate misura):

- **il prompt non si ridisegna**: il gate conta i `" > "` nel tratto digitato
  (1–2 per riga, "prompt una volta per riga"); un repaint del prompt intero
  aggiungerebbe occorrenze;
- solo con **buffer vuoto** e fuori continuazione: è lì che il cursore è
  subito dopo il prompt, e solo lì l'ora è visibile senza ambiguità;
- **nessuna scrittura se le cifre non cambiano** (confronto con l'ultima ora
  disegnata): a 100 Hz il risveglio è al secondo, non al tick.

**Difetto trovato e chiuso per strada**: `get_time_str()` restituiva 9 byte
(`"HH:MM:SS\0"`) perché `from_utf8` prendeva il buffer intero — il NUL finiva
come cella dopo l'ora nel prompt. Ora sono esattamente 8 byte (è anche la
lunghezza su cui il repaint fa i conti).

### 5. `useruptime` sul timer dell'executor

Il vecchio loop `futex_wait` a deadline (un timer con le sembianze di un
futex) diventa `block_on(Sleep::until(..))`: stesso costo (processo `Blocked`,
PIT che sveglia), ma è l'**uso reale** del timer dell'executor in produzione e
nel gate — se `Sleep` o la deadline minima si rompono, si vede subito.

## Consequences

### Positive

- **Una sola attesa** per "messaggio o tempo": niente `select`, niente
  eventfd, nessuna reply differita, nessuna tabella di timer nel kernel.
- L'orologio del prompt avanza **mentre la shell dorme**, senza risvegli a
  tick: CPU al prompt invariata (0,0–1,0% misurata), un risveglio al secondo.
- L'executor acquista i timer senza cambiare dominio (`Sleep` si compone con
  `join`/`run`); `uptime` lo esercita in produzione.
- Compatibilità totale: `arg1 = 0` è il vecchio `recv`, quindi i ~40
  chiamanti esistenti non cambiano una riga.
- Nuovi test nel gate: **t71** (scadenza già passata → `Timeout`; messaggio
  vince su una deadline lontana; timeout a 1 tick senza hang né perdite),
  **t72** (`run` di due `Sleep` a +30/+60 tick → ≥ 60 tick; `block_on` di
  `Sleep::ticks(25)` → ≥ 25). usertests 66 → **68**.
- Il gate copre anche l'orologio: **33° check** della shell nativa (l'ora
  compare e *avanza* al prompt fermo, forma esatta `ESC[D`×13 + ora +
  `ESC[C`×5: un saldo diverso da zero, o un suffisso cancellato, fallisce). Se il
  repaint si rompe, il check fallisce.

### Negative

- `SYS_RECV.arg1` è ora occupato: una futura "recv con flag" dovrà passare da
  `arg2`/`arg3` (nessun conflitto oggi, ma il numero è speso).
- Un processo ha **una** deadline di `recv` alla volta — coerente con "un
  `recv` per processo", ma due attese con scadenze diverse nello stesso
  processo richiedono l'executor (che le fonde nella minima).
- Lo sweep è O(`MAX_PIDS`) per tick (128 confronti su campi del PCB): misurato
  irrilevante, ma non è gratis come il futex (16 slot).
- `-2` è un valore di ritorno nuovo: chi chiama `SYS_RECV` a mano (nessuno
  fuori da `civis`) deve distinguerlo da `-1`.
- Il repaint dell'orologio è una scrittura periodica verso il console server:
  costa un frame di danno al secondo, e il mirror seriale cresce (~60 B/s).

### Neutral

- Nessun numero di syscall nuovo, nessun tag IPC nuovo: `SYS_RECV` cambia
  solo semantica dell'argomento (0 = mai).
- La shell POSIX resta a polling: è la personalità di compatibilità, il debito
  dichiarato in ADR-0082 non cambia.
- Il modello "notify = hint, drain = verità" non è toccato: il timeout non
  trasporta dati, sveglia soltanto.

## Alternatives Considered

- **`run([Input, Clock])`** (il piano scritto in ADR-0082): avrebbe richiesto
  di trasformare il loop di lettura in un `Receivable`, e l'executor
  possiede il `recv` — con un router in mezzo, i byte del device e i messaggi
  di `porta` finirebbero nello stesso canale di smistamento, con il rischio di
  scartare un notify per un task non accettante. La deadline *sulla stessa
  attesa* fa la stessa cosa con zero macchine in più. Deviazione dichiarata.
- **Tabella di timer nel kernel** (come `futex_waiters`): slot orfani da
  ripulire alla morte, tetto artificiale, e un PID riusato che eredita una
  scadenza. Il campo nel PCB non ha nessuno di questi problemi.
- **`futex_wait` + `futex_wake` da `porta`**: il word del futex è per
  *indirizzo*, non per processo: `porta` dovrebbe conoscere l'indirizzo del
  waiter e resterebbe un timer con un campanello. Scartato.
- **`select`/`epoll`/eventfd**: superficie nuova per un problema che i
  messaggi esistenti risolvono.
- **Ridisegnare il prompt intero a ogni secondo**: più semplice, ma rompe il
  contratto del gate ("prompt una volta per riga") e sporca la cronologia del
  terminale.
- **Ridisegnare l'ora senza muovere il cursore** (sovrascrittura con `\r` e
  riscrittura della riga): la riga contiene il comando digitato, che va
  preservato; il movimento di 13+5 celle è la parte minima che tocca solo l'ora
  (e il saldo nullo è ciò che tiene ferma la riga).

## References

- Implementazione kernel: `kernel/src/ordo/sched/ipc.rs`
  (`ipc_recv_until`, `timeout_result`, `tick_recv_deadlines`),
  `kernel/src/ordo/sched/tick.rs` (sweep in `on_tick`),
  `kernel/src/ordo/process.rs` (`recv_deadline`),
  `kernel/src/syscall/{ipc,dispatch}.rs` (`sys_recv(deadline)`),
  `syscall-numbers/src/lib.rs` (contratto di `SYS_RECV`).
- Meccanismo: `libs/civis/src/ipc.rs` (`RecvError`, `recv_timeout`),
  `libs/civis/src/task.rs` (`Receivable::deadline`, `Sleep`, `Join`,
  `block_on`/`run`), `libs/civis/src/input.rs` (`Wait`, `wait_until`).
- Vittoria: `userland/interpres/src/main.rs` (`refresh_clock`,
  `next_clock_deadline`, `get_time_str`), `userland/uptime/src/main.rs`
  (`block_on(Sleep)`), `libs/term/src/font.rs` (larghezza dei glifi di stato).
- Test: `testland/usertests/src/t_time.rs` (t71, t72),
  `scripts/test-shell-native.py` (check orologio), `run-tests.sh`.
- Documentazione: `docs/src/06-syscalls.md`, `docs/src/07-ipc.md`,
  `docs/src/11-testing.md`.
- ADR-0008/0009 (IPC), ADR-0019 (executor), ADR-0067 (attese senza
  busy-wait), ADR-0081 (gate), ADR-0082 (E0).
