# ADR-0058: Creazione utenti, token di sessione e protocollo Identis (Fase 3.1)

## Status

Accepted

## Context

ADR-0055 disegna la gestione utenti: un servizio `auth` (poi realized come
**Identis**) che detiene il bucket `identis:config` e un programma
`create-user` che interagisce con lui via IPC. La Fase 3.1 realizza la parte
**server** di quel disegno (sessione + creazione utente) e il client
`create-user`, ma il protocollo si e' evoluto rispetto al testo di ADR-0055 e
alcuni buchi vanno chiusi *prima* di poter aggiungere un utente.

Stato del codice all'inizio della fase:

- `Identis` implementava **solo** `IDENTIS_VERIFY` (0x70). I tag
  `IDENTIS_ADD_USER` (0x71) e `IDENTIS_LIST` (0x72) erano dichiarati in
  `syscall-numbers` ma **non gestiti**: il server cadeva nel ramo `_ => {}` e
  **non rispondeva**. `civis::send` e' bloccante (aspetta la reply): un client
  che avesse inviato 0x71 sarebbe **restato appeso per sempre**.
- La macchina a stati di ricezione aveva **una sola transazione globale** e non
  controllava il canale: in stato RECEIVING, `if !TXN_ACTIVE` decideva da solo
  se il messaggio era un header o un chunk. Un secondo client che bussava a
  metà trasmissione veniva **consumato come chunk del primo** → buffer
  corrotto (e reply su un canale sbagliato).
- Il bucket `identis:config` **non era protetto**: e' un path FS come un altro.
  La policy di cardo (Fase 45, ADR-0037) classifica i canali per hash
  misurato e assegna un *tetto di operazioni*, ma il tetto del default
  ignoto (`DEFAULT_UNKNOWN_OPS` = 0x19F) **include OPEN/READ/WRITE**: chiunque
  poteva aprire e scrivere il bucket delle credenziali. Il subtree per canale
  e' *self-restriction* (solo DROP): nessuno puo' vietare a un altro.
- Il modello di autorizzazione era aperto: chiunque risolvesse il canale di
  Identis poteva (in teoria) chiedere di creare utenti.

Due strade per l'autorizzazione erano sul tavolo: **identita' misurata**
(l'hash FNV-1a del binario `create-user`, confrontato con `peer_info`) o
**token di sessione** emesso da Identis al login riuscito. La prima ha un
ostacolo non banale: il manifest degli hash (`gen-service-hashes.sh`) e'
generato **dopo** i binari userland, quindi un binario che lo include a
compile-time non puo' stare nel manifest (ciclo instabile, vedi ADR-0037) →
Identis non puo' conoscere a compile-time l'hash di `create-user` senza
perdere il proprio pinning. La seconda non ha questo problema.

## Decision

### 1. Un solo crate per il protocollo: `libs/identis-proto`

Il framing (magic + versione + campi), i codici di risposta, la costruzione e
il parsing dei payload e il client chunked vivono in **un crate condiviso**
(`identis-proto`, no_std, solo `core` + `civis`), usato da: il servizio
`identis`, il client `identis-login`, il client `create-user` e il test
`identistest`. Prima la stessa logica sarebbe stata copiata in quattro
punti. Il protocollo e' un **dato** (magic, versione, campi), non una
struttura kernel: niente di tutto questo finisce nell'ABI delle syscall.

Framing (messaggi sincroni su canale IPC):

```
header: tag=OP, w0=total_len, w1=0
chunk:  tag=OP, w0=offset,   w1=4 byte payload (little-endian)
EOF:    tag=OP, w0=0xFFFFFFFF, w1=0
reply:  tag=OP, w0=codice,   w1=token (solo VERIFY ok)
```

### 2. Token di sessione: la capability

Al VERIFY riuscito, Identis **emette un token** (u64 casuale) e lo restituisce
nella reply (`w0 = R_OK`, `w1 = token`). Il token e' un **bearer**: chi lo
possiede agisce come l'utente. Tabella sessioni: 8 slot, TTL 300 s, il slot
libero o il piu' vecchio viene sostituito. La generazione usa RDRAND quando la
CPU lo espone e funziona, altrimenti un fallback xorshift64\* seminato da
`rdtsc + ticks + pid` ( sotto TCG RDRAND non funziona mai: vedi la nota del
PAL, S2.5d → **il token non e' crypto-grade in TCG**, dichiarato).

Il token non viaggia mai in una password: la **password in chiaro non lascia
mai il client**. Il client calcola l'hash BLAKE2s-256 in locale e manda solo
l'hash (32 byte), come gia' faceva il login.

### 3. `IDENTIS_ADD_USER` (0x71): creazione utente

Payload `ACUS`: magic + versione + token (8 byte) + nome + hash 32 byte.
Identis valida, in quest'ordine:

1. token valido e non scaduto → altrimenti `R_TOKEN_INVALID`;
2. la sessione e' di **root** → altrimenti `R_UNAUTHORIZED`;
3. nome conforme (`[a-z0-9_-]`, 1..32 byte) → altrimenti `R_NAME_INVALID`;
4. nome non gia' presente → altrimenti `R_EXISTS`;
5. append della riga `nome:hash_hex` al bucket → altrimenti `R_FS_ERROR`.

Il controllo del nome e' **server-side e autorevole**: il client puo' sbagliare,
il server no. **Solo Identis scrive nel bucket**: `create-user` non lo tocca
mai (decide il protocollo, non il formato).

### 4. `IDENTIS_LIST` (0x72): numero utenti

Richiede un token valido (in `w1` della richiesta) e risponde con il numero
di utenti registrati in `w1`. L'elenco dei **nomi** (risposta multi-frame,
lato server) e' rinviato: serve alla shell nativa (Fase 3.2), non al
protocollo di creazione.

### 5. La transazione e' legata al canale

La transazione in corso registra **tag e canale** di chi l'ha aperta. Un altro
client che bussa riceve `R_BUSY` e il suo messaggio **non** viene consumato
come chunk (il buffer del primo client resta intatto). I client **ritentano**
l'intera transazione su `R_BUSY` con attesa a spin puro (mai busy-loop su
syscall: mascherebbe gli interrupt e affamerebbe il timer). Timeout di
inattivita' (5 s) e `EXIT_NOTIFY` (morte del client) scattano la transazione.

### 6. Il bucket `identis:config` resta aperto (debito dichiarato)

La protezione per path del bucket (solo il canale del servizio Identis puo'
aprirlo) e' **rinviata** alla Fase 3.3, insieme alle ACL per utente. Motivo:
con il modello a token bearer, proteggere il bucket *solo* contro i client
non autorizzati avrebbe dato una falsa sensazione di sicurezza (il token
resta in chiaro nell'ambiente del processo). La chiusura deve venire con il
token → capability per-canale, non prima. Il debito e' dichiarato qui e nel
test (`identistest` t11 legge il bucket direttamente: quando il bucket sara'
protetto, quel caso passa per `IDENTIS_LIST`).

### 7. Il bucket home `usr:<nome>` e' rinviato

ADR-0055 §2 prevede un bucket home per utente. Non e' creato in 3.1: `mkdir`
in ArcaFS crea directory **in RAM** (ephemere, ADR-0042) e `open(CREATE)`
crea una chiave **file** (persistenza, ma non puo' contenere file). La scelta
del layout (file vs dir emergente) ha senso solo con il modello di sessione e
l'ACL per utente → Fase 3.2/3.3.

### 8. Il token viaggia in ambiente

`create-user` riceve il token dalla variabile d'ambiente `VELORDO_SESSION`
(hex). E' una **convenzione di dati** nel programma nativo (ADR-0025: niente
strutture kernel), non un meccanismo del kernel. La shell nativa (Fase 3.2)
passera' il token ai programmi che lo richiedono. Il token e' un bearer: chi
legge l'ambiente di un processo puo' usarlo → debito dichiarato, chiuso dalla
capability per-canale in 3.3.

## Consequences

### Positive

- `IDENTIS_ADD_USER` e `IDENTIS_LIST` rispondono davvero: nessun client puo'
  piu' restare appeso su un tag dichiarato e non gestito.
- Il protocollo ha **una sola copia**: client e server non possono divergere
  nel framing.
- Un secondo client non corrompe piu' il buffer del primo (il bug era
  latente: con un solo client non si vedeva mai).
- L'autorizzazione e' **esplicita e verificabile**: solo una sessione root
  crea utenti; il resto e' negato con un codice preciso.
- Il gate copre il protocollo: 11 casi su sessione, verifica, creazione,
  duplicati, nomi non conformi, elenco e formato del bucket.

### Negative / limiti

- Il bucket `identis:config` resta **leggibile e scrivibile da chiunque**
  fino alla Fase 3.3 (debito dichiarato, §6).
- Il token e' un **bearer** e viaggia in chiaro nell'ambiente (§8): chi legge
  l'ambiente di un processo puo' agire come root.
- Sotto TCG il token **non e' crypto-grade** (RDRAND non funziona): la
  casualita' viene dal fallback. Su HW con RDRAND vero il path e' quello.
- `create-user` in 3.1 **non e' lanciato da nessuno**: il gate copre il
  protocollo, non il binario end-to-end. Il lancio arriva con la shell nativa
  (Fase 3.2).
- Ogni chunk e' un round trip (protocollo sincrono): un payload da ~60 byte
  sono ~15 round trip. Accettabile per operazioni rare.

### Neutral

- Il servizio resta **single-threaded** con stato statico (`static mut`,
  `#![allow(static_mut_refs)]`): i processi utente di Velordo non hanno
  thread reali (ADR-0046: 1:1 con thread del kernel).
- La tabella sessioni e' fissa (8 slot): niente alloc, niente frammentazione.

## Alternatives Considered

- **Autorizzazione per hash del binario (`peer_info` vs manifest)**:
  scartato per il ciclo di build (il manifest e' generato dopo i binari: un
  binario che lo include non puo' stare nel manifest) e perche' non distingue
  *chi* esegue `create-user` da *cosa* e' `create-user`.
- **Token emesso da `init` invece che da Identis**: init non conosce le
  credenziali (le ha solo in chiaro al primo boot): separare il servizio
  dalla gestione delle credenziali perde il punto di forza di Identis (un solo
  punto di verita').
- **`create-user` come client diretto del bucket** (scrive `identis:config`
  da solo): scartato: rompe "solo Identis tocca il bucket" e dupplica la
  validazione del nome in due posti.
- **Risposta multi-frame server→client per l'elenco utenti**: rinviata (non
  serve in 3.1; `LIST` conta e basta). Il protocolmo di risposta multi-frame
  richiede un ping-pong ack/reply e complica il server: meglio introdurlo
  quando la shell lo richiede davvero.
- **Nome utente libero (spazi, maiuscole)**: scartato: il nome finisce in una
  riga `nome:hash` e nel path del bucket home; un charset stretto evita
  ambiguita' e injection nelle righe.

## Testing

- Gate: `testland/identistest` → `[identistest] PASS 11/11`: lookup servizio,
  VERIFY ok (con token), VERIFY fallito, ADD_USER senza token, ADD_USER ok,
  duplicato, nome non conforme, nome vuoto, login del nuovo utente, `LIST`,
  formato della riga nel bucket.
- Il gate resta **6 righe di suite + 1 nuova**: testfs 5/5, testfat 7/7,
  testsarca 62/62, posixtests 4/4, **identistest 11/11**, usertests 58/58,
  threadtest 12/12, zero FAIL/PANIC/FAULT.

## Relazioni con altre decisioni

- **ADR-0055** (gestione utenti): questo ADR realizza il protocollo e
  corregge due punti di quel testo (il nome del servizio e' `Identis`, non
  `auth`; la password **non** transita in chiaro: e' il client a hasharla —
  vedi la revisione in quel documento).
- **ADR-0008** (IPC per nome, reply implicita): `Identis` e' un servizio
  risolto per nome; il reply e' implicito, il chunking e' un protocollo
  applicativo sopra.
- **ADR-0037** (policy su identita' + sandbox build): il tetto per canale e'
  il meccanismo usato per il bootstrap; la protezione per path del bucket
  (Fase 3.3) si appoggia allo stesso primitivo.
- **ADR-0025** (meccanismo vs personalita'): il protocollo e' meccanismo
  condiviso; nessun POSIX coinvolto.
