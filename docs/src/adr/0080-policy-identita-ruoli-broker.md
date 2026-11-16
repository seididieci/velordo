# ADR-0080: Policy per identità, ruoli multipli e broker di rete

## Status

**Accepted** (2026-11). Fasi 5–6. Dipende da ADR-0078 (identità) e ADR-0079
(classi). La fase 7 (consenso con prompt) resta **parcheggiata**: richiede il
DE e la capability di sessione (S3.3(c)).

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

- possiede `sys:policy` (policy amministrativa, firmata) e
  `usr:<nome>:consent` (decisioni per utente);
- espone `CUSTOS_QUERY(identity, capability) → granted` e
  `CUSTOS_GRANT(identity, capability, decisione)`;
- il grant effettivo è
  `manifest ∩ policy admin ∩ consenso dell'utente`;
- **fail-closed**: policy assente o illeggibile → nega.

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

`IDENTIS_ADD_USER` accetta il ruolo; la sessione porta il ruolo (non solo
"root sì/no"). Regola anti *confused deputy*: un utente può concedere solo
`min(richiesta, autorità del proprio ruolo, policy admin)`.

### 3. Policy per identità, non per hash

- cardo: `ceiling_for` usa l'identità (`peer_info` a 128 bit + classe) e la
  riga di `sys:policy`;
- `net`: `can_bind(identity, port)` consulta `Custos`; lo stub
  `policy_allow_wk` sparisce. Le porte ben note richiedono un grant esplicito
  per identità.

### 4. Broker di rete per il codice non firmato

`userland/netbroker` (`Service::NetBroker = 19`):

- API stretta: `BROKER_RESOLVE(nome)`, `BROKER_CONNECT(host, porta)` con
  proxy a ring;
- allowlist di porte, rate limit per identità, audit in Vestigia;
- il codice `Untrusted` **non ottiene mai** `Service::Net`: solo il broker.

Motivo: nel microkernel il privilegio vive in un broker, non nell'app. Così il
tetto è strutturale e non dipende dalla correttezza di una tabella.

### 5. Consenso: statico ora, prompt in fase 7

In fase 6 il consenso è **statico per ruolo**: `Custos` consulta
`usr:<nome>:consent`; se manca, nega e logga. Il dialogo al primo avvio
(fase 7) richiede il DE e la sessione-capability di S3.3(c) — senza quella il
consenso sarebbe rubabile insieme al token.

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

- Nessuna syscall nuova (solo servizi e tag IPC).
- Il kernel resta senza policy: `Custos` è userspace.

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
- `userland/net/src/main.rs` (`load_policy`, `can_bind` — stub da sostituire)
- `userland/identis/src/main.rs` (utenti, sessione)
- `userland/cardo/src/policy.rs` (`ceiling_for`)
