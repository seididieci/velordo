# ADR-0028: `exec` in-place + shell che lancia programmi

## Status

Accepted (implementata, Fase 37 — gate 5/5 + 7/7 + 52/52 + shell 37/37,
zero FAIL/PANIC/FAULT).

## Context

Velordo crea processi con `spawn` (embedded) e `spawn_image` (da disco, Fase
21) e li duplica con `fork` (Fase 34, ADR-0024): entrambi danno un processo
*nuovo*. Manca la sostituzione *in-place* dell'immagine del chiamante
(stesso PID): senza, la shell non puo' lanciare programmi di terzi (motivo
threat-model di ADR-0026: con `exec` il modello cooperativo non regge piu',
e lo Strato 1+2 e' stato costruito apposta prima). `spawn_image` e' spesso
descritto come "come fork+exec", ma crea un NUOVO processo — la distinzione
e' semantica, non solo di performance (supervisione init, job control,
`peer_info` stabile per i server).

Vincoli ereditati: il kernel non tocca mai il FS (ADR-0005 — niente exec da
path nel kernel, la M2a file-backed resta chiusa dalla Fase 30); i canali
IPC non sono ereditabili/duplicabili (ADR-0008/0024); `argv`/`envp` sono
convenzioni POSIX che non devono diventare struttura (ADR-0025).

## Decision

**`SYS_EXEC` (48): semantica POSIX-like, meccanismo microkernel.**

- Sostituzione in-place: stesso PID/parent/priorita'/canali (fd server-side,
  code IPC e registrazioni sopravvivono: sono indicizzati per canale, non
  per immagine); cade TUTTO l'address space e ne viene caricato uno nuovo
  dai byte in memoria del chiamante; stack nuovo; `image_hash` rimisurato;
  porte I/O azzerate (least privilege, come il fork); nome display invariato
  (lo cambiera' la shell via argv[0] in futuro, mai il kernel).
- Argomenti `(img_ptr, img_len, args_ptr, args_len)` — registri esistenti,
  nessuna nuova ABI: `args` = blocco `[argc:8][payload NUL-separated]` entro
  `ARGS_MAX` (8 KiB, single source in `syscall-numbers`); `(0, 0)` = argc=0
  (`exec_image` resta il fast path). Il kernel COPIA byte+args in heap prima
  del teardown (le sorgenti user spariscono con lo spazio) e VALIDA tutto
  prima di toccare qualunque stato (ELF malformato o args malformati = -1,
  processo intatto). Successo = nessun ritorno (frame syscall riscritto:
  RIP→entry, RSP→stack nuovo, `sysretq` atterra nella nuova immagine).
- Teardown con PML4 tenuto + TLB flush (`exec_clear_user`: foglie owned via
  `deref`, page-table private liberate, entry azzerate); reset bookkeeping
  (`HEAP_BRK`, VMA con ref shm rilasciati, ring, text vecchia); reload via
  `elf::load` (stesso percorso dello spawn: text sharing, W^X, NX).
  Mai smantellare sotto i propri piedi (disciplina reclaim Fase 14): niente
  blocking e IF=0 per tutta l'operazione.
- **Stack argv stile Linux come CONVENZIONE DI DATI neutra** (ADR-0025
  §Neutral: formato versionabile, mai struttura): stringhe in alto, array
  `argv[]`+NULL, envp NULL (terminatore presente, vuoto per disegno — env
  vero col posix-server futuro), argc in basso, `rsp % 16 == 8`. Confermato
  dai fatti e non solo dallo standard: le stringhe SOTTO rsp finirebbero
  nella red zone (clobberate dal prologo). `setup_user_stack` scrive lo
  stesso layout degenere (argc=0) per OGNI spawn: ogni processo nasce con
  argc valido, i vecchi `_start` lo ignorano.
- **`libr::entry!` (CRT minimale esplicito, NON uno strato runtime)**:
  naked shim (`mov rdi, rsp` + `jmp`, rsp invariato prima di qualunque
  prologo) + `main(sp)` + `args_from_stack` con bound e validazione
  (spazzatura → `None` → exit loud, mai UB). Migrazione meccanica 19/19
  `_start`. Dettagli tecnici: root `#[used]` + operando `sym` (il `jmp`
  testuale non risolve il mangling e `--gc-sections` scarterebbe `real_main`).
- **`libr::exec(path, argv)` = `load_file` + serializza + `exec_image_args`**
  (piu' `serialize_argv` pubblica per chi carica prima del fork: la shell —
  il figlio post-fork ha l'FS avvelenato e non puo' piu' caricare).
- **Shell (37.2)**: `run <path> [args...] [&]`, `jobs`, `wait [pid]` su
  `EXIT_NOTIFY` (nessun `wait` kernel: la notifica unificata basta). Il
  parent carica file+argv prima del fork (byte COW-condivisi in lettura);
  job non-detached (muoiono con la shell); fg annuncia `[exit N]` se N != 0,
  `&` prompt subito. Path esatti richiesti (niente ricerca/PATH — `cannot
  load` e' corretto). Niente job control interattivo (foreground senza
  scampo: i longevi con `&`; segnali/redirezioni → posix-server futuro).
  Nuovo `userland/runhello` (`/bin`, NON servizio: stampa argv, `fail`→3).

## Consequences

### Positive

- Chiude il cerchio POSIX-like con `fork` (34): la shell lancia programmi
  di terzi con job control minimale, senza toccare il kernel oltre `exec`.
- `peer_info` resta veritiero attraverso l'exec (rimisura obbligatoria,
  non opzionale: senza, la regola same-image 36.5 sarebbe bypassabile con
  l'exec di un binario diverso tenendo l'hash vecchio).
- Zero cambi di protocollo; la suite copre nucleo (stesso PID, hash,
  operativita') e argv (report dal fresh `_start`).

### Negative

- OOM a load = panic come `create_user` (proprieta' pre-esistente ereditata,
  non introdotta: `map_private` fa `expect` — il teardown e' gia' avvenuto,
  mai introdotto un nuovo modo di fallire a meta' oltre quelli esistenti).
- `exec` e' cross-cutting come `fork` (contesto + walk + risorse): la
  superficie di bug resta ampia, coperta da t52 ma non da fuzzing.
- La shell eredita i limiti del modello: niente redirezioni fd in 37 (solo
  eredita'), niente foreground killable, `run` senza ricerca path.

### Neutral

- `fork` + `exec` separati (non `posix_spawn` atomico): l'intervallo e'
  osservabile, come in POSIX; la shell lo usa per jobbookkeeping pre-exec.
- t52 resta a 52 test totali (esteso, non duplicato); shell coperta da
  `test-shell.py` (30/30 → 37/37), non dalla suite.

## Alternatives Considered

- **spawn+exit in userspace** (fork, il figlio fa `spawn_image` di un nuovo
  processo e il padre esce): scartata — cambia PID (rompe supervisione init,
  job control e stabilita' `peer_info` per i server); `exec` e' primitiva,
  non zucchero.
- **`vfork`** (condivisione senza COW finche' non exec): scartata con gli
  stessi argomenti di ADR-0024 (padre sospeso, semantica delicata).
- **Exec da path nel kernel** (il kernel legge il file): scartata — contro
  ADR-0005 (il kernel non ha FS e non deve averlo; M2a chiusa in Fase 30).
- **Handoff argv via registri** (argc in rdi, argv in rsi all'entry):
  scartata — formato custom meno standard dello stack, stesso churn `_start`
  comunque; lo stack resta l'unica convenzione documentata.
- **Env completo subito**: scartato — envp NULL di placeholder, env vero col
  posix-server (stessa regola del protocollo FS, ADR-0015 §2).

## References

- ADR-0025 (modello nativo, personalita'; argv come dato neutro),
  ADR-0026 (threat model: motivazione exec), ADR-0024 (`fork`, caveat
  risorse ereditati), ADR-0008 (canali tenuti), ADR-0027 (hash rimisurato)
- `SYS_EXEC` (48), `kernel/src/sched_rt/exec.rs` (`exec_current`,
  `exec_clear_user`, `layout_argv`), `kernel/src/syscall/exec.rs`,
  `libs/libr/src/args.rs` (`entry!`, `args_from_stack`),
  `libs/libr/src/spawn.rs` (`exec*`, `serialize_argv`),
  `userland/interpres/src/cmd_run.rs`, `userland/runhello/`
- Fase 37 (37.0 syscall+loader, 37.1 stack-argv, 37.2 shell, 37.3 chiusura),
  t52 (`testland/usertests/src/t_lifecycle.rs`), `test-shell.py` 37/37
