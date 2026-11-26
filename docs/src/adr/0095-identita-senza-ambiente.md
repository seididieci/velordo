# ADR-0095: L'identità del terminale non è un ambiente — il prompt la chiede alla sessione (S3.3d)

## Status

**Accepted** (2026-11-26). Implementata in S3.3d. Gate invariato: `[testfs] 5/5`,
`[testsarca] 64/64`, `[posixtests] 4/4`, `[identistest] 11/11`, `[usertests]
72/72`, `[threadtest] 12/12`, `[shell-native] 33/33`, zero FAIL/PANIC/FAULT.

Chiude il residuo di ADR-0094: dopo S3.3c l'identità era nel server, ma il
**nome** arrivava ancora ai programmi per ambiente. Dipende da ADR-0089/0090
(sessioni), ADR-0092 (DE) e ADR-0094 (identità attestata).

## Contesto

S3.3c ha tolto il token, ma non l'ultima **copia dichiarata** dell'identità:
`identis-login` passava `USER=<nome>` al DE, il DE lo ripassava a ogni shell,
e la shell lo leggeva dallo stack (`env_from_stack`) tenendolo in un buffer
statico. Tre problemi, tutti piccoli ma tutti nella direzione sbagliata:

1. **Un'identità in ambiente è una dichiarazione, non un'attestazione**: chi
   la legge non sa se è vera, e chi la scrive deve ricordarsi di aggiornarla.
2. **Una copia da tenere allineata**: ogni nuovo consumatore (il prompt, il
   DE, la provenienza del consenso) ricopia il nome, e ogni cambio futuro
   dell'identità dovrebbe inseguire tutte le copie.
3. **È l'ultimo pezzo che assomiglia al bearer di ADR-0055**: un valore che
   viaggia per essere creduto.

Il server sa già tutto: la sessione ha l'utente (`SESS_AUTH_SET`) e il kernel
dà il pid. L'ambiente era solo una scorciatoia per non fare una IPC.

## Decisione

**Nessuna variabile d'ambiente.** L'unica fonte del nome utente è l'identità
di sessione, e chi la mostra la **chiede**:

```
interpres::read_user()  ->  civis::sess::auth().as_str()   (una IPC, a ogni prompt)
impluvium::spawn_shell  ->  niente env: l'identità è già nella sessione
identis-login::spawn_de ->  niente env: l'identità è già nella sessione 0
```

`read_user()` non è più una copia con cache: si chiede a ogni prompt, perché
il nome è un fatto del server. Costo: **una IPC per riga** di prompt (la shell
ne fa già diverse per riga: orologio, stato, suggerimento). In cambio, un
`SESS_AUTH_SET` fatto da chiunque, in qualsiasi momento, si vede al prompt
successivo senza codice di sincronizzazione.

Su errore (servizio morto o cache stantia) si butta la cache della sessione
(`forget_current`) e si riprova una volta; se fallisce ancora il prompt resta
**senza nome** — l'output non si perde e non si inventa un'identità.

## Alternative valutate (e perché no, ora)

### `su` come comando esterno (nested shell)

Scartato **per ora**, non perché impossibile ma perché non serve a nessuna
delle tre esigenze reali, che qui sono già coperte meglio:

| esigenza | come la fa Unix | in Velordo |
|---|---|---|
| scalare per **un'operazione** | `sudo`/`su` | **consenso per azione** (Custos + `sys:caps`, ADR-0080 fase 7): l'azione la esegue il servizio con l'autorità di chi ha acconsentito, l'identità non si sposta |
| una **sessione** come un altro utente | `su`, altra tty | **una tile = una sessione**: il DE chiede le credenziali e apre la tile (meccanismo pronto: D8e + `SESS_AUTH_SET`) |
| **declassare** un programma non fidato | `su nobody` | **classe dell'immagine** + floor/ops (S3.4): il programma *nasce* `Untrusted` |
| lanciare **un programma** come un altro utente | `su -c` / `sudo -u` | `runas <utente> -- prog` (vedi sotto) — non c'è trigger oggi |

In Unix `su` esiste perché l'uid è una proprietà del **kernel** per processo
(`setuid` + shell). Qui l'identità è un fatto del **server** e il kernel non ha
né uid né `setuid`: «diventare un altro» non è un'operazione di sistema, è una
scelta di prodotto. Come shell annidata costerebbe la complessità appena
tolta (lettore, history, `exit` che significa due cose, identità per pid) e
insegnerebbe il modello sbagliato («diventi root e ci resti») in un sistema
dove il privilegio si chiede per azione.

### `runas` (la forma che avrebbe senso)

Se e quando servirà «lancia *questo* programma come un altro utente», la forma
è un **launcher senza shell**: verifica le credenziali, prende un'identità per
il proprio albero di processi (`SESS_AUTH_SET` con il proprio pid — l'istanza
per-pid di ADR-0094), `exec` del programma e basta. Niente shell da chiudere,
niente `exit` da reinventare: l'identità muore col processo.

**Trigger dichiarati**: (a) far girare un tool/build come utente non-root;
(b) il DE che apre una tile come un altro utente (che però è **identità di
sessione**, non per-pid). Fino ad allora resta parcheggiato: nessuna feature
dichiarata lo richiede.

## Conseguenze

- **L'ultima identità in ambiente sparisce**: il nome utente esiste solo nel
  server. La catena è chiusa — kernel (pid) → server (sessione + utente) →
  chi lo chiede.
- **`identis-login` e `impluvium` si semplificano**: spariscono `LOGIN_USER`,
  `user: String`, `read_user(sp)`, `pack_args` e l'unico uso di
  `env_from_stack` nella shell.
- **Il prompt è onesto**: se la sessione non è autenticata il prompt è senza
  nome, invece di mostrare un nome che potrebbe essere stantio.
- **Costo**: una IPC per prompt. Misurato nel gate: CPU al prompt invariata
  (0,0-1,0%), nessun risveglio a tick.

## Test

- Gate invariato (`[shell-native] 33/33`): il prompt continua a mostrare il
  nome (`⚡ root@velordo …`), che ora arriva dalla sessione; `users` e
  `create-user` funzionano come prima (identità di sessione).
- `scripts/run-desktop-tests.sh`: le due shell (tile 0 e tile creata da
  `split`) mostrano un'identità — la propagazione `SESS_AUTH_SET` del DE resta
  l'unico canale.
- Doc check: `rg '"USER"|USER=' userland libs` → zero.
