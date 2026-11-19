# ADR-0082: Eventi, non polling — l'input della tastiera

## Status

**Accepted** (2026-11-19). **E0 ed E1 implementate e nel gate.** E0 = input a
eventi (notify + drain, nessuna modifica al kernel); E1 = `recv` con deadline
(ADR-0083, con una deviazione dichiarata rispetto al piano qui sotto: la
scadenza viaggia su `SYS_RECV` invece di comporre `run([Input, Clock])`). E2
(eventi di rete) è il piano dichiarato, non ancora realizzato.

Dipende da ADR-0008 (IPC per nome), ADR-0009 (IPC asincrona), ADR-0067
(attese in futex) e ADR-0081 (gate end-to-end).

## Context

Il percorso fisico dell'input era già event-driven in basso:

```
IRQ1 → kbd legge 0x60 → KBD_NOTIFY (async) → porta (dorme in recv)
     → decode → coda `input` → DEV_READ via cardo → client
```

Il polling stava **nell'ultimo salto**: `porta` rispondeva subito a
`/dev/input/keyboard` (`0` byte = vuoto), quindi il lettore non aveva modo di
sapere *quando* arrivava un tasto e si svegliava a ogni tick:

| File | Attesa (prima) | Nota |
|---|---|---|
| `userland/interpres` (`LineEditor::read`, `read_line_from_keyboard`) | `futex_wait` a 1 tick + `recv_poll` | il futex word è locale e nessuno lo sveglia: è un timer |
| `userland/identis-login` (`read_line_from_keyboard`) | `futex_wait` a 1 tick + `recv_poll` | idem |
| `userland/create-user` (`read_line_from_keyboard`) | **`spin_loop()` nudo** | busy-wait al 100%, contro ADR-0067 (la guardia CPU del gate copriva solo questo e con soglia 50%) |

Sul totale di 57 `futex_wait` del progetto, la quasi totalità è
`sleep 1 tick e riprova`: non è sincronizzazione, è un polling con le
sembianze di un'attesa. La diagnosi corretta non è quindi "async al posto di
futex" — la sintassi non c'entra. È **eventi al posto del polling**: serve
che il produttore *spinga* una notifica, e che il consumatore abbia una sola
attesa su cui dormire.

## Decision

### E0 — input come eventi (nessuna modifica al kernel)

Tre tag nuovi in `syscall-numbers` (riesportati da `civis`):

| Tag | Direzione | Semantica |
|---|---|---|
| `INPUT_ATTACH` (0x46) | client → porta | sync, reply 0: "voglio i tasti". Il client ha già un canale da `service_lookup(Porta)` |
| `INPUT_DETACH` (0x47) | client → porta | sync: rinuncia al focus |
| `INPUT_NOTIFY` (0x48) | porta → focus | **async, fire-and-forget**: `w0` = byte in coda. È un hint |

1. **Focus LIFO in `porta`.** La lista dei sottoscrittori è ordinata per
   attach: l'ultimo attaccato riceve i tasti; `EXIT_NOTIFY` del titolare (o un
   `send_async` fallito) lo pota e il focus torna al precedente. È il primo
   mattone di "chi ha la tastiera" nel DE.
2. **notify = hint, drain = verità.** Il notify non trasporta i byte: dopo
   ogni notify il client drena `/dev/input/keyboard` finché `read` non
   ritorna 0. Così un notify perso (o ridondante) non perde tasti, e sotto
   burst la coda async da 8 slot non è nel percorso dati. È lo stesso pattern
   che `porta` usa già con `kbd` (`KBD_NOTIFY` + pump).
3. **Client: `civis::input`.** `attach()` (idempotente, con retry a 1 tick
   solo durante l'handshake/riavvio di `porta`), `read()` (un `read_fs`, mai
   bloccante), `wait()` (blocca in `recv()`; su morte del servizio fa **una**
   riattaccata, poi `false` = uscita loud). Nessun `futex_wait`, nessun
   `spin_loop` nei loop di input dei tre lettori.
4. **Niente modifiche al kernel, nessun `select`/`epoll`/eventfd, nessuna
   reply differita.** Il modello resta "un `recv` per processo", gli eventi
   sono messaggi, la superficie del kernel non cresce.
5. **La shell POSIX resta sul path a polling** (limite dichiarato: è la
   personalità di compatibilità, fuori da E0).

### E1 — `recv` con deadline (kernel)

`SYS_RECV` (17) blocca senza scadenza; `SYS_FUTEX_WAIT` (56) ha la deadline
ma non lo sveglia un IPC. L'executor `civis::task` può quindi attendere **solo**
IPC (per questo è reply-only). E1 (**fatta**, ADR-0083):

- kernel: `ipc_recv_until(deadline)` + campo `recv_deadline` nel PCB, sweep in
  `on_tick` accanto a quello dei futex; timeout = `-2`; **la coda vince sul
  timeout**; arg1 di `SYS_RECV` (0 = mai, compatibile con tutti i chiamanti);
- `civis`: `recv_timeout`, `task::Sleep`, `block_on`/`run` che armano la
  deadline minima fra i task, `input::wait_until`;
- gate: `t71 recv con deadline` (scade; un messaggio in gara non si perde),
  `t72 Sleep + Join` (usertests 66 → 68) + 33° check della shell (l'ora del
  prompt avanza da ferma);
- vittoria: la shell tiene l'orologio aggiornato *mentre* attende input, con
  **una** attesa e zero risvegli a vuoto (deviazione dal piano originale:
  la deadline sta sulla stessa `recv` dell'input, non in `run([Input, Clock])`
  — vedi ADR-0083 §Alternatives).

### E2 — eventi di rete (dove async paga sul serio)

- `net`: canale owner per endpoint + `NET_EP_EVENT` coalescente su
  enqueue/accept/fin/rst;
- `civis`: `Endpoint::wait(deadline)`; `net::Stream` componibile
  nell'executor;
- migrazione: `netecho` come prova (listener + echo come due task), poi
  `dns`/`siit`/`broker`;
- vittoria: `run-net-tests.sh` PASS con la sola API a eventi;
  `rg futex_wait userland/{net,dns,siit,netbroker}` → 0.

## Consequences

### Positive

- **CPU al prompt misurata: 1,0%** sul prompt principale della shell e
  **2,0%** sul prompt di `create-user` (gate KVM, `/proc/<qemu>/stat`), contro
  il busy-wait nudo di `create-user` e il risveglio a 100 Hz degli altri due.
  Due check nuovi nel gate: prompt principale e `create-user`, entrambi
  `< 5%` (prima: uno solo, soglia 50%).
- Latenza input = un IPC (nessun tick di ritardo); la digitazione veloce non
  perde tasti (notify coalescente + drain).
- Spariti `wait_input_tick`, `INPUT_FUTEX` e lo `spin_loop()` di
  `create-user`: `rg 'futex_wait|spin_loop\(\)' userland/{interpres,identis-login,create-user}/src`
  → 0 nei loop di input.
- Focus LIFO già pronto per il DE e per i programmi foreground: chi muore
  lascia la tastiera al precedente senza codice nel client.
- Il gate copre l'input end-to-end: se l'attach si rompe, login o i check
  della shell falliscono subito.

### Negative

- Un client che non legge e non si stacca lascia i byte in `porta` (cap
  256 B): è una scelta (nessun tasto perso al cambio di focus), ma il device
  ha un buffer, non è più "leggi e basta". Con il DE il buffer diventa parte
  del contratto.
- La coda async dei messaggi è 8 slot: in teoria 8 notify non letti e poi un
  nono perso. Non è un problema pratico (il primo notify sveglia e il drain
  svuota tutto), ma è un invariante da ricordare: **mai** mettere dati nel
  notify.
- `porta` è un single point: se muore, i tre lettori fanno una riattaccata e
  poi escono loud (init risupervisiona `porta`; il login viene riavviato con
  backoff).
- La shell POSIX continua a pollare (debito dichiarato, non nascosto).

### Neutral

- Nessun syscall/ABI nuovo; il kernel è intatto. Tre tag IPC nuovi.
- `Service::Porta` ora è risolto per nome anche dai client (prima solo per la
  supervisione di init).
- `usertests` invariato (66/66): E0 non tocca il meccanismo.

## Alternatives Considered

- **Payload nel messaggio** (`INPUT_NOTIFY` con i byte in `w0`/`w1` o in un
  ring): più semplice da usare, ma la coda da 8 slot diventa il percorso dati
  → tasti persi sotto burst. Scartato: il notify è un hint.
- **Reply differita di `DEV_READ`** (la read resta appesa finché non arriva un
  tasto): bella latenza, ma un driver che blocca cardo mentre serve un altro
  cliente è esattamente il deadlock chiuso in Fase 15; e servirebbe stato
  "read pendente" per client in cardo/porta.
- **`select`/`epoll`/eventfd nel kernel**: superficie nuova per un problema
  che si risolve con i messaggi che ci sono già.
- **Attesa a futex con wake da `porta`** (`futex_wake` sul word del client):
  avrebbe richiesto di conoscere l'indirizzo del waiter (per-address, non
  per-processo) e resta un timer con un campanello — il modello a eventi è
  più semplice e già disponibile.
- **`async`/`await` nel linguaggio della shell**: fuori scope. Il REPL non ha
  nemmeno funzioni utente (ADR-0065): `read_key()` bloccante è il passo
  naturale, `await` negli script è un progetto di linguaggio a sé.

## References

- Implementazione: `syscall-numbers/src/lib.rs` (tag), `libs/civis/src/input.rs`,
  `userland/porta/src/main.rs`, `userland/{interpres,identis-login,create-user}`.
- `scripts/test-shell-native.py` (check CPU prompt principale e
  `create-user`), `docs/src/11-testing.md`, `docs/src/07-ipc.md`.
- ADR-0008/0009 (IPC), ADR-0067 (attese senza busy-wait), ADR-0081 (gate).
