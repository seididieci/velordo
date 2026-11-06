# ADR-0067: Attese di input in userland — futex, non spin

## Status

Accepted (2026-11). Corregge la regressione che teneva la CPU al 100% al
prompt della POSIX shell e al prompt di `create-user` della shell nativa.

## Context

Due attese di input erano diventate busy-wait:

1. **POSIX shell** (`flavours/posix/shell/src/term.rs`, `kbd_read_byte`):
   con la tastiera vuota chiamava `spin_brief()` — 1.000.000 di
   `spin_loop` — per sempre. Al prompt `$ ` la CPU host era al **99,7%**
   (misurato su QEMU, sia KVM sia TCG).
2. **Shell nativa** (`userland/interpres/src/main.rs`,
   `read_line_from_keyboard`): la `futex_wait` era stata sostituita da un
   commento in `0a54272` e la word inutilizzata rimossa in `5cf4244`
   ("rimuove warning"). Risultato identico: **99,7%** ai prompt
   `Nome utente:`/`Password:` di `create-user` e del refresh token.

Il rationale storico (ADR-0011, ADR-0034, ADR-0035) era: «niente
busy-loop su `get_ticks`, perché la syscall maschera gli interrupt;
meglio spin puri con IF=1». È corretto **contro il polling con
`get_ticks`**, ma non contro il futex: `futex_wait` mette il processo in
`Blocked` e restituisce la CPU allo scheduler, mentre il PIT (100 Hz)
continua a girare e risveglia il waiter alla deadline. È lo stesso
meccanismo già usato da `useruptime`, `identis-login` e dal prompt
principale della shell nativa (ADR-0046).

Lo spin resta legittimo solo per bound **brevi e certi** (continuazioni
ESC: microsecondi) e per i retry di boot.

## Decision

1. **Un solo modo di attendere l'utente in userland**: `futex_wait` su una
   word statica (mai scritta, valore 0) con `deadline = get_ticks() + 1`
   (tick assoluti). Il processo va `Blocked`; latenza massima 10 ms, come
   il prompt principale della shell nativa.
2. **POSIX shell** (`term.rs`): `static INPUT_FUTEX: u32 = 0` e
   `wait_input_tick()`; `kbd_read_byte` la chiama quando `read_fs`
   restituisce 0. `spin_brief` resta solo per il retry di `term_init`;
   `spin_esc` (bound ESC) resta invariato.
3. **Shell nativa** (`interpres`): un solo helper `wait_input_tick()`
   (drena `recv_poll` + futex), usato sia da `LineEditor::read` sia da
   `read_line_from_keyboard`: la duplicazione era la causa della
   divergenza (una copia corretto, una copia no).
4. **Guardia di regressione**: i test delle shell campionano la CPU del
   processo QEMU via `/proc/<pid>/stat` e richiedono < 50% a prompt
   (`test-shell-base.py` per `$ `, `test-shell-native.py` per
   `create-user`). Un busy-wait è ~100%, un futex ~0–2%: soglia ampia e
   indipendente dal carico host.

Non cambia: `wait_fg`/`cancel_job` mantengono i loro spin brevi durante un
comando **foreground** (devono alternare IPC e tastiera senza bloccarsi in
`recv`); non è una attesa "idle" ed è fuori da questa decisione.

## Consequences

### Positive

- Prompt POSIX e prompt `create-user`: **99,7% → 0,0–0,3%** di CPU host.
- Nessuna diluizione: mentre la shell dorme, gli altri processi (e idle
  con `hlt`) girano normalmente; il figlio foreground non perde quantum.
- Latenza di input ≤ 10 ms, identica alla shell nativa.

### Negative / limiti

- Resta un **polling a 100 Hz** (nessuna notify IPC tastiera→shell): il
  futex non è event-driven. Un futuro `KBD_NOTIFY` verso il client
  azzererebbe anche quel residuo.
- Ogni prompt deve ricordarsi di usare l'helper; la guardia CPU rende il
  dimenticarlo un test rosso, non un bug silenzioso.

## Verifica

- Misure host (`/proc/<qemu>/stat`, finestra 2–3 s):

  | Stato | Prima | Dopo |
  |---|---|---|
  | POSIX shell, prompt `$ ` | 99,7% | 0,0% |
  | POSIX, dopo `help` | 99,7% | 0,3% |
  | Shell nativa, prompt `>` | 0,0% | 0,0% |
  | `create-user`, `Nome utente:` | 99,7% | 0,3% |
  | `create-user`, `Password:` | 99,7% | 0,0% |

- `test-shell-base.py`: PASS «prompt POSIX idle senza busy-wait».
- `test-shell-native.py`: PASS «prompt create-user idle senza busy-wait»
  e «nome vuoto rifiutato».
- Gate invariato (6 righe PASS, zero FAIL/PANIC/FAULT), unit test 39/39.

## Riferimenti

- ADR-0046 (thread kernel + futex: `futex_wait` con deadline a tick).
- ADR-0034 (line editing: `spin_esc` come bound di microsecondi).
- ADR-0035 (job control: spin di `wait_fg` durante il foreground).
- ADR-0011 (terminale userspace: perché nacque lo spin al posto del
  polling con `get_ticks`).
- `docs/src/14-cronologia-fasi.md`, voce D7/bis.
