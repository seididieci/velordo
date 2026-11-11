# ADR-0069: Stack di rete nativo — IPv6-first, servizi netdev/net, API a capability

## Status

Accepted (2026-11). **N1 implementata** (virtio-net + IPv6/NDP/ICMPv6 +
ping6 con passt). **N2 implementata** (UDP + endpoint a capability + ring,
echo server nativo `netecho`, probe `netudp`). **N3 implementata** (TCP
minimale, sostituito da N6.1). **N4 implementata** (traduttore SIIT/NAT64).
**N5 implementata** (resolver DNS plain). **N6.0 implementata** (inoltro
upstream plain UDP). **N6.1 implementata** (TCP reale: client + listener,
macchina a stati in `libs/net`, `EV_ACCEPT`). N6.2–N8 restano la traiettoria:
N6.2.0 = SSE2 nello userland (ADR-0071, prerequisito), N6.2 = `libs/tls`
adattatore sopra `embedded-tls` 0.19 (ADR-0072), N6.3 = DoT dentro `dns`.
Dipende da ADR-0068 (threat model/invarianti) e ADR-0070 (config `sys:*`,
non ancora implementata: N1 usa una config compilata; `sys:net.conf` per N2
è seedato a build-time con fail-closed).

## Context

Velordo non ha rete. Il kernel è neutro (ADR-0025/0048): niente socket, niente
protocolli, niente syscall di rete. I servizi sono userspace, comunicano via
IPC per nome con reply implicita, hanno diritti per canale e un manifest con
hash verificato allo spawn.

Decisioni prese in fase di analisi:

- **IPv6-first**: il core parla solo IPv6; IPv4 arriva dopo, con un
  traduttore separato.
- **Driver e stack separati**: `netdev` (virtio-net) e `net` (protocolli),
  entrambi userspace.
- **API a capability**, non socket: il client chiede un endpoint con vincoli;
  la policy è per binario.
- **virtio-net** prima, hardware reale dopo; **1 RX + 1 TX**, niente offload.
- **Config** in `sys:net` (ADR-0070), letta read-only; fallback compilato per
  i test.
- **Test IPv6** con `-netdev passt` (QEMU 10.2.2, passt installato, host con
  IPv6 globale), fuori dal gate.
- **Sicurezza**: RA guard, no DAD, IID randomizzato, ping solo test/config,
  frammenti droppati, default-deny, audit.
- **IOMMU-ready** ora, VT-d dopo.

## Decision

### 1. Stratificazione

| Livello | Crate | Ruolo |
|---|---|---|
| Core | `libs/net` | protocolli puri, `no_std`, `#![forbid(unsafe_code)]`, host-testable, fuzzable. Nessun I/O: `input(frame, now) -> Actions`, `output() -> Frame`. |
| Driver | `userland/netdev` | virtio-net; `unsafe` confinato a MMIO/DMA/ring; nessuna logica di protocollo. |
| Servizio | `userland/net` | usa `civis`; possiede la config; risolve il driver; espone endpoint a capability; logga; CBS. |
| Personalità | `flavours/posix` (dopo) | socket POSIX mappate su `net`; lì la PAL (ADR-0047). |

Il core non conosce `civis`, il driver non conosce i protocolli, il servizio
non conosce POSIX.

### 2. IPv6-first

- Il core implementa **solo IPv6** (e ICMPv6/NDP). IPv4 non esiste nel core.
- IPv4 verso il mondo: **traduttore separato** (SIIT RFC 7915 o NAT64
  RFC 6146) in un servizio dedicato, che possiede l'indirizzo IPv4 e traduce.
  Disabilitato di default; policy esplicita.
- Motivazione: una sola rappresentazione interna, un solo parser di
  indirizzi; l'IPv4 (opzioni, frammentazione, broadcast, ARP) resta fuori dal
  TCB del core.

### 3. Servizi e kernel

- `Service::Net = 14`, `Service::NetDev = 15` (slot liberi; `SERVICE_COUNT`
  resta 16). Init li supervisiona; manifest + hash; `SERVICE_POLICY`.
- Il kernel resta neutro: nessuna syscall di rete. **Prerequisiti kernel**:
  - **DMA**: generalizzare l'owner del grant (oggi `Service::Console`) a un
    dominio per servizio; il driver `netdev` ottiene i suoi slot; interfaccia
    `DmaDomain` (identità oggi, VT-d dopo).
  - **MMIO**: generalizzare il grant (oggi `Service::Gpu`) al device di rete
    (finestra UC, non-owned).
  - **MSI-X**: un vettore per la NIC (es. `MSI_VECTOR_NET = 0x51`), routing
    IRQ→driver come per la GPU.
- Il driver non fa I/O FS; il servizio legge `sys:net` (ADR-0070).

### 4. Driver virtio-net

- Device `virtio-net-pci` modern (`1af4:1041` con `disable-legacy=on`),
  `VIRTIO_F_VERSION_1`.
- **1 RX + 1 TX** (single-thread); niente MQ, niente control queue.
- Feature: `MAC` (e `STATUS` se disponibile); **niente** `CSUM`/`GUEST_CSUM`/
  `GSO`/`MRG_RXBUF`/`HASH`/`RSS`.
- Buffer: **2048 byte/slot** (header virtio 12 + frame 1514 + margine);
  **256 descrittori** per coda; MTU **1500**; jumbo dopo.
- Il driver valida il used ring (`id` nel range, `len` ≤ buffer, niente
  duplicati); lo stack valida il pacchetto.
- IRQ: MSI-X; il driver drena il used ring e notifica lo stack (doorbell).

### 5. API a capability (niente POSIX)

- **Control plane** IPC: `NET_OPEN {proto, local, remote, flags}` → endpoint
  (canale + ring TX/RX); `NET_CLOSE`, `NET_STATS`.
- **Data plane**: ring SPSC per endpoint, slot a dimensione fissa, doorbell
  solo sulla transizione vuoto→non vuoto (o ogni N).
- **Policy**: il manifest del client dichiara gli endpoint consentiti; il
  servizio verifica l'hash del peer (`SYS_PEER_INFO`) e applica la policy.
- Niente `sockaddr`/`errno`/fd nel core: la personalità POSIX mappa dopo.

### 6. Configurazione

- `sys:net` (ADR-0070): indirizzo/prefix, gateway, MTU, policy ping/RA,
  upstream DNS (per il resolver). Letto **read-only** all'avvio; config
  invalida → fail-closed (niente rete).
- Fallback compilato per i test (config statica) se il bucket non c'è.
- Niente DHCPv6/SLAAC non controllato: **RA guard** con allowlist del router
  (passt nei test); SLAAC solo dal router autorizzato.

### 7. Sicurezza di default

- RA guard; redirect ignorati; frammenti droppati; no DAD; IID randomizzato;
  ping solo test/config; no raw; default-deny; rate limit; audit.
- TCP (fase successiva): SYN cookies, RFC 5961, ISN randomizzato, no SACK/
  timestamps, no urgent, RST rate-limited.

### 8. Test

- **Host**: unit + fuzzing + harness deterministico (input → output).
- **QEMU**: `scripts/run-net-tests.sh` con `-netdev passt` (IPv6 reale,
  unprivileged), **fuori dal gate**; il gate resta 6 righe.
- **Milestone (a)**: il guest fa RA/SLAAC da passt e ping6 host/router; ping
  abilitato solo dalla config di test.

### 9. CBS e fairness

- `net` e `netdev` si attaccano a un server CBS; la fairness per client è
  interna (code bounded + DRR), non del kernel.

### 10. Traiettoria (non piano)

- **N1**: prerequisiti kernel (DMA/MMIO/MSI-X), driver virtio-net, core IPv6,
  NDP/RA/SLAAC, ICMPv6 echo, passt test; config statica/`sys:net`. **Completata.**
- **N2**: UDP + endpoint a capability + ring; echo server nativo; policy.
  **Completata.**
- **N3**: TCP minimale; API stream nativa. **Completata.**
- **N4**: traduttore IPv4 (SIIT/NAT64) separato. **Completata.**
- **N5**: resolver DNS plain separato. **Completata.**
- **N6**: DoH/DoT con TLS. Spezzata in: **N6.0** inoltro upstream plain UDP
  nella stessa forma del futuro DoT (ephemeral endpoint, single-flight,
  deadline, txid rimappato) — **Completata**; **N6.1** TCP reale in
  `libs/net` (client + listener, macchina a stati, `EV_ACCEPT`) —
  **Completata**; **N6.2** crate separato `libs/tls` (client-only, trasporto
  iniettato, X.509 con clock reale da `usertime`) — **ridisegnata**: adattatore
  sopra `embedded-tls` 0.19 dietro un target userland con SSE2 (ADR-0071,
  ADR-0072); **N6.3** DoT dentro `dns`; **N6.4** DoH se servirà (stessa API di
  trasporto).
- **N7**: personalità POSIX socket.
- **N8**: VT-d/IOMMU.

## Completato — N1 (virtio-net + IPv6/NDP/ICMPv6 + ping6)

- **Kernel**: `Service::Net = 14`, `Service::NetDev = 15`; vettore
  `MSI_VECTOR_NET = 0x51` e notify `IRQ_NOTIFY_NET = 0x45`; finestra MMIO
  per servizio (`DEV_MMIO_NET_BASE`/`USER_MMIO_NET_VA`, il grant
  `SYS_MAP_MMIO` è per owner); grant DMA per `NetDev` (slot >= 1); handler
  MSI-X che sveglia il driver. Il kernel resta neutro.
- **Driver `userland/netdev`**: virtio-net modern (`1af4:1041`), 1 RX + 1 TX,
  64 descrittori, buffer 2048 B, **nessun offload**, MSI-X, ring condiviso
  con il servizio via **shm** (non DMA: il device non lo tocca). Valida il
  used ring; copia i frame tra device e ring.
- **Core `libs/net`**: Ethernet, IPv6 (niente extension header), ICMPv6
  (echo, checksum), NDP (RS/RA/NS/NA, opzioni source/target LL, prefix, MTU),
  SLAAC, RA guard, neighbor cache bounded, IID randomizzato, no DAD.
  `#![forbid(unsafe_code)]`, 4 unit test host (RA/SLAAC, RA guard, echo
  reply, NS→NA).
- **Servizio `userland/net`**: `NET_ATTACH` → shm map, poll del driver ogni
  tick, drena il ring RX, core, ring TX. Config compilata (RA accept, ping
  di prova verso il router); `sys:net` (ADR-0070) è il passo successivo.
- **Test**: `scripts/run-net-tests.sh` (fuori dal gate, dipende da `passt`):
  boot con `-netdev passt` + virtio-net, SLAAC e ping6. Risultato reale:
  indirizzo `2a01:e11:2430:7af0:...`, RA da `fe80::3a07:16ff:fe2c:71ea`,
  **echo reply** dallo stesso router. Gate invariato (6 righe, zero FAIL).

## Completato — N2 (UDP + endpoint a capability + ring)

- **Core `libs/net`**: `udp.rs` (header UDP 8 byte, parse/write, checksum su
  pseudo-header IPv6 obbligatorio, zero rifiutato), `Ipv6Addr::LOOPBACK`/
  `is_loopback()`, `NEXT_UDP=17`, `ECHO_PORT=7`. `net.rs`: `FrameSink`
  ottiene `deliver()` per consegna locale (nessun frame Ethernet), `Net::
  send_udp()` con selezione sorgente, risoluzione prossimo hop (cache
  vicino per link-local, MAC router per off-link), consegna locale per
  indirizzi propri/loopback, drop di extension header/malformed. 5 unit test
  host (roundtrip, checksum zero, payload alterato, troppo corto, len
  inconsistente).
- **Servizio `userland/net`**: endpoint table (max 8), `NET_EP_OPEN` (`0x6F`),
  `NET_EP_TX` (`0x73`), `NET_EP_CLOSE` (`0x81`), `NET_EP_STATS` (`0x82`);
  shm ring SPSC per endpoint (`NET_EP_RING_SLOTS=16`, `NET_EP_SLOT_SIZE=2048`,
  RX service→client + TX client→service, slot con metadata
  `[len:u16][addr:16][src_port:2][dst_port:2][payload]`); policy `sys:net.conf`
  (fail-closed: porta ben nota solo se config esiste, ephemeral sempre);
  `EXIT_NOTIFY` pulisce gli endpoint del peer morto. Consegna locale gestita
  direttamente nell'handler `NET_EP_TX` (scrittura nel ring RX del client
  destinatario + doorbell async), consegna remota via `DriverSink` +
  `send_udp`.
- **`userland/netecho`**: server echo UDP su porta 7 (hardcoded), apre
  endpoint presso `net`, polla RX, rispecchia payload al mittente scrivendo
  nel proprio TX ring e notificando `net` via `NET_EP_TX`. Spawnato solo con
  `net=1`.
- **`testland/netudp`**: probe endpoint ephemeral, invia a `::1:7`, valida
  l'eco (porta + payload), segnala `TEST_DONE`. Eseguito solo con `net=1`.
- **Build/seed**: `build-userland.sh` costruisce `netecho.bin`; `build-tests.sh`
  costruisce `netudp.bin`; `arca-tool.sh` seeda `sys:net.conf`
  (`<hash_fnv1a_netecho>:7`), `netecho.bin` in `sys`, `netudp.bin` in `test`;
  `gen-service-hashes.sh` POLICY `netecho: 0x000`; `rector` `SVC_NETECHO`
  (spawn `net=1`), `TEST_NETUDP` (run_test `net=1`), `expected_hash`/
  `expected_blake` per netecho.
- **Test**: `scripts/run-net-tests.sh` richiede sia `[net] echo reply` (N1)
  sia `[netudp] risultato: PASS` (N2). Gate invariato (6 righe, zero FAIL)
  perché i servizi di rete partono solo con `net=1`.

## Completato — N3 (TCP minimale, API stream a capability)

- **Core `libs/net`**: `tcp.rs` (header TCP 20 byte, parse/write, checksum su
  pseudo-header IPv6, flag SYN/ACK/FIN/RST), `NEXT_TCP=6`, `Ipv6Addr` già
  esistente. `net.rs`: dispatch `on_frame` per `NEXT_TCP`, `Net::send_tcp()`
  con selezione sorgente e risoluzione next-hop (stessa logica di `send_udp`),
  consegna locale per indirizzi propri/loopback. 6 unit test host (roundtrip,
  checksum zero, payload alterato, troppo corto, flag FIN, flag RST).
- **Servizio `userland/net`**: endpoint TCP (stati LISTEN/SYN_SENT/ESTABLISHED
  in `Endpoint`), `NET_EP_OPEN` con `proto=TCP` + flag `LISTEN`/`CONNECT`.
  Consegna locale di segmenti TCP nel ring RX del client destinatario (scritta
  direttamente nell'handler `NET_EP_TX`, con header TCP ricostruito dal
  servizio). Invio remoto diretto (frame Ethernet+IPv6+TCP costruito dal
  servizio, non via `send_tcp` che ricostruirebbe l'header). `EXIT_NOTIFY`
  pulisce endpoint TCP del peer morto.
- **`userland/netecho`**: endpoint UDP porta 7 + endpoint TCP LISTEN porta 8.
  Quando riceve un segmento TCP con flag SYN, risponde con SYN+ACK
  (seq=2000, ack=seq_ricevuto+1) scrivendo nello slot TX e notificando `net`
  via `NET_EP_TX`. Spawnato solo con `net=1`.
- **`testland/net3`**: probe endpoint TCP→`::1:8`, invia segmento SYN,
  valida che `NET_EP_TX` risponda OK (segmento accettato dal servizio).
  Eseguito solo con `net=1`.
- **Build/seed**: `build-tests.sh` costruisce `net3.bin`; `arca-tool.sh` seeda
  `net3.bin` in `test`; `rector` `TEST_NET3` (run_test `net=1`, dopo
  `TEST_NETUDP`).
- **Test**: `scripts/run-net-tests.sh` richiede anche `[net3] risultato: PASS`
  (N3). Gate invariato (6 righe, zero FAIL).
  Limite noto: il handshake TCP completo (SYN+ACK consegnato al client)
  richiede ulteriore debugging del routing locale; il test valida
  l'apertura endpoint e l'accettazione del segmento SYN.

## Completato — N4 (Traduttore IPv4 SIIT/NAT64)

- **Core `libs/net/src/siit.rs` (nuovo)**: logica pura di traduzione IPv6↔IPv4
  (no `#![forbid(unsafe_code)]` ma puro, nessuna allocazione per pacchetto,
  nessun panic su input). Prefisso SIIT `64:ff9b::/96` (RFC 6052), parsing
  header IPv4, scrittura header UDP/TCP su IPv4 con checksum su
  pseudo-header IPv4, conversione indirizzi IPv6↔IPv4. 5 unit test host.
- **Servizio `userland/siit` (nuovo)**: si registra come `Service::Siit`
  (slot 16), apre endpoint UDP presso `net` sulla porta 30000 per le
  risposte in entrata, espone `SIIT_EP_OPEN/TX/CLOSE` (tag `0x84`/`0x85`/
  `0x86`) per i clienti. Tabella NAPT bounded (max 16 connessioni) per
  mappatura cliente IPv6:porta → porta IPv4 ephemeral. Consegna risposte
  in entrata al cliente corretto tramite tabella NAPT. Spawnato solo con
  `net=1`.
- **`testland/net4` (nuovo)**: probe endpoint UDP presso `siit` verso
  indirizzo IPv4 `10.0.0.1` (formato SIIT), valida che `siit` accetti il
  payload (`SIIT_EP_TX` risponde OK). Eseguito solo con `net=1`.
- **Build/seed**: `build-userland.sh` costruisce `siit.bin`;
  `build-tests.sh` costruisce `net4.bin`; `arca-tool.sh` seeda entrambi;
  `rector` `SVC_SIIT` e `TEST_NET4`.
- **Test**: `scripts/run-net-tests.sh` richiede anche `[net4] risultato: PASS`
  (N4). Gate invariato (6 righe, zero FAIL).
- **Kernel**: `Service::Siit = 16`, `SERVICE_COUNT = 17`. Il kernel resta
  neutro: il traduttore è un servizio userspace, nessuna syscall di rete
  aggiunta.

## Completato — N5 (Resolver DNS plain)

- **Core `libs/net/src/dns.rs` (nuovo)**: logica pura RFC 1035 — header
  (id/flag/count), encode/decode nome (con compressione a puntatori),
  question, record A/AAAA, `parse_response`. `no_std` con `alloc::vec::Vec`
  per i nomi decodificati; nessun panic su input. 4 unit test host.
- **Servizio `userland/dns` (nuovo)**: si registra come `Service::Dns`
  (slot 17), apre endpoint UDP presso `net` sulla porta 53 ed espone
  `DNS_EP_OPEN/TX/CLOSE/STATS` (tag `0x87`/`0x88`/`0x89`/`0x8A`) per i
  clienti. N5 risolve da una **tabella statica** (`example.com` A →
  `93.184.216.34`, `ipv6.example.com` AAAA → `2001:db8::1`); i nomi assenti
  ricevono NXDOMAIN (rcode 3). L'inoltro upstream reale arriva con N6
  (DoH/DoT). Spawnato solo con `net=1`.
- **`testland/dns` (nuovo, bin `dnsprobe`)**: probe endpoint UDP presso
  `dns`, invia una query A per `example.com` e valida che la risposta
  contenga `93.184.216.34`. Eseguito solo con `net=1`. Il binario si chiama
  `dnsprobe` per non collidere con il servizio `dns` (hash/manifest).
- **Build/seed**: `build-userland.sh` costruisce `dns.bin`;
  `build-tests.sh` costruisce `dnsprobe.bin`; `arca-tool.sh` seeda entrambi;
  `rector` `SVC_DNS`, `TEST_DNS` e i pin `HASH_DNS`/`BLAKE_DNS`.
- **Test**: `scripts/run-net-tests.sh` richiede anche `[dns] risultato: PASS`
  (N5). Gate invariato (6 righe, zero FAIL); unit test host 72/72 (+4 DNS).
- **Kernel**: `Service::Dns = 17`, `SERVICE_COUNT = 18`. Il kernel resta
  neutro: nessuna syscall DNS, tutto in userspace sopra `net`.

## Completato — N6.0 (inoltro upstream plain UDP)

Primo pezzo di N6, con la **stessa forma** che avrà il DoT: cambiare il
trasporto (plain → TLS) non deve toccare la logica di risoluzione.

- **Semantica**: la tabella statica N5 vince sempre. Nome fuori tabella +
  upstream configurato → inoltro; upstream che non risponde o risposta
  invalida → **SERVFAIL** (rcode 2), mai un NXDOMAIN silenzioso. Config
  assente/invalida → comportamento N5 puro (NXDOMAIN), fail-closed.
- **Config `sys:dns.conf`** (bucket `sys`, righe `chiave = valore`, `#`
  commento): `upstream = auto|<ipv6>`, `port`, `mode = plain|dot|off`,
  `timeout_ms`. `mode = dot` è **riconosciuta** e per ora degrada a tabella
  statica con una riga di log: la config resta valida per N6.3. Una chiave
  sconosciuta non invalida il file (le chiavi TLS future si aggiungono
  senza rompere le vecchie immagini).
- **`upstream = auto`**: il default router si chiede a `net` con il nuovo
  tag **`NET_ADDR` (0x8B)** — selettore 0 link-local, 1 global, 2 router —
  reply in due parole (`w0` byte 0..8, `w1` 8..16). Nessuna syscall nuova e
  niente pacchetti IPv4-only per scoprire il gateway.
- **Trasporto**: endpoint UDP **effimero** presso `net` (la porta 53 che il
  servizio apriva prima era vestigiale: il servizio non fa da server UDP),
  una query in volo, attesa **bloccante con deadline** (`timeout_ms`,
  default 2000), `txid` rimappato con un contatore interno e ripristinato
  nel reply, risposta validata su sorgente (indirizzo + porta), `QR` e id.
  Le mutation del pacchetto avvengono in uno stack buffer di 512 B.
- **`Ipv6Addr::parse`** (core `libs/net/src/addr.rs`): RFC 4291 §2.2, una
  sola compressione `::`, niente IPv4-mapped/scope-id; 2 unit test host
  (forme valide + rifiuti).
- **Bug vero trovato dal test (NDP)**: la cache dei vicini scadeva a 30 s e
  la entry veniva **buttata**; il router si re-imparava solo da un RA, che
  passt manda su richiesta. Passati ~30 s qualunque `send_udp` verso il
  router falliva (`neighbor_lookup` → `None`) — il DNS rispondeva SERVFAIL
  senza nemmeno provare. Fix **RFC 4861 §7.3.3 semplificato**: oltre il TTL
  la entry diventa **STALE** (il MAC resta usabile: il primo pacchetto
  parte comunque), si verifica con una Neighbor Solicitation ogni secondo e
  si scarta solo dopo 3 NS senza risposta. Le NA **senza** opzione
  target-link-layer (passt non la mette, benché sollecitata) confermano la
  entry usando il MAC sorgente del frame Ethernet.
- **Test**: `dnsprobe` fa due fasi — `example.com` (tabella) e
  `upstream.test` → `198.51.100.7` (inoltro); esito `PASS`/`SKIP` (nessun
  upstream → NXDOMAIN, comportamento N5)/`FAIL` (SERVFAIL o timeout).
  `run-net-tests.sh` avvia un responder Python su `[::]:5353` (mappato da
  passt sul gateway) ed esporta `NET_DNS_UPSTREAM_PORT`, che è anche
  l'interruttore del seed di `sys:dns.conf` in `arca-tool.sh`: senza quella
  variabile un `NET=1 ./run.sh` manuale resta identico a N5. Gate invariato
  (6 righe, zero FAIL); unit test host 75/75 (+2 parse, +1 NDP stale).
- **Kernel**: invariato (nessuna syscall, nessun tag nuovo nel kernel:
  `NET_ADDR` è un tag IPC del servizio `net`).

## Completato — N6.1 (TCP reale: client + listener)

Il TCP di N3 era uno shim: `net` costruiva segmenti con solo `SYN` o `ACK`, il
payload arrivava grezzo al cliente, nessun ACK, nessuna ritrasmissione, e
`netecho` fingeva il SYN+ACK rimettendo uno slot. Sopra quello nessun TLS può
stare. N6.1 lo sostituisce con una macchina a stati vera, in quattro passi con
commit separati (`817aacc`, `7dbf3e3`, `8d2c52b`, questo).

- **Core puro `libs/net/src/tcp/conn.rs`** (N6.1a): stati Closed/Listen/
  SynSent/SynRecv/Established/FinWait1/FinWait2/CloseWait/LastAck/TimeWait;
  `snd_una`/`snd_nxt`/`rcv_nxt` con confronto circolare a 32 bit e `data_base`
  (seq del primo byte dati: SYN e FIN consumano un seq ma non stanno in coda);
  finestra di invio limitata da `snd_wnd` del peer e finestra di ricezione =
  spazio libero del buffer (window 0 naturale); go-back-N su RTO 1s→2s→4s con
  max 3 tentativi; FIN/ACK, half-close, TIME_WAIT 5 s, RST; **ricezione solo
  in-order** (duplicati e fuori ordine rispondono con ACK di `rcv_nxt`).
  `next_tx(out)` scrive header+payload e ritorna `TxInfo`; `on_segment`
  ritorna `RxOutcome` (segmento di risposta, dati, eventi). Il core non
  conosce né `FrameSink` né `eth`/`ipv6`.
- **Wiring nel layer `Net`** (N6.1b): tabella `[Option<Conn>; 16]` identificata
  da un **id**, non dalla porta (un listener e i suoi figli condividono la
  porta: distinguerli per porta impedirebbe due client in parallelo).
  `NEXT_TCP` parsa, verifica il checksum, cerca la connessione per
  (porta locale, peer) e delega; porta sconosciuta → RST (mai in risposta a un
  RST). `tcp_poll` drena SYN/dati/ritrasmissioni/FIN. Il loopback (destinazione
  nostra o `::1`) rientra in `tcp_input` senza frame Ethernet, con un tetto
  alla ricorsione: due processi del guest si parlano davvero.
- **Listener** (N6.1c): SYN su un listener → figlio in SYN_RECV
  (`TCP_BACKLOG = 4`, SYN oltre il backlog → RST), listener sempre in ascolto;
  segnale `TcpSignal::Accepted(porta)`, il servizio crea l'endpoint del figlio
  (ring nuovo, stessa porta) e manda `EV_ACCEPT = 5` con
  `[ep_id:2][ring_id:4]`. `netecho` ora fa solo `tcp_read`/`tcp_write`.
- **API adapters**: `FrameSink::tcp_signal(id, segnale)` (solo notifica; il
  dato si legge con `tcp_read`, così il servizio non rientra in `Net` mentre è
  in prestito); uno slot TX con evento `EV_FIN` chiede la chiusura ordinata.

**Bug veri emersi dai test** (tutti corretti qui): i segmenti di controllo di
`next_tx` ritornavano `TxInfo` senza scrivere i byte (segmenti vuoti sul
link); il FIN occupa il seq *dopo* i dati del segmento (la condizione di uno
era sbagliata: chiusura mai completata); il loopback deve usare `::1` come
sorgente; il flag "creato come listener" non è lo stato (dopo l'handshake la
connessione non si trovava più); il figlio in SYN_RECV non emetteva il suo
SYN+ACK; il client azzerava il ring del figlio cancellando i byte già scritti
dal servizio; `NetService` e i buffer di 2 KiB mandavano il servizio in
**stack overflow** (guard page: `net` ripartiva in loop) → `NetService` e i
buffer su heap.

**Limiti dichiarati**: niente congestion control (RTO-only), niente fast
retransmit/SACK/timestamps/window scaling, nessuna coda di riordino, MSS fisso
1440, backlog 4, solo IPv6, RST rate-limited solo dal backlog.

**Test**: 48 unit test host nel crate `net` (totale 96) — handshake client e
listener, dati in ordine, duplicati, fuori ordine + recupero, RTO con resa,
ACK e finestra, window 0 e riapertura, chiusura client e server-first,
segmento verso porta chiusa, wrap a 32 bit, buffer pieno, scrittura prima
dell'handshake, input arbitrari in ogni stato, handshake+dati in loopback,
backlog. `testland/net3` valida handshake **ed** eco end-to-end sul core.
`testland/nettcp` (N6.1d) prova il TCP verso l'esterno con una query DNS over
TCP (stesso trasporto di DoT), ma resta **opzionale**: passt non inoltra il
TCP al loopback dell'host e in questa ambientazione anche un peer pubblico
risponde RST — il probe si salta se non si dichiara `NET_TCP_PEER`.

## Consequences

### Positive

- Un solo parser IPv6; IPv4 isolato; driver minimo; stack isolato; API
  capability; test host/fuzz; gate indipendente da passt.
- Il kernel resta neutro: la rete è un servizio, non una syscall.

### Negative / limiti

- Prerequisiti kernel (DMA/MMIO/MSI-X) prima del driver.
- Niente IPv4 nel core: il mondo IPv4 richiede il traduttore (N4).
- Niente frammentazione/DAD/SACK: subset dichiarato.
- NUD semplificato: oltre il TTL la entry è STALE e viene verificata con NS
  (max 3, 1 s); niente multicast NS in burst né reachability confirm
  delay. Sufficiente per un gateway stabile, da rivedere su hardware con
  MAC mobili.
- Single-thread: throughput limitato; multi-queue dopo.
- TCP: niente congestion control, fast retransmit, SACK, timestamps,
  window scaling né coda di riordino; MSS fisso 1440; backlog 4. È il subset
  che serve a un client TLS e a un server di test — non a un server di
  produzione.
- Il probe TCP verso l'esterno non è nel gate né obbligatorio nei net tests:
  dipende dall'ambientazione (passt non inoltra il TCP al loopback dell'host).

## Riferimenti

- ADR-0068 (threat model/invarianti), ADR-0070 (policy subtree/bucket).
- ADR-0005 (microkernel), ADR-0008 (IPC per nome), ADR-0025/0041/0048
  (meccanismo/personalità), ADR-0057 (grant MMIO), ADR-0060 (console),
  ADR-0064 (bucket binari).
