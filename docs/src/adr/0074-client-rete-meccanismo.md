# ADR-0074: Client di rete nel meccanismo

## Status

**Accepted** (2026-11). N7.0 implementata: `libs/net-ep` (formato degli slot)
e `civis::net` (endpoint e stream TCP). I client esistenti sono migrati.
Nessuna syscall, tag o ABI nuova. La personalita' POSIX (`socket`) resta N7.1.

## Context

Lo stack di rete funziona (N1-N6.3, ADR-0068/0069/0072), ma il lato **client**
del protocollo a endpoint era copiato in otto posti:

| file | variante |
|---|---|
| `userland/netecho` | ring pieno scartato **in silenzio** |
| `userland/dns` (upstream) | ring pieno -> `None` (query persa) |
| `libs/tls-net::TcpStream` | `tx_seq % 16` + retry `w0 == 0` fino a deadline |
| `testland/net3` | **nessun controllo** di capacita' |
| `testland/netudp` | metadata di 31 byte (errata: funzionava per caso) |
| `testland/net4`, `testland/nettcp` | altre copie con varianti |

Tre politiche di backpressure diverse (drop, errore, retry) e due modelli di
cursore. Le stesse regole erano scritte una volta sola solo nel servizio
`userland/net`; il commento in `libs/tls-net` diceva gia' che il trasporto
"va scritto una volta sola".

Il costo non e' la duplicazione in se': e' che una correzione (es. il bug
`no_data_before_handshake` di N6.2) va replicata in ogni copia, e ogni copia
puo' divergere su cosa significhi "ring pieno".

## Decision

Due livelli, con confini netti:

1. **`libs/net-ep` — formato, puro.** Crate `no_std`, zero dipendenze,
   `#![forbid(unsafe_code)]`. Definisce `Layout` (parametrico sulla lunghezza
   dell'indirizzo: `IPV6` 23 byte per `net`, `IPV4` 11 byte per `siit` e il
   lato client di `dns`), `write`/`read`/`check` di uno slot e il cursore di
   ring (`push_index`, `next`). Lavora su una finestra `&[u8]` che il
   chiamante ha gia' validato: volatile e `fence` restano nei wrapper.
2. **`civis::net` — client del meccanismo.** `Endpoint` (open/attach/send/
   recv/drain/stats/close), `Stream` TCP blocking (connect/read/write/
   `set_timeout`/`abort`/`shutdown`), tipi `Proto`/`Flow`/`Event`/`SendError`/
   `StreamError`, `PAYLOAD_MAX`.

Invarianti, scritti nel crate e validi per tutti i client:

1. **TX**: il cursore autorevole e' locale (`tx_seq`). Il servizio legge
   l'indice dello slot in `NET_EP_TX` (`w1`); i contatori del ring TX del
   servizio sono **consultivi** e non vanno usati per decidere se c'e' posto.
2. `w0 == 0` = payload preso in carico -> il cursore avanza; `w0 == !0` =
   buffer TCP pieno (`SendError::Full`) -> si riscrive lo **stesso** slot;
   errore IPC = `SendError::Dead` (mai attesa infinita).
3. **RX**: la testa avanza solo **dopo** che il payload e' stato copiato nel
   buffer del chiamante. Uno slot con `len` fuori range viene consumato e
   saltato: mai panic, mai frame parziale.
4. Nessun buffer grande sullo stack (userland 16 KiB): il buffer di ricezione
   e' del chiamante, il pending di `Stream` sta sull'heap.
5. `Endpoint::attach` (connessione accettata) **non** azzera i contatori: il
   servizio puo' aver gia' scritto byte nel ring del figlio.
6. `Endpoint::close` e' idempotente (ring nullo), cosi' `Stream::shutdown`
   puo' girare su `&mut`.

`libs/tls-net` resta un **newtype** di 90 righe: la *orphan rule* vieta
`impl tls::Transport for civis::net::Stream` fuori dai due crate, quindi il
tipo locale intermedio e' l'unico modo per esporre il trait.

## Perche' non un solo wrapper per tutti i servizi

`siit` e il lato client di `dns` parlano un layout IPv4 di 11 byte con tag
propri (`SIIT_EP_*`, `DNS_EP_*`): forzarli dentro `Endpoint` (che parla con
`net`) avrebbe significato un'astrazione che astrae un solo caso d'uso reale.
Il formato e' condiviso dal codec; la **politica** (tag, semantica) resta del
servizio.

## Conseguenze

- **Una sola definizione del formato**: client, `net`, `siit` e `dns`
  passano da `net-ep`. Il campo `len` invalido non e' piu' un caso locale.
- **Backpressure uniforme**: chi vuole attendere (`Stream`) ritenta fino alla
  deadline; chi vuole scartare (`netecho`) lo fa esplicitamente su `Full`.
  Nessun drop silenzioso.
- **Bug eliminati per costruzione**: `netudp` usava un metadata di 31 byte
  (incompatibile col servizio) e funzionava solo perche' il servizio legge i
  primi 23 byte e ignora il resto; ora e' il layout vero.
- **Nessun cambiamento di ABI**: syscall, tag, formati su filo e semantica di
  `net` sono invariati. Le suite net/TLS provano che la migrazione e'
  trasparente.
- **Costo**: un crate in piu' (`net-ep`, 6 test host), un `exclude` nel
  workspace root (path dep con workspace proprio), un newtype in `tls-net`.
- **Test**: unit host 117/117 (include `net-ep`), net tests PASS, TLS/DoT
  PASS, gate 6/6 e zero FAIL/PANIC/FAULT.

## Non-obiettivi

- **POSIX `socket`** (N7.1): la personalita' arrivera' sopra `civis::net`;
  qui non entra nessun verbo o tipo POSIX.
- **DoH** (N6.4): resta condizionale, sulla stessa API di trasporto.
- **Nuove syscall**: nessuna. Il kernel resta neutro e fuori dal percorso
  dati.

## Riferimenti

- `docs/src/adr/0069-stack-rete-ipv6-capability.md` (protocollo a endpoint).
- `docs/src/adr/0072-tls-dot-embedded-tls.md` (trasporto TLS, DoT).
- `libs/net-ep/src/lib.rs` (layout e cursore).
- `libs/civis/src/net/{endpoint,stream}.rs` (client del meccanismo).
- `libs/tls-net/src/stream.rs` (newtype `Transport`).
- `userland/net/src/main.rs` (server sul codec).
- `scripts/run-unit-tests.sh`, `scripts/run-net-tests.sh`,
  `scripts/run-tls-tests.sh`.
