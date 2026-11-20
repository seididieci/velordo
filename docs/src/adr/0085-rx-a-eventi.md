# ADR-0085: RX a eventi — il driver sveglia `net`, il servizio dorme sulla prossima scadenza

## Status

**Accepted** (2026-11-20). **N8 implementata e verificata** (`run-net-tests.sh`
PASS, CPU idle 0,0%, gate verde). Passo dichiarato da ADR-0084 (E2): l'ultimo
anello del piano "eventi, non polling" (ADR-0082 input, ADR-0083 `recv` con
deadline, ADR-0084 client di rete).

Dipende da ADR-0008/0009 (IPC), ADR-0069 (stack di rete e driver virtio-net),
ADR-0083 (E1: `recv_timeout`), ADR-0084 (E2: hint + drain).

## Context

Con E2 tutti i **client** di rete dormono su un hint. Il servizio `net` no:
chiudeva il giro con `recv_timeout(get_ticks() + 1)` e a ogni tick faceva tre
cose che quasi sempre non servivano a niente.

| Lavoro per tick | Serve quando |
|---|---|
| `send(driver, NET_POLL)` | il device ha ricevuto frame |
| `send(driver, NET_TX)` | abbiamo scritto frame nel ring TX |
| `on_tick` (RS/echo/aging NDP) + `tcp_poll` (SYN/dati/FIN/retry) | un timer è scaduto o c'è una coda da svuotare |

Il costo non è solo CPU: il servizio è **single-threaded** e ogni giro è un
IPC sincrono verso il driver (`NET_POLL` + `NET_TX`), cioè due round-trip ogni
10 ms per sempre — anche a rete ferma. Inoltre il tick **mascherava** un
difetto: `Conn::rto_expired` scattava anche su una connessione inattiva, e
dopo `MAX_RETRIES` poll la chiudeva. Con il polling a tick il difetto era
invisibile (la connessione veniva chiusa solo se restava ferma ~4 s e
nessuno se ne accorgeva); senza tick sarebbe stato un timeout silenzioso.

## Decision

### 1. `NET_RX_EVENT` (0x8D): il driver avvisa `net`

Il driver `netdev`, quando l'IRQ del device fa passare il ring RX condiviso da
**vuoto a non vuoto**, manda `send_async(net_chan, NET_RX_EVENT, 0, 0)` al
servizio (canale appreso al `NET_ATTACH`). Fire-and-forget, mai reply.

Il messaggio **non trasporta frame**: il ring resta la verità, l'evento è un
campanello. Un hint perso (coda piena) non perde dati — il servizio riprende
al risveglio successivo — e un hint in più è un giro a vuoto che si esaurisce
subito. La stessa regola di E0/E2: **hint + drain**, mai dati nel messaggio.

Il driver drena anche il ring **TX** su IRQ (`Device::tx()`): un frame rimasto
in coda al device per una virtqueue piena non deve dipendere da un kick del
servizio, altrimenti resterebbe fermo fino al traffico successivo.

`NET_POLL` (0x6C) è stato **rimosso** dall'ABI e dal driver nella pulizia E
(era rimasto per retrocompatibilità ma non aveva più chiamanti): la guardia è
il compilatore, non un grep.

### 2. `Net::next_deadline` — il servizio dorme sulla prossima scadenza

```rust
pub fn next_deadline(&self, now: u64) -> Option<u64>
```

Il minimo fra i lavori che hanno una scadenza **assoluta**:

- retry di RS (router non ancora noto) e di echo (link up, ping abilitato);
- aging dei vicini (STALE → NS di verifica, il più tardi fra `last + TTL` e
  `probed + PROBE`);
- per ogni connessione TCP: `rto_deadline` se c'è spazio di sequenza non
  confermato (SYN, dati, FIN) o se è in TIME_WAIT;
- byte **bufferizzati e mai partiti** (handshake non concluso o finestra del
  peer a zero): `now + TCP_UNSENT_RETRY` — non "subito", o una finestra a zero
  farebbe girare il servizio a vuoto.

`None` = non c'è niente in scadenza: il servizio chiama
`recv_timeout(0)` (mai) e si sveglia **solo** su un evento (frame dal driver,
richiesta di un client). `0` è la sentinella di E1, quindi la traduzione è
diretta: `next_deadline(now).unwrap_or(0)`.

### 3. Il giro del servizio

```
1. drain del ring RX del driver            (il ring è la verità)
2. richieste dei client (recv_poll)        (NET_EP_EVENT/NET_RX_EVENT: hint, si scartano)
3. on_tick + tcp_poll                      (timer e code; i segnali si applicano subito)
4. kick NET_TX **solo se** head != tail    (nessun IPC inutile)
5. se il ring RX ha ancora slot → torna a 1 (hint perso, il ring non mente)
6. recv_timeout(next_deadline().unwrap_or(0))
```

Il punto 4 confronta la posizione del **consumatore** (head, scritta dal
driver) con quella del **produttore** (tail, scritta da `net`): non serve
ricordare "ho scritto" fra le iterazioni, e un frame scritto in coda al giro
precedente non resta fermo. Il primo RS (emesso da `link_up` prima del loop)
ha il suo kick esplicito.

### 4. `Conn::rto_expired` conta solo il lavoro vero

```rust
fn retransmittable(&self) -> bool   // snd_nxt != snd_una, escluso TimeWait
fn rto_expired(&self, now) -> bool  // retransmittable() && now >= rto_deadline
```

Prima era `!closed && now >= rto_deadline`: una connessione Established
**ferma** aveva `rto_deadline` nel passato, quindi ogni poll le faceva
crescere `retries` fino a chiuderla. Con il tick era un bug latente; con le
deadline sarebbe stato un timeout silenzioso su ogni connessione inattiva.
TIME_WAIT ha la sua scadenza in `on_tick` e non deve passare da `next_tx`
(che altrimenti la rimanderebbe all'infinito).

## Consequences

### Positive

- **CPU a rete ferma: 0,0%** misurata su QEMU (`run-net-tests.sh` campiona
  `/proc/<pid>/stat` di un boot idle con `net=1`, soglia 5% come E0).
- Due IPC sincroni ogni 10 ms in meno: il servizio non parla col driver se
  non c'è niente da dirgli.
- **Latenza migliore**: un frame che arriva sveglia `net` subito, non al
  tick dopo.
- Il difetto di `rto_expired` è chiuso con due unit test host
  (`tcp_idle_established_non_muore_ai_tick`,
  `next_deadline_segue_i_lavori_in_sospeso`).
- Il tag `NET_POLL` è **rimosso** dall'ABI (E): un riferimento non compila.
  Al suo posto, il gate `[munmap]` (geometria non di pagina = leak shm
  silenzioso) in `run-net-tests.sh` e nel gate principale.

### Negative

- **Un hint perso è un ritardo, non una perdita**: se la coda async è piena
  nel momento esatto dell'IRQ, il frame aspetta il risveglio successivo (una
  richiesta di un client o una deadline TCP). Accettato: la coda ha 8 slot e
  l'alternativa (dati nel messaggio) romperebbe il modello a ring.
- Il driver deve sapere **a chi** mandare l'hint (`net_chan` appreso al
  `NET_ATTACH`): se `net` riparte, il canale si aggiorna al nuovo attach; nel
  frattempo gli hint vanno a un canale morto e `send_async` fallisce (ignorato).
- Il kick TX è un IPC sincrono quando c'è traffico: invariato rispetto a
  prima, ma ora è condizionato invece che fisso.

### Neutral

- Un tag IPC nuovo (0x8D) e uno ritirato (`NET_POLL`, 0x6C), nessuna syscall,
  nessuna modifica al kernel, ai permessi o all'ABI (solo rimozione).
- `libs/net` guadagna due funzioni pubbliche (`next_deadline`,
  `has_unsent_data`) e `tcp_endpoints`; `Conn` guadagna
  `next_deadline`/`has_unsent_data` e perde l'RTO "a tempo".

## Alternatives Considered

- **Continuare a pollare il driver a tick basso (es. ogni 10 tick)**: riduce
  il costo ma non lo elimina, e introduce latenza fino a 100 ms sul primo
  pacchetto. Scartato: il driver sa quando ha ricevuto qualcosa, dirlo è
  gratis.
- **Mettersi in polling sul ring condiviso** (spin su head != tail): costo
  CPU pieno su un servizio single-threaded. Scartato.
- **Mandare i frame nell'evento**: la coda da 8 slot diventerebbe il percorso
  dati e sotto burst si perderebbero pacchetti — proprio ciò che il ring
  esiste per evitare. Scartato.
- **Un canale dedicato driver→`net`**: il kernel crea canali solo per `spawn`
  e `service_lookup`; servirebbe una syscall per un problema che il canale
  già esistente (quello dell'attach) risolve.
- **`next_deadline` come "prossimo tick" quando non c'è nulla**: è il polling
  che si voleva togliere. Scartato.

## References

- Implementazione: `syscall-numbers/src/lib.rs` (`NET_RX_EVENT`),
  `userland/netdev/src/{main,virtio}.rs` (`net_chan`, hint su transizione
  vuoto→non vuoto, `tx()` su IRQ), `userland/net/src/main.rs`
  (`drain_driver_rx`, `run_timers`, `rx_pending`, `tx_head`/`tx_tail`,
  `next_deadline`), `libs/net/src/net.rs` (`next_deadline`, `tcp_endpoints`,
  `has_unsent_data`), `libs/net/src/tcp/conn.rs` (`retransmittable`,
  `rto_expired`, `next_deadline`).
- Test: `scripts/run-net-tests.sh` (suite + CPU idle < 5% + zero `[munmap]`
  malformati), unit test host in `libs/net` (`tcp_idle_established_non_muore_ai_tick`,
  `next_deadline_segue_i_lavori_in_sospeso`).
- ADR-0067 (attese senza busy-wait), ADR-0069 (driver e ring), ADR-0082 (E0),
  ADR-0083 (E1), ADR-0084 (E2).
