# ADR-0062: il display di default e il login come gate

## Status

Accepted. Implementata (Fase 59, 22 ottobre 2026). Dipende da ADR-0060
(console virtuale), ADR-0061 (modeset e host-sync), ADR-0055/0058
(gestione utenti).

## Context

Tre difetti raccolti nella stessa sessione di prove manuali (terminale
pixel, QEMU con UI GTK):

1. **La finestra mostrava il boot stantio.** Con la VGA presente
   (`-vga` di default di QEMU) la **console 0** e' la VGA: il terminale
   pixel (console 1, virtio-gpu) non ha finestra, e quello che si vede e' il
   testo di boot scritto dal kernel nella VGA — che nel path pixel la
   console non scrive piu'. Il terminale c'era, ma **non lo si vedeva**.

2. **Il login non era un gate.** Il programma di autenticazione chiedeva
   le credenziali e poi **lanciava la shell comunque**: un login fallito
   apriva la shell come se fosse riuscito (l'utente entrava senza
   autenticarsi). Il codice di uscita del login non diceva niente a nessuno:
   `init` lo avviava e non lo guardava.

3. **Il backspace non cancellava nel campo utente.** L'eco della
   cancellazione esisteva solo nel campo **mascherato** (password): nel
   campo username non c'era, e lo schermo mostrava piu' caratteri di quanti
   ne restassero nel buffer — cioe' il login autenticava una stringa
   **diversa** da quella mostrata.

In più, un quarto difetto emerso dal confronto fra il terminale e il
**mirror seriale**: la sequenza di cancellazione `\x08 \x08` (3 byte)
arrivava sul filo come **7 byte**. La ragione: il driver `uart_16550`
implementa `send()` espandendo il backspace nella sequenza da terminale
"BS SP BS" (comportamento giusto per un *console*), e il percorso raw del
kernel (`serial::_write_bytes`, usato da `sys_write`) lo usava: la
promessa del codice — "byte in = byte sul filo", l'audit che rende il
mirror confrontabile col terminale — era falsa.

## Decision

### 1. Il display di default e' il terminale pixel

`run.sh` espone `FB_WINDOW`:

| `FB_WINDOW` | Effetto |
|---|---|
| `auto` (default) | `-vga none` se si vuole un display **grafico** (`RUN_DISPLAY != none`) **e** il device virtio-gpu e' presente; altrimenti VGA presente |
| `1` | `-vga none` sempre (forza il terminale pixel) |
| `0` | VGA presente (forza il fallback testuale della console) |

La regola: **una sola console visibile**. Con la VGA presente QEMU apre
la console 0 (VGA) e la console 1 (virtio-gpu) resta senza finestra: due
console, una sola visibile, e quella visibile e' quella **morta**. Tolta
la VGA, il virtio-gpu diventa la console 0 e la finestra mostra il
terminale.

La condizione "il device virtio-gpu e' presente" non e' un dettaglio: senza
il device la console non ha **dove** scrivere (il fallback "VGA testuale"
richiede il device VGA, che `auto` ha appena tolto) → con `VIRTIO_GPU=0`
`auto` **non** toglie la VGA.

`FB_WINDOW=1` resta il forcing esplicito per gli headless (dump
`screendump` della superficie virtio-gpu).

### 2. Il login e' il gate: la shell parte solo dopo l'autenticazione

- Il login prova fino a **3 volte** (`MAX_ATTEMPTS`): l'utente ha diritto
  di sbagliare la password; il login **riprova**, non chiude.
- La shell viene lanciata **solo** dopo un `SUCCESS` (verifica via
  Identis: la password non transita mai in chiaro, l'hash resta in locale
  e viaggia nel payload).
- Esauriti i tentativi il login esce con **codice != 0** e **nessuna
  shell**: il fallimento è visibile.

### 3. `init` supervisiona il login come un getty

Il login è un **servizio registrato** (`Service::Login` = 13) e `init` lo
supervisiona con una **politica di morte propria** (`OnDeath::LoginGate`):

- uscita con codice 0 (shell avviata) → **nessun riavvio**: il login ha
  fatto il suo lavoro;
- uscita con codice != 0 (accesso negato) → **riavvio**, come un getty:
  l'utente trova un altro prompt invece di un sistema muto. Il backoff
  evita la tempesta di riavvii (pausa + tetto ai riavvii: dopo 3 in
  300 tick il login resta **HELD**, accesso negato).

La supervisione è **separata** da quella dei servizi: un servizio che
muore viene riavviato **sempre**; il login solo se ha **fallito**.

### 4. Il mirror seriale è byte-fedele

`serial::_write_bytes` (il percorso raw di `sys_write`) usa `send_raw`:
il byte che il programma scrive arriva sul filo **identico**. L'espansione
del backspace resta a `send()`, cioè nel percorso **di testo** del kernel
(`serial_print!`), dove è un comportamento da console e non un audit.

Corollario: la sequenza di cancellazione del login (`\x08 \x08`, 3 byte)
sul filo sono 3 byte, e il mirror seriale combacia con il terminale.

### 5. L'eco della cancellazione c'è in ogni campo

Il backspace stampa `\x08 \x08` (BS SP BS: il carattere sotto il cursore
viene coperto da uno spazio e il cursore torna indietro) **sia** nel campo
mascherato **sia** in quello non mascherato. Senza, lo schermo e il buffer
diventano due storie diverse e il login autentica una stringa diversa da
quella mostrata.

## Consequences

### Positive

- `RUN_DISPLAY=gtk ./run.sh` mostra **il terminale**, non il testo di boot
  nella VGA.
- Un login fallito **non** apre la shell; l'utente deve autenticarsi.
- Dopo aver sbagliato la password l'utente trova **un altro prompt**
  (il login viene riavviato da `init`), non un sistema muto.
- Il campo username mostra **esattamente** quello che verrà
  autenticato.
- Il mirror seriale e' confrontabile con il terminale byte per byte.

### Negative

- Con `-vga none` il **fallback testuale** della console non ha dove
  scrivere: `FB_WINDOW=auto` lo evita (il device e' la condizione), ma
  `FB_WINDOW=1` con `VIRTIO_GPU=0` e' una combinazione incoerente (lo
  dichiara esplicitamente `run.sh`).
- Il login fallito ha **un costo**: 3 tentativi + riavvio. La pausa di
  backoff e' voluta (una tempesta di riavvii spammerebbe il log e
  burnerebbe CPU).
- Il gate di regressione **non raggiunge piu' la shell**: il boot finisce
  al prompt del login (che aspetta la tastiera) e il gate si chiude col
  timeout, come prima (il contratto del gate sono le righe `PASS`).

### Neutral

- `Service::Login` = 13 aggiunge uno slot di servizio nel registry del
  kernel (nessuno lo usava: il login non era risolvibile **per nome**).
- Il numero di tentativi (`MAX_ATTEMPTS`) è nel login, non nel kernel: il
  kernel resta neutrale (ADR-0025).

## Alternatives Considered

- **Alternative 1**: lasciare la VGA e mostrare il terminale in una
  seconda finestra (`-display sdl` con finestre multiple) - Rejected
  because QEMU apre una finestra **per console**, non per device: con la
  VGA presente la finestra del terminale non c'e'. Il problema e' la
  *console 0*, non la finestra.
- **Alternative 2**: far partire la shell dal kernel dopo il login
  (cioe' togliere il gate al programma di autenticazione) - Rejected
  because il gate diventa una feature del boot e non del programma di
  autenticazione: l'utente che sbaglia la password deve restare fuori,
  e la shell nativa (Fase 3.2) erediterà il token di sessione dal login —
  il login deve restare **davanti** alla shell.
- **Alternative 3**: riavviare il login dentro il login (loop infinito)
  invece che in `init` - Rejected because un processo che si riavvia da
  se' non ha un supervisore: la sua morte non e' distinguibile da un
  crash, e `init` non vede nulla. Il getty e' **fuori** dal login, per
  costruzione (stesso principio del restart dei servizi, Fase 14).
- **Alternative 4**: filtrare i byte di controllo nel mirror seriale
  (scartare 0x08/0x7F) - Rejected because il mirror deve essere
  **fedele**: filtrare nasconde la differenza fra schermo e filo invece di
  eliminarla. Il problema era nel percorso raw, e lì si e' risolto.

## References

- `run.sh` (`FB_WINDOW`), `userland/identis-login/src/main.rs`
  (login-gate + eco), `userland/create-user/src/main.rs` (eco),
  `userland/rector/src/main.rs` (`OnDeath::LoginGate`, `restart_login`),
  `kernel/src/serial.rs` (`send_raw` sul percorso raw),
  `kernel/src/syscall/service.rs` + `syscall-numbers` (`Service::Login`).
- ADR-0055/0058 (gestione utenti, token di sessione), ADR-0060/0061
  (console virtuale, modeset), ADR-0025 (il kernel resta neutro).
