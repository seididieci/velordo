# ADR-0084: Eventi di rete — il client dorme sull'hint di `net`

## Status

**Accepted** (2026-11-19). **E2 implementata e verificata** (`run-net-tests.sh`
PASS con la sola API a eventi, DoT/TLS invariati). Terzo passo del piano di
ADR-0082 (dopo E0 input, ADR-0082, ed E1 `recv` con deadline, ADR-0083).

Dipende da ADR-0008/0009 (IPC), ADR-0067 (attese senza busy-wait), ADR-0069
(stack di rete), ADR-0074 (client di rete nel meccanismo), ADR-0080 (broker),
ADR-0083 (E1: `recv_timeout`), ADR-0082 (E0: hint + drain).

## Context

Dopo E1 il meccanismo sa attendere "un messaggio **oppure** il tempo", ma i
client di rete non lo usavano: lo stack era a **polling a tick** (100 Hz) in
ogni punto in cui si aspettava un dato.

| Dove | Prima | Costo |
|---|---|---|
| `civis::net::Stream::read`/`write` | `wait_tick()` = futex a 1 tick | un risveglio ogni 10 ms per tutta la durata di un trasferimento TLS (`dns` DoT, `tlsprobe`) |
| `userland/netecho` | `poll` dei ring + futex a 1 tick | un risveglio ogni 10 ms su un echo server |
| `userland/dns` (loop di servizio) | `recv_poll` + futex a 1 tick | idem, per sempre |
| `userland/dns` (inoltro upstream) | futex a 1 tick mentre aspetta la risposta | idem, per query |
| `userland/siit` (loop) | `recv_poll` + drain + futex a 1 tick | idem |
| `userland/netbroker` (loop) | `recv_poll` + drain + futex a 1 tick | idem |
| `libr::sock::recvfrom` (POSIX) | futex a 1 tick | idem, sul percorso delle socket |
| `userland/net` (loop di servizio) | futex a 1 tick in coda al giro | il servizio non reagiva a una richiesta prima del tick |

Il difetto era lo stesso di E0: il produttore (il servizio `net`) **sapeva**
che c'erano dati nuovi, ma non lo diceva. Anzi: lo diceva già, male — usava
`send_async(peer_chan, NET_EP_TX, ep_id, 0)`, cioè un **tag di richiesta**
riusato come notifica, con un payload che nessun client leggeva.

## Decision

### 1. `NET_EP_EVENT` (0x8C): hint coalescente dal servizio al proprietario

`net` manda al proprietario dell'endpoint, **fire-and-forget**, un messaggio
`NET_EP_EVENT {ep_id}` (`w0` = endpoint id, `w1` = 1) quando scrive slot nuovi
nel suo ring RX: dati, `EV_ACCEPT`, `EV_FIN`, `EV_RST`, `EV_ABORT`. Un solo
helper (`notify`) sostituisce i quattro punti che prima riusavano
`NET_EP_TX`.

Il messaggio **non trasporta dati**: il ring resta la verità. Un hint perso
(coda piena) non perde niente, perché il client controlla il ring prima di
dormire; un hint in più è un giro a vuoto che si esaurisce subito.

### 2. `civis::net::wait` — l'attesa unica del client di rete

```rust
pub enum Wait { Event, Timeout, Dead }        // stesso vocabolario di input::Wait (E1)
pub fn wait(chan: u64, deadline: u64) -> Wait
pub fn pending(eps: &[&Endpoint]) -> bool
impl Endpoint { fn pending(&self) -> bool; fn wait(&self, deadline: u64) -> Wait }
```

`Endpoint::wait` = `pending()` (ring) **oppure** `wait(chan, deadline)` =
`recv_timeout` (E1). Le tre proprietà che rendono sicuro dormire:

1. **`pending()` prima di dormire**: un hint arrivato mentre il processo era
   occupato (o perso a coda piena) non deve far dormire chi ha già dati.
2. **`Event` = "c'è qualcosa da fare"**, non solo "hint di rete". Un messaggio
   di altro tipo (es. la richiesta di un cliente arrivata mentre un resolver
   aspetta l'upstream) finisce nello **stash** di `ipc` e `recv`/`recv_poll`
   lo restituiscono: mai perso, e il risveglio è immediato — senza, un
   servizio che aspetta un hint mentre ha una richiesta in coda non la
   servirebbe mai (deadlock provato: il broker non rispondeva più a
   `netudp`).
3. **`Dead`** solo su `EXIT_NOTIFY` **del canale di `net`**: la morte di un
   altro peer è un messaggio da mettere da parte, non la fine del mondo.

Lo stash è un `Vec<IpcMsg>` in `civis::ipc` (processo singolo: niente lock,
come il modello "un `recv` per processo"); `recv_timeout` **non** lo consulta,
così un'attesa selettiva non si ri-mangia il messaggio che ha appena messo da
parte (niente ping-pong).

### 3. La trappola della reply implicita (trovata a runtime)

La reply è implicita al messaggio corrente (ADR-0008): `pop_msg` fissa
`reply_chan`/`reply_req` **a ogni** messaggio letto. Se un servizio legge un
hint **mentre ha una reply in sospeso**, il target della reply cambia: il
client che aspetta resta appeso per sempre e il server riceve una reply
spaiata. È esattamente ciò che è successo a `dns` (attesa upstream con la
richiesta del probe ancora da confermare).

Regola (invariante, non solo fix): **non si dorme con una reply in sospeso —
si conferma prima (ack), si pubblica dopo nel ring**. Il client di `dns` non
aspetta i dati nella reply (li legge dal ring, `ask` fa polling), quindi
confermare all'inizio dell'handler è anche il contratto giusto per una query
lenta (DoT: handshake + TLS). Il kernel resta intatto: non sa cosa sia un
hint, e non deve.

### 4. Migrazioni

- `Stream::read`/`write`: `wait_tick()` → `ep.wait(deadline)`. Sparisce
  `static FUTEX` da `civis::net`.
- `netecho`: loop unico — drena tutto, poi se nessun endpoint (udp, listener,
  figli) ha slot dorme su `net::wait(net_chan, 0)`; `Dead` = uscita loud.
- `dns`, `siit`, `netbroker`: loop di servizio **bloccante** (`recv`), hint
  filtrati (`NET_EP_EVENT` non è una richiesta: non si risponde), drain dei
  ring a ogni risveglio e **controllo del ring prima di dormire**.
- `dns` inoltro upstream: `up.wait(deadline)` → `Timeout` = deadline
  dell'upstream, `Dead` = servizio morto.
- `net` (servizio): il giro non finisce più con un futex; **apre** con
  `recv_timeout(prossimo tick)`, così una richiesta di un client sveglia il
  servizio subito invece di aspettare il tick (prima fino a 10 ms).
- `libr::sock::recvfrom` (personalità POSIX): `ep.wait(deadline)` invece del
  futex a tick — anche le socket dormono.

Restano a tick i **retry di boot** (`service_lookup` prima che il servizio sia
registrato): attese senza un produttore che notifica, esattamente quelle che
ADR-0067 ammette. Il servizio `net` resta periodico (timer TCP/NDP + poll del
ring del driver): il driver **non** gli manda un evento di RX — risvegliarlo
sull'IRQ del device è il passo dichiarato successivo (N8), fuori da E2.

## Consequences

### Positive

- **Zero risvegli a tick** nell'attesa di rete: `rg 'futex_wait'
  libs/civis/src/net libs/tls-net/src` → 0 (gate strutturale in
  `run-net-tests.sh`); nei servizi restano solo i retry di boot.
- Latenza migliore: un dato che arriva sveglia il lettore **subito**
  (prima: fino a un tick); una richiesta sveglia `net` subito.
- `run-net-tests.sh` **PASS** con la sola API a eventi — echo reply, UDP echo
  via broker, TCP handshake+echo, SIIT, DNS (statico + upstream), socket
  POSIX. DoT invariato (`dot: PASS` ×2 + `dotfail: PASS` in
  `run-tls-tests.sh`).
- Il vocabolario è uno solo: `Wait::{Event, Timeout, Dead}` di E1
  (`input::wait_until`) e di E2 (`net::wait`) hanno la stessa semantica, la
  stessa primitiva sotto (`recv_timeout`) e la stessa regola (hint + drain).
- Il kernel non cambia: un tag nuovo, nessuna syscall, nessun permesso.

### Negative

- **La reply implicita vincola i servizi**: chi dorme con una reply in
  sospeso la perde. L'ack-before-work è una convenzione, non un controllo del
  tipo: se un servizio futuro sbaglia, il sintomo è un client appeso (e una
  reply spaiata). Un `reply_to`/`SYS_REPLY_CHAN` esplicito è il rimedio
  strutturale, dichiarato come follow-up.
- Lo stash in `civis::ipc` è memoria per processo che cresce se
  l'applicazione non legge mai (in pratica: bounded dalla coda del kernel,
  8 slot).
- `NET_EP_EVENT` è coalescente: il conteggio dei byte non ci viaggia. Chi ha
  bisogno di sapere *quanto* legge il ring (ed è già così).
- Il servizio `net` resta a 100 Hz per timer e driver-poll: il guadagno di
  CPU su quel processo arriverà con N8 (evento di RX dal driver).

### Neutral

- Un tag IPC nuovo (0x8C) e un helper (`notify`) nel servizio; i tag
  `NET_EP_*` restano quelli.
- Nessuna modifica all'ABI, al kernel o ai permessi: la classe `Untrusted`
  continua a essere filtrata da `net`/`siit` come prima (i probe restano sul
  broker).

## Alternatives Considered

- **Solo `pending()` senza notifica** (il client dorme a tick corto): è il
  polling che c'era; scartato.
- **Hint con i dati nel messaggio** (payload nel messaggio o scambio di
  indici): la coda da 8 slot diventerebbe il percorso dati → si perdono
  pacchetti sotto burst, proprio ciò che il ring esiste per evitare.
- **Un canale dedicato per gli eventi**: il kernel crea canali solo per
  `spawn` e `service_lookup`; servirebbe una syscall nuova per un problema
  che l'hint risolve.
- **`reply_to` esplicito nella reply** (kernel): la soluzione strutturale al
  punto 3, ma è una syscall/ABI nuova — rimandata, dichiarata.
- **`net::wait` che scarta i messaggi estranei**: perderebbe le richieste dei
  client (deadlock reale osservato); scartato.
- **`net::wait` che resta addormentato dopo aver messo da parte un messaggio
  estraneo**: era la prima versione, ed è il deadlock del broker osservato in
  prova; scartato.

## References

- Implementazione: `syscall-numbers/src/lib.rs` (`NET_EP_EVENT`),
  `userland/net/src/main.rs` (`notify`, loop d'ingresso su `recv_timeout`),
  `libs/civis/src/net/{mod,endpoint,stream}.rs` (`Wait`, `pending`, `wait`),
  `libs/civis/src/ipc.rs` (stash), `userland/{netecho,dns,siit,netbroker}`,
  `flavours/posix/libr/src/sock.rs`.
- Test: `scripts/run-net-tests.sh` (suite di rete + gate strutturale "zero
  `futex_wait` in `civis::net`/`tls-net`"), `scripts/run-tls-tests.sh` (DoT).
- ADR-0067 (attese), ADR-0069/0074 (stack e client), ADR-0082 (E0),
  ADR-0083 (E1).
