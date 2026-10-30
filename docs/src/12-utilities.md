# Utilities (shell + utility utente, Fase 18)

## Panoramica

Velordo ha una **shell interattiva** (`usershell`) con comandi built-in e vari
**servizi userspace** che eseguono in Ring 3. Tutti usano `civis` come meccanismo
condiviso (e i programmi POSIX anche `libr`). POSIX e' API di `libr`, non ABI del
sistema ([ADR-0015](./adr/0015-posix-api-libr-protocollo-interno.md)):
i builtin usano nomi POSIX ma il protocollo cardo sottostante e' interno.

> **Fase 18 completata**: builtin utente (18.1: echo/clear/wc/hexdump/kill +
> cwd; 18.2: rm/cp/mv/rmdir via `R_DELETE` + `OPEN_CREATE` + contratto
> EOF=0), prompt con cwd (18.1-bis), `ls` che mostra i mount (18.1-ter),
> disciplina di linea anti-prompt-eating in tty (18.0).

## Programmi implementati

### usershell

Shell interattiva con comandi built-in. Legge input da `/dev/input/keyboard`
e scrive output sullo stesso fd (il console server disegna sulla VGA). Tiene
una cwd client-side: i path relativi si risolvono contro di essa, il prompt
la mostra (`/prova$ `, `$ ` a root).

| Comando | Descrizione |
|---------|-------------|
| `ls [-l] [path]` | Elenco directory (default: cwd; mostra anche i mount: `fat`, `dev`). Con `-l` una riga per entry `tipo size nome[ (ro)]` (tipo `d`/`-`/`v`, via `R_STAT`, Fase 19.2) |
| `cat <file>` | Stampa contenuto file; senza file legge stdin (`<`, Fase 40.4) |
| `touch <file>` | Crea file vuoto (`OPEN_CREATE`) |
| `mkdir <dir>` | Crea directory |
| `mount <src> <tgt>` | Monta un device/`UUID=`/`LABEL=` su un target (Fase 16b) |
| `umount <tgt>` | Smonta un target (rifiutato se busy, Fase 16b) |
| `echo [args]` | Stampa gli argomenti |
| `clear` | Pulisce lo schermo (form feed, gestito dalla console) |
| `wc <file>` | Conta righe/parole/byte (`l p b nome`; senza file legge stdin, nome `-`) |
| `hexdump <file>` | Dump esadecimale a righe di 16 byte (senza file: stdin) |
| `kill <pid\|servizio>` | Termina un processo (`kill init` rifiutato dal kernel) |
| `cd [dir]` | Cambia directory (sonda con `readdir`, default `/`) |
| `pwd` | Stampa la directory corrente |
| `cp <src> <dst>` | Copia file (client-side: read+write) |
| `mv <src> <dst>` | Sposta file (cp+rm, la sorgente si rimuove solo a copia riuscita) |
| `rm <file>` | Cancella file (`R_DELETE`; su `/fat` rifiutato: niente unlink, fuori scope) |
| `rmdir <dir>` | Cancella directory vuota (rifiutata se piena) |
| `ps` | Tabella processi stile Linux: PID NAME PRIO STATE TIME PARENT (syscall 37, Fase 19.1) |
| `export [NAME=val]` | Variabili shell (Fase 41): set persistente o lista; `NAME=valore` nudo equivale; TUTTE passano ai figli via envp (43a, niente flag export) |
| `VAR=v cmd` | Ambiente mono-comando (43a): builtin con save/set/restore, esterni via envp, stadi pipe scoped; `$VAR` nella stessa riga vede il vecchio (espansione al parse, come bash) |
| `run <prog> [args...] [&]` | Lancia un programma via fork+exec (37.2, PATH in 43a: senza `/` cerca in `$PATH`, default D1 `/usr/bin:/bin`, fallback `.bin`; `argv[0]` = path risolto); `&` = background (prompt subito), senza = foreground (attende; `[exit N]` se N != 0) |
| `prog args...` | Bare word (43a): non-builtin cercato in PATH ed eseguito come `run` (ignoto = 127); con `/` è path diretto |
| `source <file>` | Esegue uno script riga-per-riga (stesso parser della tastiera: `; && \|\|`, pipe, redirect, heredoc, `$VAR/$?`, glob, `run`). `$?` iniziale = esterno, exit dello script = ultimo comando; `exit` termina lo script (mai la shell); esecuzione silenziosa (niente eco). Anticipa la Fase 43 (script `.sh`); nato per velocizzare i test (1 riga digitata invece di N) |
| `jobs` | Tabella job (`[id] pid P run\|stopped\|done C cmd`; i finiti restano finche' `wait`) |
| `wait [pid]` | Attende i job (tutti o uno) e li rimuove, stampa `pid P: exit C` (gli Stopped si riportano e restano; attesa bloccante senza Ctrl-Z) |
| `fg [%N\|pid]` | Porta un job in foreground e lo attende (44a; se Stopped lo riprende prima) |
| `bg [%N\|pid]` | Riprende in background un job sospeso (44a) |
| `help` | Mostra comandi disponibili |
| `exit [code]` | Termina la shell (Fase 41: code opzionale per `$?`/`&&`/`||`) |

> **Fase 37.2**: job = figli diretti (non-detached: muoiono con la shell);
> uscita via `EXIT_NOTIFY` (nessun `wait` kernel). Il parent carica file+argv
> prima del fork (il figlio ha l'FS avvelenato: solo `exec_image_args`).
> **Fase 44a**: job control su `run` singolo — Ctrl-Z sospende il fg
> (`SYS_SUSPEND` neutro, `[N]+ Stopped`), `fg`/`bg` riprendono
> (`SYS_RESUME`); durante il fg la shell intercetta solo Ctrl-Z (altri tasti
> scartati, documentato); `&` su pipeline resta non supportato (job
> multi-pid, fase futura).
> **Fase 44b**: Ctrl-C selettivo sul fg — cancel cooperativo (`JOB_CANCEL`
> sul canale di nascita: il programma puo' gestirlo) + escalation
> `kill(130)` dopo ~20 tick se vivo (`[exit 130]`); causa di morte 128+SIGINT
> al bordo, come `FAULT_EXIT_CODE`.

### Redirect (Fase 40.4)

Sintassi bash-like (ultimo vince per slot; `2>&1` aliasa sullo slot 1 del
*momento*: `> /o 2>&1` manda stderr nel file, `2>&1 > /o` lo lascia al
terminale). `> /f` da sola crea/tronca senza eseguire nulla:

| Sintassi | Effetto |
|----------|---------|
| `cmd > /f` | stdout su file (crea + tronca) |
| `cmd >> /f` | stdout in append (crea se manca) |
| `cmd < /f` | stdin dal file (deve esistere) |
| `cmd 2> /f`, `2>>` | stderr su file / in append |
| `cmd > /o 2>&1` | stdout+stderr nello stesso file |

Errori distinti sul terminale (mai nel file): `no such file or directory`
(`ENOENT`), `is a directory`, `read-only file system`. Gli errori dei builtin
non inquinano mai `>` (sink separato `term_err`); `run` fallito riporta
`[exit N]` sul terminale.

Meccanismo (ADR-0031, modello B): per i builtin la shell apre + `set_stdio`
con restore; per `run` apre + `dup_grant` pre-fork e contrabbanda
`(vfd, nonce)` nell'ultimo argv (magic `0x7f`, hex senza NUL — il kernel
rifiuta code extra); lo startup (`entry!`) fa claim + `set_stdio` e nasconde
la spec ad `args_from_stack`. Data plane sempre diretto (mai relay nella
shell); grant cancellati a morte osservata. Dettagli e alternative scartate
(relay, nonce sul canale di nascita, pipe per i file) in ADR-0031 e AGENTS.

### Parser (Fase 41)

Sintassi bash-like (subset), implementata in `flavours/posix/shell/src/parser.rs`
(client-side, zero cambi IPC/protocollo):

| Sintassi | Effetto |
|----------|---------|
| `'...'` | Letterale (niente espansione/split/glob/redirect) |
| `"..."` | Raggruppa; solo `$` espande (`\$` resta letterale) |
| `\x` | Escape fuori quote (qualunque char letterale) |
| `#` | Commento (non quotato, a inizio parola) |
| `;` | Sequenza (corre sempre) |
| `&&` / `\|\|` | Short-circuit su exit code (`$?` threadato) |
| `&` | Background per `run` (connettore, non più in argv) |
| `$V` / `${V}` / `$?` / `$$` | Variabili shell / ultimo code / pid (unset = vuoto) |
| `~` | Directory home (`/`) a inizio parola |
| `*` `?` | Glob via `readdir` (match ordinati, no-match letterale, dotfile solo se il pattern inizia per `.`) |
| `export [N=v]` / `N=v` | Set persistente o lista (senza comando) |

I builtin ritornano `i64` (0 ok, 1 errore, 127 ignoto, 2 parse) per `$?`/`&&`/`||`.
Connettori consecutivi: vince l'ultimo. `VAR=v comando` = ambiente mono-comando (Fase 43a);
virgolette non chiuse = resto riga letterale (niente continuazione).

> **Nota tastiera**: `\` e `|` arrivano dal tasto ANSI `0x2B`, che
> `pc-keyboard 0.7` mappa su `Oem7` (non gestito da `Us104Key`): `porta` usa
> un layout `Us104Fix` che lo mappa a `\` / `|` con shift. Senza, i nomi QEMU
> `backslash`/`shift-backslash` erano validi ma i byte non arrivavano mai.

### Pipe + heredoc (Fase 42)

Stadi concorrenti via fork (builtin e `run` condividono handoff e dispatch);
redirect file/heredoc espliciti vincono sui pipe-link per-slot:

| Sintassi | Effetto |
|----------|---------|
| `a \| b \| ...` | Pipeline N stadi (status gruppo = ultimo, `$?` threadato) |
| `a \| b > /o` | Pipe + redirect combinati (esplicito vince sul link) |
| `cat <<EOF` | Heredoc: corpo letterale letto pre-exec (prompt `> `), stdin dello stadio |

Meccanismo (ADR-0032): pipe-buffer **in cardo** (feature dell'OS:
`FileEntry::Pipe` + `PipeTable` cap 8192, `R_PIPE_CREATE` 0x20,
`ERR_EMPTY`/`ERR_CLOSED` → `EAGAIN`/`EPIPE` al bordo POSIX); specifica POSIX
(`pipe()`/`dup2()`, composizione) in `civis`/shell. Handoff stadi = grant con
reservation al grant (stesso nonce COW di ADR-0031); `civis` riprova throttled
su `Empty` (server mai bloccante); EOF vero solo a scrittori esauriti.
Streaming oltre la capacità via intercalazione scheduler. `&` su pipeline
rifiutato oltre la 44a (job multi-pid, fase futura); pipe trailing ignorata.

### Env / PATH / shebang (Fase 43a)

Ogni programma lanciato riceve `argv` + `envp` (blocco
`[argc][envc][argv][magic?][env]`, budget unico `ARGS_MAX`; il kernel stende
byte opachi — neutralità verificabile, ADR-0033). La shell passa tutte le
VARS + `PWD=cwd` (se assente); i programmi leggono con `civis::{Env,
env_from_stack}` (`runhello` dumpa l'env con `runhello: env:K=v`).

| Sintassi | Effetto |
|----------|---------|
| `export FOO=bar` / `FOO=bar` | Persistente + ereditato dai figli |
| `A=1 cmd` | Solo per quel comando (anche `run` e stadi pipe) |
| `runhello` / `run prog` | Ricerca in `$PATH` (default D1 `/usr/bin:/bin`) |
| `run ./x.sh` | Shebang `#!interp [arg]` → `argv=[interp, script, args...]` (bound 4) |

Limiti onesti: `argv[0]` delle bare word = path risolto (non digitato);
shebang solo shell-side (il kernel resta ELF-puro); redirect esterno +
interni annidati non si combinano (limite noto); doppio messaggio
(`cannot load` + `unknown command`) per stadi pipe ignoti senza `/`.

### Script con `source`

`source /fat/test/sh/smoke.txt` esegue il file riga-per-riga con lo stesso
codice del REPL (estratto in `run_one_line`: parse, heredoc, short-circuit —
zero divergenze tastiera/script). Gli script di test vivono in `/test/sh` su
`/fat` (iniettati da `scripts/inject-bins.sh`, nomi 8.3); il pilot copre
builtin, redirect, pipe, heredoc, `run`, variabili, `$?`, `exit` (termina lo
script, mai la shell), guardia di annidamento (max 4) ed error paths
(`scripts/test-shell-source.py`, 22 check in 1 boot).

Limiti onesti: file vuoto = no-op; directory/device = errore; redirect
esterno + redirect interni annidati non si combinano (il restore interno
cancella anche quello esterno: niente stack stdio in `libr`); `source` in
pipeline gira nel figlio (effetti scoped, `$?` iniziale 0).

### Limiti onesti (redirect + parser + pipe)

- **`2>&1` ≠ zsh `MULTIOS`**: niente tee, ultimo-vince come bash/POSIX.
- **`<` su device** puo' troncare/EOF subito (solo file testati); `cat`
  di `/dev/zero` non termina (come da file — stesso comportamento).
- **stderr dei figli quasi-muto**: niente in userland scrive fd 2 oggi; `2>`
  su `run` crea il file ma resta vuoto finche' un programma non lo usa.
- **`open(OPEN_CREATE)` crea i padri** (ramfs `find_or_create`, mkdir -p):
  `> /nodir/x` crea `/nodir` invece di `ENOENT` (semantica server
  pre-esistente, fuori scope 40.4).
- **Offset dup copiato, non condiviso** (dup-for-handoff, ADR-0031).

### runhello

Primo programma lanciabile (`flavours/posix/cli/runhello`, `usr/bin/runhello.bin`
sulla vista POSIX — non un servizio: init non lo spawna; nel bucket `usr` la
chiave e' piatta: `runhello`). Stampa gli argv (uno per riga) su
seriale ed esce 0; con argomento `fail` esce 3 (dopo aver stampato). Con
stdin redirectato stampa anche `runhello: stdin:<byte>` (Fase 40.4d). Serve ai
test shell (`scripts/test-shell-*.py`) come target fg/bg con exit code osservabile.

### Shell nativa: programmi da bucket `binary` (ADR-0064)

La shell nativa (ADR-0062, entry point del login) non usa `PATH`: chiede a
cardo i bucket di tipo `binary` (`BIN_LIST`), cerca il nome digitato **cosi'
com'e'** (namespace piatto: niente sottocartelle ne' suffisso `.bin`),
verifica `RIGHTS_EXECUTE` e lo esegue con `spawn_image`, passando gli argv con
`pack_args`. Oggi l'unico bucket `binary` e' `usr` (`runhello`, `arca`,
`cuuser`); `sys`/`vela`/`tst` restano `object` con i loro path. Il mirror `ns`
conserva la vista POSIX (`usr/bin/runhello.bin`).

Verifica: `scripts/test-shell-native.py` (fase `native` di
`test-shell-all.sh`) — 9 check, incluso il response ring FS ancora
sincronizzato dopo lo spawn (`users` subito dopo `runhello`).

Line editing (Fase 43b, readline nella shell su tty raw): Up/Down history
comandi (Enter accoda: non vuota, no duplicato consecutivo; heredoc esclusi),
Left/Right/Home/End cursore, Delete sotto cursore, Esc ignorato. L'editor
possiede buffer+cursore+echo console-only (mai seriale); redraw senza
conoscere il prompt (`ESC[D`×screen + `ESC[K` + buffer + riposiziona).
La console capisce `ESC[D/C` (cursore senza erase) ed `ESC[K` (erase-to-EOL).
Il backspace a riga vuota non mangia il prompt (floor migrato da `porta`
nella shell — Fase 18.0 superata). Solo ASCII; righe oltre 80 colonne non
editabili (wrap VGA).

### Limiti onesti

- **Write su `/fat`, si** (Fase 20, scrivibile write-through): `cp` verso
  `/fat` crea/scrive con persistenza al reboot (ramfs resta volatile). Resta
  rifiutato: `rm`/`rmdir`/`mkdir` su `/fat` (niente unlink, fuori scope).
- **argv ai binari, si** (Fase 37.1/37.2, supera il limite Fase 18): stack
  stile Linux come convenzione di dati neutra, `_start` via macro `entry!`,
  `libr::exec(path, argv)` (il kernel non tocca il FS). I builtin restano
  builtin; i programmi separati partono con `run` (split futuro: ogni `.bin`
  in piu' resta piccolo, ~17 KiB runhello).
- **`ls -l` minimale**: 1 round trip `R_STAT` per entry (ok per dir piccole);
  niente owner; `mtime` in `Stat` dalla Fase 50 ma non mostrato (display
  futuro); entry sparita tra `readdir` e `stat` → riga `? nome`, mai abortito.
- **Read oltre EOF torna `0`** (contratto 18.2-bis); `open` senza `OPEN_CREATE`
  non crea (POSIX, 18.2).

La shell NON mappa la VGA: tutti i passaggi di input/output avvengono
tramite il device `/dev/input/keyboard`, servito dal terminal server
`porta` (il server `gpu` fa solo rendering `/dev/console` — Fase 15).

### init

Processo radice (PID 1). Spawna i servizi in ordine e poi esegue i test
in sequenza prima di lanciare la shell (i PID sono indicativi: i peer si
raggiungono per nome/canale, non per PID). Solo disk/fs/vestigia sono
embedded; i servizi partono per `object_id` dal volume ArcaFS via
`spawn_image` (D1, bucket `vela`/`sys`, niente fallback FAT):

1. `vestigia` (log) + `block` (disk) — embedded, in parallelo (Fase 57/R1-R9)
2. `cardo` — file system server embedded (ramfs + FAT32 via block, servizio `Cardo`)
3. `time` — data/ora da disco (servizio `Time`, Fase 50; serve `/fat`, dopo cardo)
4. FLUSH a `vestigia` (dopo fs+time) + `gpu` — rendering VGA da disco (servizio `Gpu`)
5. `uptime` — contatore PIT
6. `vela` — `/dev/null`, `/dev/zero` (servizio `Vela`, hub `/dev`)
7. `kbd`/`porta` — tastiera + terminale (servizi `Kbd`/`Porta`, Fase 15)
8. Test: `testfs` → `testfat` → `testarca` → `tests` (attende `TEST_DONE`)
9. `shell` — shell interattiva (ultima, dopo la suite)

### Terminale video (gpu)

Rendering VGA in userspace. Gestisce:
- Scrittura VGA (testo, cursore hardware CRTC)
- Registrazione device `/dev/console` presso cardo via `FS_REGISTER`

La tastiera è gestita da `kbd`/`porta` (Fase 15): input da `/dev/input/keyboard`, echo su `/dev/console` verso la shell.

### File system server (cardo)

File system server con mount table dinamica:
- `/` → ramfs (BTreeMap, scrivibile)
- `/fat` → FAT32 scrivibile (via `block` — Fase 16, scrittura Fase 20)
- `/dev` → vela (instradamento IPC)
- mount dinamici via `mount`/`umount` (Fase 16b, [ADR-0013](./adr/0013-mount-syscall.md)):
  tabella `Vec<FsMount>` con longest-prefix, attivazione lazy, re-apply delle
  spec statiche a ogni boot

### Hub `/dev` (vela)

Server minimale per device speciali:
- `/dev/null` — read = 0 byte, write = scarta
- `/dev/zero` — read = N byte zeropadded, write = scarta

## Libreria (civis)

Tutti i programmi userspace usano `civis` (`libs/civis/`). Include:
- Heap on-demand (free-list, sbrk syscall 25)
- Wrappers FS su ring SPSC: `open`, `read_fs`, `write_fs`, `close`, `readdir`,
  `mkdir` (+ varianti async Fase 13: `read_async`/`fs_collect`)
- FS init lazy: ring alloc (`SYS_RING_ALLOC`, 26) + handshake `FS_BUF_REG`
- `print`/`println` (write su seriale)
- IPC: `spawn`, `send`/`recv`/`reply` + async (`send_async`, `recv_poll`,
  `wait_reply`); risoluzione servizi per nome (`service_register`/`lookup`)

## Build

```bash
# Build servizi utente
./scripts/build-userland.sh

# Build test suite
./scripts/build-tests.sh

# Build + QEMU (tutto incluso)
./run.sh
```

## Riferimenti

- [Writing an OS in Rust - Testing](https://os.phil-opp.com/testing/)
- [OSDev Wiki - Userspace](https://wiki.osdev.org/Kernel_Type_Userspace)
