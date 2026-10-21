# ADR-0055: User and Authentication Management (S3.x)

## Status

Proposed

## Revisione 1 (Fase 3.1, 21 ottobre 2026)

Il testo originale resta valido come **disegno**; la realizzazione ne ha
chiarito tre punti, corretti qui (le ADR non si riscrivono: si annotano).

1. **Il servizio si chiama `Identis`, non `auth`.** Il bucket e'
   `identis:config` (non `auth:config`). Il nome `auth` era un segnaposto: il
   servizio copre identita' *e* autenticazione, e "auth" si prestava a
   confusione con il meccanismo di autenticazione del canale IPC (che e'
   altro: capability per canale, ADR-0008).
2. **La password NON transita in chiaro.** Il testo originale diceva che il
   servizio ricevesse la password in chiaro e la hashasse. La realizzazione
   fa il contrario: e' il **client** a calcolare l'hash BLAKE2s-256 in locale
   e a mandare **solo l'hash** (32 byte). Motivo: la password non deve
   attraversare l'IPC (ne' la RAM del servizio, ne' un log). Il server
   confronta hash con hash.
3. **Il modello di sessione e' un token bearer.** Al login riuscito il
   servizio emette un token di sessione (w1 della reply), con TTL; la
   creazione utente richiede una sessione **root**. I dettagli (canale
   legato alla transazione, codici, LIST) sono in
   [ADR-0057](0057-creazione-utenti-token-sessione.md).

Due punti restano **rinviati** (era previsto, ora e' esplicito):

- il **bucket home `usr:<nome>`** non e' creato in 3.1 (`mkdir` in ArcaFS e'
  in RAM, `open(CREATE)` crea una chiave file: la scelta del layout ha senso
  solo con il modello di sessione e l'ACL per utente → Fase 3.2/3.3);
- le **ACL in userspace** sul bucket home (proprietario vs altri) e la
  **protezione del bucket `identis:config`** (solo il canale di Identis puo'
  aprirlo) sono debiti dichiarati, con chiusura in Fase 3.3 insieme al
  passaggio da token bearer a capability per-canale.

## Context

Velordo is a microkernel x86_64 written in Rust (ADR-0005). Architettura microkernel: il kernel contiene scheduling, IPC, gestione della memoria e routing degli interrupt; driver e servizi (console, file system, devfs, shell) sono processi userspace che comunicano via IPC per nome con reply implicita, supporto async e trasferimento dati zero-copy su ring SPSC per-processo. Lo scheduler è unico RT a 32 priorità con Constant Bandwidth Server (CBS) per la bandwidth reservation.

Non c'è supporto POSIX nel meccanismo (`civis`, `civis::posix` è personalità separata, ADR-0041). Non ci sono UID/GID tradizionali, né `/etc/passwd` né `/etc/shadow`. L'identità utente si basa su capability IPC e token di sessione. L'obiettivo è un sistema di gestione utenti che:

- Memorizzi credentials in modo sicuro (hashate, senza cifratura per il momento).
- Isolamenti i bucket ArcaFS per utente (`usr:<nome>`).
- Fornisca un meccanismo di login/logout via console seriale/shell.
- Definisca ACL di default per i bucket utente (proprietario vs altri).
- Separi chiaramente il primo utente (`root`) dagli utenti successivi.
- Rimanga semplice da estendere con cifratura in futuro.

Non esistevano precedenti nel codice: non c'è `/etc/passwd`, non ci sono sysuser tradizionali, il kernel non gestisce nomi utente. Tutto deve essere implementato in userspace sopra ArcaFS e i servizi IPC.

## Decisioni

### 1. Bucket di configurazione di sistema: `auth:config`
- Il bucket di sistema risiede nel namespace ArcaFS con chiave `auth:config`.
- Contiene entry hashate per utente (nome utente → hash BLAKE2s-256 della password).
- È leggibile e scrivibile **solo** dal processo `auth` in userspace.
- Non èaccessibile direttamente dagli utenti né dai binari di sistema tramite mcopy/open FS.
- Motivo: centralizzare la gestione auth in un unico servizio affidabile, evitare che binari casuali possano leggere/modificare le credenziali.

## 2. Home bucket per utente: `usr:<nome>`
- Ogni utente ha un bucket dedicato `usr:<nome>` dentro il namespace `ns` di ArcaFS (es. `usr:alice`).
- Il bucket contiene i file personali dell'utente (configurazione, dati, ecc.).
- L'accesso è riservato al proprietario dell'account (e a `root`/`init` per scopi di sistema).
- Il bucket viene "montato" nella sessione utente al login, associabile a un percorso shell `/home/<nome>` (da implementare nella shell native).

## 3. Processo `auth` e flusso di autenticazione
- Esiste un processo userspace `auth` che rimane in esecuzione fin dal boot (lanciato da `init`).
- `auth` detiene il bucket `auth:config` e gestisce:
  - Creazione nuovo utente (`add_user <nome> <password_hash>`).
  - Verifica password all'accesso (`check_password <nome> <password_in_chiaro>`).
  - Revoca accesso al bucket home in caso di logout/blocco.
- Il processo `init` avvia `auth` al boot e si occupa della procedura di *first-boot setup*:
  - Al primo avvio, `init` crea l'utente `root`, imposta la password (chiesta all'utente alla console), crea il bucket `usr:root` e avvia `auth`.
  - Successivamente, ulteriori utenti possono essere aggiunti tramite il binario `create-user` (o equivalente), che interagisce con `auth` via IPC.
- La procedura di login:
  1. L'utente digita nome utente e password alla console/shell.
  2. `auth` riceve la richiesta, legge l'hash dal bucket `auth:config`, lo confronta con l'hash della password digitata.
  3. Se corrispondente, `auth` concede all'utente l'accesso al proprio bucket `usr:<nome>` e avvia la shell.
  4. Se fallisce, `auth` rifiuta l'accesso e ripete la richiesta (massimo 3 tentativi, poi blocco temporaneo).

## 4. Password e hashing
- Le password sono memorizzate **hashate con BLAKE2s-256** (già disponibile in `blake2s/` in repo).
- Non viene utilizzato salt per semplicità iniziale; si può aggiungere in futuro.
- La password in chiaro non viene mai scritta su disco (tranne il momento dell'impostazione iniziale da parte di `init`).
- Il confronto avviene in userspace: `auth` riceve la password in chiaro, la hashata e confronta con l'hash memorizzato.

## 5. ACL di default per bucket utente
- Per ora le ACL sono implementate **in userspace** dal servizio `auth`, non nel motore ArcaFS.
- La politica predefinita è: **solo il proprietario del bucket può leggere/scrittura**; `root` e `init` hanno accesso completo per scopi di gestione.
- L'adesione formale nel motore ArcaFS (separazione a livello di kernel) è rimandata a un ADP successivo, non appena si rende necessario per performance o funzionalità avanzate.
- I diritti vengono ereditati attraverso `spawn`: se `init` lancia un processo, il figlio eredita i diritti del genitore (stesso meccanismo già usato per canali IPC e TSS).

## 6. Sessione e logout
- Una sessione utente è legata al canale IPC che collega l'utente al servizio `auth` e al proprio bucket home.
- Il **logout** avviene quando l'utente digita il comando `logout` o quando il shell rileva la chiusura della console: `auth` revoca i canali associati all'utente, il bucket home diventa di nuovo inaccessibile e il processo shell termina.
- Non vi è concetto di "utenti multipli simultanei" nella versione iniziale: la shell è single-user per sessione, ma possono esistere più account definiti nel sistema.

## 6. Future extension points
- **Cifratura del bucket utente:** quando attivata, la chiave del bucket verrà unwrappata con la password dell'utente. La struttura attuale (`usr:<nome>`) rimane invariata.
- **Gruppi utente:** possibilità di associare più utenti a uno stesso gruppo per diritti condivisi.
- **Password con salt:** aggiunta di un sale casuale per l'hashing, stored accanto all'hash nel bucket `auth:config`.
- **Integrazione POSIX:** eventuali `chmod`/`chown` a livello shell possono essere implementati sopra questi bucket in un secondo momento, senza toccare il nucleo delle decisioni attuali.

## Conseguenze

- **Semplice modello di minaccia:** le uniche credenziali risiedono nel bucket `auth:config`, accessibile solo al servizio `auth`. Non ci sono file di password globali leggibili.
- **Isolamento per utente:** i dati di ciascun utente risiedono nel proprio bucket `usr:<nome>`, evitando interferenze.
- **Nessun file `/etc/passwd`/etc/shadow:** l'identità è basata su capability IPC, non su numeri.
- **Complessità userspace:** serve un processo `auth` in esecuzione permanente e una piccola logica di login/logout nella shell.
- **Futibile estensione:** la cifratura del bucket utente può essere aggiunta in seguito senza cambiare la struttura dei bucket né il flusso di login.
- **ACL differite:** l'implementazione formale nel motore ArcaFS è rinviata; per il momento la logica è tutta in userspace, il che è più facile da iterare e debuggare.

## Relazioni con altre decisioni

- Si affianca a **ADR-0041** (separazione meccanismo/personalità POSIX): l'identità utente non è legata a UID/GID POSIX, ma a capability IPC.
- Si relaziona a **ADR-0008** (IPC per nome, registry + channel): i canali di auth e home bucket sono risolti per nome.
- È indipendente da **ADR-0047** (PAL std velordo): le password e l'autenticazione sono a livello di servizio userspace, non nel kernel.
- Le decisioni di **F1** (staticone su ArcaFS) e **F2a** (mini-sysroot) non sono influenzate da questo ADP, ma l'eventuale aggiunta di cifratura ai bucket utente potrebbe influenzare il lavoro F2a in un secondo momento.

## Status

Proposto. Le decisioni sopra definite possono essere implementate iterativamente:
1. Implementare il processo `auth` e il bucket `auth:config`.
2. Implementare la creazione utente e il primo boot da parte di `init`.
3. Implementare la shell nativa e il login/logout.
4. Aggiungere le ACL formali nel motore ArcaFS (opzionale, rinviabile).
5. Eventuale aggiunta di cifratura del bucket utente.

---
*Registro delle modifiche: primo redatto il 20 ottobre 2026 per il ciclo F1+F2a di Velordo.*