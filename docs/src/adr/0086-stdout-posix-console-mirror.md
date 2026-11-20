# ADR-0086: Output dei programmi POSIX sulla console — il mirror seriale sta nel writer

## Status

**Accepted** (2026-11-20). **Implementata e verificata** (gate verde, `runhello`
dalla shell appare nel terminale e nel log seriale).

Dipende da ADR-0041 (personalità POSIX + hook `civis::persona`), ADR-0060
(console virtuale, device `/dev/console`), ADR-0062 (terminale pixel e gate),
ADR-0081 (gate end-to-end).

## Context

Il default stdout di un programma POSIX (`civis::println!`) passa da
`print::flush` → hook `civis::persona::route_out` →
`flavours/posix/libr/src/stdio.rs::route_out`. `route_out` instradava **solo**
quando un redirect era attivo (`STDIO[1] >= 0`); senza redirect ritornava
`false`, e `print::flush` cadeva su `sys::write(CONSOLE_OUT)` — che nel kernel
(`kernel/src/syscall/misc.rs`) scrive **solo sul seriale**.

Conseguenza: il terminale (pixel o VGA-testuale) mostrava la shell (che scrive
esplicitamente sul device) ma **non** l'output dei programmi lanciati da essa
(`runhello`, e qualunque cosa futura). L'output esisteva, ma solo nel log
seriale. Il doc di `runhello` lo dichiarava apertamente: "non apre alcun
device".

Nel repo il pattern affermato è **"scrive il writer, non la console"**: i
programmi interattivi nativi (`identis-login`, `interpres`, `logout`) hanno una
`console_write` che fa `write_fs(/dev/console)` **e** `print_str` (seriale); il
console server su `DEV_WRITE` fa **solo** `term.feed` (nessun mirror). La shell
POSIX, per la sua parte, fa lo stesso in `term_write_bytes`. Il percorso POSIX
via `println!` era l'unico a non seguire il pattern.

## Decision

Il ramo **default** di `route_out` (`STDIO[1] < 0`) diventa il ramo "terminale":
scrive sul device `/dev/console` **e** specchia sul seriale, esattamente come i
writer nativi.

```rust
pub(crate) fn route_out(bytes: &[u8]) -> bool {
    let fd = STDIO[1].load(Ordering::Relaxed);
    if fd >= 0 {
        // redirect: file-only (invariato)
        ...
    }
    // default: terminale = device + mirror seriale
    let Some(cfd) = console_fd() else { return false };
    if write_fs(cfd, bytes, bytes.len()).is_err() {
        CONSOLE_FD.store(0, Ordering::Relaxed); // fd stantio, riproverà
    }
    let _ = sys::write(1, bytes.as_ptr(), bytes.len());
    true
}
```

Punti fermi:

1. **Le due metà sono inscindibili.** `print::flush` scrive sul seriale solo se
   `route_out` ritorna `false`. Ritornare `true` dopo la write sul device
   **senza** il mirror farebbe sparire l'output dal seriale: il gate perderebbe
   `[posixtests] PASS 4/4` e `runhello: hello` (`scripts/test-shell-native.py`
   legge il log seriale). Il mirror e la write sul device vivono nella stessa
   funzione, quindi nello stesso commit per costruzione.
2. **Fail-open.** Se `/dev/console` non è apribile, `route_out` ritorna `false`
   e `print::flush` scrive il seriale come prima: nessun output perso.
3. **Il redirect resta invariato**: `STDIO[1] >= 0` → file-only, mai seriale
   (la separazione dei sink è di ADR-0031).
4. **Apertura lazy, cache del solo successo.** Il fd di `/dev/console` è risolto
   al primo `println!` (`civis::open("/dev/console", 0)`) e cachato in un
   `AtomicI64` (0 = non risolto, > 0 = fd + 1). Un fallimento non avvelena la
   cache: una console non ancora registrata al primo output viene ritentata.

Nessuna modifica a kernel, console server, `porta`, shell nativa o login.

## Consequences

### Positive

- `runhello` (e ogni programma POSIX via `civis::println!`) appare **sia** nel
  terminale **sia** nel log seriale: nessun output perso, gate invariato.
- Coerenza col pattern esistente: un solo posto in più che fa "device + serial",
  nessun doppio mirror, nessuna modifica al percorso interattivo nativo che oggi
  è verde.
- Il seriale resta il contratto del gate: le suite POSIX continuano a essere
  verificabili a testo.

### Negative

- Il primo `println!` di un programma POSIX paga un `open` (IPC verso cardo): una
  volta per processo, trascurabile.
- Se la console riavvia, il fd cachato diventa stantio: la write fallisce, la
  cache si invalida e il prossimo output riapre. Nel frattempo l'output è
  comunque sul seriale (il mirror è incondizionato).
- Durante la suite (prima del login) l'output POSIX ora disegna anche sul
  terminale: effetto visivo innocuo (il login riscrive il proprio banner).

### Neutral

- Nessuna syscall, tag, ABI o permesso nuovo. Modifica confinata a
  `flavours/posix/libr/src/stdio.rs`.
- **Fuori scope dichiarato**: il sink PAL `std::io::stdout`
  (`pal/velordo/new/sys/stdio/velordo.rs` → `write_console` → `SYS_WRITE`) resta
  seriale-only; il percorso `std` avrà il suo routing dedicato.

## Bug latenti scoperti dal gate (e corretti)

Il sink funzionava subito (`[posixtests] PASS 4/4`, righe specchiate sul
seriale), ma il gate è diventato rosso **dopo** — due difetti preesistenti che
il terminale prima quasi vuoto mascherava.

### 1. `t63`: l'invariante del re-adattamento era sbagliato

Il test pretendeva `non_empty` **identico** dopo il modeset. La finestra che
**cresce** (128×36 → 192×49) riporta a video lo **scrollback**: le celle non
vuote aumentano (900 → 1221) perché riappaiono righe vecchie *sopra* il testo.
Il flusso non cambia: **cresce in testa**. L'invariante giusto è "la coda del
flusso è quella di prima" → `Grid::text_crc_last(limit)` (CRC delle ultime
`limit` celle non vuote) e `CONSOLE_STATE` con `w0` = limite nella richiesta
(`0` = tutto). Unit test host in `libs/term`
(`crescendo_riappare_lo_scrollback_ma_il_testo_e_in_coda`). Il test passava
solo perché prima del login il terminale era quasi vuoto (22 celle di banner):
l'output POSIX ora lo riempie di storia.

### 2. Deadlock cardo ↔ console su `EXIT_NOTIFY`

L'handler della console re-registrava `/dev/console` a **ogni** morte di peer.
Ma:

- ogni processo che stampa è un peer della console (canale implicito di
  `CONSOLE_OUT`), e la console è peer di cardo;
- alla morte di un test (che col mount non c'entra) la console entrava in
  `fs_register` **sincrono**, mentre cardo era in attesa di una write verso la
  console stessa.

Ciclo: `cardo` `BlockedOnReply wait=console`, `console` `BlockedOnReply
wait=cardo`; `rector` appeso → login mai al prompt (gate FAIL per timeout).
Fix: si reagisce **solo** se il morto è il peer FS, con il pid misurato mentre
il canale è vivo (`fs_peer_pid()`: alla morte il canale è già invalidato). È lo
stesso filtro per pid che `porta` applica ai suoi sottoscrittori (ADR-0082).

**Lezione**: la console ha due classi di peer (chi la monta e chi le scrive);
un handler "peer morto" che non distingue le due classi trasforma un evento
innocuo in un ciclo IPC. `[block]`/`[vela]`/`[kbd]` assumono cardo come unico
peer mortale — per la console non vale.

## Alternatives Considered

- **Far specchiare la console** (il console server fa `term.feed` + mirror sul
  seriale): obbligherebbe a rimuovere i mirror espliciti dei writer nativi
  (`login`/`interpres`/`logout`/`term_write_bytes`) per evitare i doppioni, e
  farebbe finire **anche l'eco dei tasti** sul seriale (oggi console-only by
  design). Tocca il percorso interattivo verde e sposta il budget del mirror.
  Scartato: più invasivo, stesso risultato.
- **Non fare nulla** (output POSIX solo seriale): il terminale non mostra i
  programmi, cioè metà dell'esperienza di una console. Scartato.
- **Aprire `/dev/console` in `__entry_prepare`** per ogni programma POSIX:
  pagherebbe un `open` anche per chi non scrive mai e cambierebbe il valore di
  default di `stdout_fd()` (che la shell usa come hook B1). Scartato in favore
  dell'apertura lazy nel solo ramo default.

## References

- Implementazione: `flavours/posix/libr/src/stdio.rs` (`route_out`,
  `console_fd`, `CONSOLE_PATH`, `CONSOLE_FD`); `libs/term/src/grid.rs`
  (`text_crc_last`); `userland/console/src/main.rs` (`CONSOLE_STATE` con
  limite, `EXIT_NOTIFY` filtrato su `fs_peer_pid`);
  `testland/usertests/src/t_display.rs` (t63, invariante di coda).
- Pattern nativo di riferimento: `userland/identis-login/src/main.rs`,
  `userland/interpres/src/main.rs`, `userland/logout/src/main.rs`
  (`console_write`), `flavours/posix/shell/src/term.rs` (`term_write_bytes`).
- Gate: `run-tests.sh`, `scripts/test-shell-native.py` (check `runhello`),
  `scripts/test-shell-redirect.py` (redirect invariato).
- ADR-0031 (fd handoff + redirect), ADR-0041 (personalità POSIX), ADR-0060
  (console virtuale), ADR-0062 (display di default e login-gate).
