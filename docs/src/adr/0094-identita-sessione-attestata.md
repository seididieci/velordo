# ADR-0094: Identità di sessione per attestazione — niente bearer token (S3.3c)

## Status

**Accepted** (2026-11-26). Implementata in S3.3c. Gate invariato: 6 suite
(`testfs` 5/5, `testsarca` 64/64, `posixtests` 4/4, `identistest` 11/11,
`usertests` 72/72, `threadtest` 12/12) + shell nativa 33/33, zero
FAIL/PANIC/FAULT.

Sostituisce il meccanismo dei **token di sessione** introdotto da ADR-0055 §5
(«token bearer in `VELORDO_SESSION`») e spostato nel server da D8f (ADR-0092).
Chiude il debito dichiarato in ADR-0055 §5 e ADR-0056: il token era un
**valore che viaggia**, quindi copiabile, leggibile dall'ambiente e senza
scadenza utile in una sessione locale. Dipende da ADR-0078 (ImageId), ADR-0079
(classi nel PCB), ADR-0080 (Custos/policy), ADR-0089/0090/0091 (sessioni,
ritiro di `/dev/console`, input come capability), ADR-0092 (tile e DE),
ADR-0093 (diritti ereditari).

## Contesto

L'identità dell'utente era un **token a 64 bit** emesso da `Identis` a ogni
`VERIFY` riuscito (TTL 300 s, tabella `token → utente`). Da D8f il token non
stava più in una variabile d'ambiente ma **nel server** (`SESS_TOKEN_SET` /
`SESS_TOKEN`, tag `0xAD`/`0xAE`), e ogni programma lo leggeva per
attestazione del pid.

Restavano tre problemi strutturali:

1. **Il token era un valore trasferibile.** Chiunque lo leggesse (un figlio
   `Untrusted` con la sessione giusta, un `println!` di debug) poteva agire
   come l'utente da un altro processo: la capability era il possesso, non
   l'identità.
2. **Non c'era un posto dove metterlo senza mentire.** Il login lo scriveva
   sulla sessione 0, il DE lo **copiava** sulle tile nuove: una propagazione
   manuale di un segreto, con una finestra in cui una tile nasceva anonima.
3. **La scadenza era finta.** In una sessione locale il token durava 300 s e
   la shell doveva rifare il login a metà lavoro (`R_TOKEN_INVALID` →
   `auth_and_get_token`), senza che nulla di reale fosse cambiato.

Nel frattempo D8c1–D8e avevano già costruito **la cosa giusta al posto
giusto**: la sessione è del server (`littera`), il pid lo dà il kernel
(`peer_pid`, `ps_info.parent`), la catena dei padri è attestazione (D8f). Il
token era l'ultimo pezzo che non si fidava di quei fatti.

## Decisione

**Vittoria**: nessun segreto viaggia e nessuna identità si copia. Un processo
è chi il **kernel** dice che è (pid), in una sessione che il **server** dice
autenticata (utente). Chi ha bisogno dell'identità di un altro la **chiede**
al server, che risponde per attestazione.

### Le due fonti, in ordine

```
caller_user(pid):
  1. credenziale locale del pid   (VERIFY riuscito di quel processo)
  2. sessione autenticata del pid (SESS_AUTH: session_of_process + auth)
  altrimenti: nessuna identità
```

1. **Credenziale locale** (`Identis`, RAM): un `VERIFY` riuscito lega il pid
   chiamante al nome. Serve ai client che si autenticano da soli — `login`,
   il probe `t69`/`t70`, `identistest` — e ai test che girano **prima** del
   login, dove nessuna sessione è autenticata. Non è un segreto: nessuno la
   legge, nessuno la trasporta; il pid lo mette il kernel. Si **purga** su
   `EXIT_NOTIFY` (nessun pid riusato che eredita un'identità).
2. **Sessione autenticata** (server `console`): è la via normale. Il login
   scrive il nome sulla sessione 0, il DE lo propaga alle tile che crea, e
   ogni processo che gira in quella sessione **la eredita per attestazione**
   (la catena dei padri la dà il kernel). Un comando lanciato in una tile
   parla con *quella* tile (D8f), senza che nessuno gli passi niente.

### Il meccanismo (ABI)

| tag | nome | semantica |
|-----|------|-----------|
| `0xAF` | `SESS_AUTH_SET {len}` | scrive l'utente sulla sessione **attaccata** al canale (nome nella metà OUT); `len = 0` = deautentica; **solo `System`** |
| `0xB7` | `SESS_AUTH {pid}` | `w0` = 1/0 (autenticata), `w1` = len del nome (nella metà IN); `pid = 0` = la sessione attaccata; **solo `System`** |

`0xAD` e `0xAE` sono **ritirati**: i numeri non si riusano (la guardia è il
compilatore, non un commento). Il resto dell'endpoint di sessione non cambia
(`SESS_*` 0x9B–0xAC, `TILE_*` 0xB0–0xB6).

**Perché `SESS_AUTH_SET` scrive sulla sessione attaccata** (e non su un id
esplicito): come `SESS_RESIZE`/`SESS_ECHO`, l'attachment *è* la capability.
Il DE che crea una tile si attacca a lei e le dà l'identità; nessun id
arbitrario viaggia.

**Perché `SESS_AUTH` è solo `System`**: leggere l'identità di una tile è un
atto di controllo (il DE, `Identis`, `Custos`). Un programma normale non deve
poter scandagliare le sessioni altrui. Il caso "chi sono io?" è coperto dal
fatto che i servizi risolvono il **pid del chiamante** e non hanno bisogno che
lui si presenti.

Nel modello della sessione (`libs/littera`) il token `u64` diventa
`auth_user: [u8; AUTH_USER_MAX]` + `auth_len` + `auth_by` (chi l'ha
stabilita: alla sua morte l'identità sparisce anche se la sessione resta viva
— rete di sicurezza per un login/DE ucciso). `purge_pid` pulisce bindings,
sessioni possedute **e** identità stabilite da quel pid.

### Chi fa cosa

| processo | identità usata | come |
|----------|----------------|------|
| `identis-login` | scrive `root` (o l'utente) sulla sessione 0 | `SESS_AUTH_SET` (è `System`) |
| `impluvium` (DE) | legge la 0 e la scrive su ogni tile nuova | `SESS_AUTH(0)` + `SESS_AUTH_SET` |
| `interpres` (shell) | nessun token: i comandi risolvono il suo pid | `IDENTIS_*` / `CUSTOS_*` |
| `users`, `create-user` | nessun token | `IDENTIS_LIST`/`ADD_USER` per pid |
| `identis` | risolve il chiamante (credenziale → sessione) | `caller_user(pid)` |
| `custos` | ruoli del chiamante | `IDENTIS_WHOAMI {w1 = pid}` |

`IDENTIS_WHOAMI` prende il **pid** in `w1` (0 = il chiamante): un altro pid è
lecito solo a `System` (il servizio di controllo). `ADD_USER`/`LIST` non
prendono nulla: il chiamante è il canale.

### Perché non «un segreto più corto»

Un token per sessione, un nonce, un HMAC col pid: tutti mantengono il difetto
di partenza — un valore che deve **viaggiare** per essere usato, quindi
copiabile e da revocare. L'attestazione non viaggia: il kernel sa il pid, il
server sa la sessione. La superficie è più piccola, non solo più corta.

### Cosa NON è (limiti dichiarati)

- **`su` / cambio utente** non esiste: `SESS_AUTH_SET` è solo `System`, e il
  login è l'unico che la chiama sulla 0. Un comando non può cambiare la
  propria identità. Fase separata.
- **Timeout della sessione remota**: fuori scope. In locale la sessione dura
  dal login al logout (D8f: il login vive quanto il DE), che è il comportamento
  voluto; quando ci sarà un accesso remoto la sua chiusura porterà via
  l'identità con `SESS_AUTH_SET 0`.
- **Provenienza del consenso**: `usr:<nome>:consent` porta un nome che il
  client dichiara (era già così col token). Non decide nulla — la decisione
  usa i ruoli attestati — e resta un debito dichiarato, non una regressione.
- **L'identità non è un UID**: gli utenti sono nomi, non numeri. Nessuna
  mappatura nome→uid, nessun `setuid`.

## Conseguenze

### Positive

- **Nessun segreto nei payload, nell'ambiente o in RAM condivisa.** Il token
  non esiste più: non c'è niente da copiare, leggere o far scadere.
- **La sessione è l'unità di identità** (doveva già esserlo): il DE propaga
  un nome, non un bearer; il login azzera l'identità alla fine della sessione.
- **La scadenza non serve**: un'identità vale finché vale la sessione. Sparisce
  la danza `R_TOKEN_INVALID` → rifai il login a metà comando.
- **Il pid è l'unica prova**, e lo dà il kernel: nessuna tabella di token da
  ripulire, nessun TTL da scegliere, nessun `rand_token` da fidarsi.
- **I test restano testabili**: la credenziale locale permette a `t69`/`t70`/
  `identistest` di esercitare i percorsi privilegiati **prima** del login,
  senza inventare un bypass.

### Negative / da sapere

- **Un processo, un'identità alla volta.** Col token si potevano tenere due
  identità in mano; ora un `VERIFY` sostituisce quella del pid. `t69` fa
  esplicitamente `login(root)` prima di creare l'utente semplice: è la
  semantica nuova, non un aggiramento.
- **Un servizio deve poter chiedere al server.** `Identis` e `Custos` sono
  `System` e pagano 1–2 IPC per risolvere il chiamante (mai nel percorso
  caldo: solo su `ADD_USER`/`GRANT`/`CONSENT`, che sono decisioni rare).
- **`Identis` tiene una credenziale per pid (max 8).** Tabella piena ⇒
  sostituzione dello slot 0, con log. In pratica le credenziali sono poche per
  costruzione (un `VERIFY` esiste solo per i client che si autenticano da soli).

### Trappole trovate

- **Il fallback di sessione non è opzionale.** La prima versione di `Identis`
  risolveva *solo* la credenziale locale: la shell (`interpres`) non fa mai
  `VERIFY`, quindi `create-user` rispondeva `R_NO_IDENTITY`. La sessione è la
  via **normale**, la credenziale è quella dei client autonomi: vanno in
  quest'ordine, mai una sola delle due.
- **L'ordine delle scritture nel DE.** `split()` deve leggere la 0, attaccarsi
  alla tile nuova e *poi* scriverci l'utente — e solo dopo `focus_session()`
  sposta di nuovo il canale. Invertendo, l'identità finisce sulla tile
  sbagliata (o su nessuna).
- **`auth_by` non è decorativo.** Senza, un login ucciso lascerebbe la sessione
  0 autenticata per il getty successivo: un accesso senza password. La purga su
  `EXIT_NOTIFY` (`littera::purge_pid`) lo chiude; l'azzeramento esplicito del
  login è la via principale, la purga la rete di sicurezza.
- **Il gate non cambia numero.** Le verifiche negative di S3.3c stanno dentro
  test già esistenti (`identistest` t2: `WHOAMI` senza identità; `t74`: i tag
  `SESS_AUTH*` negati a un `Untrusted`), così i conteggi del gate restano
  quelli dichiarati altrove.

## Test

- `libs/littera` (unit host): `identita_di_sessione` — set/clear, UTF-8 e
  lunghezza fail-loud, purga per `auth_by`.
- `identistest` 11/11: t2 `WHOAMI` senza identità → `R_NO_IDENTITY`; t3
  `VERIFY` lega il pid; t4 una verifica errata **non** cambia l'identità; t10
  un'identità non-root non crea utenti; t11 `LIST` per attestazione.
- `usertests` t69: la stessa sequenza con Custos (ruoli live, GRANT
  autorizzato/negato per ruolo, revoca) con `login` che *diventa* l'utente.
- `usertests` t70: consenso per immagine con identità attestata (persistenza,
  provenienza, revoca, "il consenso non crea privilegio").
- `usertests` t74: `SESS_AUTH_SET`/`SESS_AUTH` negati a `Untrusted`.
- Shell nativa 33/33: `users` e `create-user` dopo il login **attraverso il
  DE**, cioè con l'identità propagata da `impluvium` alla tile 0.
