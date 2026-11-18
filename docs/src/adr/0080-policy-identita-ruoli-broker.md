# ADR-0080: Policy per identità, ruoli multipli e broker di rete

## Status

**Accepted** (2026-11). Fasi 5–6, entrambe **implementate**. Dipende da
ADR-0078 (identità) e ADR-0079 (classi).

- **Fase 5** (2026-11-18): `Custos`, ruoli multipli, policy per identità di
  `net`, t69.
- **Fase 6** (2026-11-18): `netbroker` (`Service::NetBroker = 19`,
  `SERVICE_COUNT = 20`), tetto **strutturale** per classe su `net` e `siit`,
  consenso per immagine (`usr:consent` + `usr:<nome>:consent`), t70,
  `consent <prog> on|off` nella shell nativa.
- La fase 7 (prompt di consenso con dialogo, consenso one-shot) resta
  **parcheggiata**: richiede il DE e la capability di sessione (S3.3(c)).

## Context

Dopo ADR-0077/0078/0079 il sistema sa **chi** è un binario (firma) e **che
classe** ha. Resta da decidere **cosa può fare** e **chi può concederlo**.

Oggi:

- cardo applica un tetto ops per hash (`SERVICE_POLICY`/`TEST_POLICY`);
- `net::can_bind` è uno **stub**: legge `sys:net.conf`, scarta hash e porta,
  e mette un booleano globale `policy_allow_wk = true`. Appena il file esiste,
  **qualunque** processo può bindare 7, 8, 53, 853;
- `Identis` conosce utenti e un solo ruolo di fatto (root vs non-root), con un
  token bearer in `VELORDO_SESSION` (debito ADR-0058);
- il codice non firmato non ha modo di usare la rete in modo controllato: o
  ottiene `Service::Net` (troppo) o niente (inutile).

## Decision

### 1. Un server di policy: `Custos`

Nuovo servizio nativo (`Service::Custos = 18`, `SERVICE_COUNT` 19):

- possiede `sys:caps` (policy amministrativa delle capability; **dato
  seedato a build-time**, unico writer runtime `Custos`) e, in fase 6,
  `usr:<nome>:consent` (decisioni per utente);
- espone `CUSTOS_QUERY(identity, capability) → granted` e
  `CUSTOS_GRANT(identity, capability, decisione)`;
- il grant effettivo è
  `manifest ∩ policy admin ∩ consenso dell'utente`;
- **fail-closed**: policy assente o illeggibile → nega.

**Deviazione dichiarata (fase 5): un dato separato, `sys:caps`.** Il testo
originale diceva `sys:policy`; realizzarlo così avrebbe costretto cardo a
rileggere la policy a ogni handshake o a chiamare `Custos` mentre applica
un handshake — con un ciclo cardo↔Custos e un deadlock possibile durante un
restart. Le due domande restano separate:

| Dato | Domanda | Writer runtime |
|---|---|---|
| `sys:policy` / `sys:test-policy` | quali **op FS** per immagine (cardo) | init (seed di build) |
| `sys:caps` | quali **capability** (rete, hardware) per immagine | solo `Custos` |

Formato di `sys:caps` (una riga per voce, l'ultima vince):

```text
<hi:016x> <lo:016x> <kind> <arg> <decisione>
```

`hi`/`lo` sono le due metà dell'`ImageId`; `decisione` è `1` (concesso) o
`0` (revocato). Capability: `CAP_NET_BIND=1` (`arg` = porta locale),
`CAP_NET_USE=2`, `CAP_HW_MMIO=3`, `CAP_HW_DMA=4` (le ultime tre dichiarative
in fase 5). Il seed di build concede a `netecho` le porte 7/8 e a `siit` la
30000 (`scripts/gen-service-hashes.sh`); tutte le altre identità partono
senza grant. `Custos` persiste **prima** su FS e poi muta la RAM: un errore
di scrittura non lascia stato divergente.

`Custos` è TCB di policy: piccolo, single-thread, nessuna allocazione non
controllata.

### 2. Ruoli multipli in `Identis`

Ruoli con capability set:

| Ruolo | Può concedere |
|---|---|
| `root` | tutto |
| `net-admin` | accesso rete, porte, broker |
| `hw-admin` | hardware (DMA/MMIO — ma la classe `System` resta strutturale) |
| `user` | solo risorse della propria sessione (home, rete via broker con consenso) |

`IDENTIS_ADD_USER` accetta una lista di ruoli (`nome:hash:ruolo[,ruolo...]`),
retrocompatibile con la riga a due campi (→ `user`). La **sessione porta solo
l'identità**: i ruoli si risolvono **live** dal bucket (nuovo
`IDENTIS_WHOAMI`), quindi cambiarli non richiede un nuovo login. Regola anti
*confused deputy*: un utente può concedere solo
`min(richiesta, autorità del proprio ruolo, policy admin)` — root concede
tutto, un `net-admin`/`hw-admin` concede solo i ruoli che possiede (più
`user`), un `user` non concede nulla. Lo stesso criterio vale per
`CUSTOS_GRANT`: `NET_*` richiede `net-admin`, `HW_*` richiede `hw-admin`,
root copre tutto.

### 3. Policy per identità, non per hash

- cardo: `ceiling_for` usa l'identità (`peer_info` a 128 bit + classe) e la
  riga di `sys:policy`; in difesa in profondità una riga `RIGHTS_ALL` della
  tabella **servizi** non promuove a ALL un'immagine non `System` (i binari
  della suite restano ALL perché vivono in `sys:test-policy`, che è build);
- `net`: `can_bind(identity, port)` consulta `Custos`; lo stub
  `policy_allow_wk` è sparito. Le porte effimere (≥ 49152) restano sempre
  libere; le porte ben note richiedono un grant esplicito per identità. La
  decisione è in cache per `(identità, porta)` (il servizio è
  single-threaded); `Full`/errore IPC = negato.
- cardo legge `sys:caps` **direttamente**, senza chiamare `Custos`:
  un'eccezione motivata dal deadlock cardo↔Custos (cardo è single-threaded e
  non può bloccarsi in IPC mentre serve un handshake).

### 4. Broker di rete per il codice non firmato

`userland/netbroker` (`Service::NetBroker = 19`):

- API stretta: `BROKER_RESOLVE(nome)`, `BROKER_CONNECT(host, porta)` con
  proxy a ring;
- allowlist di porte, rate limit per identità, audit in Vestigia;
- il codice `Untrusted` **non ottiene mai** `Service::Net`: solo il broker.

Motivo: nel microkernel il privilegio vive in un broker, non nell'app. Così il
tetto è strutturale e non dipende dalla correttezza di una tabella.

### 5. Consenso: statico ora, prompt in fase 7

Due dati, un solo writer (`Custos`), separati per ruolo:

| Dato | Cosa dice | Chi lo scrive |
|---|---|---|
| `usr:consent` | tabella **autorevole** per immagine: `<hi> <lo> <decisione> [utente]` | solo Custos (`CUSTOS_CONSENT`) |
| `usr:<nome>:consent` | **provenienza**: chi ha dato il consenso su quell'immagine | solo Custos (stessa transazione) |

L'autorizzazione (`CUSTOS_AUTHORIZE`) è `policy admin ∩ consenso`:
`sys:caps` (fase 5) non basta, `usr:consent` non basta. Assente = negato. La
provenienza per utente permette la revoca per utente e rende la tabella
spiegabile; l'indicizzazione per immagine è necessaria perché chi autorizza
(il broker) conosce l'`ImageId`, non l'utente.

La scrittura richiede un **token di sessione valido** (session owner o root):
senza, chiunque (anche un figlio che eredita `VELORDO_SESSION`) si
autoconsentirebbe e il consenso sarebbe teatro.

Nella shell nativa il consenso si dà con `consent <prog> on|off`; al lancio
di un programma senza riga la shell stampa un **avviso non bloccante** (la
shell non sa se il programma usa la rete, e un prompt su ogni lancio
romperebbe le esecuzioni non interattive). Il dialogo `s/n` all'avvio arriva
con la fase 7 (DE + sessione-capability di S3.3(c)): senza quella il consenso
sarebbe rubabile insieme al token.

## Consequences

### Positive

- Chiude un buco reale: `net` non è più "tutti possono bindare le porte note
  appena esiste un file di config".
- Ruoli multipli: delegare la rete non richiede dare root.
- Il codice non firmato resta **utile** (naviga) senza ottenere privilegi.
- Audit: ogni grant e ogni connessione brokerata è registrata.

### Negative

- Due servizi TCB in più (`Custos`, `netbroker`): superficie nuova, da tenere
  minimale e fail-closed.
- `netbroker` è la parte più delicata (proxy a ring: forwarding,
  backpressure, chiusura).
- La catena di autorità è a tre livelli (manifest, admin, utente): più potente
  ma più difficile da spiegare. Serve un `custos status` per la diagnosi.
- `Identis` cresce di un modello di ruoli: tocca login, sessione e i comandi
  `create-user`/`users`.

### Neutral

- Nessuna syscall nuova (solo servizi e tag IPC: `CUSTOS_QUERY=0x80`,
  `CUSTOS_GRANT=0x81`, `CUSTOS_STATUS=0x82`, `CUSTOS_AUTHORIZE=0x83`,
  `CUSTOS_CONSENT=0x84`, `IDENTIS_WHOAMI=0x73`,
  `BROKER_OPEN/TX/CLOSE/STATS=0x90–0x93`). Il kernel conosce l'enum
  `Service::NetBroker` e lo slot 19, nient'altro.
- Il kernel resta senza policy: `Custos` e `netbroker` sono userspace.
- Test: `t69` e `t70` in `usertests` (gate `usertests 66/66`); il
  comportamento di `net`, `siit` e del broker è verificato dal run dedicato
  `scripts/run-net-tests.sh` (fuori dal gate, `net=1`).
- Debiti dichiarati: rate limit e audit del broker sono in RAM (un restart
  del broker azzera i contatori), il consenso non ha ancora scadenza
  (one-shot) e non c'è revoca a caldo dei grant già in cache nei client.

## Alternatives Considered

- **Root unico**: più semplice, ma ogni delega richiede root e il consenso
  diventa binario. Scartato (scelta esplicita dell'utente).
- **Grant diretto di `Service::Net` al codice non firmato**: il tetto
  strutturale si perde al primo bug di policy. Scartato in favore del broker.
- **Prompt di consenso subito**: richiede DE e sessione-capability; un prompt
  senza quei prerequisiti è teatro (o peggio, rubabile). Rinviato a fase 7.
- **Policy nel kernel**: contrario ad ADR-0005 e ADR-0025; il kernel tiene solo
  i bit strutturali (ADR-0079).

## References

- ADR-0055/0058 (utenti, token, S3.3), ADR-0069 (endpoint di rete),
  ADR-0077/0078/0079
- `userland/custos/src/main.rs`, `libs/custos-proto` (fase 5)
- `userland/net/src/main.rs` (`can_bind` + cache, `query_custos`, gate di
  classe su `NET_EP_OPEN`), `userland/siit/src/main.rs` (stesso gate su
  `SIIT_EP_OPEN`)
- `userland/netbroker/src/main.rs`, `libs/broker-proto`, `libs/civis/src/broker.rs`
- `userland/custos/src/main.rs` (`AUTHORIZE`, `CONSENT`, `usr:consent`),
  `userland/interpres/src/main.rs` (`consent <prog> on|off`),
  `testland/usertests/src/t_consent.rs` (t70)
- `userland/identis/src/main.rs` (ruoli, `IDENTIS_WHOAMI`, `roles_of`)
- `userland/cardo/src/policy.rs` (`ceiling_for`, `caps_writer`)
- `docs/src/14-cronologia-fasi.md` (voce fase 5)
