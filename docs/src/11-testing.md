# Test Suite (Fase 9.5)

> I conteggi di suite citati negli ADR e nelle sotto-fasi del libro sono
> **snapshot all'epoca** di ciascuna fase (es. 17/17, 21/21, 32/32). Il gate
> corrente e' quello qui sotto (5/5 + 64/64 + 4/4 + 11/11 + 72/72 + 12/12 +
> 33 check della shell) e in `AGENTS.md`. La suite **FAT32** e' **fuori dal gate**
> (ADR-0059: il FAT non e' piu' il filesystem di sistema): vive in
> `scripts/run-fat-tests.sh`, un run dedicato con un disco FAT attaccato.
> Fuori dal gate ci sono anche gli **unit test host** dei crate condivisi
> (`scripts/run-unit-tests.sh`): millisecondi, nessun boot — vedi "Run
> dedicati".

La regressione automatica del sistema gira **dentro QEMU** a ogni boot: i
binari di test sono processi user reali, spawnati da `init` in sequenza prima
della shell.

## Layout

```
userland/   servizi NATIVI: init, block, cardo, gpu, kbd, vela, porta,
            vestigia, time, uptime; tools/arca
libs/civis  meccanismo di sistema condiviso (runtime + allocatore)
flavours/posix/
            personalita' POSIX (ADR-0041): libr (crate di traduzione),
            server (posix-server), shell, cli (runhello)
  tests         userposixtests — personalita' POSIX (errno/fd/redirect/jobctl) → PASS 4/4
testland/   test suite meccanismo + repro + demo storiche
  testfs        usertestfs   — ramfs (read/write/mkdir/errori)   → PASS 5/5
  testfat       usertestfat  — FAT32 scrivibile (Fase 20) + /dev/null, /dev/zero → PASS 7/7
  testsarca     usertestsarca — ArcaFS P5+A1+56.1+56.2a+56.2b+56.2c (B+tree COW + commit, recovery/GC + sys-dal-volume) + logging L1 Fase 57 (gateway `Log` RAM-first, bucket per identita', seal, stats, bounce) + namespace POSIX 56.3 (dir emergenti + set RAM, ADR-0042) + quota per bucket A3 (budget, ERR_NOSPC, persistenza, ADR-0044) + rename/symlink S1.1 (file/dir, link+follow, ERR tipizzati) + range read 2 MiB S2.5f/F0 + prefix-overwrite W1 (merge esplicito `ns_write`) + extent dinamici S2.5w/W2 (ADR-0050: COW a blocchi, liveness, XSTATS) + append 512K bound (test59) + cache write-through S2.5w/W3 (ADR-0051: fasce R0/R1/R2, sizing meminfo, hit/reads in XSTATS) + epoche minime S2.5w/W4 (ADR-0052: fd-write a epoca, crash kill-mid recovery, test60) + packing sub-blocco stesso-bucket S2.5w/W5 (ADR-0053: tipo nodo PACKED, bump per bucket, refcount eager + rebuild al bind, test61) + blocchi 64 KiB e rimozione cap 16 MiB S2.5w/W6 (ADR-0054: ARCA_VERSION 3, nessun cap oggetto, walk streaming, test62) + identita' del volume v4 (ADR-0059: superblock 192 B, label + mountpoint dichiarato, test63-64) → PASS 64/64
  usertests     usertests    — suite meccanismo (72 test, +t59 rename/+t60 symlink/+t61 chdir/+t62 chmod S1.1/+t63 modeset D4/+t64 scroll vs repaint T4/+t65 shm ownership + destroy/+t66 firma immagini ADR-0078 fase 2/+t67 classe nel PCB ADR-0079 fase 3/+t68 porte I/O dal manifest fase 4/+t69 Custos ruoli e capability ADR-0080 fase 5/+t70 consenso per immagine + broker ADR-0080 fase 6/+t71 recv con deadline +t72 Sleep in run/block_on E1/ADR-0083/+t73 finestre del compositor D8b/ADR-0088/+t74 sessioni di terminale D8c1/D8c2/ADR-0089+0090, `out` e ritiro di `/dev/console`/+t75 input dalle sessioni D8d/ADR-0091/+t77 diritti ereditari S3.4a/ADR-0093) → PASS 72/72
  identistest   identistest  — protocollo Identis (ADR-0058/0094/0096): WHOAMI senza identita', verifica credenziali (lega il pid), verifica errata che non cambia l'identita', creazione utente, duplicati, nomi non conformi, autorita' non-root, elenco utenti (`nome:ruoli`, mai l'hash) e **riservatezza** di `sys:identis` (lettura negata a un `tst`) → PASS 12/12
  threadtest    userthreadtest — thread 1:1 (S-T, ADR-0046: create/stack/TLS/futex/IPC/fd/stress-32/FPU-S2.5b) → PASS 12/12
  usertest-client usertestcli  — helper a modalita' (ECHO/ZEROREAD/NULLW/SRV/CHURN/KILLME/SRVDIE/SYNCWAIT/MNTDIE/OPENDIE/MAPHAMMER/FLOOD/NEST/FAULT_*/SHMDEMO/COWDEMO/FORKDEMO/ORPHAN/HARDEN/REG51/EXECDEMO/DUPCLAIM/DUPGRANT/DUPSIBCLAIM/SEEKDENY/SUSPENDENY/SIGCATCH/GRANTDENY/SHDESTROY)
  usertest-spin  usertestspin  — busy-loop a budget di tick (batch 512 spin puri, priorita' via SpawnMeta) + ramo SQUAT (sonda di squat FS_REGISTER, t51)
  utcbstest     utcbstest    — helper CBS: crea server e si attacha (Fase 11.5)
  hogheap / devreader         — stress/repro standalone
  demo                        — demo storica Fase 7
```

I `.bin` dei servizi/test da disco (`/bin`, `/test` su `/fat`, Fase 21) sono
iniettati a build (`scripts/inject-bins.sh`) e spawnati via `spawn_image`
(38); solo il boot-TCB (init/disk/fs/log — log dalla Fase 57, ADR-0039) resta
embedded nel kernel via `include_bytes!` (`kernel/src/user_binary.rs`).

## Esecuzione

```bash
./run-tests.sh           # gate di regressione: suite + shell nativa (un boot)
./run.sh                 # produzione: boot SENZA test, shell subito usabile
KVM=1 ./run-tests.sh     # idem, accelerato (richiede /dev/kvm accessibile)
```

L'accelerazione e' **opt-in** (`KVM=1` in `run.sh` → `-enable-kvm -cpu host`;
senza `/dev/kvm` avvisa e resta su TCG). Il default resta TCG perche' il gate
deve girare identico su macchine senza KVM: i numeri cambiano, il contratto no
(gate validato su entrambi: `append512` `dt=5631` su TCG, `4926` su KVM).

Di default (`./run.sh`) init SALTA i test (feature `skip_tests`, shell subito
usabile in ~2 s); con `RUN_TESTS=1` (`./run-tests.sh`) init spawa i test in
SEQUENZA, aspettando un IPC `TEST_DONE` (tag `0x7E`)
da ciascuno prima dello spawn successivo: i binari condividono la ramfs di
cardo (path e file di lavoro) e la sequenza rende output e PID deterministici
(con i ring SPSC per-processo, Fase 10.2, nessuna race da buffer condivisi).
Dopo la suite parte il **login** (l'ultimo programa interattivo: legge la
tastiera). `run-tests.sh` esporta `ARCA_IMG=1`: il gate
ha terzo drive ArcaFS root (sdc) + quarto (partizione GPT su arca-gpt.img,
sdd/sdd1), quindi `testsarca` gira 62/62 (E2: 22-40 sul root vivo in bucket
isolati, 41-50 su mount root esplicito, 51-54 quota A3 in bucket `q5*`,
55-62 namespace/rename/range/W1-W6; senza quarto drive salta 12-13, resta
PASS adattivo).

Ordine di suite: testfs, testsarca, **posixtests**, **identistest**,
usertests, threadtest (posixtests PRIMA di usertests cosi' t54 precede i drop
di diritti di t34; identistest dopo posixtests e prima di usertests: parla
con Identis e scrive nel bucket delle credenziali, ma non tocca i diritti FS
che t34 verifica; threadtest ultimo: non dipende dai diritti e stressa lo
scheduler a servizi caldi). **La suite FAT32 non c'e' piu'** (ADR-0059: il FAT
e' fuori dal boot): `scripts/run-fat-tests.sh` la esegue in un run dedicato
con un disco FAT attaccato (feature `fat_tests` di init).

Dopo la suite il boot finisce **al prompt del login** (D5/ADR-0062: il
login e' il gate — la shell parte solo dopo l'autenticazione, e senza
tastiera il prompt resta li').

Il gate e' **end-to-end in un solo boot** (ADR-0081): `run-tests.sh` mette il
seriale su file e apre il monitor QEMU (`SERIAL=file:… MONITOR=unix:…`),
aspetta `[login] Nome utente:` (segnale di fine suite, al posto del vecchio
timeout esterno), fa il login via `sendkey` e si attacca alla **stessa** QEMU
(`scripts/test-shell-native.py --attach-pid`) per i **33 check** della shell
nativa. Il gate e' autoverificante: esce `!= 0` se manca una riga, se compare
`FAIL|PANIC|#.* FAULT` o se la fase shell fallisce. Valvole: `GATE_SHELL=0`
(solo suite), `GATE_TIMEOUT` (attesa del prompt, default 600 s),
`SHELL_TIMEOUT` (default 900 s). Su KVM il gate completo e' ~4-5 min (suite
~104 s + shell ~145 s di tempo guest).

Righe di gate:

```
[testfs] PASS 5/5
[testsarca] PASS 64/64
[posixtests] PASS 4/4
[identistest] PASS 12/12
[usertests] PASS 72/72
[threadtest] PASS 12/12
[shell-native] PASS 33/33
```

Il gate verifica anche il **contratto** del boot: il segnale di fine suite e'
il marker `[gate] boot: login-ready` (D8c2, ADR-0090 — emesso da
`identis-login` dopo il reply della write del prompt), non piu' la stringa di
UX `[login] Nome utente:`; e dal D8c2 ci sono tre **asserzioni strutturali**:
nessun sorgente cita `/dev/console` (unica eccezione: `t_sess.rs`, il test che
ne prova il ritiro), `DEV_CONSOLE` non e' piu' nell'ABI, e `t64` deve aver
girato davvero (una suite verde con il test in `SKIP` sarebbe il modo
silenzioso in cui il ritiro fallisce). Dal D8d le asserzioni sono cinque: si
aggiungono "nessun sorgente cita `/dev/input`" (eccezione: i test che ne
provano il ritiro) e "`DEV_KEYBOARD` fuori dall'ABI".

### Test delle tile (`desktop=1`, fuori dal gate)

`./scripts/run-desktop-tests.sh` — un boot **corto** (`RUN_TESTS=0`) con
`desktop=1`: init spawana `tileprobe` (`usr`, quindi `ThirdParty`, come
`posixsock`) e il probe verifica il layout multi-tile. Perché fuori dal gate:
il layout lo dispone solo un'immagine ≥ `ThirdParty`, e la suite del gate gira
come `tst` (`Untrusted`).

Le prove (`[tileprobe] ... PASS`): la tile di boot è una e la sessione 0 è in
focus; `SESS_CREATE` + due `TILE_SET` + `TILE_COMMIT` danno **due tile**
affiancate; scrivendo nella seconda, il `TILE_CRC` della **prima non cambia** e
quello della seconda sì (il clip per tile è strutturale: nessun pixel sborda);
lo **zoom** nasconde la seconda e dà lo schermo alla prima (`TILE_INFO` conta
una tile), e al ritorno il testo c'è ancora; `SESS_FOCUS` sposta i tasti (e
`TILE_INFO` lo dice); `TILE_DROP` + `SESS_CLOSE` riportano a una tile.

La **seconda parte** del run prova il DE: `impluvium` non è più un servizio di
boot (D8f) — lo lancia `identis-login` dopo l'autenticazione — quindi lo script
**fa il login** dal monitor (`sendkey` di `root`/`root`), aspetta che il DE abbia
preso la tastiera e solo allora digita i keybinding (`sendkey meta_l-shift-5`
ecc.), controllando le righe che il DE stampa — `[impluvium] split Col ->
tile=2 focus=1`, `zoom on/off`, `focus 0 (h)`: la prova che `Super+%`/`z`/`h`
arrivano al DE e che i tasti non consumati vanno alla sessione (le 33 check
della shell del gate, che girano **attraverso** il DE, sono l'altra metà della
prova).

### Test dei diritti ereditari (fuori dal gate)

`./scripts/run-rights-tests.sh` — boot corto (`RUN_TESTS=0`), login dal monitor,
poi `floorprobe` dalla shell. Perché fuori dal gate: il caso positivo richiede
un processo `ThirdParty` (la shell nativa lo lancia con floor `ThirdParty`),
mentre la suite gira come `tst` (`Untrusted`) e non può spawnare `ThirdParty`
(no elevazione: è la regola introdotta da S3.4). Il probe
(`usr/bin/floorprobe.bin`) verifica che un'immagine `Untrusted` è **rifiutata
dal kernel** (floor), una `ThirdParty` è ammessa, l'`exec`/`spawn` di `System`
è negato e `req_floor = Untrusted` eredita (non abbassa); poi
`floorposix` (`usr/bin/floorposix.bin`, personalità POSIX) verifica che
`libr::exec` di `System` e di `Untrusted` è negato. Nel gate `t77` copre le
stesse vie da `tst` più `ceiling_for` (helper `rightshelper` con `OPEN|READ`:
read ok, write negata), i bit riservati, il fork che eredita floor/ops, i gate
strutturali (`trust_image`, `dma_alloc` device, `map_mmio`) e il **subtree
ereditario** (`subtreehelper`: il padre restringe a `tmp`, il figlio legge
dentro e non fuori).

Il build userland (servizi nativi, test suite, flavour POSIX) usa
`targets/x86_64-unknown-velordo-user.json`: clone di `x86_64-unknown-none`
con `+sse,+sse2,+fxsr` e **senza** soft-float (N6.2.0, ADR-0071); il kernel
resta `x86_64-unknown-none`. Con SSE2 LLVM emette libcall `mem*` invece di
espanderle inline, quindi i simboli sono nostri in `libs/civis/src/rt.rs`
(bulk `rep movsb`/`rep stosb`, taglie piccole `movdqu`); **non** si abilita
`-Zbuild-std-features=compiler-builtins-mem` (la sua versione costa +46% su
`testsarca` append 512 KiB sotto QEMU TCG).

> Write-path ArcaFS (Fase 1a/1b): il test `append 512KiB sequenziale` e'
> tornato sotto il bound dopo free a un settore + header-ext differita
> (`dt=5697` su KVM, limite `6000`; prima `6571`). Il bound non e' stato
> toccato e `testsarca` resta `62/62`.
>
> **Bound append512 (N6.2.0)**: il tempo assoluto dipende dalla latenza IRQ
> del guest e dal filesystem host (misurato `4926 -> 9032` tick sullo stesso
> codice), quindi il criterio e' ora **strutturale**: `half1` (prima meta'
> delle 512 append) contro `half2` (`half2 <= 2*half1 + 400`). Con gli extent
> ogni op tocca O(1) blocchi e le due meta' costano uguale; il layout
> quadratico fa crescere il file e la seconda meta' costa ~3x. Il bound
> assoluto resta largo (`12000`) come guardia. Inoltre `scripts/arca-tool.sh`
> prealloca `arca.img` con `fallocate`: senza, il file sparso faceva allocare
> blocchi all'host a ogni run e il timing cambiava tra un gate e l'altro.
>
> L0a (LAPIC + convivenza PIC, ADR-0056): nessuna nuova riga di suite. Il
> gate esistente valida PIC/PIT/IRQ1/IRQ14/15; il marker di boot e'
> `[lapic] base=0xfee00000 id=... svr=0x1ff LINT0=ExtINT`. La build
> `selftest` include `int 0x50` (IDT + EOI LAPIC) prima del NULL-#PF.
>
> D3 (console virtuale: driver `gpu` + servizio `console`, ADR-0060):
> nessuna nuova riga di suite. Il boot (con il device, default di `run.sh`)
> mostra i marker
> `[console] font: MapleMono-NF 16px cell=10x22`,
> `[console] surface: pixel 1280x800 grid=128x36` (o `640x480 grid=64x21` se
> la finestra host e' piccola) e `[gpu] scanout: 1280x800 stride=5120`.
> Il rendering e' **verificabile**: la console logga il BLAKE2s-256
> dell'**intero framebuffer** (`[console] fb blake2s=...`, deterministico:
> il rasterizer e' in punto fisso). Il confronto col host (stesso crate,
> stessa sequenza) dà la stessa immagine **pixel per pixel** (la cella del
> banner combacia riga per riga). Valore di riferimento (Maple Mono NF
> 16 px, banner "Velordo console server", 1280x800):
> `5a0bd570b3e21205842c8d340447a515d264eb5abd5442a90ffa1618341a6a3c`
> (cambia se cambiano font, dimensione o banner: va aggiornato qui).
> Prova *automatiche* dello schermo (headless, senza X): il monitor QEMU
> sa scaricare la superficie di un device —
> `screendump /tmp/shot.ppm <device-id>` (serve `-device
> virtio-gpu-pci,disable-legacy=on,id=virtg0`): il PPM contiene il
> terminale disegnato (1280x800).
> Fallback: `VIRTIO_GPU=0` → `[console] surface: vga-text 80x25` (zero
> FAIL/PANIC). Attenzione: nel path pixel la **finestra VGA resta vuota**
> ("Booting from ROM"): il terminale e' nella console del device virtio-gpu.
> Con il UI GTK di QEMU quella console **non ha una finestra propria** (il
> UI mostra solo la console predefinita): per vederla usare
> `RUN_DISPLAY=gtk FB_WINDOW=1 ./run.sh` (toglie la VGA standard, cosi'
> l'unica finestra e' quella del framebuffer) oppure il dump del monitor
> (`screendump`). La prova grafica (resize della finestra → config change)
> resta **manuale** (l'host non ha xvfb/xdotool).
>
> D4 (modeset e host-sync, ADR-0061): **una riga di suite in più** —
> `[usertests] t63 modeset + re-adattamento` (il gate passa da 58/58 a
> **59/59**). Il test chiede al terminale una risoluzione **più grande**
> (modeset dal guest a **1920×1088** = 2040 pagine: è il caso che il bound
> vecchio del device buffer — 4 MiB — rifiutava), verifica che la griglia si
> ricalcoli (celle intere, resto centrato), che il device non abbia ceduto
> (l'host non ha insistito: in headless l'UI non si muove) e che **il testo
> sia sopravvissuto** (lo CRC del contenuto resta identico); poi prova una
> misura **oltre il bound** (8192×8192): deve essere **rifiutata senza
> toccare il terminale** (griglia e CRC invariati), e infine ripristina la
> risoluzione iniziale. Marker del percorso:
> `[gpu] scanout: 1920x1088 stride=7680`,
> `[console] display: 8192x8192 troppo grande (65536 pagine, max 8192)` e
> `[t63] modeset 1920x1088 OK (griglia 128x36 -> 192x49 -> 128x36, testo
> conservato, oltre-bound rifiutato)`. Il test verifica anche il **contratto
> del refresh** (`GPU_DISPLAY_STATS`): dopo il modeset `presents` è cresciuto
> di uno e c'è almeno un `FLUSH` **dopo** l'ultimo `PRESENT`; col flush
> mancante fallisce con `refresh mancante: presents N->N+1 flushes dopo
> PRESENT 0` (regressione trovata a mano: schermo nero fino al primo tasto).
>
> La prova del **config change vero**
> (resize della finestra dell'host → l'host insiste → la console cede e si
> riadatta) resta **manuale** con `RUN_DISPLAY=gtk FB_WINDOW=1`: in headless
> l'UI non si muove e l'evento non scatta. È stata fatta in **tre giri** e ha
> trovato due limiti veri: il bound del device buffer a 4 MiB (→ ADR-0061
> §3bis, alzato a 32 MiB con rifiuto spiegato dalla console) e lo schermo
> nero fino al primo tasto (→ ADR-0061 §3ter, "`PRESENT` ≠ flush", con la
> guardia `GPU_DISPLAY_STATS` in t63). Il terzo giro ha **confermato** il
> comportamento voluto: al resize il terminale si aggiorna **subito**, con il
> testo conservato.
>
> L0b (virtio-gpu modern + MSI-X, ADR-0057): nessuna nuova riga di suite.
> Marker deterministici nel boot con il device (default di `run.sh`):
> `[gpu] virtio-gpu 1af4:1050 @00:04.0`,
> `[gpu] BAR1=0xfe000000 BAR4=0xfe004000`,
> `[gpu] MSI-X: 3 vect, config=0 -> 0x50, queue=0xffff`,
> `[gpu] display info: 1280x800 enabled=1`. Fallback con `VIRTIO_GPU=0`:
> `[gpu] virtio-gpu assente: resto su VGA`, zero PANIC/FAULT. Il config
> change MSI-X (resize della finestra host) e' **validazione manuale** con
> `RUN_DISPLAY=gtk`: su questo host non ci sono xvfb/xdotool/client VNC e il
> monitor QEMU non ha comandi display; atteso
> `[gpu] config change: events=0x1` seguito dalla nuova risoluzione.
>
> D8c2 (ADR-0090): `t64` scrive ora sull'**endpoint di sessione**
> (`SESS_ATTACH_DEFAULT` + `write_screen`, una write per riga: il feed
> interattivo che esercita lo shift è lo stesso). Il gate verifica che `t64`
> abbia davvero girato: una suite verde con il test in `SKIP` sarebbe il modo
> silenzioso in cui il ritiro di `/dev/console` fallisce.
>
> D7/T4 (redraw a danno, ADR-0066): **una riga di suite in più** — `t64
> scroll vs repaint` (il gate passa da 59/59 a **60/60**). Scrive 120 righe
> una per write (feed interattivo → shift dei pixel), abilita una bg
> uniforme via `CONSOLE_TEST_MODE` (con il wallpaper `bg_rows_equal` è
> false), poi confronta il CRC del framebuffer con un `CONSOLE_REPAINT`:
> uguali. Il test ha trovato due bug veri: la chiamata a `shift_rows_up`
> passava `gh - cell_h` (restavano ferme due righe di celle) e un glifo che
> sbordava verticalmente scriveva nei margini (shift e repaint divergevano);
> fix: clip verticale del glifo alla cella.

## Test shell interattivi (QEMU + sendkey)

Un boot QEMU per file di test: seriale su file + monitor su unix socket,
comandi in script via `source` (1 riga digitata per gruppo, script in
`/test/sh` sul volume root da `scripts/sh/` via `arca-tool.sh` seed (E1;
`/fat` resta montato per i dati e la copertura FAT dedicata); sendkey solo per
la riga `source`, backspace/clear VGA e i pid di kill/wait — KEYMAP
verificata su QEMU 10.2.2), assert sul log seriale (la shell specchia
l'output; `source` e' silenzioso: niente eco, gli assert di assenza restano
validi). La fase `native` e' l'eccezione: guida la **shell nativa** (entry
point del login, ADR-0062) digitando i comandi uno per uno, senza `source`
(che e' un builtin POSIX) e senza il prompt `$ ` (`boot_native` si sincronizza
sul banner `[shell] Velordo shell nativa`). La fase `native` e' **nel gate**
(ADR-0081): nel gate l'harness non spawna QEMU, si attacca a quella della
suite (`Shell.attached` + `--attach-pid`) e la stessa `run_checks()` gira in
entrambi i modi. Harness comune in
`scripts/shell_harness.py`; runner `scripts/test-shell-all.sh` (sequenziale o
`--jobs N` con overlay qcow2 privati per istanza — due QEMU sullo stesso raw
read-write si corromperebbero):

```bash
./scripts/test-shell-all.sh            # seq (le fasi POSIX sono storiche, vedi sotto)
./scripts/test-shell-all.sh --jobs 5   # parallelo, un overlay per fase
./scripts/test-shell-all.sh native     # shell nativa + bucket binary (1 boot: anche nel gate)
./scripts/test-shell-all.sh source     # legacy (richiede il login POSIX)
```

| File | Fase | Check |
|------|------|-------|
| `test-shell-base.py` | 9.4/18/19/20 (ls/cat/mkdir, backspace, builtin, cd/pwd, ls -l, kill, ps, rm/cp/mv, FAT scrivibile, clear) + guardia CPU al prompt (ADR-0067) | 29 |
| `run-tui-tests.sh` | **fuori dal gate** (ADR-0098/0099): `usr/bin/fullscreen` prende la tela (alt screen, 127x36) e il **primo frame è a video senza premere tasti** (screendump: i pixel del bordo ci sono; il feed di una tela non è "bulk"), i tasti muovono la stella e **un tasto costa un frame piccolo in una feed** (34 byte, 1 chunk: il lampeggio era un IPC per cella, ~660 render+flush; il primo frame è intero, 1564 byte), una **toppa** 12x1 (10560 byte) cambia il **CRC della riga**, un **modeset chiesto dall'app** la ridimensiona (102x34) e lei si riadatta da sola, `q` esce e la shell **ridisegna il prompt**; poi la **GTUI** `usr/bin/gtui` (log con i rami in pixel + barra + sparkline): 144 KB di toppe con il CRC della riga non nullo, **restano nella cronologia** dopo 40 righe (stessi byte) e **muoiono con le loro righe** oltre le 1000 (0 byte); poi **split mentre gtui gira** (Super+%): la tile si stringe (62x34), l'app viene svegliata dalla notifica e **ridisegna la grafica** (`[gtui] resize … patches=N`, N > 0); infine `gtui` nella **seconda tile**: la grafica esce anche lì e nessuna toppa cade fuori dalla vista; chiude il giro `fullscreen` in una tile **spostata** (origine > 0): un passo = una cella, la stella si ferma al bordo senza "memorizzare" righe, e il frame raggiunto **per differenziale** è identico a quello di un **repaint da zero** (`r` → `CONSOLE_REPAINT`), cioè il fondo ridisegnato viene dal wallpaper |
| `test-shell-native.py` | S3.2b-4/ADR-0064 + ADR-0065 (shell nativa: `BIN_LIST` → oggetto piatto `usr` → EXECUTE → spawn con argv → exit, ring sincronizzato, comando ignoto; interprete: variabili, aritmetica, if/while/for, commenti, `dump`/`type`; prompt una volta per riga, hint inline, Up) + nessun diniego/errore di permesso su `usr` (la shell spawnata dal login è verificata: ADR-0080 fase 6) + guardia CPU al prompt principale e a quello di `create-user` (E0/ADR-0082: eventi, non polling; entrambe &lt; 5%) + orologio del prompt che **avanza in-place** mentre la shell dorme (E1/ADR-0083: `ESC[D`×13 + `HH:MM:SS` + `ESC[C`×5, saldo zero delle celle, nessun prompt ridisegnato) + il prompt mostra il nome **letto dalla sessione** (`SESS_AUTH`, S3.3d/ADR-0095: niente `USER` in ambiente) | 33 |
| `test-shell-run.py` | 37.2 (run fg/bg, argv, exit-code, jobs/wait) | 8 |
| `test-shell-redirect.py` | 40.4 (`> >> < 2> 2>&1` + run redirectato) | 20 |
| `test-shell-41.py` | 41 (quoting/escape, `$VAR/$?/~/$$`, `; && \|\|`, commenti, glob) | 37 |
| `test-shell-42.py` | 42 (pipe N stadi, pipe+redirect, status ultimo, stadi run, bg rifiutata, heredoc, EOF, streaming >8192B) | 18 |
| `test-shell-source.py` | `source` (smoke 1-riga, exit, nesting, errori, vuoto) | 22 |
| `test-shell-43.py` | 43a (env ereditato, VAR=v, PWD, PATH/bare-word, shebang, env stadi) | 17 |
| `test-shell-43b.py` | 43b (history Up/Down, Left/Right/Home/End/Delete, Esc; digitazione reale) | 9 |
| `test-shell-44.py` | 44a (fg/bg, Ctrl-Z su run singolo, jobs/ps stopped, selettivita', cleanup) | 14 |
| `test-shell-44b.py` | 44b (Ctrl-C → 130 su fg non cooperante, selettivita', cleanup) | 5 |
| `foreign.bin`     | t57: attore "ignoto" fuori tabella policy — default fail-closed   | — |

La fase `native` e' **33/33 verde e nel gate** (ADR-0081, E0 in ADR-0082, E1 in ADR-0083).
Le 10 fasi POSIX
(179 check) restano la
specifica eseguibile del parser/redirect/pipe POSIX: non sono nel gate di boot
(il login nativo non le lancia da solo), ma `test-shell-all.sh` le guida via
`boot()` digitando `usershell` al prompt nativo. `base` e `native` includono
la guardia CPU di ADR-0067: al prompt il processo QEMU deve stare sotto il
50% (un busy-wait di attesa input e' ~100%); la fase `native` ha la soglia
stretta **&lt; 5%** sui due prompt dell'input a eventi (E0, ADR-0082: misurato
1,0% principale e 2,0% `create-user`). Stesso kernel produzione del gate:
il kernel
embedda init/fs/disk, quindi va ricompilato DOPO `build-userland.sh` — ordine
di `run.sh` — altrimenti il manifest Strato 2 di init non matcha i binari su
disco e il boot fallisce loud. `_boot_start` fa il **login** (root/root, ADR-0062)
e aspetta `Nome utente:`: senza quel passo i test si fermano al gate di
autenticazione. Se il kernel su disco e' l'ultimo build **con i test**
(`run-tests.sh`), il boot esegue la suite e il login arriva dopo: ricompila in
produzione (`./scripts/build-userland.sh && cargo build --release`) prima di
`./scripts/test-shell-all.sh base native` — il **gate** invece non ricompila:
si attacca a quel boot quando il prompt compare (`--attach-pid`).

> Timing adattivi (`shell_harness.py`): su KVM (`/dev/kvm`) gli sleep sono
> corti (tasti 0.06s, drain 0.25s, run 0.4s), su TCG restano i valori storici
> conservativi (0.18s/1.0s/1.0s) contro l'overrun PS/2. Gli sleep espliciti
> passati dai test restano rispettati; il boot aggiunge `-cpu host` come
> `bench.sh`, mentre su TCG usa gli stessi flag di `run.sh`
> (`qemu64,+fsgsbase,+rdrand,+rdseed`: `qemu64` liscio non li ha e lo
> userland non partirebbe). `SHELL_KVM=auto|0|1` forza il modello di timing:
> il gate passa `KVM` così la fase shell usa lo stesso acceleratore della
> suite.

## Run dedicati (fuori dal gate)

Boot separati, costosi per costruzione (toolchain host + RAM/volume grandi):
non entrano in `run-tests.sh`, si lanciano a mano e hanno esito esplicito nel
log seriale.

- **unit test host** (`scripts/run-unit-tests.sh`): `cargo test` dei crate
  condivisi che hanno `#[cfg(test)]` — `libs/term` (rasterizzatore:
  copertura dell'ultimo pixel, `ceil` della bbox, inchiostro = area,
  determinismo; **modello a celle**: il cursore **non salta in fondo** al
  re-adattamento, il testo non scivola, il reflow lascia il cursore sul
  testo; **surface**: il layer 0 viene composto riga per riga rispettando
  l'alpha dell'immagine e i colori sono `0xRRGGBBAA`; **T2/T3**: fill a
  parole, `copy_from`/`shift_rows_up` con clipping, `draw_ink` che non tocca
  il background), `libs/net` (IPv6/NDP/TCP/SIIT/DNS, `Ipv6Addr::parse` e
  cache dei vicini STALE con NS di verifica), `libs/net-ep` (formato degli
  slot dei ring: roundtrip IPv6/IPv4, limiti del `len`, cursore di ring
  pieno/vuoto/wrap; N7.0), `libs/valloc`, `blake2s` e
  `libs/tls` (trasporto/adapter, DRBG ChaCha20, fail-closed della
  configurazione: SNI/CA mancanti e buffer record troppo piccoli danno
  `Error::Config`), `libs/musivum` (compositor D8a: `Rect`/`Damage`, blit con
  clip, fette di wallpaper, z-order, `Backing::Direct`, degradazione oltre il
  tetto), `libs/broker-proto`, `libs/littera` (sessioni di terminale D8c1:
  ANSI/scroll/wrap/SGR/backspace, eco e coda di input, dump con coda
  allineata ai confini UTF-8, resize con reflow, tabella con
  creazione/focus/binding/attachment/purge e tetto a 8, identità di sessione:
  set/clear, UTF-8 e lunghezza fail-loud, purga per `auth_by`; S3.3c) e
  `libs/sess-proto` (layout della mailbox: OUT/IN, `MAX_CHUNK`, `fits`),
  `libs/pictura` (primitive bitmap delle toppe: fill/clip, barra, sparkline,
  linea con spessore) e `libs/tabula` (canvas a celle con **copia ombra**:
  primo frame intero, nessun cambio = zero byte, fette consecutive in un solo
  `CUP`, celle non adiacenti, riga che si accorcia, resize che riparte da zero)
  (**213 test** in tutto;
  `libs/net` ne ha 54, 23 dei quali sulla macchina a stati TCP e sul wiring, 2
  sul framing RFC 7858). Girano da una
  **directory neutra**: il `.cargo/config.toml` del repo impone
  `build.target = x86_64-unknown-none` + `build-std` in base alla directory
  corrente, e da dentro il repo `cargo test` proverebbe a compilare l'harness
  per il target freestanding (doppio `alloc`). `CARGO_TARGET_DIR` in
  `target/unit-tests`. Il test del rasterizer e' nato da un bug vero: la
  copertura perdeva l'ultimo pixel di ogni span (testo assottigliato e
  irregolare, ADR-0060 appendice). I test del **cursore** sono nati da un
  altro bug vero: al re-adattamento (resize) il cursore finiva **in fondo**
  allo schermo, lontano dal punto in cui l'utente stava scrivendo
  (ADR-0061 §4, corretto in D5/ADR-0062).

- **rete N1–N6.1** (`scripts/run-net-tests.sh`, ADR-0068/0069): boot con
  `-netdev passt` + virtio-net (IPv6 reale, unprivileged), **fuori dal
  gate** perché `passt` è una dipendenza host. Il guest fa SLAAC/NDP e
  ping6 verso il router; PASS se il log contiene `[net] echo reply` (N1),
  `[netbroker] broker attivo` (ADR-0080 fase 6), `[netudp] risultato: PASS`
  (N2), `[net3] risultato: PASS` (N3), `[net4] risultato: PASS` (N4),
  `[neteploop] risultato: PASS` (regressione: ciclo di vita endpoint, shm
  sempre liberato), `[dns] risultato: PASS` (N5) e `[dns] upstream: PASS`
  (N6.0).
  **E2 (ADR-0084)**: lo script include anche un **gate strutturale** — i
  client di rete del meccanismo non fanno polling a tick
  (`rg futex_wait libs/civis/src/net libs/tls-net/src` → 0) — e l'intera
  suite gira con la sola API a **eventi** (`NET_EP_EVENT` +
  `civis::net::wait`): `netecho`, `dns`, `siit`, `netbroker`, `Stream` e le
  socket POSIX dormono sull'hint del servizio. I `futex_wait` che restano
  nei servizi sono solo i retry di boot `service_lookup` (ADR-0067).
  **N8 (ADR-0085)**: anche il **servizio** `net` è a eventi — il driver manda
  `NET_RX_EVENT` quando il ring RX passa da vuoto a non vuoto e il servizio
  dorme su `recv_timeout(Net::next_deadline())` (retransmission TCP,
  TIME_WAIT, RS/echo, aging NDP) senza risvegli a tick. Due gate in più:
  il tag `NET_POLL` è **rimosso** dall'ABI (un riferimento non compila) e la
  **CPU idle** misurata su un boot dedicato senza suite (`net=1`): deve stare
  sotto il 5% (misurato: 0,0%; un polling a 100 Hz sta a ~100%).
  `NET_IDLE_CPU=0` salta la misura (boot più corto).
  Terzo gate: `munmap` con geometria non di pagina (lunghezza non multipla,
  la forma del leak shm) — il kernel logga `[munmap]` loud e lo script
  richiede zero righe; lo stesso controllo è nel gate principale.
  I probe di **policy** (`netudp`/`net3`/`net4`) sono in `tst`, quindi
  `Untrusted`: `net`/`siit` negano loro l'endpoint diretto e navigano solo
  via broker — se passano, il modello della fase 6 è verificato end-to-end.
  `net4` verifica anche il **diniego**: un IPv4 pubblico è negato senza
  consenso, uno privato (LAN) passa. `posixsock`/`nettcp` parlano con `net`
  in diretta e sono `usr` (`ThirdParty`): provano il trasporto.
  Risultato N1: indirizzo `2a01:e11:2430:7af0:...`, RA da
  `fe80::3a07:16ff:fe2c:71ea`, echo reply dallo stesso router.
  Risultato N2: `netecho` apre endpoint porta 7, `netudp` (endpoint
  ephemeral) invia a `::1:7` e valida l'eco.
  Risultato N3: `netecho` apre endpoint TCP porta 8, `net3` (endpoint TCP
  ephemeral) invia SYN a `::1:8` e valida che il servizio accetti il
  segmento.
  Risultato N4: `siit` apre endpoint UDP porta 30000, `net4` (endpoint
  ephemeral presso `siit`) invia payload verso indirizzo IPv4
  `10.0.0.1` (formato SIIT `64:ff9b::a00:1`) e valida che `siit` accetti
  il segmento (`SIIT_EP_TX` risponde OK).
  Risultato N5: `dns` apre endpoint UDP porta 53, `dnsprobe` (endpoint
  ephemeral presso `dns`) invia una query A per `example.com` e valida che
  la risposta contenga `93.184.216.34` (tabella statica N5).
  Risultato N6.1: `netecho` è un listener TCP vero (handshake nel core,
  firma `EV_ACCEPT`), `net3` invia un payload e valida `EV_SYNACK` **e**
  l'eco end-to-end — handshake, ACK, dati e chiusura passano tutti dalla
  macchina a stati. Il probe `nettcp` (query DNS over TCP verso un peer
  esterno, stesso trasporto di DoT) è **opzionale**: passt non inoltra il TCP
  al loopback dell'host, quindi si esegue solo dichiarando
  `NET_TCP_PEER=<ipv6> NET_TCP_ECHO_PORT=53 ./scripts/run-net-tests.sh`
  (che rende `[nettcp] risultato: PASS` obbligatorio). Un **peer pubblico**
  è raggiungibile: misurato con `NET_TCP_PEER=2620:fe::fe` (Quad9:53) →
  `[nettcp] DNS over TCP su rete reale: risposta 61 byte (rcode=0)`; il
  limite di passt riguarda il loopback dell'host, non l'uscita.
  Risultato N6.0: `dnsprobe` invia anche una query A per `upstream.test`,
  nome fuori tabella; `dns` la inoltra via UDP plain all'upstream
  configurato e restituisce l'indirizzo `198.51.100.7`. L'upstream è un
  responder Python sull'host (`[::]:5353`, mappato da `passt` sul
  gateway); `arca-tool.sh` seeda `sys:dns.conf` (`upstream = auto`,
  `port = 5353`) **solo** se chi chiama esporta
  `NET_DNS_UPSTREAM_PORT`, cosicché un `NET=1 ./run.sh` manuale resti N5.
  I servizi `netdev`/`net`/`netecho`/`siit`/`dns` partono solo con `net=1`
  in cmdline (il gate non lo setta).

- **Client DNS nativo** (`dns-resolve`, ADR-0075): comando della shell, non
  un test. Con `NET=1 ./run.sh` (o `RUN_DISPLAY=gtk` per vedere il
  terminale) dal prompt: `dns-resolve example.com` stampa
  `[dns-resolve] example.com: 93.184.216.34 (A)` ed esce 0;
  `dns-resolve example.com aaaa` interroga il tipo AAAA. Senza `net=1` il
  servizio `dns` non esiste e il comando risponde "servizio DNS non
  disponibile" (uscita 1). **Fuori dal gate**: il gate resta 6 righe e non
  setta `net=1`.

- **Socket POSIX** (`posixsock`, N7.1, ADR-0076): probe automatico, spawnato
  da init **solo con `net=1`** dopo `dnsprobe`. Apre le socket con le sole API
  POSIX della personalità (`libr::socket/connect/sendto/recvfrom/send/recv/
  close`) e fa echo UDP verso `netecho:7` e TCP verso `netecho:8`. Contratto:
  `[posixsock] udp PASS` + `[posixsock] tcp PASS` +
  `[posixsock] risultato: PASS`, richiesto da `scripts/run-net-tests.sh`.
  Prova il percorso personalità → `libr::sock` (fd 64..72) → `civis::net`
  (N7.0) → servizio `net` → core TCP/UDP: nessuna syscall nuova, kernel
  intatto. Il binario è seedato anche in `usr/bin/posixsock.bin`, quindi dalla
  shell nativa `posixsock udp ::1 7 ciao` (o `tcp`) fa la stessa prova a mano.
  Il primo giro ha trovato un bug **vero** di traduzione: su TCP il ring
  consegna `SynAck` (len 0) prima dell'echo, e un `recv` che lo trattava come
  dato faceva leggere 0 byte (= EOF per POSIX) — ora `libr::sock` scarta gli
  eventi di controllo e mappa `Fin`→0, `Rst`→`-ECONNRESET`.

- **TLS 1.3 + DoT reali** (`scripts/run-tls-tests.sh`, N6.2/N6.3,
  ADR-0072): boot con `tls=1` (+ `net=1`) e virtio-net/passt. I server TLS
  girano **sull'host**: la sandbox non ha il binario `openssl`, quindi i
  certificati (CA + leaf RSA-2048 con SAN multipli `tls.test,dns.test` +
  un leaf self-signed "cattivo") li genera il tool host
  `tools/tls-test-certs` (puro Rust: `x509-cert` + `rsa`), e i server sono
  Python `ssl` (OpenSSL 3.x, TLS 1.3, ALPN `dot`). `arca-tool.sh` seeda
  `sys:tls.conf` (`sni`, `port`) e `sys:ca.der` **solo** se chi chiama
  esporta `TLS_TEST_CA`, e `sys:dns.conf` in `mode = dot` **solo** con
  `NET_DNS_DOT_PORT`: il gate resta identico.
  In **un solo boot** si provano due cose:
  1. `tlsprobe` (stack grande) apre un endpoint TCP verso il gateway di passt
     (`NET_ADDR_ROUTER`, stessa strada del responder DNS N6.0), fa
     l'handshake con catena **e hostname verificati** (non `NoVerify`),
     invia `GET / HTTP/1.0` e valida una risposta `HTTP/`. PASS con
     `[tlsprobe] handshake: PASS` + `appdata: PASS` + `risultato: PASS`
     **e** `alpn='dot'` nel log del server (un probe saltato non è una
     vittoria: `SKIP` = FAIL).
  2. il resolver `dns` in `mode = dot` (una connessione per query, framing
     RFC 7858) risolve `dot.test` **due volte** (la seconda prova il
     reconnect sullo stesso endpoint) e una terza volta contro un certificato
     fuori dalla trust anchor: deve rispondere **SERVFAIL**
     (`[dns] dotfail: PASS`), mai una risposta in chiaro né NXDOMAIN. Il
     responder sceglie il certificato per numero di connessione, quindi
     nessun secondo boot; lo script verifica anche che la quarta connessione
     **non** abbia completato l'handshake.
  I probe hanno trovato tre **bug veri**: (a) `next_tx` mandava dati anche in
  `SynSent` (ACK=0 → passt resetta; invisibile sul loopback perché il SYN+ACK
  è sincrono), coperto da `no_data_before_handshake_completes`; (b) su RST il
  servizio `net` liberava l'endpoint, lasciando il client con un ring
  smappato (ora un endpoint CONNECT sopravvive alla connessione); (c) gli
  eventi stantii della connessione precedente chiudevano il primo read della
  successiva (ora `EV_ABORT` libera lo slot TCP e scarta il ring RX).
  Variante contro un resolver **pubblico** (facoltativa, dipende dalla rete):
  `NET_DNS_UPSTREAM=2620:fe::fe NET_DNS_DOT_PORT=853` +
  `NET_DNS_DOT_SNI=dns.quad9.net`, con `TLS_TEST_CA` che punta alla root in
  `third_party/ca/DigiCertGlobalRootG3.der`. La catena di Quad9 è
  **ECDSA-SHA384**, quindi copre la feature `p384` che i certificati RSA di
  `tls-test-certs` non toccano; anchor = root (non l'intermediate inviata dal
  server). Successo: `[dns] risposta dot: ... rcode=3` e nessun
  `handshake fallito`.

- **hello std** (S1.3…S2.4): `scripts/build-pal.sh` (PAL + sysroot in
  `$PAL_WORK`) produce `pal/build/hello-std.bin`, seedato come
  `/test/stdhello.bin`; `python3 scripts/test-shell-std.py` fa il resto
  (SKIP rumoroso se il binario manca).
- **staticone rustc** (S2.5d/F1): `scripts/build-velordo-rustc.sh velordo` +
  copia in `build-meta/rustc-velordo.bin`, poi
  `RUN_S25=1 RUN_MEM=1G ./run.sh`. Con `RUN_S25=1` il seed mette il payload
  nel bucket `ns` di ArcaFS (`tool/rustc.bin`, 100 135 752 B) e il volume
  root sale a 384 MiB (`ARCA_SIZE_MIB` esplicito vince); `s25fuse` lo legge a
  slice da 4 KiB via range read F0 e lo spawna. Misura S2.5f/F1: load in
  ~47.5 s (~2.1 MB/s), `--version` → `rustc 1.100.0-nightly`, exit 0.
  Il tetto e' il costo per round trip seriale, non ArcaFS: 1 IPC a cardo per
  slice da 4 KiB + 19 IPC DMA per blocco da 64 KiB (`DISK_MAX_SECTORS` = 7
  settori) → candidato W7 (finestra DMA multi-pagina + payload piu' grandi).

## Cosa copre `usertests` (69 test meccanismo; t34 per ultimo: i drop dei
diritti sono irrevocabili sul canale della suite)

| Test | Cosa verifica |
|------|----------------|
| t1 | getpid |
| t2 | ticks monotoni (timer attivo, IF=1 in user) |
| t3 | heap lazy demand-zero: pagina sbrk fresca letta = 0 |
| t4 | allocatore reuse/coalescenza |
| t5 | spawn + getpid del figlio |
| t6 | ramfs: read hello.txt |
| t7 | ramfs: write multi-chunk (>1 pagina FS) + read-back |
| t8 | ramfs: mkdir + readdir |
| t9 | FS: error paths (open path vuoto, fd invalido) |
| t10 | /dev/null |
| t11 | /dev/zero |
| t12 | map_physical aliasing (pagina scratch kernel `MAP_TEST_PHYS`) |
| t13 | IPC single echo |
| t14 | IPC multi-client (reply_target, no cross-talk) |
| t15 | devfs concorrente + heap churn (regressione lazy/IPC) |
| t16 | preemption ring-3 (contatore su pagina scratch) |
| t17 | priorita' High > Normal |
| t18 | CBS admission control |
| t19 | CBS bandwidth: audio (CBS 30%) + hog (no CBS) |
| t20 | FS async 1-in-volo (Fase 13) |
| t21 | IPC async N-in-volo + backpressure (Fase 13) |
| t22 | lifecycle churn (Fase 14): 42 spawn/exit di helper CHURN (~2 MiB heap ciascuno) oltre il vecchio limite cumulativo → riuso PID + niente frame leak (notifiche `EXIT_NOTIFY` attese per ogni figlio) |
| t23 | kill + exit notify (Fase 14): kill di un helper KILLME con code noto → notifica con (code, pid); il pool accetta ancora spawn |
| t24 | notifica unificata di morte (Fase 14): server SRVDIE (registra `Service::Test`, mai risponde) + client SYNCWAIT (lookup + send sync bloccato); kill → `wait_reply` da' `ServerDied{pid,code}` esatti (path async), il client sbloccato osserva EXIT_NOTIFY e riporta T_DONE (path sync), slot servizio liberato, pool sano |
| t25 | purge mount alla morte driver (Fase 14): driver MNTDIE registra `/tdie`, open instradato, kill, re-registrazione stesso prefix → open via nuovo driver (senza purge lo stale avvelenerebbe `resolve_mount`); kill D2 + smoke ramfs |
| t26 | purge rings/ftable alla morte client (Fase 14): 10 helper OPENDIE aprono /dev/null+/dev/zero+hello.txt e muoiono senza close → smoke FS completo (null/zero/hello/write/mkdir/readdir) prova server sano |
| t27 | init-restart di devfs (Fase 14): kill via `service_pid` → sparizione dallo slot → ricomparsa (pid anche riusato: osserva sparizione→ricomparsa, non confronto) → /dev/null di nuovo operativo + smoke ramfs |
| t28 | restart di cardo end-to-end (Fase 14): kill via `service_pid` → fixture fresh (mkdir/write/read), hello.txt ricreato, probe ramfs sparito (wipe via readdir), /fat leggibile (persistente), /dev/null operativo (driver re-registrati) |
| t29 | map-flap isolation (diagnosi t28): martella `map_physical` su una VA verificando marker, da solo poi con helper sulla stessa VA (altre tabelle/frame) → niente cross-talk |
| t30 | fairness scheduler sotto carico IPC: helper FLOOD (open+write+close /dev/null a regime dopo warm-up) + kill vela + latenza mount (bound 300 tick, osservato 0–1) → becca regressioni di rotazione/starvation (es. bug di parita' round-robin). NON misura saturazione cardo: con client sync (≤1 in volo) la coda non si riempie mai |
| t31 | keyboard stack userspace (Fase 15; D8d/ADR-0091): servizi `Kbd`/`Porta` registrati + open di `/dev/kbd/kbd` (il device **raw** che legge solo `porta`); `/dev/input/keyboard` non esiste più e `INPUT_ATTACH` è **negato** a `Untrusted` (la tastiera è una capability). Niente digitazione reale (serve QMP/sendkey: coperta dai test shell interattivi sotto) |
| t32 | driver block in userspace (Fase 16; topologia via protocollo dalla Fase 51; durabilità dalla Fase 52): open raw `/dev/sda` + settore 0 con firma boot 0x55AA; kill block via `service_pid` → sparizione/ricomparsa (init-restart) → raw di nuovo operativo + `/fat/HELLO.TXT` leggibile via riconnessione lazy di cardo; `disk_list` (≥2 dischi) + `disk_info` (coerenza LIST/INFO, fatti strutturali, UDMA≤2, dump topologia) + `disk_info(99)` rifiutato; ciclo `disk_sync` (prev umask-like, modo ignoto rifiutato) + statvfs `/fat` (blocchi/libéri coerenti) e `/` (MAX illimitato) + `/dev/null` rifiutato |
| t33 | mount/umount espliciti (Fase 16b): mkdir ramfs + mount `/dev/sda`→`/mnt` + contenuto FAT + re-mount idempotente + umount busy rifiutato + umount ok (`/mnt` torna ramfs) + error paths (sorgente/target invalidi, doppio umount, umount `/`) |
| t35 | resolve nome→handle lato driver (Fase 16c): nomi ignoti senza stato (niente spec fantasma), bad-replace innocuo, mount valido operativo |
| t36 | identità stabile (Fase 16d): mount per `UUID=` e per `LABEL=` del secondo disco + contenuto MARKER (prova il disco giusto), open raw dei by-path con firma+seriale, listing sintetizzato `/dev`/by-uuid/by-label. Gira anche con `SWAP_DRIVES=1` (lettere cambiano, chiavi no) |
| t37 | snapshot processi (Fase 19.1; meminfo dalla Fase 52): idle/init presenti con parent `None`, self Ready, count >= 8, TIME di init > 0 e TIME proprio crescente dopo spin + `SYS_MEMINFO` coerente (total>0, free≤total, free+used==total) |
| t38 | `stat` metadati senza open (Fase 19.2; FAT senza readonly dalla Fase 20; mtime veri dalla Fase 50): file/dir ramfs (size reale, vita dopo mkdir/rm), file/dir FAT, device (`/dev/null`), padri sintetizzati (`/dev`), error paths (inesistente, sotto-device) + `Time` raggiungibile/monotono/plausibile, mtime ramfs plausibile e crescente su write, mtime FAT noto |
| t39 | servizi da disco (Fase 21): `/bin`+`/test` presenti e non vuoti, tutti i servizi registrati per nome (= boot da disco funzionante) |
| t40 | detach + reparent a init (Fase 22): MID intermedio spawna due KILLME (uno detached via flag, uno no) poi esce; foglia normale sparita da `ps`, detached viva con parent == 1, poi cleanup-kill (osservazione solo via `ps`, mai distruttiva prima del check) |
| t41 | `block_on` + echo async (ADR-0019): 1 send_async a helper MODE_SRV, raccolta con router (`on_chan`: stale scartate), teardown T_STOP+T_DONE |
| t42 | `run` 2-task + morte server (ADR-0019): due helper MODE_SRV (secondo prima per mescolare l'ordine), ogni risultato matcha il proprio req (routing per req_id, non FIFO); poi SRVDIE + kill → `ServerDied{pid,code}` esatti via router |
| t43 | composizione annidata `Join<Join<W,W>,W>` (ADR-0019): tre helper MODE_SRV, invii in ordine inverso all'albero, guidati da `block_on`; ogni risultato matcha il proprio req (routing multi-livello) |
| t44 | mmap anonimo nel basso canonico (Fase 28): pattern R/W su 3 pagine, spot-check 3 MiB multi-PT (1536 fault demand-zero), fixed/overlap/len-0/hint-disallineato rifiutati, munmap parziale rifiutato senza stato, munmap interi + riuso fixed con zeri freschi, `write` seriale da buffer mappato (prova `is_user_range` esteso) |
| t45 | protezioni (Fase 29): `mmap_prot` RO + `mprotect` RO↔RW (contenuto preservato) + →NONE (pagine cadono, riuso a zeri) + error paths (prot W-solo rifiutato, mprotect parziale rifiutato senza stato); 6 helper `utcli` che provocano fault (write su RO, read su NONE, exec su NX, write sulla guard page, `in` su porta non concessa → #GP, write a `USER_CODE` → codice RX) muoiono con `FAULT_EXIT_CODE` osservato via `EXIT_NOTIFY` |
| t46 | memoria condivisa (Fase 30): `shm_create` + `shm_map`; il parent scrive un pattern, un helper mappa la stessa regione (stesse pagine), verifica il pattern e scrive un marker che il parent vede (visibilita' bidirezionale); `munmap` rilascia il ref, id inesistente rifiutato, riuso dello slot (regione fresca a zeri) |
| t47 | shared text (Fase 32): 3 helper concorrenti dallo stesso binario condividono i segmenti immutabili (`text_stats`: `hits` cresce); alla loro morte i ref sono rilasciati (`live` cala di 3). Delta attorno alle proprie operazioni (il baseline assoluto di `live` non e' stabile: altri test lasciano reclaim pendenti). Se lo sharing non avviene il test fallisce **dicendo perche'**: `text_full()` (`r10` di `text_stats`) distingue la tabella satura ("tabella shared text PIENA: N fallback privati", fix N6.3) da un vero difetto del loader |
| t48 | COW su shm (Fase 33): il parent crea una regione (mappata normale RW) con un pattern; l'helper la mappa COW (`shm_map_cow`), legge il pattern (shared-read) e scrive due pagine (2 COW fault → copie private, `cow_count` +2); il parent non vede le scritture (isolamento); `shm_map_cow` su id inesistente rifiutato; riuso slot + zeri freschi |
| t65 | ownership + destroy di shm (Fase 30.1): `shm_create` senza `shm_map` non lascia frame orfani (destroy + riuso slot); `shm_destroy` e' owner-only (secondo destroy rifiutato, helper non-owner rifiutato); la mappa esistente resta leggibile/scrivibile dopo il destroy, i nuovi `shm_map` falliscono (fail-closed); `munmap` libera i frame all'ultimo ref |
| t66 | firma delle immagini e classi (ADR-0078 fase 2): `R_RIGHTS_EXECUTE` risponde `w0` = verdetto e `w1` = `ImageClass` — `vela:bin/gpu.bin` firmato `system` → eseguibile/`System`, `usr:runhello` firmato `thirdparty` → eseguibile/`ThirdParty`, `tst:test/tests.bin` non firmato → eseguibile/`Untrusted`; `vela:bin/unsigned.bin` (bucket obbligatorio) → negato; `tst:test/sigbad.bin` (byte flippato con il sidecar valido di un ALTRO binario) → negato; `tst:test/sigunk.bin` (sidecar con `key_id` azzerato) → negato; oggetto assente → `NotFound`. Gli artefatti li seeda `arca-tool.sh` host-side: e' l'attacco reale, non una simulazione nel client |
| t67 | classe nel PCB dal kernel (ADR-0079 fase 3): `SYS_TRUST_IMAGE` popola la trust table (solo cardo), il kernel ricalcola l'`ImageId` sui byte caricati e assegna `ImageClass`; `peer_info_class` legge la classe autorevole. Casi: helper firmato `system` spedito in `tst` → `System` (la firma vince sul bucket); helper non firmato → `Untrusted`; **TOCTOU** (verificato il contenuto su disco, si spawna una copia con un bit diverso) → `Untrusted`; gate strutturale: usertests (figlio di init, `Untrusted`) non registra `Service::Net` |
| t68 | porte I/O dal manifest (ADR-0079 fase 4): `ioprobe` (firmato `system`, riga di manifest `80-80`) legge la porta `0x80` → exit 0 (le porte vengono dal manifest, zero rebuild del kernel); la stessa sonda spawnata con `SpawnMeta.io_ranges = 0x2F8` legge `0x2F8` → `#GP` → `FAULT_EXIT_CODE` (la dichiarazione del chiamante è ignorata) |
| t69 | Custos, ruoli e capability (ADR-0080 fase 5): `CUSTOS_QUERY` concede le porte seedate a `netecho` (7/8) e nega la 9 e un'identità ignota (fail-closed); `IDENTIS_WHOAMI` risolve i ruoli **live** (`root`, `net-admin,user`, `user`) con l'identità che cambia a ogni `login` (S3.3c: `VERIFY` lega il pid, nessun token); un `user` non crea utenti né fa `GRANT`, un `net-admin` concede rete ma non hardware, root concede e revoca; `obj_put sys:caps` da usertests (che ha `RIGHTS_ALL` da `sys:test-policy`) è **negato** (carve-out `caps_writer`) e la riga del grant è persistita (persistenza prima della RAM) |
| t49 | fork COW (Fase 34): l'helper duplica se stesso; padre e figlio scrivono un globale COW e verificano l'isolamento; il figlio riporta valore+return sul canale di nascita (SYNC) ed esce 0; il padre verifica report + `EXIT_NOTIFY` con code 0 |
| t70 | consenso per immagine + broker (ADR-0080 fase 6): `CUSTOS_AUTHORIZE` nega un'immagine con grant ma senza consenso (assente = negato); `CUSTOS_CONSENT` accetta la scrittura con un'**identità attestata** (nessun token, S3.3c), persistendo la riga in `usr:consent` (tabella autorevole) **e** in `usr:<nome>:consent` (provenienza: chi ha acconsentito); la revoca disattiva l'autorizzazione; il consenso **non** crea privilegio dove manca il grant admin; `obj_put usr:consent` da usertests (`RIGHTS_ALL`) è negato (carve-out `consent_writer`). Solo con `net=1`: `net` nega l'endpoint a `Untrusted` e il **broker** lo serve (allowlist: porta 9 negata, effimera concessa) |
| t71 | `recv` con deadline (E1, ADR-0083): scadenza già passata → `RecvError::Timeout` senza consumare la coda; **la coda vince** — con una deadline lontana (+10000 tick) il `T_ACK` dell'helper `M_ECHO` appena spawnato viene consegnato (nessun evento perso per un timer in gara); timeout a 1 tick su un helper che non risponde mai → `Timeout` senza hang e senza perdite (poi `kill`)
| t72 | timer dell'executor (E1, ADR-0083): `run([Sleep(+30), Sleep(+60)])` torna dopo ≥ 60 tick (l'executor arma la **minima** dei task pending e non anticipa); `block_on(Sleep::ticks(25))` dorme ≥ 25 tick (misure con `get_ticks`, nessun risveglio a vuoto)
| t73 | finestre del compositor (D8b, ADR-0088): un handle di tile **inesistente** è negato (la tile è una capability); `WIN_OPEN` sulla tile primaria crea la superficie (shm) e restituisce l'id; il cliente la mappa RW, disegna un colore noto e il primo `WIN_COMMIT` rende la finestra visibile e **cambia il frame** (prova che i pixel sono arrivati); `WIN_STATS` conta la finestra; `WIN_CLOSE` la rimuove e il CRC del frame torna **byte-identico** a prima (la tile `Direct` viene ridisegnata sotto). SKIP nel fallback VGA |
| t75 | input dalle sessioni (D8d, ADR-0091): `input::attach` dichiara il lettore sulla sessione **attestata** (la stessa di `sess::out`) e spegne l'eco del modello; `read` = 0 a coda vuota con `wait_until(deadline)` = `Timeout` entro il bound (E1 intatto, coda non toccata); l'iniezione (`SESS_TEST_INPUT`, lo stesso percorso della tastiera) mette i byte in coda **e sveglia il lettore dichiarato** (il primo messaggio è l'hint `SESS_INPUT_EVENT`); `echo(true)` = il modello mostra i byte, `echo(false)` = no (li mostra il lettore); la coda si drena (i test successivi non trovano tasti fantasma); `detach` non chiude la sessione e `attach` la ritrova |
| t74 | sessioni di terminale (D8c1/D8c2, ADR-0089/0090): `SESS_STATS` conta la sessione 0 in focus; `whoami` **prima** di creare non trova nulla (`NotAttached`) e `WIN_OPEN(0)` («la tile della mia sessione») è negato senza attachment; `create` restituisce id + mailbox e `whoami` la **ritrova per pid** senza argomenti; geometria = quella del display, eco accesa, focus spento; il feed arriva al modello (`state`/`dump`) e il **sink del meccanismo** `civis::sess::out` scrive nella sessione attestata; il resize fa reflow senza perdere il testo; eco spento = l'input si legge ma non appare, eco acceso = appare; `bind` è negato a `Untrusted` (è del DE, ≥ `ThirdParty`) e `SESS_AUTH_SET`/`SESS_AUTH` sono negati a `Untrusted` (l'identità si tocca solo da `System`, S3.3c); `focus` su una sessione ≠ 0 è negato e `focus(0)` non cambia il CRC del frame; `close` riporta a 1/0 e `whoami` torna a fallire; `close(0)` e un id inesistente sono negati; **D8c2/D8d**: né `/dev/console` né `/dev/input/keyboard` sono apribili — il ritiro è loud (il testo è una sessione, i tasti una capability). Focus 0 garantito all'uscita |
| t50 | hardening (Fase 35, ADR-0026): un helper prova a killare un fratello (non suo figlio) e a registrare un servizio di sistema (`Init`) → entrambi rifiutati; usertests prova `map_physical` di RAM del kernel (0x100000) → rifiutato; prova a killare devfs (non suo figlio) → rifiutato (servizio vivo) |
| t51 | identita' misurata (Fase 36, ADR-0027): `peer_info` su Console/Devfs == manifest generato; stabilita' hash tra istanze; same-image positivo (X2 rimpiazza X1 vivo non-init-child, il mount sopravvive al kill); squat con hash diverso rifiutato (mount purgato, open fallisce); `peer_info` a canale morto → Err (helper REG51 + ramo SQUAT di spin) |
| t52 | exec in-place (Fase 37.0 nucleo + 37.1 argv, env in 43a): helper EXECDEMO diventa testspin su T_GO — stesso PID (T_ACK pre/post), hash rimisurato (diverso da prima, uguale a spin fresco), nuova immagine operativa (T_DONE); gamba argv+env (w1=1, exec ["ARGPROBE","hello","world"] + `T52E=envok`) con report T_DONE(argc,fnv) dal fresh `_start` (env verificato dalla sonda: assente = T_DONE(0,0)); reap via `poll_gone` (i `recv_done` consumano le EXIT_NOTIFY: `wait_exit` dopo sarebbe hang) |
| t57 | policy su identita' (Fase 45, ADR-0037): helper noto (riga test-policy ALL) — GET default ALL, drop GRANT→grant negato, drop PIPE→pipe_create negata, op valida dopo; attore ignoto `foreign.bin` fuori tabella policy — mount/grant/pipe_create negati dal default fail-closed (0x19F), open+read+write+seek lecite; read valida dopo i rifiuti (anti-wedge ring) **Fase 0 (ADR-0077)**: lo stesso attore ignoto prova i bucket di fiducia — `obj_put(b"sys", …)` negato, `open("/bin/vestigia.bin", TRUNC)` negato, `open("sys:identis", CREATE)` negato **e anche in lettura** (`open` + `obj_get` riservati, ADR-0096: esiti 0b111111111 = 9 bit); il diniego arriva come `Error::Denied` (prima un `ERR_DENIED` nel response ring era letto come lunghezza) |
| t58 | bucket oggetti nativi + BLAKE2s (Fase 55, N0; D1: driver in `vela`): oggetto vela/bin/gpu.bin byte-identico a /fat/bin/gpu.bin, blake2s == manifest `BLAKE_*` (stesso predicato di `verify_image` in init), byte flippato → digest diverso (rifiuto), chiave assente → errore, sys/bin/gpu.bin assente (trasloco provato), bound nomi oltre 16/255B rifiutati (hygiene, mai troncamento) |
| t34 | diritti per-canale lato server (Fase 17, per ultimo: drop irrevocabili): GET default ALL+root, drop WRITE (write -1/read ok), drop MOUNT+subtree /fat (mount/open-fuori -1, open-dentro+read+readdir-dentro ok, readdir-fuori -1), widen rifiutato + GET conferma |
| testsarca | ArcaFS P5+A1+56.1+56.2b+56.2c (Fase 54/55/56, binario separato `usertestsarca`, 50 check; E2: volume root + GPT): vettori BLAKE2s (empty/abc/lungo), `R_GET_HASH` ramfs == ricalcolo, tamper→hash diverso, round-trip `R_OBJ_PUT/GET` piccolo, chunking 10000B, chiave assente→errore, scan per magic ACFS + mount `/arca` + open/readdir rifiutati + umount, protective-MBR GPT (byte 450) + ACFS in partizione GPT + mount/umount, versioni (catena + latest), snap create, rollback come nuova head, snap delete, retention 8, delete oggetto, clone bucket, stat/get_id, open volume + alloc distinti + write/read nodi + stat volume (freelist popolata tollerata) + free/realloc LIFO + rifiuti allocatore, bind motore + seed classi (USEDISK tollerato, commit per-op), semantica 56.1 identica su blocchi, split multi-livello 120 chiavi, overflow 3000B + chiavi lunghe + bound, refcount pin oltre delete, crash kill-cardo + remount LOAD (dati committati intatti, gen monotona, R/W riparte), logging L1 Fase 57, vista POSIX 41-50 su mount root esplicito (v28 legge `vela` per gpu). 22-40 girano sul root vivo in bucket isolati (`test`/`v56`/`d56`/`dpin`/`dcrash`/`dgc`); parser MBR fuori dal gate (PC: 4 IDE, quarto drive = GPT). Senza quarto drive salta 12-13, resta PASS adattivo |

## Cosa copre `posixtests` (4 test; personalita' POSIX, ADR-0041)

Binario separato `userposixtests` (`flavours/posix/tests`), spawnato da init
PRIMA di `usertests`. Fissa la traduzione POSIX (errno, fd virtuali/redirect,
job control) sopra il meccanismo `civis`.

| Test | Cosa verifica |
|------|----------------|
| t53 | fondamenta posix (Fase 39, ADR-0030; skeleton 40.3; pipe 42, ADR-0032): `Posix` registrato e supervisionato (lookup ok, pid figlio di init), tabella `to_errno` totale (17 varianti: 15 + `Empty`/`Closed`→EAGAIN/EPIPE), `R_PIPE_CREATE` 0x20, gate di registrazione sul nuovo slot 8 via helper HARDEN esteso (kill + register Init + register Posix rifiutati) |
| t54 | fd virtuali + redirect a livello libr/server (Fase 40.5): `OPEN_TRUNC` (size 0 + rewrite), `OPEN_APPEND` (offset ignorato), `lseek` START/CURRENT/END + oltre-EOF lecito + negativo/whence-ignota/remoto = `Invalid` con offset invariato, codici esatti (`NotFound`/`IsDir`/`Exists`/`Invalid`, grant remoto e claim ignoto), handoff DUP modello B (claim con offset copiato, single-use, cancel, attestazione parentela via sibling: helper DUPCLAIM/DUPGRANT/DUPSIBCLAIM), routing stdio diretto (println→file, stdin drain+EOF, restore), diniego SEEK via diritti (helper SEEKDENY → `Failed`); fixture `/t54*` con cleanup |
| t55 | suspend/resume (Fase 44a, ADR-0035): gate (self/morto rifiutati, idempotenza, resume no-op), figlio running con TIME congelato su ~40 tick + resume→T_DONE/exit 0, figlio bloccato con `send_async` accodata senza sveglia + reply su resume, hardening non-parent (helper SUSPENDENY) |
| t56 | cancel cooperativo + escalation (Fase 44b, ADR-0036): catcher esce 42 al `JOB_CANCEL` senza kill; KILLME vivo oltre il grace poi esce 130 via `kill(EXIT_SIGINT)` |

> Il CBS e' sempre attivo (lo scheduler RT e' l'unico): t18/t19 sono test
> reali, non ci sono modalita' "vuote".

> I loop `recv`/`wait_reply` della suite sono **EXIT-aware** (Fase 14): le
> notifiche `EXIT_NOTIFY` che arrivano quando un helper termina vengono
> ignorate/skippate (mai scambiate per una reply o un'estranea da fallire).

### Dettagli degni di nota

- **Modalita' helper**: `usertestcli` sceglie la modalita' dal primo messaggio
  CFG dell'orchestratore. In `ZEROREAD` i client concorrenti aprono e leggono
  `/dev/zero` in parallelo (nessuna race: ogni client ha i propri ring SPSC,
  Fase 10.2); l'handshake `OPENED` + `GO` resta come semplice barriera di
  coordinamento.
- **Priorita'**: il test t17 usa High vs Normal. I server Normal idle
  (fs/shell) girano in recv-loop sempre-`Ready` (fix anti-deadlock), quindi una
  fascia `Low` non e' schedulabile finche' girano: Low resta usato solo da
  `useruptime` nel boot reale.
- **Stesso binario, piu' priorita'**: `usertestspin` gira a priorita' diverse
  via flag `prio` in `SpawnMeta` (Fase 21: t17 lo spawna a 31, gli altri usi a
  16/1). Prima della Fase 21 erano righe diverse in `NAMED_BINARIES`
  (`usertestspin`/`utspin_norm`/`utspin_high`) sullo stesso binario embedded;
  oggi solo init/disk/fs restano embedded.
- **Polling throttled nei test di restart (t27/t28)**: le attese di
  operativita' riprovano ogni ~20 tick via `civis::poll_wait`/`open_wait`,
  MAI in busy-loop su syscall FS. Igiene da buon vicinato (Livello 1):
  ogni tentativo e' un round-trip servito da cardo e non c'e' motivo di
  inondarlo. NOTA di onesta': l'attribuzione causale del vecchio FAIL t27
  al solo storm e' debole (N=1; la coda da 8 slot con client sync non puo'
  saturarsi per costruzione) — il throttle resta come disciplina, non come
  fix provato. Esperimento B (Fase C in busy-loop non throttled): t27 PASSA
  comunque → self-storm NON causale, confound confermato. Vedi t30 per il
  gate di fairness.
- **Binario copiato per processo** (`user_binary.rs::copy_binary`): i frame del
  binario embedded vengono copiati in frame privati a ogni spawn. Mappare gli
  stessi frame a piu' processi condividerebbe `.bss`/`.data` mutabili (es. la
  free-list dell'allocatore di `civis`) e corromperebbe lo stato di due istanze
  della stessa bin.
