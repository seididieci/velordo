#![no_std]

// ── Syscall numbers ────────────────────────────────────────────────────────
// Single source of truth: kernel e userland (civis) dipendono da questo crate.

pub const SYS_EXIT: u64 = 0;
// I numeri 3-7 (vecchie FS kernel-side) restano liberi (rimossi in Fase 10.2,
// mai riassegnati).
/// Scrive su console (`con` = maniglia console, non fd POSIX: nessun fd-table
/// nel kernel; 1 = standard-output, 2 = standard-error per coincidenza
/// storica coi numeri Unix, mai per semantica). `(con, ptr, len)` → byte
/// scritti o -1.
pub const SYS_WRITE: u64 = 2;
/// Maniglie console per SYS_WRITE (valori storici, vedi sopra).
pub const CONSOLE_OUT: u64 = 1;
pub const CONSOLE_ERR: u64 = 2;
pub const SYS_GETPID: u64 = 8;
/// IPC su channel (ADR-0008): `send(channel, tag, w0, w1)` — invia il
/// messaggio al peer del canale e BLOCCA il mittente finche' il peer non fa
/// `reply(tag, w0, w1)` (reply implicita al messaggio corrente). Riusa il
/// numero della vecchia send per PID (16).
pub const SYS_SEND: u64 = 16;
/// `recv()`: riceve il prossimo messaggio dalla propria coda e restituisce
/// `(channel, tag, w0, w1)`. Per le richieste (`req_id >= 0`) `channel` porta
/// il canale sorgente; per le risposte async (`req_id < 0`, Fase 13) porta il
/// `req_id` negativo. Riusa il numero di SYS_RECV (17).
pub const SYS_RECV: u64 = 17;
/// `reply(tag, w0, w1)`: risponde al mittente del messaggio che il chiamante
/// sta correntemente elaborando (reply implicita, `reply_chan` di ADR-0008).
/// Se il peer e' bloccato in `send` la risposta viaggia nel `reply_slot`
/// (sync); se e' async il kernel la accoda con `req_id = -reply_req` (Fase 13).
pub const SYS_REPLY: u64 = 18;
/// IPC async (Fase 13): `send_async(channel, tag, w0, w1)` — come `send` ma
/// NON blocca il mittente. Ritorna il `req_id` assegnato (>= 1), o -1 se la
/// coda del peer e' piena (backpressure) o il canale e' morto.
pub const SYS_SEND_ASYNC: u64 = 33;
/// IPC async (Fase 13): `recv_nonblock()` — come `recv` ma se la coda e'
/// vuota ritorna -1 subito, senza bloccare.
pub const SYS_RECV_NONBLOCK: u64 = 34;
/// Occupa lo slot del servizio `service` (ADRD-0008): il chiamante diventa
/// l'owner del servizio. Fallisce (-1) se il nome e' gia' occupato.
pub const SYS_SERVICE_REGISTER: u64 = 31;
/// Risolve il servizio `service` in un channel verso l'attuale owner.
/// Ritorna il channel id o -1 se il servizio non e' registrato.
pub const SYS_SERVICE_LOOKUP: u64 = 32;
pub const SYS_SPAWN: u64 = 20;
pub const SYS_MAP_PHYSICAL: u64 = 21;
pub const SYS_GET_TICKS: u64 = 22;
pub const SYS_MKDIR: u64 = 23;
pub const SYS_FS_REGISTER: u64 = 24;
pub const SYS_SBRK: u64 = 25;
/// Alloca due pagine fisiche per il ring buffer SPSC del processo corrente
/// (request + response), le mappa a `USER_FS_BUFFER` e `USER_RESP_RING`
/// (= `USER_FS_BUFFER+0x10000`), e ritorna gli indirizzi fisici (req in rax,
/// resp in rdi). Il chiamante registra entrambi presso il fs server con una
/// IPC `FS_BUF_REG`. Ogni chiamata da' pagine fresche (multi-coppia, Fase 16).
pub const SYS_RING_ALLOC: u64 = 26;
/// Mappa `count` pagine fisiche a partire da `phys` all'indirizzo virtuale
/// `virt` nello spazio del processo `pid` (usato da cardo per mappare la
/// pagina del client in un driver remoto (hub `vela`, `gpu`, ...) a `USER_FS_BUFFER`).
pub const SYS_MAP_IN: u64 = 27;
/// Crea un server CBS (budget, period) → id o -1 (admission control).
pub const SYS_CBS_CREATE: u64 = 28;
/// Lega un server CBS al processo corrente → 0 o -1.
pub const SYS_CBS_ATTACH: u64 = 29;
/// Informazioni CBS (budget/period/remaining) → budget in rax, period in rdi,
/// remaining in rsi, oppure -1.
pub const SYS_CBS_GET_INFO: u64 = 30;
/// Termina un processo user `pid` con il codice `code` (Fase 14, ADR-0010).
/// 0 se il processo e' stato terminato, -1 se il pid non esiste / non e'
/// killabile (init, processi kernel, se stesso).
pub const SYS_KILL: u64 = 35;
/// Ritorna il pid dell'owner attuale del servizio, o -1 se non registrato
/// (Fase 14, init-restart: supervisione e diagnostica).
pub const SYS_SERVICE_PID: u64 = 36;
/// Snapshot `ps` del processo `pid` (Fase 19.1): 0 se lo slot e' vivo, -1 se
/// vuoto/terminato. Campi multi-registro (pattern `CBS_GET_INFO`): nome (16 B,
/// il piu' lungo oggi e' "userdevreader"=13) in rdi+rsi (LE), `rdx` packed,
/// `r10` = tick consumati. Layout `rdx`: bit 0-7 stato (0=Ready, 1=Blocked,
/// 2=Stopped via `SYS_SUSPEND`, Fase 44a), 8-15 prio (0-31), 16-23 parent+1
/// (0=nessuno), 24-31 ipc (0=None, 1=OnRecv, 2=OnReply). Il chiamante marca
/// "run" il proprio pid (da `getpid`).
pub const SYS_PS_INFO: u64 = 37;
/// Spawna un processo dal binario in memoria del chiamante (Fase 21, servizi
/// da disco): `(img_ptr, img_len, meta_ptr, meta_len)`. Primitiva generale
/// (crea-figlio + carica-immagine): le porte I/O sono privilegio root
/// con `io_count == 0`); prio 1..31 per tutti. `meta` e' uno SpawnMeta da 40 B
/// (vedi sotto); ritorna il channel di nascita o -1.
pub const SYS_SPAWN_IMAGE: u64 = 38;
/// Mappa `len` byte anonimi privati (zero-fill lazy) nel basso canonico
/// (Fase 28, mmap): `(hint, len, prot, flags)`. Ritorna la base o -1.
/// Solo anonimo in 28: `prot` deve essere `PROT_READ|PROT_WRITE`, `flags`
/// 0 (hint consigliato, 0 = scelta kernel) o `MMAP_FIXED` (hint obbligatorio).
pub const SYS_MMAP: u64 = 39;
/// Smappa `[addr, addr+len)`: solo VMA intere in 28 (parziali = -1 senza
/// cambiare stato). Ritorna 0 o -1.
pub const SYS_MUNMAP: u64 = 40;
/// Cambia le protezioni di `[addr, addr+len)` (Fase 29, mprotect):
/// `(addr, len, prot)`. Solo VMA intere (come `munmap`). Ritorna 0 o -1.
pub const SYS_MPROTECT: u64 = 41;
/// Crea una regione di memoria condivisa (Fase 30): `(len)` → id (>= 1) o -1.
pub const SYS_SHM_CREATE: u64 = 42;
/// Mappa una regione condivisa (Fase 30): `(id, hint, prot, flags)` → base o
/// -1. Le pagine sono le stesse per tutti i mappatori (zero-copy tra processi).
pub const SYS_SHM_MAP: u64 = 43;
/// Contatori shared text (Fase 32, debug/test): ritorna `hits` in rax,
/// `misses` in rdi, `live` in rsi (nessun argomento).
pub const SYS_TEXT_STATS: u64 = 44;
/// Crea un figlio-copia che condivide l'address space del chiamante in COW
/// (Fase 34): nessun argomento. Ritorna al padre `(pid_figlio,
/// canale_nascita)` (pid in rax, canale in rdi via multi-registro), al figlio
/// `(0, canale_nascita)` (il figlio usa il canale 0 = `CHANNEL_PARENT`); -1
/// se non c'e' un PID libero o il pool canali e' esaurito.
pub const SYS_SPAWN_COPY: u64 = 45;
/// `peer_pid(chan)`: pid del peer del canale `chan` (0 = canale di nascita,
/// come `send`/`recv`), o -1 se il canale non esiste/`chan` non ne fa parte
/// (Fase 35, hardening: i server possono attribuire una richiesta a un
/// processo — es. la policy `FS_REGISTER` di cardo distingue i figli di
/// init). Non rivela nulla che `ps_info` non mostri gia'.
pub const SYS_PEER_PID: u64 = 46;
/// `peer_info(chan)`: hash dell'immagine (`image_hash`, FNV-1a sull'ELF) del
/// peer del canale `chan` (0 = canale di nascita), o -1 se il canale non
/// esiste/il peer e' morto (Fase 36, identita' misurata, Strato 2 di ADR-0026).
/// Multi-registro (pattern `PS_INFO`): rax = 0 + rdi = hash; -1 = errore.
/// I server lo usano per la policy su identita' (manifest init, `FS_REGISTER`
/// in cardo). Non rivela nulla oltre l'identita' del binario (nomi e pid sono
/// gia' visibili via `ps_info`/`peer_pid`).
pub const SYS_PEER_INFO: u64 = 47;
/// Sostituisce l'immagine del chiamante (Fase 37, `exec` in-place):
/// `(img_ptr, img_len)` = ELF in memoria del chiamante (mai il FS: il kernel
/// non tocca il disco, ADR-0005). Stesso PID/parent/priorita'/canali (fd
/// server-side sopravvivono); cade tutto l'address space e ne viene caricato
/// uno nuovo; stack nuovo (argc=0 in 37.0, argv in 37.1); `image_hash`
/// rimisurato (Strato 2); porte I/O azzerate. Non ritorna mai al chiamante
/// (salta all'entry nuova); -1 = validazione fallita, processo intatto.
/// Stesso bound di `spawn_image` (256 KiB).
pub const SYS_EXEC: u64 = 48;
/// Alloca `pages` (1..=DMA_PAGES_MAX) frame fisici CONTIGUI azzerati per DMA
/// Bus-Master (Fase 38.1, ATA DMA): li mappa RW/NX a `USER_DMA_VA` e ritorna
/// il fisico base (il chiamante programma PRD e BMIBA con phys reali — VA
/// non bastano al device). Single-slot per processo (seconda alloc = -1);
/// free a teardown/exec, mai ereditata dal figlio-copia. Precedente: `SYS_RING_ALLOC`
/// (26) ritorna gia' phys alle ring — stessa neutralita' (ADR-0005: il kernel
/// non tocca il disco, alloca solo frame).
pub const SYS_DMA_ALLOC: u64 = 49;
/// Sospende un processo user `pid` (Fase 44a, job control): fuori dalle ready
/// queue finche' `SYS_RESUME` (i wake lo saltano, i messaggi restano in coda).
/// Meccanismo neutro (ADR-0025): niente segnali numerati. 0 se sospeso (o gia'
/// sospeso: idempotente), -1 se il pid non esiste / non e' sospendibile
/// (init, processi kernel, se stesso, non-figlio, terminato).
pub const SYS_SUSPEND: u64 = 50;
/// Rimette in schedulazione un processo sospeso (Fase 44a, job control):
/// no-op ok se gia' running. Stessi gate di `SYS_SUSPEND`.
pub const SYS_RESUME: u64 = 51;
/// Fermo immagine RAM del PMM (Fase 52, P3 durabilita'): nessun argomento.
/// Multi-registro (pattern `text_stats`): rax = frame liberi, rdi = frame
/// totali, rsi = usati (= totali - liberi). Istantanea senza lock (atomici):
/// due chiamate possono differire sotto allocazione concorrente. Sensore per
/// swap futuro (B1) e quota (A3); mai decisioni kernel (il kernel non fa
/// OOM-kill: negativa ADR-0028).
pub const SYS_MEMINFO: u64 = 52;
/// Command line di boot PVH (`-append` QEMU): copia i byte in un buffer user
/// (`dst`, `cap`), ritorna la lunghezza totale (se `> cap`, troncato: il
/// chiamante rialloca). 0 = assente/vuota. Single source kernel+civis.
/// Meccanismo neutro (ADR-0025): byte opachi, il parse `root=UUID=` vive in
/// cardo (Fase 2 root su volume, mai first-found).
pub const SYS_BOOT_CMDLINE: u64 = 53;
// ── Thread 1:1 + futex (S-T, ADR-0046) ──────────────────────────────
// THREAD_CREATE(entry, stack, fs_base, flags): crea un thread nel gruppo del
// chiamante (stesso address space, canali/fd condivisi col leader; kernel
// stack + TSS propri, user stack fornito dal chiamante via mmap). Ritorna il
// tid o -1. THREAD_EXIT(code): termina SOLO il thread corrente (l'ultimo
// chiude il gruppo con la via completa di exit). THREAD_SET_FS(base):
// aggiorna la base TLS (FS) del thread corrente. FUTEX_WAIT(addr,
// expected, deadline_tick): dorme se *addr == expected fino a WAKE o
// deadline (0 = mai); FUTEX_WAKE(addr, n): sveglia fino a n waiter.
// Ritorni: create = tid / -1; wait = 0 svegliato / 1 deadline / -1 errore;
// wake = svegliati.
pub const SYS_THREAD_CREATE: u64 = 54;
pub const SYS_THREAD_EXIT: u64 = 55;
pub const SYS_FUTEX_WAIT: u64 = 56;
pub const SYS_FUTEX_WAKE: u64 = 57;
pub const SYS_THREAD_SET_FS: u64 = 58;
// ── Directory di lavoro (S1.1, cwd nel PCB) ─────────────────────────
// CHDIR(path_ptr, len): imposta la cwd del GRUPPO (leader; i thread la
// condividono come POSIX) — solo path assoluti (il client unisce i relativi
// alla cwd cached; il kernel resta byte-opaco, niente `..`/normalizzazione).
// GETCWD(buf, cap): copia cwd + NUL, ritorna len senza NUL (cap corta = -1,
// il chiamante rialloca come boot_cmdline). CWD_MAX 256 = MAX_PATH cardo.
pub const SYS_CHDIR: u64 = 59;
pub const SYS_GETCWD: u64 = 60;
pub const CWD_MAX: usize = 256;
/// Bound command line salvata dal kernel (stesso del parser PVH, mai oltre).
pub const BOOT_CMDLINE_MAX: usize = 512;
/// Cap pagine di `SYS_DMA_ALLOC` (38.1: 1 pagina = PRD + 7 settori bastano).
pub const DMA_PAGES_MAX: usize = 4;
/// Bound del payload argv+env serializzato (Fase 37.1, esteso in 43a):
/// blocco `[argc:8][envc:8][argv NUL-separated][env NUL-separated
/// "NAME=val"]` oltre cui `exec` rifiuta fail-loud. Single source
/// kernel+user (`civis` lo riesporta): 8 KiB bastano a shell e test con margine.
/// Il kernel tratta le stringhe come byte opachi (mai ispezione `=`: la
/// convenzione `NAME=val` vive in `civis`/shell, il kernel resta neutro).
pub const ARGS_MAX: u64 = 8 * 1024;
/// Immagine massima spawabile/eseguibile (Fase 21: 64 frame = 256 KiB; i binari
/// sono < 70 KiB — un singolo spawn non puo' svuotare il pool frame). Single
/// source kernel (`sys_spawn_image`, stesso bound per `exec` in Fase 37) +
/// user (`civis` pre-valida prima della syscall per errori precisi).
pub const SPAWN_IMAGE_MAX: usize = 256 * 1024;
/// Protezioni `mmap`/`mprotect` (29: NONE/R/RW con enforcement; W solo e
/// PROT_EXEC rifiutati — eseguibile solo il codice di spawn).
pub const PROT_NONE: u64 = 0x0;
pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
/// Flag `mmap`: piazza esattamente a `hint` (o fallisci), niente fallback.
pub const MMAP_FIXED: u64 = 0x1;
/// Flag `shm_map` (Fase 33, COW): mappa i frame della regione `RO`+`COW`
/// (copy-on-write) invece che condivisi-scrittibili. Solo con
/// `prot == PROT_READ` (la scrittura materializza la copia privata al primo
/// fault). Ogni mappatura COW incrementa il refcount per-frame: due processi
/// leggono gli stessi dati finche' non scrivono, poi isolati.
pub const MAP_COW: u64 = 0x2;
/// Layout stack user condiviso kernel+test (Fase 29, single source qui):
/// lo stack vive a `USER_STACK_TOP` (cresce verso il basso, 4 frame);
/// la pagina a `USER_STACK_GUARD` (subito sotto) NON e' mai mappata:
/// lo stack overflow fa #PF li' → kill (mai corruzione silenziosa).
/// NB: `USER_BASE + 0x2000_0000` (0x0000_4000_2000_0000): spostato a +512M
/// in S2.5c (la finestra immagine occupa [0, +256M), lo stack non deve
/// starci dentro).
pub const USER_STACK_TOP: u64 = 0x0000_4000_2000_0000;
pub const USER_STACK_FRAMES: usize = 4;
pub const USER_STACK_GUARD: u64 = 0x0000_4000_2000_0000 - 5 * 0x1000;
/// Codice di uscita con cui il kernel termina un processo user che provoca
/// un fault di memoria non recuperabile (Fase 29: #PF di protezione, accesso
/// a PROT_NONE, stack overflow nella guard). Il parent lo osserva via
/// EXIT_NOTIFY (w0). 139 = 128 + 11 (SIGSEGV, convenzione POSIX).
pub const FAULT_EXIT_CODE: i64 = 139;
/// Causa di morte per Ctrl-C (Fase 44b, job control): il parent che uccide
/// un job non cooperante dopo il cancel usa questo code. 130 = 128 + 2
/// (SIGINT, stessa convenzione POSIX di `FAULT_EXIT_CODE`). Solo convenzione
/// al bordo (shell/civis): il kernel vede un banale exit code.
pub const EXIT_SIGINT: i64 = 130;
/// Base del codice user (link address del binario ELF, single source
/// kernel+test): il loader ELF mappa i segmenti al `p_vaddr` di link e
/// l'entry e' `e_entry`. Prima solo nel kernel (`layout.rs`).
pub const USER_CODE: u64 = 0x0000_4000_0000_0000;
/// Blocco VA alte per DMA/ring/relay (S2.5c): `USER_BASE + 1T
/// (0x0000_4100_0000_0000)`, offsets intra-blocco invariati. Spostato da
/// +32M perche' la finestra immagine occupa [USER_CODE, +256M) e lo heap
/// (da +512M fino a +512G) attraverserebbe il vecchio blocco.
const FS_BLOCK: u64 = 0x0000_4100_0000_0000;
/// Finestra staging DMA del processo corrente (Fase 38.1, single source
/// kernel+user come `USER_CODE`): `SYS_DMA_ALLOC` mappa qui i frame contigui
/// (PRD + dati). Dopo CLI_RESP (`+0x230_000`) e DISK_RESP (`+0x250_000`):
/// prima VA libera.
pub const USER_DMA_VA: u64 = FS_BLOCK + 0x260_000;
/// Finestre ring del processo corrente (single source S2.2: prima letterali
/// duplicati in `layout.rs` del kernel, `civis/src/fs/ring.rs` e
/// `cardo/src/rings.rs` con commenti "coincide con..."). `SYS_RING_ALLOC`
/// mappa qui la coppia di pagine (request + response).
pub const USER_FS_BUFFER: u64 = FS_BLOCK + 0x200_000;
/// Finestra response ring (USER_FS_BUFFER + 0x1000).
pub const USER_RESP_RING: u64 = FS_BLOCK + 0x210_000;
/// Finestre relay cardo→driver (zero-copy DEV_*, Fase 10.2): cardo inietta
/// qui via `map_in` i ring del client. Single source come sopra (prima solo
/// in `civis/src/fs/ring.rs`, usate dai driver via `civis::`).
pub const CLI_REQ_VA: u64 = FS_BLOCK + 0x220_000;
/// Finestra response per i relay cardo→driver (vedi sopra).
pub const CLI_RESP_VA: u64 = FS_BLOCK + 0x230_000;
/// Flag `SpawnMeta.flags` (Fase 22, detach): il figlio non partecipa alla
/// cascata di morte del parent — alla morte del parent viene ri-parentato a
/// init invece di terminare. Deciso dallo spawner (il figlio non puo'
/// auto-staccarsi); irrevocabile. Bit riservati: devono essere 0 (rifiuto).
pub const SPAWN_FLAG_DETACH: u8 = 0x01;
/// Bound di scansione per `ps` (Fase 19.1): i PID vivono in 0..PS_SCAN_MAX.
/// Deve restare uguale al `MAX_PIDS` del kernel (32, Fase 14).
pub const PS_SCAN_MAX: u32 = 32;
/// IPC tag: il client ha scritto nel request ring e notifica il server.
pub const FS_NOTIFY: u64 = 0x32;
// ── Tag IPC userland, single source (centralizzazione DocsB: prima duplicati
// in `civis`, cardo/block/init/tty/kbd e come letterali nei test) ────────
// - FS_REGISTER (0x30): un driver registra il prefix di mount (frame
//   R_REGISTER nel request ring, letto da cardo).
// - FS_BUF_REG (0x31): handshake register-only "i miei ring sono req=w0,
//   resp=w1" (client e driver verso cardo).
// - KBD_NOTIFY (0x40): kbd → porta, scancode in coda (w0 = count).
// - JOB_CANCEL (0x43): parent → figlio, cancel cooperativo job control
//   (Fase 44b: Ctrl-C della shell; w0 = 2/SIGINT, informativo). Il figlio
//   puo' gestirlo (cleanup + exit a sua scelta) o ignorarlo (il parent
//   scala a `kill(EXIT_SIGINT)` dopo un grace).
// - SVC_READY (0x7D): servizio → init, "sono su" (fire-and-forget a boot).
// - TEST_DONE (0x7E): test suite → init, fine sequenza (w0 = ok count).
// - INIT_BOUNCE (0x7F): figlio → init, "uccidi+riavvia il servizio `w0`"
//   (Fase 35, hardening: uccidere un server supervisionato e' operazione da
//   supervisore — i test guidano il caos tramite init invece di killare
//   direttamente; init risponde con reply (0 = pid ucciso, ERR = ignoto)).
pub const FS_REGISTER: u64 = 0x30;
pub const FS_BUF_REG: u64 = 0x31;
pub const KBD_NOTIFY: u64 = 0x40;
pub const JOB_CANCEL: u64 = 0x43;
pub const SVC_READY: u64 = 0x7D;
pub const TEST_DONE: u64 = 0x7E;
pub const INIT_BOUNCE: u64 = 0x7F;
// ── Protocollo DEV_* (cardo→driver: vela/gpu/kbd/tty/block, DocsD) ───
// Single source of truth dei tag e dei device type (w0 di DEV_OPEN): prima
// duplicati in cardo/block/vela/gpu/kbd/tty. cardo instrada per
// prefix al server e inoltra l'op; il driver risponde sul relay.
// - OPEN/READ/WRITE/CLOSE/READDIR: op sui nodi device (raw o sintetizzati).
// - Type: NULL/ZERO (vela), KEYBOARD (tty, `/dev/input`), CONSOLE (gpu,
//   `/dev/console`), KBD (kbd, `/dev/kbd`); block usa handle disco<<16|sub.
pub const DEV_OPEN: u64 = 0x20;
pub const DEV_READ: u64 = 0x21;
pub const DEV_WRITE: u64 = 0x22;
pub const DEV_CLOSE: u64 = 0x23;
pub const DEV_READDIR: u64 = 0x24;
pub const DEV_NULL: u64 = 0;
pub const DEV_ZERO: u64 = 1;
pub const DEV_KEYBOARD: u64 = 2;
pub const DEV_CONSOLE: u64 = 3;
pub const DEV_KBD: u64 = 4;
// ── Protocollo TIME_* (client→usertime, Fase 50/P1) ─────────────────────
// Single source of truth dei tag. Il server `Time` e' il fornitore di
// data/ora del sistema (CMOS all'avvio + monotono PIT): serve il FS (mtime)
// e qualunque servizio client. Una sola op sincrona, server veloce:
// - NOW (0x60): richiesta data/ora; reply `w0` = secondi epoch (UTC),
//   `w1` = centesimi di secondo nel secondo corrente (0..99).
pub const TIME_NOW: u64 = 0x60;
// ── Protocollo LOG_* (client→vestigia, Fase 57/L1, ADR-0039) ───────────
// Single source of truth dei tag. Il servizio `Vestigia` e' il gateway centrale
// di logging (bucket `log` nativo + coda RAM quando il volume manca):
// - REG (0x61): handshake register-only "i miei ring sono req=w0, resp=w1"
//   (stampo FS_BUF_REG; niente frame). Il server archivia (chan→phys) +
//   peer_pid/peer_info per attribuzione. Reply (0,0).
// - APPEND (0x62): il client ha scritto un frame `[level:1][taglen:1][tag][msg]`
//   (livello+formato in `civis::log`; il tag e' solo hint leggibile, il bucket
//   e' DERIVATO dal server dall'identita' del chiamante) nel proprio ring LOG
//   e notifica con w0 = payload-len (expect, come R_OBJ_GET); il server mappa
//   il ring via `map_physical` (stampo `map_client_req_ring` di cardo), timbra
//   tick+epoch e accoda in RAM (+ volume dopo la FLUSH). Reply (seq, durable).
// - READ (0x63): payload `[giorno:8][seq:8]` sul bucket PROPRIO del chiamante
//   (seq=0 → latest); risposta nel response ring del client (stampo DEV) +
//   reply (len, seq). Niente nomi sul wire (ADR-0039).
// - SEAL (0x64): snapshot esplicito del bucket `log` (retention via
//   snapshot+GC esistenti); reply (snap_id, 0). Niente R_SYNC: commit per-op.
// - STATS (0x65): contatori (appended, evicted); reply (appended, evicted) +
//   frame `[durable:8][last_seal:8]`. Solo registri+frame, mai blocco.
// - FLUSH (0x66): solo dal parent (init, dopo fs+time): handshake FS +
//   backdate/re-key RAM + riversamento incrementale + dual-write da li'.
//   Gli altri ricevono ERR. Reply (0,0) = accettato.
pub const LOG_REG: u64 = 0x61;
pub const LOG_APPEND: u64 = 0x62;
pub const LOG_READ: u64 = 0x63;
pub const LOG_SEAL: u64 = 0x64;
pub const LOG_STATS: u64 = 0x65;
pub const LOG_FLUSH: u64 = 0x66;
/// Bound protocollo LOG (Fase 57, ADR-0039): sorgente 1..=32 B (niente `/`
/// ne' NUL: e' un segmento di chiave oggetto), messaggio 1..=1024 B (il
/// record `[tick:8][epoch:8][level:1][srclen:1][src][msg]` resta in un frame).
pub const LOG_SRC_MAX: usize = 32;
pub const LOG_MSG_MAX: usize = 1024;
/// Coda RAM del servizio `vestigia` (Fase 57): ultimi N record sempre leggibili
/// anche senza volume (degrado RAM-only); oltre si butta il piu' vecchio e si
/// conta in `evicted` (drop ammesso e contato, mai wedge il chiamante).
pub const LOG_RAM_TAIL: usize = 128;
/// Tag kernel→parent: un figlio e' terminato (exit o kill). Il kernel lo invia
/// sul canale di nascita con `w0` = exit code e `w1` = pid del figlio morto
/// (Fase 14, ADR-0010). Non e' una richiesta: il parent non deve rispondere.
pub const EXIT_NOTIFY: u64 = 0x7C;

// ── Protocollo DISK_* (data-plane cardo→block, Fase 16) ───────────────
// Single source of truth dei tag (Fase 16c): prima duplicati in
// `userland/fs/src/ipc_disk.rs` e `userland/disk/src/main.rs`. I tag viaggiano
// nei registri IPC; i payload (nomi, settori) nei ring dedicati.
//
// Canale diretto cardo→block (service_lookup(Disk)):
// - HELLO/OPEN/CLOSE: solo registri, niente frame.
// - READ: un settore per chiamata, frame `[512:8][0:8][settore]` nel ring DISK_RESP.
// - WRITE (20): un settore per chiamata, frame `[512:8][settore]` nel ring
//   DISK_REQ (handle in w0, lba in w1 dei registri); reply w0 = 0 o ERR,
//   nessun frame di risposta.
// - RESOLVE (16c): il nome nodo ("sda", "sda1") viaggia in un frame
//   `[namelen:8][name]` nel ring DISK_REQ; la reply porta l'handle in w0
//   (o ERR). block e' l'unico proprietario della mappa nome→handle:
//   cardo non indovina piu' nulla dal nome.
pub const DISK_HELLO: u64 = 0x50;
pub const DISK_OPEN: u64 = 0x51;
pub const DISK_READ: u64 = 0x52;
pub const DISK_CLOSE: u64 = 0x53;
pub const DISK_RESOLVE: u64 = 0x54;
/// Scrive un settore (Fase 20, FAT scrivibile): vedi sopra.
pub const DISK_WRITE: u64 = 0x55;
// ── Topologia disco (Fase 51, P2 vocabolario) ───────────────────────
// - LIST (0x56): nessun input (w0=w1=0, niente frame REQ); reply `w0` =
//   numero dischi + 1 frame RESP con N entry fisse da 16 B ciascuna
//   `[sectors:8][flags:8]` (ordine dischi: sda=0, sdb=1, ...).
// - INFO (0x57): `w0` = handle (vale la parte disco, sub ignorata), niente
//   frame REQ; reply `w0` = settori, `w1` = flags + 1 frame RESP fisso 76 B
//   `[model_len:8][model:40][serial_len:8][serial:20]` (il client sa sempre
//   cosa leggere; lunghezze reali nei primi u64 di ogni meta').
// Layout `flags` (u64, condiviso LIST/INFO/R_DISK_*): bit 0 = LBA48, bit 1 =
// TRIM capability (word 169.0, solo rilevata mai usata), bit 8-11 = modo UDMA
// negoziato (0-2) o 0xF = PIO, bit 16-31 = rotation rate word 217 (1 = SSD,
// altrimenti RPM), bit 32-47 = settore logico (B), bit 48-63 = fisico (B).
pub const DISK_LIST: u64 = 0x56;
pub const DISK_INFO: u64 = 0x57;
// - FLUSH (0x58, Fase 52): `w0` = handle (vale la parte disco), niente frame;
//   emette FLUSH CACHE sul drive (barriera della write-cache del device).
//   Reply 0 o ERR. Usato dalla barriera `R_SYNC(GROUP)`.
pub const DISK_FLUSH: u64 = 0x58;

// ── Formato superblock ArcaFS: casa propria (Fase 56.2a) ──────────────
// ARCA_MAGIC/VERSION/BLOCK_SIZE/SUPER_LEN/OFF_* vivono in `arcafs::format`
// (condivisi guest/host); il commento di formato resta qui sotto come
// mappa: blocco 128 B a LBA0 (+ shadow LBA1), checksum FNV-1a u64 su
// [0..120] in [120..128] (self-verifying). Il seal BLAKE2s-256 (§3) NON
// sta qui: wiring in A2.

// ── Tag delle operazioni FS (nel frame del ring, non nell'IPC) ────────────
// Single source of truth (Fase 17): prima duplicati in `civis`, `cardo` e
// (R_REGISTER) `block`. Il formato frame e' `[tag:4][w0:8][w1:8][payload]`.
pub const R_OPEN: u32 = 0x10;
pub const R_READ: u32 = 0x11;
pub const R_WRITE: u32 = 0x12;
pub const R_CLOSE: u32 = 0x13;
pub const R_READDIR: u32 = 0x14;
pub const R_MKDIR: u32 = 0x15;
/// Monta una sorgente su un target: payload "source\0target\0".
pub const R_MOUNT: u32 = 0x16;
/// Smonta un target: payload "target".
pub const R_UMOUNT: u32 = 0x17;
/// Cancella un file o una directory VUOTA: payload = path (Fase 18.2).
/// Solo ramfs (su FAT manca l'unlink e i device remoti rifiutano con ERR).
pub const R_DELETE: u32 = 0x1A;
/// Metadati del path (Fase 19.2, zero kernel): payload = path; risposta
/// self-written `[size:8][kind:8]` + `[mtime:8]` (Fase 50). Nessun fd coinvolto.
pub const R_STAT: u32 = 0x1B;
/// Sposta l'offset di un descrittore LOCALE (Fase 40, P1): w0 = descrittore,
/// w1 = offset (bit reinterpretati come i64: negativi leciti per
/// SEEK_END/SEEK_CURRENT), payload 1 byte = origine (SEEK_*).
/// Solo Local (Remote → ERR_INVALID); dir →
/// ERR_ISDIR. Ritorna il nuovo offset o una sentinella ERR_*. Nessun cambio
/// se il check fallisce (two-phase: valida prima, applica dopo).
pub const R_LSEEK: u32 = 0x1C;
/// Grant single-use per handoff fd al figlio (Fase 40, modello B): w0 = fd
/// locale del chiamante, nessun payload. Registra uno snapshot dell'entry
/// (offset copiato al claim, entry indipendente) e ritorna un nonce u64 che
/// il figlio usa in CLAIM. Solo Local (Remote → ERR_INVALID).
pub const R_DUP_GRANT: u32 = 0x1D;
/// Claim di un grant (Fase 40): payload `[nonce:8]`. Solo il FIGLIO del
/// registrante (`ps_info(me).parent == registrant_pid` + `peer_pid` sul
/// canale del registrante). Ritorna il nuovo fd o ERR_INVALID. Single-use:
/// il grant viene consumato (retry = nuovo grant).
pub const R_DUP_CLAIM: u32 = 0x1E;
/// Cancella un grant pendente (Fase 40, cleanup parent): payload `[nonce:8]`.
/// Best-effort idempotente: sempre Ok, anche se il nonce non esiste (mai
/// wedge il parent nel cleanup dei job). Come CLOSE/DROP/GET, sempre
/// consentito (nessun bit di diritto).
pub const R_DUP_CANCEL: u32 = 0x1F;
/// Crea una pipe (Fase 42): nessun payload (expect 0), w0 = hint di capacita'
/// in byte (clampato server-side a [4096, 16384], default 8192 a hint 0).
/// Ritorna l'fd di LETTURA nel result e quello di SCRITTURA in w1 (il frame
/// di risposta porta entrambi: `[result:8][w1:8]`). Buffer in cardo, semantica
/// non-bloccante: read a vuota con writer aperti → ERR_EMPTY (riprova
/// throttled), a writer chiusi → 0 (EOF); write a piena → parziale, a lettori
/// chiusi → ERR_CLOSED. Mai blocco del server single-threaded.
pub const R_PIPE_CREATE: u32 = 0x20;
// ── Topologia disco via cardo (Fase 51, P2: relay verso DISK_*, riusato da
// `arca list` in P5) ───────────────────────────────────────────────────
// - R_DISK_LIST (0x21): nessun payload (expect 0); risposta self-written
//   `[count:8][0:8][entry...]` con entry fisse 16 B `[sectors:8][flags:8]`
//   (layout flags come DISK_LIST, ordine dischi sda=0, ...).
// - R_DISK_INFO (0x22): w0 = indice disco, nessun payload; risposta
//   self-written `[sectors:8][flags:8]` + frame fisso 76 B come DISK_INFO.
//   Solo lettura (mai settori): gate RIGHTS_READDIR, niente subtree
//   (topologia globale).
pub const R_DISK_LIST: u32 = 0x21;
pub const R_DISK_INFO: u32 = 0x22;
// ── Durabilita' e sensori (Fase 52, P3) ─────────────────────────────
// - R_SYNC (0x23): w0 = modo (`SYNC_NONE/GROUP/PERWRITE`), niente payload.
//   Imposta l'aspettativa di durabilita' del canale e ritorna il modo
//   precedente (pattern umask, testabile); `GROUP` esegue anche la barriera
//   subito (FLUSH di tutti i mount FAT attivi). Solo risultato, mai frame.
// - R_STATVFS (0x24): payload = path; risposta self-written
//   `[0:8][0:8][bsize:8][blocks:8][bfree:8][bavail:8]` (32 B). Nessun fd.
pub const R_SYNC: u32 = 0x23;
pub const R_STATVFS: u32 = 0x24;
// ── Integrita' (Fase 54, P5: opzione A compute-on-query) ────────────
// - R_GET_HASH (0x25): payload = path; risposta self-written `[0:8][0:8]` +
//   32 B (BLAKE2s-256 del contenuto, riletto e hashado a chunk 4K — nessuno
//   stato, nessuno store; il seal nativo arriva con ArcaFS in A1). Solo
//   mount `Local` (ramfs/FAT via trait `read`); device/remoti/sintetici e
//   mount inattivi → ERR. Gate RIGHTS_READDIR + subtree come R_STAT.
pub const R_GET_HASH: u32 = 0x25;
/// Rinomina `old` in `new` sullo STESSO mount (S1.1): payload
/// `old\0new\0` (w0 = lunghezza totale). Stesso-mount o rifiuto
/// (`ERR_INVALID` cross-mount: niente EXDEV tipizzato, mai silent-copy);
/// mount remoti/FAT → rifiuto (FAT: namespace immutabile come mkdir/remove;
/// readonly). Sovrascrittura di `new` esistente = replace atomico (la
/// catena di `new` e' liberata nello stesso commit su Arca; su ramfs move
/// di chiave). Limite noto: fd aperti sul vecchio nome vanno stale
/// (NotFound al prossimo uso, mai dati altrui — gli handle sono per-path).
/// Numero 0x33: R_* e FS_* condividono il match di dispatch in cardo —
/// 0x30/0x31/0x32 sono FS_REGISTER/FS_BUF_REG/FS_NOTIFY (0x31 ha dirottato
/// l'handshake dei ring a boot: osservato, mai piu').
pub const R_RENAME: u32 = 0x33;
/// Symlink S1.1: R_SYMLINK crea (`link\0target\0`, w0 = len), R_READLINK
/// legge (`path`, w0 = len; reply w0 = len target + frame col target).
/// Il target NON e' risolto alla creazione (dangling ammessi come POSIX);
/// la risoluzione avviene all'open (max 8 hop). FAT: namespace immutabile
/// (READONLY come rename); remoti: INVALID.
pub const R_SYMLINK: u32 = 0x34;
pub const R_READLINK: u32 = 0x35;
/// chmod S1.1 (projection, non enforcement): payload path (w0 = len),
/// w1 = mode (12 bit Unix: setuid/setgid/sticky + rwx×3, gli altri bit si
/// mascherano). Accetta SEMPRE su path esistente (i build invocano chmod:
/// fallire hard romperebbe il porting toolchain — spec arcafs.md); la mode
/// si conserva dove banale (ramfs), altrove no-op documentato; enforcement
/// zero fino ad A4/login boundary. FAT/remoti: accettano anche loro (neutro).
pub const R_CHMOD: u32 = 0x36;
// ── ArcaFS: casa propria (Fase 56.2a) ────────────────────────────────
// Tag R_OBJ_*/R_SNAP_*/R_ARCA_DEBUG, bound OBJ_*_MAX e formato blocchi
// (ARCA_*) vivono nel crate `arcafs` (il kernel non li usa — verificato).
// Qui restano solo i tag che kernel o piu' server condividono.
/// Modi `R_SYNC` (Fase 52, P3): nessuna garanzia richiesta / barriera
/// esplicita (flush+barriera) / ogni write stabile prima della reply.
/// `PERWRITE` e' gia' il comportamento FAT (write-through); ramfs resta
/// volatile in ogni modo (dichiarato nel contratto, mai promesso).
pub const SYNC_NONE: u32 = 0;
pub const SYNC_GROUP: u32 = 1;
pub const SYNC_PERWRITE: u32 = 2;
/// `kind` per R_STAT (Fase 19.2): bit 0-1 tipo + bit 7 readonly.
pub const STAT_FILE: u64 = 0;
pub const STAT_DIR: u64 = 1;
pub const STAT_DEVICE: u64 = 2;
/// Symlink (S1.1): quarto tipo nei 2 bit. `stat` NON segue (lstat: riporta
/// il link); `open` segue (fino a 8, oltre = INVALID come ELOOP senza numero
/// dedicato). Mapping al bordo POSIX (S_IFLNK) in S1.3.
pub const STAT_SYMLINK: u64 = 3;
pub const STAT_READONLY: u64 = 0x80;
/// Flag `open`: crea il file se non esiste (Fase 18.2: prima l'open creava
/// sempre su ramfs ignorando i flag — senza OPEN_CREATE il file deve
/// esistere). Viaggia in w1 del frame R_OPEN (civis lo passava gia', il server
/// lo ignorava).
pub const OPEN_CREATE: u32 = 0x200;
/// Flag `open` (Fase 40, P1): azzera il file esistente (size → 0). Con OPEN_CREATE
/// su file esistente: tronca invece di aprire intatto. Viaggia in w1 di
/// R_OPEN come OPEN_CREATE (bit indipendenti, combinabili).
pub const OPEN_TRUNC: u32 = 0x400;
/// Flag `open` (Fase 40, P1): ogni write accoda a fine file (l'offset del canale
/// e' ignorato in scrittura; la lettura usa l'offset normale). Viaggia in w1
/// di R_OPEN. Combinabile con OPEN_CREATE (crea se manca, poi accoda).
pub const OPEN_APPEND: u32 = 0x800;
/// Origini di R_LSEEK (Fase 40, P1): dall'inizio, dal corrente, dalla fine.
/// Payload 1 byte del frame R_LSEEK; altri valori → ERR_INVALID.
pub const SEEK_START: u64 = 0;
pub const SEEK_CURRENT: u64 = 1;
pub const SEEK_END: u64 = 2;
/// Un driver registra il proprio prefix di mount.
pub const R_REGISTER: u32 = 0x30;
/// Riduce i propri diritti sul canale (Fase 17, self-restriction only):
/// w0 = mask dei bit da TENERE (solo shrink: new = old & w0), w1 = len
/// subtree, payload = subtree (vuoto = solo-ops). Mai widen, mai auth.
pub const R_RIGHTS_DROP: u32 = 0x18;
/// Legge i propri diritti (Fase 17): niente payload; risposta self-written
/// `[ops:8][sublen:8][subtree]` (subtree normalizzato, "" = root).
pub const R_RIGHTS_GET: u32 = 0x19;

// ── Bit dei diritti per-canale lato cardo (Fase 17, 18.2) ───────────────
// Solo riduzione (DROP fa AND), default ALL. CLOSE sempre consentito (rilascia
// stato, mai escalation: nessun bit). Diritti effimeri: restart cardo =
// re-handshake full; niente policy per-identita' (serve il kernel).
pub const RIGHTS_OPEN: u32 = 0x01;
pub const RIGHTS_READ: u32 = 0x02;
pub const RIGHTS_WRITE: u32 = 0x04;
pub const RIGHTS_READDIR: u32 = 0x08;
pub const RIGHTS_MKDIR: u32 = 0x10;
pub const RIGHTS_MOUNT: u32 = 0x20;
pub const RIGHTS_UMOUNT: u32 = 0x40;
/// Cancellazione file/dir vuote (Fase 18.2, `R_DELETE`).
pub const RIGHTS_DELETE: u32 = 0x80;
/// Spostamento offset via R_LSEEK (Fase 40, P1): senza, lseek e' negato ma
/// read/write sull'offset corrente restano (bit indipendenti).
pub const RIGHTS_SEEK: u32 = 0x100;
/// Creazione grant per handoff fd (Fase 45, `R_DUP_GRANT`): senza, il grant
/// e' negato (CLAIM/CANCEL restano liberi: consumano grant propri).
pub const RIGHTS_GRANT: u32 = 0x200;
/// Creazione pipe (Fase 45, `R_PIPE_CREATE`): senza, `pipe()` e' negato.
pub const RIGHTS_PIPE: u32 = 0x400;
/// Barriera di durabilita' (Fase 52, `R_SYNC` Group): senza, la sync
/// esplicita e' negata ma le write restano stabili per costruzione
/// (write-through: il diniego non indebolisce le write, nega solo il costo
/// di un FLUSH pilotato — anti sync-storm). Bit 11: primo oltre lo storico
/// 0x7FF (discriminanti 0-10 intoccati).
pub const RIGHTS_SYNC: u32 = 0x800;
pub const RIGHTS_ALL: u32 = 0xFFF;

// ── Sentinelle di errore FS (Fase 40, P1) ─────────────────────────────
// cardo distingue i rifiuti invece del generico ERR: il client li mappa
// nelle varianti di dominio di `civis::posix::Error` (ADR-0030: i numeri POSIX
// restano solo in `to_errno`, mai nel kernel/wire). Valori ALTI da `!0` a
// scendere, MAI `-errno`: `-2` colliderebbe con ERR_NOHANDSHAKE (retry
// handshake) e `-1` con ERR generico. `R_DUP_*` usa solo ERR_INVALID
// (capability, mai path: un grant o e' valido o non esiste).
/// Errore IPC generico: tutti i bit a 1 (S2.2: prima solo in `civis`, ma lo
/// invia cardo — e' ABI).
pub const ERR: u64 = !0u64;
/// Il server non conosce (piu') i nostri ring (S2.2: idem sopra) → rifare
/// handshake + 1 redo.
pub const ERR_NOHANDSHAKE: u64 = !0u64 - 1;
pub const ERR_NOTFOUND: u64 = !0u64 - 2;
pub const ERR_ISDIR: u64 = !0u64 - 3;
pub const ERR_NOTDIR: u64 = !0u64 - 4;
pub const ERR_EXISTS: u64 = !0u64 - 5;
pub const ERR_READONLY: u64 = !0u64 - 6;
pub const ERR_BUSY: u64 = !0u64 - 7;
pub const ERR_INVALID: u64 = !0u64 - 8;
/// Pipe vuota con writer ancora aperti (Fase 42): non EOF, riprova throttled.
/// Il client la mappa in `Error::Empty` (EAGAIN al bordo POSIX).
pub const ERR_EMPTY: u64 = !0u64 - 9;
/// Estremita' opposta della pipe chiusa (Fase 42): write senza lettori o
/// grant/claim su stato morto. Il client la mappa in `Error::Closed` (EPIPE).
pub const ERR_CLOSED: u64 = !0u64 - 10;
/// Budget quota superato (A3, ADR-0044): put/clone/rollback oltre
/// `quota_blocks` del bucket. Il client la mappa in `Error::NoSpace` (ENOSPC
/// al bordo). Mai oltre il bound prima di allocare: niente mezze scritte.
pub const ERR_NOSPC: u64 = !0u64 - 11;

// ── Costanti condivise kernel/userland ─────────────────────────────────────
// Pagina fisica scratch riservata dal kernel all'avvio (phys_mem::reserve):
// usata dalla test suite per verificare `map_physical` (aliasing write/read)
// senza toccare memoria di altri processi. 64M: oltre immagine, heap, bitmap
// e tabelle statiche (16M) in ogni config; sempre RAM nei config test.
// (Prima a 16M: collideva con le tabelle `.tables_high` dell'higher-half 27.3
// — t12 scriveva pattern sopra le PD direct → fault ritardato. Mai piu':
// invariante di non-sovrapposizione verificata dal compilatore in kernel.)
pub const MAP_TEST_PHYS: u64 = 0x4_000000;
/// Frame scratch riservati (2 pagine: P1 e P2 di t29/maphammer — il
/// data-plane di test usa due frame distinti sulla stessa VA in processi
/// diversi). Fase 35: solo questi frame + ring + VGA sono mappabili via
/// `map_physical`; con 1 frame il secondo scratch veniva rifiutato.
pub const MAP_TEST_FRAMES: u64 = 2;

/// Identita' misurata di un'immagine ELF (Fase 36, Strato 2 di ADR-0026):
/// FNV-1a a 64 bit sui byte dell'ELF. Single source kernel+user: il kernel la
/// misura allo spawn (`image_hash` nel PCB) e la espone via `SYS_PEER_INFO`;
/// init/cardo la ricalcolano sui byte caricati per la policy (manifest,
/// `FS_REGISTER`). Stesso algoritmo dello sharing text (Fase 32, che ora usa
/// questa funzione): sui `.bin` di build i due valori coincidono bit-per-bit.
pub fn image_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Servizi di sistema raggiungibili per nome (ADR-0008, IPC per nome).
/// Il discriminant coincide con l'indice di slot nel registry del kernel
/// (`channels.rs`): `#[repr(u64)]` + assegnazione esplicita per rendere
/// stabile l'ABI attraverso le syscall. Nessuna magic string nel kernel.
///
/// Nota: questo enum elenca SOLO i servizi scopribili per nome. I processi che
/// non sono servizi (helper di test, demo) non registrano nomi: comunicano
/// col parent tramite il canale di nascita creato da `spawn`.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// Terminale video (VGA). Usato da kbd_process e dai client.
    Gpu = 0,
    /// Perno file (cardo): tutti i client FS lo risolvono per nome.
    Cardo = 1,
    /// Hub `/dev` (vela, prefix `/dev`; pseudo-device null/zero).
    Vela = 2,
    /// Processo init (root della process tree). Non registra attivamente, ma
    /// lo slot esiste per chi deve raggiungerlo (es. test suite).
    Init = 3,
    /// Servizio sacrificale della test suite (Fase 14, t24): un helper lo
    /// registra per farsi raggiungere da un secondo helper via
    /// `service_lookup`, poi viene killato per verificare la notifica
    /// `EXIT_NOTIFY` a TUTTI i peer (non solo al parent). Mai usato in
    /// produzione: nessun server reale lo registra o lo risolve.
    Test = 4,
    /// Driver tastiera PS/2 in userspace (Fase 15, `kbd`): pubblica
    /// scancode raw sul device `/dev/kbd`. Il kernel (IRQ1) risolve questo
    /// servizio per nome e sveglia l'owner (routing + EOI, mai lettura porte).
    Kbd = 5,
    /// Soglia d'ingresso in userspace (Fase 15, `porta`, R9): decodifica i tasti,
    /// fa echo e serve `/dev/input/keyboard`. Registrato per la supervisione
    /// init (restart); nessun altro lo risolve per nome (i client usano il FS).
    Porta = 6,
    /// Driver ATA a blocchi in userspace (Fase 16, `block`, R6): rileva i dischi,
    /// espone `/dev/sdX` (+`/dev/sdXn` per le partizioni MBR). cardo lo
    /// risolve per nome per il data-plane `DISK_*`; init lo supervisiona.
    Block = 7,
    /// Server di personalita' POSIX in userspace (Fase 39, P0 della roadmap
    /// 39-45, ADR-0030): tabelle fd virtuali, pipe, job control. Solo
    /// controllo e stato globale POSIX; il kernel resta neutro (ADR-0025) e
    /// il data plane resta diretto client→cardo.
    Posix = 8,
    /// Fornitore di data/ora in userspace (Fase 50, P1 orologio, `usertime`):
    /// legge il CMOS all'avvio (epoch) e serve `TIME_NOW` (epoch + monotono
    /// PIT). Supervisionato da init come gli altri driver.
    Time = 9,
    /// Gateway centrale di logging L1 (Fase 57, `vestigia`, ADR-0039): append
    /// nel bucket `log` nativo (+ coda RAM senza volume), lettura, seal via
    /// snapshot, contatori. Supervisionato da init dopo `Time`.
    Vestigia = 10,
}

/// Massimo numero di servizi conosciuti = dimensione del registro kernel.
/// Fase 39: 8→16 (slot liberi per futuri servizi senza ritoccare il
/// kernel; discriminant storici intoccati, ABI stabile). Fase 50: slot 9
/// assegnato a `Time`; Fase 57/R8: slot 10 assegnato a `Vestigia`; liberi 11-15.

/// Massimo numero di servizi conosciuti = dimensione del registro kernel.
pub const SERVICE_COUNT: usize = 16;

/// Canale predefinito del processo: il canale di nascita verso il parent.
/// Ogni processo nasce con canale 0 = parent (o `CHANNEL_NONE` per init/idle).
pub const CHANNEL_PARENT: u64 = 0;
/// Valore "nessun canale" usato quando il parent non esiste.
pub const CHANNEL_NONE: u64 = u64::MAX;

/// Tag del messaggio con cui il kernel sveglia `kbd` su IRQ1 (Fase 15):
/// bridge interrupt→IPC — un wake senza messaggio non farebbe mai ritorno
/// da `recv()` (la coda vuota ri-blocca in kernel), quindi l'handler accoda
/// questa notify (fire-and-forget, MAI risposta: canale 0, nessun peer) e
/// kbd drena l'hardware ad ogni giro comunque (anche se la notify si perde
/// per coda piena, il drain successivo recupera).
pub const IRQ_NOTIFY_KBD: u64 = 0x41;
/// Tag del messaggio con cui il kernel sveglia `block` su IRQ14/15 (Fase 38,
/// ATA DMA: bridge interrupt→IPC come IRQ1→kbd — un wake senza messaggio non
/// farebbe mai ritorno da `recv()`; block drena lo status Bus-Master ad ogni
/// giro, anche su wake spurio o notify persa per coda piena). Fire-and-forget,
/// MAI risposta: canale 0, nessun peer.
pub const IRQ_NOTIFY_DISK: u64 = 0x42;
