# System Calls

## Panoramica

Le system call sono l'interfaccia tra user space e kernel space. Permettono ai programmi utente di accedere a risorse protette (dischi, memoria, processi).

## Piano di implementazione (Fase 6 — 4 sotto-fasi)

La Fase 6 viene sviluppata in sotto-fasi incrementali, ciascuna verificabile in QEMU.
Decisioni architetturali prese: **syscall/sysret** (moderno, come sotto), **separazione minima
di memoria per-processo** (page table per processo, kernel *non* accessibile dagli user via
bit `USER`), **programma user embedded** nel kernel via `incbin` (nessun ELF loader / file
system in questa fase).

- [x] **6.1 — Infrastruttura**: GDT con segmenti user (DPL 3); TSS con `RSP0` dinamica
      (aggiornata a ogni context switch); allocazione di page table per-processo che condividono
      la mappa kernel (entry senza `USER`) e mappano una regione user con `USER_ACCESSIBLE`;
      `Process` con `cr3` + kernel stack; context switch che carica `CR3` e aggiorna `TSS.RSP0`.
      *Verifica*: keyboard/uptime/idle continuano a girare invariati. ✅ completata
- [x] **6.2 — Entry Ring 3**: frame CPU user (`CS/SS` RPL 3, `RSP` user, `RIP` entry) + trampoline
      `iretq`; primo processo (stub `jmp $` in `user_stub.rs`) gira in ring 3 e viene preemptato dal
      timer. *Verifica*: uptime prosegue mentre userdemo busy-loppa in ring 3. ✅ completata
      (nota storica: `USER_BASE` con pml4 index ≠ 0 per non collidere con
      l'identity map del kernel a PML4[0] — oggi `PML4[0] = 0` a runtime e il
      basso ospita le mappe utente (Fase 28, ADR-0020), ma l'indice resta).
- [x] **6.3 — Meccanismo syscall/sysret**: MSR `STAR`/`LSTAR`/`SFMASK` + `EFER.SCE`; entry assembly
      con salvataggio/ripristino RSP; handler base `getpid`/`write`/`exit`. *Verifica*: un
      processo user chiama una syscall e il kernel risponde. ✅ completata
- [x] **6.4 — Embed binario utente + demo**: crate `userland/demo` freestanding → binary raw
      (`objcopy -O binary`) incluso nel kernel via `include_bytes!` (`kernel/src/user_binary.rs`);
      caricamento nello spazio utente; demo `getpid + write + busy-loop` che dimostra la preemption
      anche in ring 3. *Verifica finale*: `[demo] hello from userland, pid=4` + `[demo] tick` mentre
      `[uptime]` continua ad avanzare (preemption in ring 3). ✅ completata
      - **ABI syscall completa**: `syscall_entry` preserva TUTTI i registri general purpose tranne
        `RAX` (ritorno) e `RCX`/`R11` (semantica syscall/sysret). Bug critico risolto: senza salvare
        `r8`/`r9`/`r10` (caller-saved per SysV), un processo che tiene un valore in `r8` attraverso
        una syscall (es. il pid in `r8` dopo `getpid`) leggeva spazzatura al ritorno → crash/wedge.
      - **PIC (position independent)**: `USER_CODE = 0x4000_0000_0000` supera il range dei reloc
        assoluti a 32 bit, quindi la userland e' compilata con `-C relocation-model=pic`
        (`scripts/build-userland.sh`) — RIP-relative in `.text`, ma le chiamate
        indirette passano dalla GOT che richiede `--apply-dynamic-relocs` (senza,
        le entry GOT restano 0x0 → page fault al primo call indiretto).
      - **Entry deterministica**: il kernel salta a `USER_CODE` (= inizio `.text`). `demo.ld`
        forza `KEEP(*(.text._start))` come prima sezione, altrimenti lld puo' mettere
        `rust_begin_unwind` (panic handler) prima di `_start` e il boot eseguirebbe il panic.

**Scelte di dettaglio**: spazio utente in una regione virtuale alta canonica (es.
`0x0000_4000_0000_0000`, pml4 index 128, bit 47=0) — all'epoca per non collidere
con la identity map bassa del kernel (oggi `PML4[0] = 0` a runtime, ADR-0020);
un solo core → variabili kernel globali per
lo stato durante syscall/sysret (una struct `PerCpu` **raggiunta via `rip`-relative**,
senza `swapgs`); la demo e' un processo aggiuntivo schedulato accanto
a keyboard/uptime/idle.

## SYSCALL/SYSRET

x86_64 fornisce istruzioni speciali per le system call:

```
User Mode (Ring 3):
  mov rax, <syscall_number>   ; Numero della system call
  mov rdi, <arg1>             ; Argomento 1
  mov rsi, <arg2>             ; Argomento 2
  mov rdx, <arg3>             ; Argomento 3
  syscall                     ; Cambia a Ring 0

Kernel Mode (Ring 0):
  Salva stato utente
  Dispatch alla handler
  Esegui operazione
  Risultato in RAX
  sysret                     ; Torna a Ring 3
```

## Tabella delle System Call

Le syscall **effettivamente implementate** sono poche e numerate in modo non contiguo.
Le posizioni 3-7/23-24 erano usate per il relay FS (ora rimosse, Fase 9.6).
La numerazione e' definita nel dispatch di `syscall_handler` in `kernel/src/syscall.rs`:

| Num | Syscall | Note |
|-----|---------|------|
| 0 | `exit(code)` | termina il processo corrente, da qualunque thread (`sched::exit_process`: tutto il gruppo) |
| 2 | `write(con, buf, count)` | `CONSOLE_OUT`/`CONSOLE_ERR` → seriale; altre maniglie → `-1` (non fd POSIX) |
| 3-7 | (ritirate) | erano `open/read/write_fs/close/readdir` kernel-side; dalla Fase 9.6 sono IPC dirette client→cardo (wrapper `civis` su ring, Fase 10.2) |
| 8 | `getpid()` | id del processo corrente |
| 16 | `send(channel, tag, w0, w1)` | IPC per canale (0 = parent), Fase 7+13 |
| 17 | `recv(deadline)` | IPC per canale: ritorna (channel, tag, w0, w1), Fase 7+13; **E1/ADR-0083**: `arg1` = tick assoluto di scadenza (0 = mai), alla scadenza `rax = -2` senza consumare messaggi (**la coda vince**) |
| 18 | `reply(tag, w0, w1)` | risponde al messaggio corrente (reply implicita), Fase 7+13 |
| 20 | `spawn(name)` | crea un processo dal binario **embedded** e ritorna il **canale di nascita** verso il figlio (Fase 8.1+13); **ADR-0079 fase 4**: solo init (pid 1) — gli altri usano `spawn_image`/`exec` |
| 21 | `map_physical(phys, virt, count)` | mappa pagine fisiche nello spazio user (Fase 8.2) |
| 22 | `get_ticks()` | ritorna il contatore PIT corrente (Fase 8.3) |
| 23-24 | (ritirate) | erano `mkdir`/`fs_register`; dal Fase 9.6 le operazioni FS sono IPC dirette a cardo |
| 25 | `sbrk(inc)` | estende l'heap (solo VA; pagine lazy demand-zero) |
| 26 | `ring_alloc()` | alloca/mappa le DUE pagine ring per-processo (request+response, Fase 10.2) |
| 27 | `map_in(channel, phys, virt, count)` | mapper generico cross-process: inietta pagine note nello spazio del peer (Fase 9.6+10.2) |
| 28 | `cbs_create(budget, period)` | crea un server CBS → id o -1 (admission control) |
| 29 | `cbs_attach(server_id)` | lega il server CBS al processo corrente → 0 o -1 |
| 30 | `cbs_get_info(server_id)` | info CBS del server: rax=budget, rdi=period, rsi=remaining |
| 31 | `service_register(service)` | occupa lo slot del servizio (ADR-0008, Fase 12); **ADR-0079 fase 3**: slot privilegiati (tutti tranne `Test`) richiedono figlio di init **e** classe `System` |
| 32 | `service_lookup(service)` | risolve il servizio in un canale verso l'owner (ADR-0008, Fase 12) |
| 33 | `send_async(channel, tag, w0, w1)` | IPC async: come `send` ma **non blocca**; ritorna il `req_id` (>= 1) o -1 (Fase 13) |
| 34 | `recv_nonblock()` | IPC async: come `recv` ma coda vuota → -1 subito (Fase 13) |
| 35 | `kill(pid, code)` | termina un processo user (exit/kill kernel-side) → 0 o -1 (Fase 14, ADR-0010) |
| 36 | `service_pid(service)` | pid dell'owner del servizio o -1 (supervisione/diagnostica, Fase 14) |
| 37 | `ps_info(pid)` | snapshot `ps`: 0 + nome in rdi+rsi, packed stato/prio/parent/ipc in rdx, tick in r10; -1 se slot vuoto (Fase 19.1; stato 2=Stopped dalla 44a) |
| 38 | `spawn_image(img, len, meta, metalen)` | come `spawn` ma il binario e' in memoria del chiamante (servizi da disco, Fase 21); `meta` = `SpawnMeta` 64 B (nome/prio; **ADR-0079 fase 4**: `io_ranges` **ignorate**, le porte vengono dalla trust entry del contenuto; S2.5e: `args_ptr`/`args_len` = blocco packed `[argc:8][envc:8][stringhe NUL]`, stesso formato di exec, zero = argc=0, layout via direct map); **S3.4a**: `rights` = diritti ereditari packed (floor di classe nei bit 0..8, maschera ops nei bit 8..40; `0` = eredita), il kernel applica la monotonia e rifiuta l'elevazione); ritorna il canale di nascita o -1 |
| 39 | `mmap(hint, len, prot, flags)` | mappa anonima privata nel basso canonico (Fase 28/29): VA subito, frame zero al primo fault; `hint` 0 = scelta kernel, `MMAP_FIXED` = piazza o fallisci; `prot` = `PROT_NONE`/`PROT_READ`/`PROT_READ\|PROT_WRITE` (W solo ed EXEC rifiutati); ritorna la base o -1 |
| 40 | `munmap(addr, len)` | smappa VMA intere (Fase 28, niente split: parziali = -1 senza stato) → 0 o -1 |
| 41 | `mprotect(addr, len, prot)` | cambia le protezioni di VMA intere (Fase 29): RO↔RW flippa il bit W, `PROT_NONE` fa cadere le pagine (riuso a zeri); stesse regole di `munmap`; ritorna 0 o -1 |
| 42 | `shm_create(len)` | crea una regione di memoria condivisa (Fase 30, max 256 KiB, frame contigui azzerati) → id (>= 1) o -1 |
| 43 | `shm_map(id, hint, prot, flags)` | mappa la regione condivisa `id` come VMA (prot R/RW, PTE non-owned pre-materializzate): le pagine sono le stesse per tutti i mappatori (scritture visibili); refcount, a 0 i frame sono liberati; con `MAP_COW` (Fase 33, solo `PROT_READ`) le pagine sono `RO`+`COW` (stessi frame finche' nessuno scrive, copia privata al primo write) con refcount per-frame; ritorna la base o -1 |
| 63 | `shm_destroy(id)` | chiude una regione condivisa (Fase 30.1, owner-only): marca `destroyed` (i `shm_map` successivi falliscono) e rilascia il riferimento dell'owner; i frame vanno via subito se non ci sono mappature, altrimenti all'ultimo `munmap`/teardown; il teardown del processo rilascia comunque i riferimenti owner delle regioni che possiede; 0 o -1 |
| 44 | `text_stats()` | contatori shared text (Fase 32, debug/test): `hits` in rax, `misses` in rdi, `live` in rsi; dalla Fase 33 `rdx` = fault COW gestiti (`cow_count`); fix N6.3 `r10` = fallback privati per **tabella piena** (`text_full()`, 0 in condizioni normali: `TEXT_MAX` = processi concorrenti massimi) |
| 45 | `spawn_copy()` | figlio-copia in COW (Fase 34, nessun argomento): padre `(pid_figlio, canale)` (rax + rdi multi-registro), figlio `(0, canale)`; -1 su PID/canali/OOM esauriti |
| 46 | `peer_pid(chan)` | pid del peer del canale `chan` (0 = nascita), o -1 (Fase 35, hardening: i server attribuiscono le richieste; abilita la policy `FS_REGISTER`) |
| 47 | `peer_info(chan)` | **identita'** del peer del canale `chan` (0 = nascita): 0 + `rdi` = `lo`, `rsi` = `hi` di un `ImageId` BLAKE2s-128, o -1 (Fase 36, identita' misurata; digest crittografico da ADR-0078: policy su identita' in init/cardo). **ADR-0079 fase 3**: `rdx` = `ImageClass` del peer (`0` `Untrusted`, `1` `ThirdParty`, `2` `System`), la classe autorevole del kernel (mai una dichiarazione). **S3.4a**: `r10` = diritti ereditari packed (floor nei bit 0..8, ops nei bit 8..40) |
| 48 | `exec_image(img, len, args, argslen)` | sostituisce l'immagine del chiamante (Fase 37, exec in-place): stesso PID/canali, nuovo address space + stack argv+env stile Linux (`args` = blocco `[argc:8][envc:8][argv][magic?][env]` entro `ARGS_MAX`, 0/0 = argc=0; env = byte opachi, kernel neutro — ADR-0033), hash rimisurato; mai ritorno (salta all'entry), -1 a validazione fallita (processo intatto) |
| 49 | `dma_alloc(pages, slot)` | alloca `pages` frame contigui azzerati per DMA Bus-Master (Fase 38.1): mappa RW/NX nella finestra dello slot, ritorna `rax` = fisico base (il device vuole phys per PRD/BMIBA), `rdi` = pagine, `rsi` = VA dello slot. **Slot 0** = staging (≤ `DMA_PAGES_MAX` = 4, VA `USER_DMA_VA`, come prima); **slot ≥ 1** = device buffer (≤ `DMA_DEVICE_PAGES_MAX` = 1024 = 4 MiB, grant riservato all'owner di `Service::Console`, D3/ADR-0060); `DMA_SLOT_ANY` = primo libero. Free a teardown/exec, mai ereditata dal figlio-copia. **ADR-0079 fase 3**: gli slot device (≥ 1) richiedono classe `System`; lo slot 0 (staging) resta aperto |
| 50 | `suspend(pid)` | congela un processo user (Fase 44a, job control, ADR-0035): fuori dalle ready queue finche' resume (i wake lo saltano, i messaggi restano in coda); meccanismo neutro. 0 se sospeso (idempotente), -1 se non sospendibile (init/kernel/se'/non-figlio/terminato) |
| 51 | `resume(pid)` | rimette in schedulazione un sospeso (Fase 44a): no-op ok se running; un bloccato con coda non vuota si sveglia subito. Stessi gate di `suspend` |
| 64 | `trust_image(id_lo, id_hi, class, ranges_ptr)` | registra `ImageId → (ImageClass, porte I/O)` nella **trust table** del kernel (ADR-0079 fasi 3–4): solo cardo (owner di `Service::Cardo`) o init; max 32 entry, mai sovrascritte da altri. `ranges_ptr` = `TrustIo` (`repr(C)`: `count` + `[(u16,u16);4]`, intervalli inclusivi; 0 = nessuna porta). Il kernel ricalcola l'`ImageId` sui byte caricati allo spawn/exec e fa lookup qui per assegnare classe (PCB) e porte (TSS). 0 o -1 (non autorizzato/tabella piena/payload malformato) |

> **Fase 29 (protezioni)**: `PROT_NONE`/`PROT_READ`/`PROT_READ|PROT_WRITE` sono
> enforced dal page-fault handler. Un fault di protezione da user mode (write
> su RO, exec su NX, accesso a NONE) o un accesso fuori da ogni regione (es. la
> guard page sotto lo stack) **termina il processo** con `FAULT_EXIT_CODE`
> (139) — mai halt del kernel. Heap, stack, pagine `mmap` e pagine iniettate
> (`map_physical`/`map_in`) sono non-eseguibili (NX, EFER.NXE); il binario user
> e' ancora RWX perche' flat (W^X richiede i confini di sezione all'embed-time,
> 29b).

> **Fase 9.6** (sostituita da 10.2): le syscall FS 3-7, 23, 24 sono state RIMOSSE
> dal percorso dati. Ogni processo alloca DUE pagine ring (`ring_alloc`, 26:
> request a `USER_FS_BUFFER`, response a `USER_RESP_RING`) e le registra presso
> cardo con una IPC register-only (`FS_BUF_REG`, tag `0x31`); le operazioni
> open/read/write/close/readdir/mkdir/fs_register sono IPC dirette client→cardo
> (1 frame `[tag][w0][w1][payload]` + `send(FS_NOTIFY)`, risposta come frame
> `[result][w1][payload]`). Il kernel non e' piu' nel percorso dati. Restano
> syscall `getpid`, `write` (stdout seriale), `spawn`/`spawn_image`,
> `send/recv/reply` (+ varianti async), `map_physical`/`map_in`, `get_ticks`,
> `sbrk`, `exit`.

> **Fase 12 (ADR-0008)**: `send`/`recv`/`reply` indirizzano per **channel**,
> non per PID; `spawn` ritorna il canale di nascita. I servizi si registrano
> per nome (`service_register` 31) e si risolvono con `service_lookup` (32).

> **Fase 14 (ADR-0010)**: `kill(pid, code)` termina un processo user (mai init,
> i processi kernel o se stesso). La morte (exit o kill) e' gestita in due
> tempi: la "morte logica" (stato `Terminated`, release di canali/servizi/CBS,
> risveglio dei peer bloccati in `send` verso il morto, cascata sulla
> discendenza) e il teardown fisico differito (stack kernel, slot TSS, address
> space user) eseguito a ogni tick, con **riuso dei PID**. Solo dopo il teardown
> il kernel notifica **tutti i peer** del morto (messaggio tag `EXIT_NOTIFY`,
> `w0` = exit code, `w1` = pid, sul canale che li collegava — single path, il
> parent e' un peer come gli altri). I client async ricevono
> `Err(ServerDied)` da `wait_reply` invece di attendere per sempre.

Le syscall classiche `fork`/`wait`/`brk`/`mmap` non sono implementate.
`read`/`open`/`close`/`readdir`/`mkdir`/`stat` esistono come **wrapper `civis`**
(IPC dirette client→cardo sui ring, zero copie — v. [File System](./09-filesystem.md)),
non come syscall kernel: i numeri 3-7/23-24 sono ritirati.

## Dispatch (handler)

Il handler **non riceve gli argomenti come parametri**: l'entry assembly li salva tutti
nell'area `PerCpu` (numero + arg1..arg4) e il handler li rilegge da li'. La firma e'
quindi `extern "C" fn() -> i64` (a differenza della classica `fn(a1,a2,a3,a4)->i64`).

```rust
extern "C" fn syscall_handler() -> i64 {
    let p = addr_of_mut!(PERCPU);
    (*p).ipc_override = 0;
    match (*p).number {
        0  => sys_exit((*p).arg1 as i64),
        2  => sys_write((*p).arg1, (*p).arg2 as *const u8, (*p).arg3 as usize),
        8  => sys_getpid(),
        16 => sys_send((*p).arg1 as usize, (*p).arg2, (*p).arg3, (*p).arg4),
        17 => sys_recv((*p).arg1),
        18 => sys_reply((*p).arg1, (*p).arg2, (*p).arg3),
        20 => sys_spawn((*p).arg1, (*p).arg2 as usize),
        21 => sys_map_physical((*p).arg1, (*p).arg2, (*p).arg3 as usize),
        22 => sys_get_ticks(),
        25 => sys_sbrk((*p).arg1),
        26 => sys_ring_alloc(),
        27 => sys_map_in((*p).arg1 as usize, (*p).arg2, (*p).arg3, (*p).arg4 as usize),
        28 => sys_cbs_create((*p).arg1, (*p).arg2),
        29 => sys_cbs_attach(),
        30 => sys_cbs_get_info((*p).arg1),
        31 => sys_service_register((*p).arg1),
        32 => sys_service_lookup((*p).arg1),
        33 => sys_send_async((*p).arg1 as usize, (*p).arg2, (*p).arg3, (*p).arg4),
        34 => sys_recv_nonblock(),
        35 => sys_kill((*p).arg1, (*p).arg2 as i64),
        36 => sys_service_pid((*p).arg1),
        37 => sys_ps_info((*p).arg1 as usize),
        38 => sys_spawn_image((*p).arg1, (*p).arg2 as usize, (*p).arg3, (*p).arg4 as usize),
        _  => -1,
    }
}
```

## System Call implementate

### Process Management

| Syscall | Numero | Descrizione |
|---------|--------|-------------|
| `exit` | 0 | Termina il processo corrente, da qualunque thread (`sched::exit_process`: tutto il gruppo) |
| `getpid` | 8 | Restituisce il PID corrente |
| `send` | 16 | IPC per canale (0 = parent): invia e blocca in attesa della reply |
| `recv` | 17 | IPC per canale: blocca finche' non arriva un messaggio; **E1/ADR-0083**: `arg1` = deadline in tick (0 = mai), scadenza → `-2` (coda vince) |
| `reply` | 18 | risponde al messaggio corrente (reply implicita) |
| `send_async` | 33 | IPC async: invia SENZA bloccare → req_id o -1 (Fase 13) |
| `recv_nonblock` | 34 | IPC async: recv non bloccante, coda vuota → -1 (Fase 13) |
| `service_register` | 31 | occupa lo slot del servizio (Fase 12) |
| `service_lookup` | 32 | risolve il servizio in un canale (Fase 12) |
| `service_pid` | 36 | pid dell'owner del servizio o -1 (Fase 14, init-restart) |
| `ps_info` | 37 | snapshot `ps` di un processo: 0 o -1; nome (16 B) in rdi+rsi, `rdx` packed (stato 0/1/2=Stopped in 44a/prio/parent+1/ipc), `r10` tick consumati (Fase 19.1) |
| `kill` | 35 | termina un processo user (Fase 14, ADR-0010) |
| `suspend`/`resume` | 50/51 | congela/rimette in schedulazione un processo user (Fase 44a, job control, ADR-0035) |
| `meminfo` | 52 | fermo immagine RAM del PMM (Fase 52, P3): rax=frame liberi, rdi=totali, rsi=usati; mai fallisce, sensore per swap/quota (il kernel non decide mai) |
| `thread_create` | 54 | thread 1:1 nel gruppo del chiamante (S-T, ADR-0046): entry/stack/fs → tid o -1; mm/canali/fd condivisi, kernel stack + TSS propri |
| `thread_exit` | 55 | termina solo il thread corrente (solo la morte del leader chiude il gruppo) |
| `futex_wait` | 56 | dorme se `*addr == expected` fino a WAKE o deadline a tick (0 = mai): 0 svegliato / 1 no / -1 errore |
| `futex_wake` | 57 | sveglia fino a `n` waiter su `addr` → svegliati |
| `thread_set_fs` | 58 | base TLS (FS) del thread corrente (S-T T2, FSGSBASE da ring 3 se presente) |
| `chdir` | 59 | imposta la cwd del gruppo (S1.1, solo assoluti ≤256B; il client unisce i relativi) |
| `getcwd` | 60 | copia cwd + NUL, ritorna len (cap corta = -1, pattern boot_cmdline) |
| `dma_free` | 62 | libera uno slot DMA del chiamante (D4: il buffer device si rialloca al resize del display) e smonta il mapping della finestra dello slot; gli slot ≥ 1 sono dell'owner di `Service::Console` |
| `map_mmio` | 61 | mappa `count` pagine di BAR device (UC, non-owned) nella finestra `DEV_MMIO_BASE`/`USER_MMIO_VA`; solo owner di `Service::Gpu` **e** classe `System` (L0b, ADR-0057; ADR-0079 fase 3) |
| `JOB_CANCEL` | — | tag cancel cooperativo parent→figlio sul canale di nascita (44b, w0=2/SIGINT informativo): il figlio puo' gestirlo o ignorarlo (escalation a kill) |
| `EXIT_SIGINT` | — | causa di morte 130 = 128+SIGINT per Ctrl-C su job non cooperante (44b, convenzione al bordo come `FAULT_EXIT_CODE`) |
| `spawn` | 20 | Crea un processo dal binario embedded `name` e ritorna il **canale di nascita** verso il figlio |
| `spawn_image` | 38 | Come `spawn` ma dal binario in memoria del chiamante (servizi da disco, Fase 21) |
| `map_physical` | 21 | Mappa pagine fisiche nello spazio user (Fase 8.2) |
| `get_ticks` | 22 | Ritorna il contatore PIT corrente (Fase 8.3) |
| `mmap` | 39 | Mappa anonima privata RW nel basso canonico (Fase 28): hint o scelta kernel |
| `mmap_prot` | 39 | Come `mmap` ma con `prot` esplicito NONE/R/RW (Fase 29) |
| `munmap` | 40 | Smappa VMA intere (Fase 28, niente split) |
| `mprotect` | 41 | Cambia le protezioni di VMA intere (Fase 29: RO↔RW, NONE) |
| `shm_create` | 42 | Crea una regione di memoria condivisa (Fase 30) → id |
| `shm_map` | 43 | Mappa una regione condivisa (Fase 30) → base (zero-copy tra processi) |
| `shm_map_cow` | 43 | Mappa una regione in COW (Fase 33, `MAP_COW`, solo `PROT_READ`) → base (copia privata al primo write) |
| `shm_destroy` | 63 | Chiude una regione condivisa (Fase 30.1, owner-only) → `Ok`/`Err` |
| `spawn_copy` | 45 | Figlio-copia in COW (Fase 34) → padre `(pid, chan)`, figlio `(0, chan)` |
| `peer_pid` | 46 | Pid del peer di un canale (Fase 35) → pid o `Err` |
| `peer_info` | 47 | Identita' del peer di un canale (Fase 36, ADR-0078) → `ImageId` (lo/hi) o `Err` |
| `exec_image` | 48 | Exec in-place senza argv (Fase 37.0) → mai ritorno, `Err` a validazione fallita |
| `exec_image_args` | 48 | Come sopra con blocco argv+env grezzo (37.1, env in 43a) |
| `serialize_argv_redir_env` | — | Blocco `[argc][envc][argv][magic?][env NAME=val]` (43a; wrapper `serialize_argv[_redir]` a env vuoto) |
| `env_from_stack` / `Env` | — | Vista envp dallo stack iniziale: `count/get_raw/get(NAME)` (43a) |
| `exec` | 48 | `exec(path, argv)` = load_file + serialize + exec_args (Fase 37.1; il kernel non tocca il FS) |
| `dma_alloc` | 49 | Alloca frame contigui per DMA (Fase 38.1, slot 0) → fisico base |
| `dma_alloc_slot` | 49 | Come sopra ma con lo slot (0 = staging, ANY = primo libero, ≥ 1 = device buffer con grant) → (fisico, VA) |
| `map_mmio` | 61 | Mappa BAR device UC, non-owned, nella finestra MMIO; solo owner di `Service::Gpu` **e** classe `System` (L0b, ADR-0057; ADR-0079 fase 3) |
| `text_stats` | 44 | Contatori shared text (Fase 32): hits/misses/live (+ Fase 33: fault COW in rdx; fix N6.3: fallback per tabella piena in r10) |
| `peer_info` | 47 | Identita' BLAKE2s-128 del peer: 0 + `rdi`/`rsi` = lo/hi (ADR-0078) |

`spawn(name_ptr, name_len)` (Fase 8.1 + 13) crea un nuovo processo a partire dal
binario user embedded il cui nome combacia con `name` (tabella `NAMED_BINARIES`
in `user_binary.rs`). Il kernel crea il **canale di nascita** tra chiamante e
figlio (ADR-0008): il figlio lo eredita come canale 0 (= parent), il chiamante
riceve il channel id come valore di ritorno. Ritorna il channel id, oppure `-1`
se il nome non e' noto, se `name` non e' nello spazio user, o se la creazione
fallisce.

`spawn_image(img, len, meta, metalen)` (Fase 21) e' la primitiva generale di
creazione (come fork+exec): il binario e' letto dalla memoria del chiamante
(servizi da disco: `/bin` e `/test` su `/fat`) invece che dalla tabella
embedded. `meta` e' uno `SpawnMeta` da 64 B (`repr(C)`, identico in `civis`; S2.5e: 56 B + S3.4a: `rights` 8 B):
nome NUL-padded 16 B (non vuoto, stampabile), priorita' 1..31 (mai 0/idle),
fino a 4 range di porte I/O. Le porte sono privilegio root: solo pid 1 (init)
puo' chiederle, gli altri devono avere `io_count == 0`. Bound 256 KiB per
singolo spawn (`SPAWN_IMAGE_MAX`, single source in `syscall-numbers` dalla
Fase 39: anche `civis` lo usa per pre-validare). Ritorna il canale di nascita
o -1. Il kernel embedda ormai solo
lo storage-TCB (init/disk/fs); tutto il resto parte da disco via init.

### Fase 39 — errore nativo in `civis` (nessuna syscall nuova)

La Fase 39 non aggiunge numeri di syscall: l'ABI kernel resta a `-1` nei
registri (tabelle sopra invariate). Cambiano i **wrapper `civis`**, che ora
ritornano `Result<T, libr::posix::Error>`: enum nativo del dominio OS
(trasporto + dominio FS) con UNICA traduzione `to_errno` al bordo POSIX
(i numeri errno non entrano mai nel kernel/wire, ADR-0015/0025). Il registry
servizi passa a 16 slot (`Service::Posix = 8`, discriminant 0-7 stabili).
Vedi [ADR-0030](./adr/0030-posix-fondamenta.md).

### Fase 50 — servizio `Time` (nessuna syscall nuova)

Come la Fase 39, nessun numero nuovo: `service_register`/`lookup`/`send`/
`recv` esistenti + `Service::Time = 9` (bracci in `service_from_disc`/
`service_name`; slot 10-15 restano liberi). Il protocollo `TIME_NOW` vive
nei registri IPC (non nei frame): vedi [IPC](./07-ipc.md).

### Fase 57 — servizio `Log` (nessuna syscall nuova)

Stesso pattern: `send`/`recv` esistenti + `Service::Vestigia = 10` (bracci in
`service_from_disc`/`service_name`; slot 11-15 liberi). Il protocollo
`LOG_*` (0x61-0x66) vive in registri + ring LOG del client (stampo FS):
vedi [IPC](./07-ipc.md) e [ADR-0039](./adr/0039-logging-l1.md). Nota: il
kernel embedda anche `vestigia` (boot-TCB con init/block/cardo: deve partire
prima che il FS esista).

### Rete — servizi e tag IPC (nessuna syscall nuova, N1–N7.0)

Lo stack di rete (ADR-0068/0069) non aggiunge numeri di syscall: driver e
servizi sono processi userspace che usano `service_register`/`service_lookup`/
`send`/`recv` esistenti, e i dati viaggiano su ring SPSC in shm. Il kernel
resta neutro (nessun parser di protocolli). Tag dei servizi:

| Tag | Nome | Servizio | Descrizione |
|-----|------|----------|-------------|
| 0x6E | `NET_INFO` | `net` | stato link/indirizzo |
| 0x6F | `NET_EP_OPEN` | `net` | apre endpoint (`w0` = proto<<16 \| porta) → `(ep_id, ring_id)` |
| 0x73 | `NET_EP_TX` | `net` | invia lo slot TX indicato |
| 0x81 | `NET_EP_CLOSE` | `net` | chiude l'endpoint e libera il ring |
| 0x82 | `NET_EP_STATS` | `net` | statistiche endpoint |
| 0x84 | `SIIT_EP_OPEN` | `siit` | apre endpoint tradotto (N4) |
| 0x85 | `SIIT_EP_TX` | `siit` | invia payload da tradurre |
| 0x86 | `SIIT_EP_CLOSE` | `siit` | chiude l'endpoint |
| 0x87 | `DNS_EP_OPEN` | `dns` | apre endpoint UDP (`w0` = proto<<16 \| porta) → `(ep_id, ring_id)` (N5) |
| 0x88 | `DNS_EP_TX` | `dns` | invia la query DNS nello slot TX (N5) |
| 0x89 | `DNS_EP_CLOSE` | `dns` | chiude l'endpoint e libera il ring (N5) |
| 0x8A | `DNS_EP_STATS` | `dns` | statistiche endpoint DNS (N5) |
| 0x8B | `NET_ADDR` | `net` | indirizzo IPv6 del nodo (`w0` byte 0..8, `w1` 8..16; `w0` = selettore: 0 link-local, 1 global, 2 router) (N6.0) |

**TCP (N6.1)**: `NET_EP_OPEN` con `NET_PROTO_TCP` crea un listener (flag
`LISTEN`) o un endpoint che apre la connessione al primo `NET_EP_TX` (flag
`CONNECT`; la destinazione sta nello slot come per UDP). Per TCP il valore di
ritorno di `NET_EP_TX` significa "preso in carico": `0` = bufferizzato e
ritrasmesso se serve, `!0` = buffer pieno o non connesso. Uno slot TX con
evento `EV_FIN` chiede la chiusura ordinata. Eventi consegnati nel ring RX:

| Evento | Valore | Significato |
|--------|--------|-------------|
| `EV_DATA` | 0 | payload (dati in ordine) |
| `EV_SYNACK` | 2 | connessione stabilita |
| `EV_FIN` | 3 | EOF: il peer ha chiuso |
| `EV_RST` | 4 | reset |
| `EV_ACCEPT` | 5 | nuova connessione su un listener: payload `[ep_id:2][ring_id:4]` del figlio |

`NET_ADDR` esiste perché un client deve poter uscire verso il default router
senza conoscerne l'indirizzo (es. `upstream = auto` in `sys:dns.conf`): due
parole IPC bastano per 16 byte, nessuna nuova syscall. Senza indirizzo la
reply è `::` (`w0 = w1 = 0`).

**N7.0 (ADR-0074)**: il lato client del protocollo (apertura, slot, cursori,
backpressure, stream TCP) vive nel meccanismo (`libs/net-ep` per il formato,
`civis::net` per endpoint e `Stream`). La migrazione non cambia **nessun**
tag, syscall o formato su filo: i servizi restano quelli qui sopra e il
kernel resta fuori dal percorso dati. Chi scrive un client usa
`civis::net::{Endpoint, Stream}` invece di parlare i ring a mano; `siit` e
il lato client di `dns` usano il codec (`net_ep::Layout::IPV4`) ma tengono i
propri tag.

Servizi: `Service::Net = 14`, `NetDev = 15`, `Siit = 16`, `Dns = 17`.
Partono solo con `net=1` in cmdline. Dettagli e traiettoria in
[ADR-0069](./adr/0069-stack-rete-ipv6-capability.md).

**Policy per identità (ADR-0080, fasi 5–6)**: `Service::Custos = 18` e
`Service::NetBroker = 19` (totale `SERVICE_COUNT = 20`). Il server di policy
espone cinque tag (payload chunked in `libs/custos-proto`, nessuna syscall
nuova):

| Tag | Nome | Semantica |
|-----|------|-----------|
| 0x72 | `IDENTIS_LIST` | `w0` = offset → `w0` = `R_OK`, `w1` = 4 byte dell'elenco `nome:ruoli` (pull: niente coda piena, niente hash), `R_DONE` a fine elenco (ADR-0096) |
| 0x73 | `IDENTIS_WHOAMI` | `w1` = pid (`0` = il chiamante; un altro pid solo `System`) → `w0` = codice, `w1` = maschera ruoli **live** |
| 0x80 | `CUSTOS_QUERY` | identità + capability → `w0` = OK, `w1` = `1/0` (solo `sys:caps`) |
| 0x81 | `CUSTOS_GRANT` | identità + capability + decisione → `w0` = codice (il chiamante è attestato dal kernel) |
| 0x82 | `CUSTOS_STATUS` | diagnostica → `w1` = numero di voci in `sys:caps` |
| 0x83 | `CUSTOS_AUTHORIZE` | identità + capability → `w0` = OK, `w1` = `1/0`: **grant admin ∩ consenso** |
| 0x84 | `CUSTOS_CONSENT` | op READ/WRITE + utente + identità (+ decisione); il chiamante è attestato dal kernel (ADR-0094) |

`net` usa `CUSTOS_QUERY` per il bind delle porte note (cache per identità +
porta, fail-closed: errore IPC = negato). `Custos` è l'unico writer di
`sys:caps` e di `usr:consent` / `usr:<nome>:consent`; cardo li legge
direttamente (niente IPC di policy nel loop FS).

**Broker di rete (ADR-0080, fase 6)**: `Service::NetBroker = 19`, API a ring
gemella di quella di `net` (formato degli slot in `libs/net-ep`, client
`civis::broker`):

| Tag | Nome | Semantica |
|-----|------|-----------|
| 0x90 | `BROKER_OPEN` | `w0` = proto<<16 \| porta, `w1` = flag → `w0` = ep_id, `w1` = ring_shm_id \| porta<<32 |
| 0x91 | `BROKER_TX` | `w0` = ep_id, `w1` = slot → `w0` = 0 preso in carico / `!0` = `Full` |
| 0x92 | `BROKER_CLOSE` | `w0` = ep_id → (0,0) |
| 0x93 | `BROKER_STATS` | `w0` = ep_id → `(rx, tx)` |

Il **tetto è strutturale**: `net` (`NET_EP_OPEN`) e `siit` (`SIIT_EP_OPEN`)
negano ogni endpoint alle immagini `Untrusted` (classe misurata dal kernel,
`SYS_PEER_INFO` + trust table), quindi per il codice non firmato il broker è
l'unica strada. `proto = NET_PROTO_IPV4` sceglie l'uscita IPv4 (`siit`).

**Finestre del DE (D8b, ADR-0088)**: il compositor `musivum` vive dentro
`Service::Console`; i clienti aprono finestre con il protocollo `WIN_*`
(client nel meccanismo: `civis::win`). Nessuna syscall nuova.

| Tag | Nome | Semantica |
|-----|------|-----------|
| 0x94 | `WIN_OPEN` | `w0` = tile handle (0 = floating), `w1` = `w<<16\|h` → `w0` = win id, `w1` = shm_id (superficie, mappata RW dal cliente) |
| 0x95 | `WIN_COMMIT` | `w0` = win id, `w1` = rect locale (0 = tutta; D8b danneggia l'intera finestra) → il server blitta nel device buffer e flusha |
| 0x96 | `WIN_SET_RECT` | `w0` = win id, `w1` = rect (solo floating; D8f) |
| 0x97 | `WIN_SET_SURFACE` | `w0` = win id, `w1` = shm_id (solo floating; D8f) |
| 0x98 | `WIN_RAISE` | `w0` = win id (mai fuori dal proprio livello; D8f) |
| 0x99 | `WIN_CLOSE` | `w0` = win id → libera shm e ridisegna il terminale sotto |
| 0x9A | `WIN_STATS` | solo test → `(visibili, totali)` |

Il **layer e il clip decidono dall'handle** (torsione 1, §10.1 del piano DE):
con un handle di tile la finestra nasce in `Layer::TileGuests`, clippata a
quella tile; senza (floating/overlay) servono classe ≥ `ThirdParty` e
consenso — policy che arriva in D8f/D8g. In D8b esiste una sola tile (id `1`,
schermo intero) e i tag 0x96–0x98 rispondono `!0` (non implementati). La
superficie la crea il server (owner dello shm); il cliente la mappa RW, il
server la tiene RO per il blit. `WIN_CLOSE` e `EXIT_NOTIFY` (con `w1` = pid)
fanno `munmap` della mappatura del server e distruggono lo shm: il leak è
impossibile per costruzione.

**Sessioni di terminale (D8c1/D8c2/D8d, ADR-0089/ADR-0090/ADR-0091)**: le sessioni di
`littera` vivono nel servizio `console` (che già possiede i pixel). La
capability è una **mailbox**: una pagina shm (`libs/sess-proto`: OUT
cliente→sessione `0..2048`, IN sessione→cliente `2048..4096`, `MAX_CHUNK`
2048) che il server crea, mappa RW e consegna. Dal D8c2 la mailbox è
**dell'attachment** (una per canale, non per sessione): due clienti della
stessa sessione non si sovrascrivono i byte, come il ring FS è per processo.
Il testo passa **solo** da qui — nessun device di testo: `/dev/console` non
esiste più (e `DEV_CONSOLE`, il tipo 3, è ritirato: il numero non si riusa).
Client nel meccanismo: `civis::sess` (`out`/`out_screen` sono il sink di
stdout: sessione attestata → default → seriale).

| Tag | Nome | Semantica |
|-----|------|-----------|
| 0x9B | `SESS_CREATE` | `w0` = flag (0 = legata al chiamante, 1 = non legata, ≥ `ThirdParty`) → `w0` = sess, `w1` = shm della mailbox |
| 0x9C | `SESS_WHOAMI` | nessun argomento → la sessione **del pid chiamante** (owner o binding): l'attestazione la fa il kernel |
| 0x9D | `SESS_ATTACH_DEFAULT` | → `(0, shm)`: l'unico ingresso **nominato** (la sessione 0, lo schermo); la mailbox è creata per il canale che chiede |
| 0x9E | `SESS_BIND` | `w0` = sess, `w1` = pid: il DE lega un figlio appena nato (≥ `ThirdParty`) |
| 0x9F | `SESS_WRITE` | `w0` = len (byte in OUT), `w1` = `0` entra nel mirror seriale / `1` solo schermo → feed della sessione attaccata |
| 0xA0 | `SESS_READ` | `w0` = max → copia fino a `max` byte di input in IN; **dichiara il canale come lettore** (è lui che riceve l'hint) |
| 0xA1 | `SESS_RESIZE` | `w0` = `cols<<16\|rows` → reflow; in D8c1 la sessione in focus (lo schermo) è negata |
| 0xA2 | `SESS_ECHO` | `w0` = on: eco dell'input sul modello (flag di **sessione**) |
| 0xA3 | `SESS_FOCUS` | `w0` = sess: la sessione **disegnata** (in D8c1 solo la 0) |
| 0xA4 | `SESS_CLOSE` | `w0` = sess (mai la 0) → `munmap` della mailbox e `shm_destroy` |
| 0xA5 | `SESS_INFO` | `w0` = sess → `w0` = `cols<<16\|rows`, `w1` = eco \| focus<<1 \| **resized<<2** (la geometria è cambiata dall'ultima domanda: il flag si consuma qui — ADR-0098) |
| 0xA6 | `SESS_STATE` | `w0` = sess, `w1` = limite → `(celle non vuote, CRC del testo)` |
| 0xA7 | `SESS_DUMP` | `w0` = sess, `w1` = max → coda del testo visibile nella meta' IN |
| 0xA8 | `SESS_STATS` | → `w0` = sessioni vive, `w1` = focus (bit 0..8) \| **byte delle toppe** (bit 8.., ADR-0099) |
| 0xAB | `SESS_ATTACH` | `w0` = sessione → `w1` = shm: mailbox su una sessione **nominata** (≥ `ThirdParty`, D8e). Non dichiara il lettore: serve a un **iniettore** (il DE) |
| 0xAC | `SESS_INPUT` | `w0` = len (byte in OUT) → accoda nel **buffer di input** della sessione attaccata (tasti, ≠ `SESS_WRITE` che è testo) e manda l'hint al lettore |
| 0xA9 | `SESS_TEST_INPUT` | `w0` = len (byte in OUT) — **solo test**: inietta input come la tastiera (stesso percorso + hint) |
| 0xB8 | `SESS_METRICS` | → `w0` = larghezza di una cella in px, `w1` = altezza (la sa solo il font: serve a dimensionare una toppa) |
| 0xB9 | `SESS_PATCH_BEGIN` | `w0 = x \| y<<16`, `w1 = cols \| rows<<16 \| alt_len<<32` (alt nella metà OUT) → `0`/`!0`: apre lo staging di una **toppa** di pixel, una per sessione (ADR-0099) |
| 0xBA | `SESS_PATCH_DATA` | `w0` = offset, `w1` = len (byte nella metà OUT) → `0`/`!0`: accumula il bitmap BGRA; **commit implicito** a misura completa, offset fuori ordine = scartato |
| 0xAA | `SESS_INPUT_EVENT` | console → **lettore dichiarato**, fire-and-forget: `w0` = byte in coda, `w1 = 1` se la geometria è cambiata. Hint coalescente (E2): la coda è la verità; il flag di resize resta finché `SESS_INFO` non lo consuma |
| 0xAF | `SESS_AUTH_SET` | `w0` = len del nome (byte in OUT) → scrive l'**utente autenticato** sulla sessione **attaccata** al canale (implicita, come `SESS_RESIZE`); `len = 0` = deautentica; solo `System` (S3.3c, ADR-0094) |
| 0xB7 | `SESS_AUTH` | `w0` = pid (`0` = la sessione attaccata) → `w0` = 1/0 (autenticata), `w1` = len del nome nella meta' IN; solo `System`. Il kernel dà il pid, il server dice sessione e utente (S3.3c, ADR-0094) |

Nessuna syscall nuova e nessun `Service` nuovo (`SERVICE_COUNT` resta 20). La
sessione di un processo si trova per **attestazione del pid**, mai con un
handle passato allo spawn o un'ambiente da fidarsi; `EXIT_NOTIFY` purga
binding, sessioni di cui il morto era owner (mai la 0) **e le mailbox dei suoi
attachment**. L'**identità di sessione** sta nel server (ADR-0094): il nome
dell'utente autenticato lo scrive chi ne ha titolo (`SESS_AUTH_SET`, solo
`System`: il login sulla 0, il DE sulle tile nuove) e lo risolve chi deve
attribuire una richiesta (`SESS_AUTH`, solo `System`, per pid). **Nessun token
e nessun segreto viaggia**: gli identificatori `0xAD`/`0xAE` (`SESS_TOKEN_SET`/
`SESS_TOKEN`) sono **ritirati** e i numeri non si riusano. **Nessuna variabile
d'ambiente** (S3.3d, ADR-0095): il prompt e i comandi chiedono l'identità al
server, nessun nome viaggia in `env`. Il **mirror seriale** della sessione in focus è del server
(formato nudo, `MIRROR_SESSION_MAX` 1 MiB per sessione come rete di sicurezza;
il budget per comando resta del client che sa cos'è un comando).

**Tile del DE (D8e, ADR-0092)**: la console disegna le sessioni in
**rettangoli** (tile); il layout lo decide il DE. Nel server una tile **non ha
stato proprio**: è `rect + sessione` (+ una finestra `Backing::Direct` in
`Layer::Tiles`), e l'handle è **`sid + 1`** — la torsione "una tile è una
sessione" resa letterale. `TILE_SET` scrive in **staging**, `TILE_COMMIT`
applica tutto in un render + flush solo (niente stati intermedi a schermo). Il
clip per tile non è una politica: è una **vista** della superficie
(`Pixel::view`), quindi nessun glifo e nessun ospite possono scrivere fuori dal
loro rettangolo.

| Tag | Nome | Semantica |
|-----|------|-----------|
| 0xB0 | `TILE_SET` | `w0` = sessione, `w1` = `(x,y,w,h)` 4×u16 (`pack_tile_rect`); `w = 0` = tile **nascosta** (sessione viva, non disegnata: è lo zoom). Solo ≥ `ThirdParty` (il layout è un privilegio) |
| 0xB1 | `TILE_COMMIT` | applica lo staging: ridimensiona le sessioni (la griglia è quella del rect), danno, un render + flush |
| 0xB2 | `TILE_INFO` | → `(tile visibili, handle in focus)` |
| 0xB3 | `TILE_DROP` | `w0` = sessione: toglie la tile (il rect torna al fondo); la sessione la chiude chi la possiede |
| 0xB4 | `TILE_CRC` | `w0` = sessione → `(byte, crc)` del **rect** della tile: la prova "solo questa è cambiata" |
| 0xB5 | `TILE_SCREEN` | console → DE, fire-and-forget: il display si è riadattato, rilancia il layout (con **una** tile il server si adatta da solo e il tag non parte) |
| 0xB6 | `TILE_SIZE` | → `(w<<16|h)` del display **in pixel**: l'unità del layout (la cella del font non è pubblica) |

`SESS_FOCUS` accetta qualunque sessione **che ha una tile** (il cursore del
terminale è solo della tile in focus); `SESS_RESIZE` resta com'è — la misura
dentro una tile la decide il layout.

Dal D8e la **tastiera è del DE**: `impluvium` è il focus owner (si sottoscrive a
`porta` con `INPUT_ATTACH`, consuma i tasti `Super+<tasto>` — prefissati da
`porta` con `INPUT_SUPER` 0x1C, perché `pc-keyboard 0.7` non traccia i
modificatori GUI — e inietta il resto con `SESS_INPUT`). `WIN_OPEN` con
`w0 = !0` crea la finestra di **chrome** del DE in `Layer::Overlay` (clip allo
schermo, solo ≥ `ThirdParty`): è il primo mattone della policy di D8g.

**Input (D8d, ADR-0091)**: i tasti **non sono un device**. `porta` (il servizio
tastiera) li serve a chi ha la **capability** (`INPUT_ATTACH`, ≥ `ThirdParty`)
con `INPUT_READ`, e il focus owner (la console; il DE in D8e) li inietta nella
coda della sessione in focus. Il lettore li legge con `SESS_READ` e si sveglia
sull'hint `SESS_INPUT_EVENT`. `/dev/input/keyboard` è **ritirato** (e
`DEV_KEYBOARD`, il tipo 2, con lui: il numero non si riusa); resta `/dev/kbd`
(scancode raw), che è l'ingresso del driver e lo legge solo `porta`.
`civis::input` è il client (`attach`/`read`/`wait`/`wait_until`/`detach`);
`civis::input::device` è il lato del focus owner (`attach` + drain).

### I/O

| Syscall | Numero | Descrizione |
|---------|--------|-------------|
| `write` | 2 | Scrive su seriale (`CONSOLE_OUT`/`CONSOLE_ERR`); altre maniglie → `-1` |

> Input e output del terminale **non sono syscall né device**: il testo è una
> sessione (`SESS_*`), i tasti una capability (`INPUT_*`). `write` resta solo il
> sink seriale di debug.

> Le classiche `read`/`open`/`close`/`readdir` non sono syscall kernel:
> sono wrapper IPC diretti in `civis` (client → cardo, v. [File System](./09-filesystem.md)).
> `spawn_copy` (45, Fase 34) e `mmap` (39, Fase 28) esistono; `sbrk` e' la 25.
> `exec` in-place (48, Fase 37.0 nucleo + 37.1 argv: `exec_image`/`exec`;
> convenzione argv stile Linux come dato neutro, `_start` via macro `entry!`)
> esiste; `wait` esplicito arriva con la shell 37.2 (`EXIT_NOTIFY` gia'
> notifica il parent). `brk` non esiste come syscall (l'heap cresce via
> `sbrk`).

### File System — ritirate (Fase 9.6)

Le syscall 3-7 (`open`, `read`, `write_fs`, `close`, `readdir`) e 23-24
(`mkdir`, `fs_register`) sono state **ritirate** con la Fase 9.6. In precedenza
il kernel instradava le operazioni FS verso il server tramite IPC, trasferendo
dati in una shared buffer page unica. Questo design causava race condition tra
client concorrenti.

Dal Fase 9.6 le operazioni FS sono **IPC dirette client → cardo**: ogni
processo alloca la propria pagina di trasferimento (`fs_buf_alloc`, syscall 26)
e la registra presso cardo (`FS_BUF_REG`, tag 0x31). Il kernel non e' piu'
nel percorso dati. Vedi [File System](./09-filesystem.md) per i dettagli.

> Le vecchie numerazioni (3-7, 23-24) sono riservate e non piu' usate.

### Esempio: write()

```rust
fn sys_write(fd: u64, buf: *const u8, count: usize) -> i64 {
    if fd != 1 && fd != 2 {
        return -1;                    // nessun file implementato
    }
    if count == 0 {
        return 0;
    }
    // Validazione: il buffer deve stare nel range user mappato (U=1).
    if !crate::arc::vmm_user::is_user_range(buf as u64, count) {
        crate::serial_println!("[syscall] write: puntatore fuori dallo spazio user");
        return -1;
    }
    let slice = unsafe { core::slice::from_raw_parts(buf, count) };
    let s = alloc::string::String::from_utf8_lossy(slice);
    crate::serial_println!("{}", s);
    count as i64
}
```

## Setup SYSCALL/SYSRET (implementazione 6.3)

Configurazione **effettiva** in `kernel/src/syscall.rs::init()` (modulo `x86_64`, la
`Star::write` applica le validazioni dei selettori):

```rust
let sel = crate::gdt::selectors();

// STAR:
//  - syscall (ring 3→0): CS = kernel code, SS = CS+8 = kernel data.
//  - sysret (ring 0→3): CS = |user_code, SS = |user_data; la CPU impone
//    SS = CS−8, quindi il selettore user_data deve stare SOTTO user_code.
let cs_sysret = SegmentSelector(sel.user_code.0 | 0x3);
let ss_sysret = SegmentSelector(sel.user_data.0 | 0x3);
Star::write(cs_sysret, ss_sysret, sel.code, sel.data)
    .expect("STAR: segmenti syscall non coerenti");

// SFMASK: maschera IF (e TF/DF/altri) durante la syscall → niente interrupt
// nel tratto critico di salvataggio/ripristino dello stack.
SFMask::write(RFlags::from_bits_truncate(0x3F7));

// EFER.SCE: abilita le istruzioni syscall/sysret.
Efer::write(Efer::read() | EferFlags::SYSTEM_CALL_EXTENSIONS);

// LSTAR → entry assembly.
LStar::write(VirtAddr::new(syscall_entry as usize as u64));

// KernelGsBase → area per-core. VESTIGIALE: l'entry non usa `swapgs`/`GS`
// (accede a PERCPU rip-relative, vedi sotto), ma il MSR resta impostato.
KernelGsBase::write(VirtAddr::new(addr_of!(PERCPU) as u64));
```

**Punto critico sull'ordine della GDT.** Su `syscall` la CPU carica `SS = CS+8` (entry
data subito dopo quella code); su `sysret` carica `SS = CS−8` (entry data subito *prima*
di quella code). Le due richieste sono contraddittorie per una singola coppia di selettori
kernel, quindi si usano **due coppie diverse**:

| Istruzione | CS | SS |
|------------|-----|-----|
| `syscall` (→ ring 0) | kernel code | kernel data (8 sopra code) |
| `sysret` (→ ring 3) | `user_code \| 3` | `user_data \| 3` (8 sotto code) |

La `gdt::init` quindi appende le entry **user_data prima di user_code** (e le entry kernel
code/data restano in testa, `data = code+8`). Solo cosi' STAR soddisfa la validazione del
crate e il comportamento hardware.

### Entry assembly (naked, `syscall_entry`)

CPU su `syscall`: salva `RIP→RCX`, `RFLAGS→R11`, carica `CS/SS` da STAR, salta a LSTAR,
**non** cambia RSP. L'entry deve quindi salvare lo RSP user, puntare allo stack kernel
per-processo, preservare **tutti** i registri general purpose (l'ABI si aspetta che una
syscall modifichi solo `RAX`/`RCX`/`R11`), dispatch, ripristinare i registri e RSP user,
`sysretq`:

> **Niente `swapgs`.** Su un kernel single-core, `PERCPU` e' uno `static` raggiungibile
> `rip`-relative: non serve `GS.base`. (In passato l'entry usava `swapgs` per indirizzare
> `PERCPU` via `GS`; una syscall che blocca — `recv`/`send` — lasciava lo stato GS
> "scambiato" attraverso il context switch, e la syscall successiva di un altro processo
> invertiva lo scambio → `GS.base=0` nel handler → scritture `gs:` nel vuoto → `user_rsp`
> stale → `sysret` su stack sbagliato → salto a `rip=0`. Soluzione: accesso `rip`-relative,
> si veda [`07-ipc.md`](./07-ipc.md).)

```rust
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() -> ! {
    core::arch::naked_asm!(
        // PERCPU raggiunto rip-relative (niente swapgs): r12 = base (callee-saved).
        "mov qword ptr [rip + {p}+0x18], rsp", // PerCpu.user_rsp (transitorio)
        "mov qword ptr [rip + {p}+0x70], r12", // PerCpu.user_r12_save (transitorio)
        "lea r12, [rip + {p}]",
        "mov [r12 + 0x20], rax", // number
        "mov [r12 + 0x28], rdi", // arg1
        "mov [r12 + 0x30], rsi", // arg2
        "mov [r12 + 0x38], rdx", // arg3
        "mov [r12 + 0x40], r10", // arg4
        "mov rsp, [r12 + 0x08]", // stack kernel per-processo (rsp0)
        // user_rsp e user_r12 sullo stack kernel (restano validi attraverso gli
        // switch: PERCPU e' condiviso e verrebbe sovrascritto da altri processi).
        "push qword ptr [r12 + 0x18]",
        "push qword ptr [r12 + 0x70]",
        // Preserva TUTTI i GPR (vedi nota ABI sotto).
        "push r8", "push r9", "push r10",
        "push rdi", "push rsi", "push rdx",
        "push rcx", "push r11",
        "call {handler}",          // risultato in rax
        "pop r11", "pop rcx", "pop rdx", "pop rsi",
        "pop rdi", "pop r10", "pop r9", "pop r8",
        // IPC multi-register: se ipc_override != 0, svuota rdi/rsi/rdx/r10
        // con i valori di ritorno ret_*.
        "cmp qword ptr [r12 + 0x48], 0",
        "je 2f",
        "mov rdi, [r12 + 0x50]",
        "mov rsi, [r12 + 0x58]",
        "mov rdx, [r12 + 0x60]",
        "mov r10, [r12 + 0x68]",
        "2:",
        "pop r12",      // ripristina user_r12
        "pop rsp",      // ripristina user_rsp (dallo stack kernel)
        "sysretq",      // RIP=RCX, RFLAGS=R11
        p = sym PERCPU,
        handler = sym syscall_handler,
    );
}
```

**Nota ABI.** L'utente si aspetta che una syscall preservi TUTTI i registri tranne
`RAX` (ritorno) e `RCX`/`R11` (sovrascritti da `syscall`/`sysretq`). Il handler e' una
funzione C che clobbera i caller-saved (`r8`-`r11`, argomenti). Senza salvarli, un
processo che tiene un valore in `r8`/`r9`/`r10` attraverso una syscall (es. il pid in
`r8` dopo `getpid`) leggerebbe spazzatura al ritorno. L'entry quindi salva/ripristina
tutti i GPR sul kernel stack.

### Handler per-core e syscall implementate

`PerCpu` (`repr(C)`) tiene lo stato del syscall corrente. Offset (bloccati dall'entry):

| Offset | Campo | Uso |
|--------|-------|-----|
| 0x00 | `current_id` | id del processo in esecuzione |
| 0x08 | `rsp0` | top dello stack kernel del processo (per RSP0) |
| 0x10 | `current_cr3` | CR3 del processo corrente |
| 0x18 | `user_rsp` | RSP user (transitorio, copiato sullo stack kernel) |
| 0x20 | `number` | numero syscall |
| 0x28..0x40 | `arg1..arg4` | argomenti |
| 0x48 | `ipc_override` | ≠0 → a sysret rdi/rsi/rdx/r10 = valori di ritorno IPC |
| 0x50..0x68 | `ret_rdi/rsi/rdx/r10` | valori di ritorno multi-register (IPC) |
| 0x70 | `user_r12_save` | staging transitorio dell'`r12` user nell'entry |

`set_current(id, rsp0, cr3)` viene chiamato dal context switch. Il dispatch (`number`)
implementa: `0=exit`, `2=write`, `8=getpid`, `16=send`, `17=recv`, `18=reply`,
`20=spawn`, `21=map_physical`, `22=get_ticks`, `25=sbrk`, `26=ring_alloc`,
`27=map_in`, `28=cbs_create`, `29=cbs_attach`, `30=cbs_get_info`,
`31=service_register`, `32=service_lookup`, `33=send_async`, `34=recv_nonblock`,
`35=kill`, `36=service_pid`, `37=ps_info`, `38=spawn_image` (Fase 21),
`39=mmap`, `40=munmap`, `41=mprotect`, `42=shm_create`, `43=shm_map`,
`44=text_stats`, `45=spawn_copy`, `46=peer_pid`, `47=peer_info`, `48=exec`,
`49=dma_alloc` (arg2 = slot, D3/ADR-0060), `50=suspend`, `51=resume` (Fase 44a, job control),
`52=meminfo` (Fase 52, P3), `54=thread_create`, `55=thread_exit`,
`56=futex_wait`, `57=futex_wake`, `58=thread_set_fs` (S-T, ADR-0046),
`59=chdir`, `60=getcwd` (S1.1, cwd nel PCB), `61=map_mmio` (L0b,
ADR-0057: grant UC delle BAR all'owner di `Gpu`), `62=dma_free` (D4,
ADR-0061: libera uno slot device), `63=shm_destroy` (Fase 30.1: chiude una
regione condivisa, owner-only).

Le vecchie syscall 3-7/23-24 (FS relay) sono state rimosse con la Fase 9.6:
le operazioni FS sono ora IPC dirette client→cardo.

## User Space Interface

### Libreria C minimale

La libreria è `libs/civis/src/lib.rs` (`civis`, condivisa userland+testland). ABI: `rax`=numero, `rdi/rsi/rdx/r10`=arg1-4,
ritorno in `rax`. Espone `syscall4` (ritorno in `rax`) e `syscall4_out` (cattura anche
`rdi/rsi/rdx/r10` di ritorno, per l'IPC multi-parola).

```rust
pub unsafe fn syscall4(number: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") number => ret,
        in("rdi") arg1,
        in("rsi") arg2,
        in("rdx") arg3,
        in("r10") arg4,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}
```

> `syscall`/`sysret` salvano `RCX` (RIP) e `R11` (RFLAGS) senza toccarli, quindi lo
> shim li dichiara `lateout` per non affermare di conservarli. Dettagli su
> `send`/`recv`/`reply` in [`07-ipc.md`](./07-ipc.md).

### Output seriale con line buffer (macros `print_str!` / `println!`)

L'output utente passa da un **line buffer** in userspace: i bytes vengono accumulati
in un buffer statico e flushati su console (`CONSOLE_OUT`) tramite una singola syscall quando
si incontra un `\n` oppure il buffer e' pieno. Questo evita la frammentazione
dell'output seriale (una syscall per ogni carattere o per ogni frammento).

**Meccanismo** (`civis/src/lib.rs`):

```
LINE_BUF: [u8; 1024]    ← buffer statico
LINE_LEN: AtomicUsize    ← lunghezza attuale

print_str(s):       push stringa nel buffer
print_fmt(args):    core::fmt::Write → push bytes nel buffer
push_byte(b):       flush() se '\n' o buffer pieno, altrimenti scrivi in LINE_BUF
flush():            singola syscall write(1, buf, len) + reset LINE_LEN
```

**Macros** (`#[macro_export]`, usabili con `use civis::{print_str, println}`):

```rust
// Bufferizza senza newline e senza flush automatico:
print_str!("[test] value={}", val);

// Bufferizza + aggiunge '\n' + flush immediato:
println!("[test] read {} bytes: {}", count, data);
```

> Ogni `println!` flusha il buffer alla fine: se il kernel legge stdout in modalita'
> riga, l'output seriale risulta compatto e leggibile, senza frammentazione.

## Riferimenti

- [Writing an OS in Rust - Testing](https://os.phil-opp.com/testing/)
- [OSDev Wiki - System Calls](https://wiki.osdev.org/System_Calls)
- [Intel SDM - SYSCALL](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html)
