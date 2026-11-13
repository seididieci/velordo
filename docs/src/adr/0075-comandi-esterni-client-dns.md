# ADR-0075: Comandi esterni e client DNS nativo

## Status

**Accepted** (2026-11). Implementata: `users` e `logout` sono binari esterni
nel bucket `usr`, `dns-resolve` e' il primo client DNS nativo lanciabile dalla
shell. Nessuna syscall, tag o ABI nuova. I dati strutturati (`VELORDO_DATA`)
sono rinviati: vedi "Non-obiettivi".

## Context

La shell nativa (`userland/interpres`, S3.2a) aveva due built-in che non
dovevano stare li':

- `cmd_users` — elenca gli utenti leggendo `identis:config` **dentro** la
  shell: la shell conosceva il formato del bucket e il protocollo Identis.
- `cmd_logout` — chiama `init_bounce(Service::Login)`.

Entrambi sono **programmi**, non sintassi della shell. Con `try_execute_external`
(S3.2b-4, ADR-0064) la shell sa gia' spawnare un binario dal bucket `usr`
passando `argv` e `env`: tenerli built-in era un residuo storico, non una
scelta.

In parallelo mancava il primo **client di rete** lanciabile dall'utente. Lo
stack funziona (N1-N6.3) e il servizio `dns` risponde (N5/N6.0/N6.3), ma
l'unico client era `testland/dns`, un probe della suite: nessun comando che
l'utente potesse digitare.

Il WIP del ramo (`feature/net_tool`) conteneva entrambe le cose, piu' un
meccanismo di **dati strutturati** (`VELORDO_DATA`: il figlio invia record al
parent, la shell li raccoglie per assegnarli a una variabile) e il documento
di progetto per `let x = exec("prog", "arg")`.

## Decision

### 1. I comandi diventano programmi

`users` e `logout` sono crate in `userland/`, seedati nel bucket `usr`
(`scripts/arca-tool.sh`) e lanciati da `try_execute_external`. La shell perde
`cmd_users`, `cmd_logout`, `print_users_from_bucket` e la costante
`IDENTIS_BUCKET`: non conosce piu' ne' il bucket utenti ne' il protocollo
Identis.

`exit` resta un built-in: non e' un programma, e' "chiudi la shell".

Il token di sessione arriva ai figli via `VELORDO_SESSION` nell'**env**, che
`civis::pack_args(argv, env)` gia' trasporta: la shell non costruisce piu' il
blocco packed a mano.

### 2. `dns-resolve`: client DNS nativo

`userland/dns-resolve` parla al servizio `dns` con il protocollo a endpoint
(ADR-0069) e il codec condiviso `net_ep::Layout::IPV4` (ADR-0074):

- slot TX/RX via `net_ep::write`/`read`/`check`, mai offset a mano;
- query costruita con l'header DNS di `libs/net::dns`;
- risposta interpretata con `net::parse_response`, rcode distinto
  (`NXDOMAIN`/`SERVFAIL`/nessun record);
- attesa con `futex_wait` a deadline di 1 tick (ADR-0067), mai spin;
- uscita: `0` ok, `1` uso/servizio, `2` timeout, `3` parse.

Uso: `dns-resolve <nome> [aaaa]`. Output:
`[dns-resolve] example.com: 93.184.216.34 (A)`.

### 3. La shell non stampa diagnostici dell'interprete per i comandi

`[ast] skip: parse error ...` compariva **prima** dell'output di ogni comando
esterno: il fallback al dispatcher e' il comportamento progettato (ADR-0065),
non un errore. I diagnostici si stampano solo per le righe che sembrano
espressioni (`let`/`if`/`while`/`dump`/`type`, `=`, `(`).

### 4. Il loop di attesa del figlio distingue i tag

`try_execute_external` usciva su **qualunque** messaggio dal canale di nascita
del figlio. Con i dati strutturati in arrivo sarebbe stato un bug: il primo
record avrebbe fatto credere alla shell che il processo fosse terminato. Ora
esce solo su `EXIT_NOTIFY`.

### 5. `logout` ha bisogno di init, e init non era raggiungibile

Estrarre `logout` ha scoperto che il comando **non funzionava** nemmeno come
built-in. `init_bounce` mandava `INIT_BOUNCE` sul canale di nascita
(`CHANNEL_PARENT`), ma la shell e' **ri-parentata** a init quando il login
esce (e' il login a spawnarla, poi termina): quel canale e' morto, quindi il
messaggio non arrivava mai e il sistema restava senza prompt. In piu'
`LoginGate` lascia `pid = -1` nella tabella dei servizi supervisionati dopo un
login riuscito, quindi `handle_bounce` non aveva nulla da uccidere e non
rispawnava niente.

Tre correzioni, tutte nel meccanismo esistente:

1. **init si registra per nome** (`Service::Init`, slot gia' esistente e mai
   registrato). Il gate di `service_register` (Fase 35) ammetteva solo i
   figli di init: ora ammette **anche pid 1**. E' una rilassatura mirata —
   init e' il supervisore, non un figlio di se stesso — e non tocca il resto
   del gate (`Test` aperto, gli altri servizi solo da figli di init).
2. **`civis::init_bounce` risolve `Service::Init` per nome**, con fallback sul
   canale di nascita per i figli diretti di init (i test).
3. **`handle_bounce` rispawna un servizio non vivo** (`pid <= 0`) invece di
   tentare un `kill(-1)`: e' proprio il caso del login dopo `LoginGate`.

`logout` resta un programma: stampa il messaggio ed esce. Il bounce lo fa la
shell, che e' figlia di init, e poi esce — se restasse viva ci sarebbero due
lettori della tastiera accanto al login riavviato.

## Conseguenze

### Positive

- **La shell non conosce i servizi**: `users` e `logout` sono programmi come
  gli altri, sostituibili senza ricompilare la shell.
- **Il client DNS usa il meccanismo**: `net_ep` e `libs/net::dns` sono gia'
  testati (unit host 117/117); `dns-resolve` non reimplementa ne' il layout
  degli slot ne' il parser DNS.
- **Bug eliminati**: il loop di attesa non confonde dati e terminazione; i
  formatter IPv4/IPv6 non usano piu' `static mut` con cache (la seconda
  chiamata restituiva l'indirizzo precedente); `Display` di
  `net::Ipv6Addr` non stampa piu' `:::1` per un indirizzo che **inizia** con
  la compressione (aveva un test solo sul `parse`, mai sul `Display`).
- **`logout` funziona**: prima non riavviava il login (canale di nascita
  morto dopo il ri-parent, e `pid = -1` per `LoginGate`).
- **Nessuna ABI nuova**: syscall, tag e formati su filo invariati. L'unico
  cambio nel kernel e' una rilassatura del gate di `service_register`
  (pid 1 puo' registrare `Service::Init`).

### Negative

- **Due binari in piu'** nel bucket `usr` e nel manifest degli hash
  (`users.bin`, `logout.bin`, `dns-resolve.bin`).
- **`logout` dipende dalla shell**: il programma stampa, ma il bounce lo fa
  la shell (un nipote non ha un canale verso init). Se `logout` venisse
  lanciato da un altro contesto, servirebbe un supervisore.
- **`dns-resolve` richiede `net=1`**: senza il servizio `dns` risponde
  "servizio DNS non disponibile". E' la stessa condizione di N5, fuori dal
  gate.
- **Il client e' testuale**: nessun valore di ritorno per l'interprete.

### Neutral

- `users`/`logout`/`dns-resolve` linkano `civis` (obbligatorio con SSE2,
  ADR-0071: i simboli `mem*` sono li').

## Non-obiettivi

- **Dati strutturati (`VELORDO_DATA`)**: il WIP conteneva `libs/civis/src/value.rs`
  (`Value`, FNV-1a, `parse_structured_data`), i tag `TAG_DATA`/`TAG_DONE`/
  `TAG_ERROR` e il ramo dati in `users`/`dns-resolve`/`interpres`. Il
  trasporto non esisteva (`send_record` inviava solo una lunghezza, senza
  payload ne' nome) e il loop di attesa della shell chiudeva al primo
  messaggio: la modalita' non poteva funzionare. E' stato **rimosso**, non
  lasciato a meta': codice nel meccanismo senza chiamanti ne' collaudo e'
  debito, non progresso. Il progetto (`let x = exec("prog", "arg")`, blob
  `[HEADER][RECORDS][DATA][CRC]`) resta in `ROADMAP.md` fra le Pianificate.
- **`exec()` come primitiva**: richiede il trasporto dati e la tabella di
  variabili dell'interprete (S3.2b/c/d).
- **POSIX `socket`** (N7.1): la personalita' arrivera' sopra `civis::net`.

## Riferimenti

- `docs/src/adr/0064-bucket-binari-piatti.md` (bucket `binary` piatti).
- `docs/src/adr/0065-interprete-rust-like.md` (interprete e fallback).
- `docs/src/adr/0067-attese-input-futex.md` (attese a deadline).
- `docs/src/adr/0069-stack-rete-ipv6-capability.md` (protocollo a endpoint).
- `docs/src/adr/0074-client-rete-meccanismo.md` (`net_ep`, `civis::net`).
- `userland/dns-resolve/src/main.rs`, `userland/users/src/main.rs`,
  `userland/logout/src/main.rs`, `userland/interpres/src/main.rs`.
- `kernel/src/syscall/service.rs` (gate di `service_register`),
  `userland/rector/src/main.rs` (registrazione di `Init`, `handle_bounce`),
  `libs/civis/src/spawn.rs` (`init_bounce`).
