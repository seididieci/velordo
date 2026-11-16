# ADR-0076 — Socket POSIX sopra `civis::net` (N7.1)

- **Stato**: Accepted
- **Data**: 2026-11
- **Fase**: N7.1
- **Contesto**: ADR-0041 (meccanismo/personalità), ADR-0048 (niente POSIX nel
  meccanismo), ADR-0069 (endpoint a capability del servizio `net`), ADR-0074
  (client di rete nel meccanismo), ADR-0067 (attese idle con `futex_wait`)

## Problema

Dopo N7.0 il **meccanismo** ha un client di rete unico
(`civis::net::{Endpoint, Stream}`), ma la **personalità** POSIX non espone
nulla: un programma della personalità che vuole parlare in rete deve linkare
`civis::net` direttamente, cioè usare il vocabolario del meccanismo. Il passo
dichiarato dopo N7.0 è N7.1: dare ai programmi POSIX le primitive che si
aspettano (`socket`, `connect`, `send`, `recv`, ...).

Il vincolo è ADR-0048: nessuna syscall nasce per POSIX, il kernel resta
neutro. Quindi le socket sono **pura traduzione** al bordo, sopra il client
che esiste già.

## Decisione

### 1. Le socket sono una tabella di fd dedicata (opzione b)

Oggi `flavours/posix/libr` non ha una fd table: i soli fd sono `STDIO[3]` e i
wrapper `read_fs`/`write_fs` di `civis`. Introdurre una tabella fd unificata
(file + socket, 0/1/2 inclusi) avrebbe richiesto di rifare insieme stdio,
redirect della shell e ogni programma della personalità — una fase dentro la
fase.

Decisione: i fd **`64 .. 64+NSOCK`** (`NSOCK = 8`) sono riservati alle socket,
con una tabella statica in `libr::sock`; i fd sotto 64 restano ai file. Il
numero è arbitrario ma **dichiarato**: chi vede un fd ≥ 64 sa che è una socket.

Debito dichiarato (follow-up, non di questa fase): **tabella fd unificata**
con `read`/`write` che dispatchano per range, così che un programma POSIX possa
trattare file e socket allo stesso modo.

### 2. Solo `AF_INET6`; `AF_INET` → `EAFNOSUPPORT`

Il servizio `net` parla IPv6 nativo; IPv4 esiste solo attraverso il traduttore
`siit` (N4) e ha un altro formato di slot (`Layout::IPV4`). Esporre `AF_INET`
significherebbe scegliere in `libr` fra due trasporti e due codec. Non serve
per la vittoria dichiarata: rifiutare con `EAFNOSUPPORT` è onesto e
fail-closed. Il percorso IPv4 (delega a `siit`) resta un follow-up.

### 3. API e semantica

| Funzione | Traduzione |
|---|---|
| `socket(domain, type, protocol)` | valida `AF_INET6` + `SOCK_DGRAM`/`SOCK_STREAM`, apre un `Endpoint` (`Flow::Ephemeral` per UDP, `Flow::Connect` per TCP) e ritorna `64+slot` |
| `connect(fd, addr, addrlen)` | memorizza la destinazione nel socket; **non** attende: su TCP l'handshake è del core (N6.1c), al primo invio |
| `send`/`sendto` | `Endpoint::send` verso la destinazione (esplicita o memorizzata) |
| `recv`/`recvfrom` | attesa bloccante a deadline, poi `Endpoint::recv` |
| `close(fd)` | `Endpoint::close` (chiude l'endpoint e smappa il ring), libera lo slot |

Convenzione d'errore: **`-errno`** (negativo), non un `errno` globale — nel
meccanismo non esiste, e inventarne uno qui sarebbe vocabolario POSIX fuori
dal bordo. L'unica traduzione nativo→errno resta `posix::to_errno`.

Dettagli che valgono la pena di essere espliciti:

- **Backpressure ≠ errore**: `Endpoint::send` che ritorna `Full` diventa
  `-EAGAIN` (il chiamante ritenta), mai un drop silenzioso. È l'invariante di
  N7.0 portato al bordo.
- **TCP: gli eventi di controllo non sono dati**. Il ring RX consegna prima
  `SynAck` (len 0) e poi `Data`; `recv` deve scartare `SynAck`/`Accept`/`Other`
  e continuare ad aspettare, altrimenti il primo `recv` "legge 0 byte" e un
  programma POSIX lo interpreta come EOF. `Fin` → `0` (EOF), `Rst` →
  `-ECONNRESET`.
- **Attese idle**: `futex_wait` a 1 tick (ADR-0067), mai spin puro; deadline
  500 tick (5 s) → `-ETIMEDOUT`.

### 4. Il meccanismo resta intatto

Nessuna syscall, nessun tag IPC, nessuna modifica al kernel o a
`userland/net`: `libr::sock` è solo un consumatore di `civis::net`. Il servizio
`net` non sa che esiste una personalità POSIX.

## Vittoria dichiarata

Il probe `testland/posixsock` (spawnato da init **solo con `net=1`**, dopo
`dnsprobe`) apre le socket con le **sole** API POSIX e fa echo:

- UDP verso `netecho:7` con `sendto` + `recvfrom`;
- TCP verso `netecho:8` con `connect` + `send` + `recv`.

Stampa `[posixsock] udp PASS`, `[posixsock] tcp PASS`,
`[posixsock] risultato: PASS`; `scripts/run-net-tests.sh` lo richiede. Il
binario è seedato anche in `usr/bin/posixsock.bin`, quindi la shell nativa può
lanciarlo a mano (`posixsock udp ::1 7 ciao`).

Il gate resta invariato (6 righe PASS, zero FAIL/PANIC/FAULT): i test di rete
sono fuori gate perché dipendono da `passt`.

## Conseguenze

- **Positive**: un programma della personalità ha la rete con il vocabolario
  che si aspetta, senza vedere `civis::net`; il percorso
  personalità → meccanismo → servizio → core è provato end-to-end da un probe
  automatico; zero debito sul kernel.
- **Negative / debito dichiarato**: fd disgiunti (non una fd table vera);
  `read`/`write` su un fd socket non funzionano (si usano `send`/`recv`);
  `AF_INET` assente; niente `bind`/`listen`/`accept` lato programma (il ruolo
  server resta al servizio `net` + `netecho`); nessuna `select`/`poll`.
- **Non-goal**: risoluzione dei nomi dentro `libr` (il client DNS è
  `userland/dns-resolve`, ADR-0075), TLS (è `libs/tls`), IPv4.
