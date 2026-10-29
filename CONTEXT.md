# Velordo — Contesto Architetturale

> **Scopo**: questo file è il "punto di ingresso" per ogni agente AI. Contiene
> la mappa del progetto, le decisioni architetturali critiche e i vincoli che
> guidano ogni modifica. I dettagli implementativi vivono nei sorgenti; qui
> c'è solo ciò che serve per orientarsi e prendere decisioni corrette.
>
> **Aggiornamento**: ogni fase che modifica un'area deve aggiornare la sezione
> corrispondente qui. Fonte di verità: `AGENTS.md` + `CONTEXT.md` insieme.

---

## 1. Cos'è Velordo

Microkernel x86_64 sperimentale scritto in Rust (nightly, `no_std`). Architettura
a servizi userspace isolati che comunicano via IPC per nome con reply implicita,
supporto async e zero-copy su ring SPSC. Scheduler RT a 32 priorità con CBS
(Constant Bandwidth Server) per bandwidth reservation.

**Obiettivo**: un microkernel capace di ricompilare se stesso (self-hosting).

---

## 2. Struttura dei crate

```
velordo/
├── kernel/                  # Kernel x86_64 bare-metal (no_std, no_main)
│   └── src/
│       ├── main.rs          # rust_main — entry point dopo boot.asm
│       ├── ordo/            # Scheduler + lifecycle processo + hardening
│       │   ├── sched/       # CPU scheduling, IPC, spawn, futex, tick
│       │   ├── aegis.rs     # Hardening: policy spawn, signature binari
│       │   ├── process.rs   # PCB, stato, risorse
│       │   └── context.rs   # Task context switch
│       ├── syscall/         # Dispatch ABI → handler (entry.rs)
│       │   ├── entry.rs     # syscall instruction handler (syscall/sysret)
│       │   ├── dispatch.rs  # rax→handler mapping
│       │   ├── ipc.rs       # send/recv/reply/wait_reply
│       │   ├── spawn.rs     # SYS_SPAWN
│       │   ├── exec.rs      # SYS_EXEC
│       │   ├── mem.rs       # sbrk, map_physical, map_in, unmap
│       │   ├── service.rs   # service_register/lookup/deregister
│       │   ├── thread.rs    # create/wait/exit
│       │   └── cwd.rs       # get/set working directory
│       ├── arc/              # Address space + VMM (page tables, heap kernel)
│       │   ├── vmm.rs       # Page table management
│       │   ├── vmm_user/    # User-space address space ops
│       │   │   ├── paging.rs
│       │   │   ├── vma.rs   # Virtual memory areas
│       │   │   ├── heap_brk.rs  # sbrk lazy allocation (page fault)
│       │   │   ├── layout.rs    # Stack/heap/user space layout
│       │   │   ├── rings.rs     # SPSC ring allocation
│       │   │   ├── shm.rs       # Shared memory
│       │   │   ├── dma.rs       # DMA alloc/free
│       │   │   ├── spawn_copy.rs
│       │   │   ├── args.rs      # Argument passing
│       │   │   └── teardown.rs  # Address space teardown (owned PTE)
│       │   ├── heap.rs      # Kernel heap (frame-based free-list)
│       │   └── phys_mem.rs  # Physical memory bitmap + reserve
│       ├── relay/           # IPC channels
│       │   └── channels.rs  # Channel struct, send/recv queues
│       ├── boot.asm         # PM32 → long mode stub (NASM)
│       ├── elf.rs           # ELF parser for user binaries
│       ├── user_binary.rs   # Known binary manifests + hashes
│       ├── interrupts.rs    # IRQ handlers table
│       ├── lapic/pit/pic    # Timer/interrupt controllers
│       └── vga/serial       # Debug facilities (temporanei → userspace)
│
├── libs/                    # Librerie condivise
│   ├── civis/               # MECCANISMO di sistema (ADR-0041)
│   │   └── src/
│   │       ├── lib.rs       # Facade: sys/ipc/heap/fs/spawn/print/args
│   │       ├── ipc.rs       # send/recv/reply/wait_reply wrappers
│   │       ├── spawn.rs     # spawn/exec helpers
│   │       ├── fs/          # File system operations (ring-based)
│   │       │   ├── ring.rs      # Ring frame format & ops
│   │       │   ├── sync.rs      # Synchronous FS ops
│   │       │   ├── ops_async.rs # Async FS ops
│   │       │   ├── obj.rs       # File/object handles
│   │       │   ├── session.rs   # Session management
│   │       │   └── cwd.rs       # Working directory per-process
│   │       ├── heap.rs      # Single allocator (free-list first-fit + split)
│   │       ├── print.rs     # stdio-like output → persona hook
│   │       ├── persona.rs   # Hook stdout routing (POSIX personality)
│   │       ├── args.rs      # Argument parsing
│   │       ├── error.rs     # Error types
│   │       ├── sys.rs       # System constants (pid_max, etc.)
│   │       ├── task.rs      # Task utilities
│   │       ├── time.rs      # get_ticks abstraction
│   │       ├── tsc.rs       # Timestamp counter
│   │       ├── scratch.rs   # Scratch page cross-process
│   │       ├── pio.rs       # PIO helpers
│   │       ├── pci.rs       # PCI bus utilities
│   │       ├── vestigia.rs  # Legacy hardware abstraction
│   │       └── test.rs      # Test harness utilities
│   ├── term/                # Terminale (D3, ADR-0060)
│   │   └── src/
│   │       ├── lib.rs       # pub mod disp + term modules
│   │       ├── disp.rs      # Display protocol (/dev/display) — driver side
│   │       ├── grid.rs      # Cell model, grid management, resize
│   │       ├── raster.rs    # Font rasterization (fixed-point 16.16)
│   │       ├── surface.rs   # Pixel/VGA text surfaces + layers
│   │       ├── font.rs      # Font loading & metrics
│   │       ├── ttf.rs       # TrueType parser (subset)
│   │       └── utf8.rs      # UTF-8 encoding/decoding
│   ├── valloc/              # Segregated-fit allocator (S2.1)
│   │   └── src/
│   │       ├── lib.rs       # Public API
│   │       └── core.rs      # Core allocator implementation
│   ├── vela/                # Driver code sharing (Fase R2, ADR-0040)
│   │   └── src/
│   │       ├── lib.rs       # Re-exports
│   │       ├── hub.rs       # Hub device abstraction (/dev)
│   │       ├── block.rs     # Block storage abstractions
│   │       ├── input.rs     # Input device abstractions (keyboard)
│   │       └── gpu.rs       # GPU device abstractions
│   ├── identis-proto/       # Protocollo identificazione utente
│   │   └── src/lib.rs       # Framing chunked (ACLI/ACUS magic, R_* codes)
│   └── vfscli/              # Client-side VFS operations wrapper
│       └── src/
│           ├── lib.rs
│           ├── ring.rs      # Ring helpers for client side
│           ├── sync.rs      # Sync FS ops via rings
│           ├── session.rs   # Session management
│           ├── abi.rs       # ABI constants
│           ├── base.rs      # Base path operations
│           ├── cwd.rs       # Working directory
│           └── error.rs     # Error types
│
├── flavours/posix/          # PERSONALITÀ POSIX (ADR-0041) — separata dal meccanismo
│   ├── libr/                # crate `libr`: errno, stdio, fork, exec + entry! macro
│   ├── server/              # posix-server (userposix.bin) — Service::Posix = 12
│   ├── shell/               # Shell POSIX interattiva (usershell.bin)
│   ├── cli/                 # Programmi POSIX lanciabili (runhello)
│   ├── build/               # Output .bin generato (gitignored)
│   └── tests/               # Suite test POSIX → flavours/posix/tests/build
│
├── userland/                # Servizi NATIVI (non-POSIX)
│   ├── cardo/               # File system server: ramfs + ArcaFS + FAT32 + remotes
│   │   └── src/
│   │       ├── main.rs      # Entry point, service registration
│   │       ├── server.rs    # Main dispatch loop (recv → handler)
│   │       ├── handlers.rs  # Request handlers (open/read/write/etc.)
│   │       ├── mount.rs     # Mount management
│   │       ├── ramfs.rs     # In-memory filesystem
│   │       ├── ftable.rs    # File table per-process
│   │       ├── rights.rs    # Permission model
│   │       ├── rings.rs     # Ring registration/handling
│   │       ├── volume.rs    # Volume abstraction
│   │       └── fat32/       # FAT32 driver (mount/read/write/dir)
│   ├── console/             # Console server (D3, ADR-0060) — /dev/console
│   ├── gpu/                 # GPU driver (virtio-gpu, BAR/MSI-X/2D/scanout) — /dev/display
│   ├── kbd/                 # Keyboard driver — registers via name
│   ├── block/               # Block storage driver — DISK_READ/WRITE via IPC
│   ├── time/                # Time service (CMOS RTC + uptime counter)
│   ├── vela/                # Hardware abstraction server
│   ├── vestigia/            # Legacy hardware server
│   ├── porta/               # I/O port management
│   ├── uptime/              # Uptime reporting (Low priority process)
│   ├── identis/             # User identity service — Service::Identis = 11
│   ├── identis-login/       # Login gate (3 attempts, shell only on success)
│   ├── create-user/         # User creation tool
│   ├── shell/               # Native shell (not POSIX)
│   ├── rector/              # Binary verification/rebuild tool
│   └── arca/                # ArcaFS tool: `arca create`
│
├── testland/                # Test suite + demos
│   ├── testfs/              # FS smoke tests
│   ├── testsarca/           # ArcaFS protocol/format tests
│   ├── threadtest/          # Threading/concurrency tests
│   ├── usertests/           # Main regression suite (59/59 PASS)
│   │   └── src/t_*.rs       # Modular test files per area
│   ├── identistest/         # User identity tests (11/11 PASS)
│   ├── devreader/           # Device reader demo
│   ├── hogheap/             # Heap stress test
│   └── usertest-client/     # Client helper for usertests
│
├── arcafs/                  # ArcaFS shared definitions (P5, ADR-0049)
│   └── src/
│       ├── lib.rs           # Re-exports: proto, wire, format, btree
│       ├── proto.rs         # Protocol tags & constants
│       ├── wire.rs          # Frame builder/parser (no I/O)
│       ├── format.rs        # On-disk block format
│       └── btree.rs         # B-tree index implementation
│
├── blake2s/                 # BLAKE2s-256 proprio (RFC 7693, no_std) — P5
├── syscall-numbers/         # Costanti syscall + costanti condivise kernel+user
├── pal/velordo/             # PAL std Velordo (S1.3, ADR-0047): adattamento POSIX→Velordo
├── tools/                   # Strumenti di sviluppo
│   ├── arca/              # Host tool: `arca create`
│   └── rustc_codegen_cranelift-velordo/  # Custom codegen (Rust → Cranelift → x86_64)
│
├── scripts/                 # Build & test scripts
│   ├── build-userland.sh    # Services → userland/build
│   ├── build-posix.sh       # POSIX binaries → flavours/posix/build
│   ├── build-tests.sh       # Test suite → testland/build
│   ├── build_common.sh      # Shared build helpers (freestanding PIC)
│   └── run-fat-tests.sh     # FAT32 regression (fuori dal gate principale)
├── run.sh                   # Build + QEMU boot (PVH) — script principale
├── ROADMAP.md               # Stato fasi + Pianificate + Parcheggiate
└── AGENTS.md                # Istruzioni operative per agenti AI
```

---

## 3. ABI Syscall (rapido riferimento)

| Nr | Nome | Descrizione | Fase |
|----|------|-------------|------|
| 0 | `exit` | Termina processo | F6 |
| 2 | `write` | Scrittura su fd | F6 |
| 8 | `getpid` | PID del chiamante | F6 |
| 25 | `sbrk` | Heap grow/shrink (lazy page fault) | F10 |
| 26 | `sys_ring_alloc` | Alloca 2 pagine ring SPSC per-processo | F10.2 |
| 27 | `map_in` | Mapper generico cross-process frame | F9.3 |
| 33 | `send_async` | Send asincrono (ritorna req_id) | F13 |
| 34 | `recv_nonblock` | Receive non-bloccante | F13 |
| 35 | `kill` | Termina processo remoto | F14 |
| 46 | `SYS_PEER_PID` | PID del peer del canale corrente | F14.10 |
| 47 | `SYS_PEER_INFO` | Info del peer (name, etc.) | F14.10 |
| 62 | `SYS_DMA_FREE` | Libera DMA slot/buffer | D4 |

**Calling convention**: `rax`=syscall_num, `rdi`/`rsi`/`rdx`/`r10`=arg1-4, ritorno in `rax`.

---

## 4. IPC — Il cuore del sistema

### Meccanismo
- **Canali bidirezionali**: creati da `spawn` (figlio→padre = canale 0) o da
  `service_register` (servizio registrato per nome).
- **Reply implicita**: il kernel alla reply guarda il peer del messaggio corrente.
  Fissato da `recv` in `reply_chan`. Niente request-id esplicito nell'ABI.
- **Async**: `send_async` → `req_id` interno a `PendingMsg`. Risposta async
  accodata con `req_id = -reply_req` se il target non è in `BlockedOnReply`.
- **Ring SPSC per-processo**: ogni processo ha 2 pagine ring (4087 B capacity
  reale). Request ring: `[tag:4][w0:8][w1:8][payload]`. Head a 0xFF8, tail a 0xFFC.

### Registry + routing
- I servizi si registrano per nome (`service_register`). Il kernel mappa il nome
  al channel_id del servizio.
- `civis` risolve `Fs`/`Console`/`Devfs` per nome tramite il registry.

---

## 5. Architetture chiave

### 5.1 Kernel higher-half (Fase 27, ADR-0020)
- **VA kernel**: `0xFFFF_FFFF_8010_0000` (-2G+1M), LMA a 1M
- **Direct map**: `[0, 64G)` a pagine 2M → `0xFFFF_8880_0000_0000`
- `PML4[0] = 0` a runtime (NULL-deref faulta)
- Conversioni: `phys_to_virt`/`virt_to_phys` in `kernel/src/addr.rs`

### 5.2 Process lifecycle (Fase 14, ADR-0010)
- **Morte logica immediata**: `Scheduler::terminate` → stato `Terminated`, release
  CBS/servizi/canali, cascata sulla discendenza, accodamento reclaim.
- **Teardown fisico differito**: `drain_reclaim` a inizio `on_tick` — stack kernel,
  slot TSS, address space user (solo foglie PTE owned).
- **Notifica unificata**: DOPO teardown, notifica TUTTI i peer (`die_peers`, max 31).

### 5.3 Address space teardown
- Bit AVL `0x200` (owned) su pagine di proprietà del processo (code, stack, ring, heap).
- `map_physical`/`map_in` NON settano owned (pagine iniettate: VGA, ring altrui).
- `teardown_user_space` libera SOLO owned + page-table private.

### 5.4 TSS per-processo (ADR-0006)
Ogni processo ha il proprio TSS con I/O bitmap. Driver userspace dichiarano
`io_ranges` in `user_binary.rs::NAMED_BINARIES`.

---

## 6. Servizi utenti — Ruoli e dipendenze

| Servizio | PID/Slot | Dipende da | Fornisce | Note |
|----------|----------|------------|----------|------|
| **init** | root | kernel | tutto | Spawn sequenziale, supervisione |
| **cardo** | 9 (Fs) | block, time | FS (/ + /fat + /dev/*) | Single-threaded, no deadlock |
| **console** | 12 | gpu | /dev/console | Modello a celle, UTF-8, font TTF |
| **gpu** | — | — | /dev/display | virtio-gpu driver (BAR/MSI-X) |
| **kbd** | — | — | keyboard input | IRQ-based, wake processi |
| **block** | — | — | DISK_READ/WRITE | DMA + PIO cache |
| **time** | — | — | get_ticks | CMOS RTC + uptime counter |
| **identis** | 11 | — | user auth | Token bearer TTL 300s, BLAKE2s-256 |
| **login** | 13 (Login) | identis | gate shell | OnDeath::LoginGate |
| **shell** | interattivo | console, FS | prompt | POSIX o nativa |

---

## 7. Vincoli critici (da non violare)

### 7.1 Meccanismo vs Personalità (ADR-0041, ADR-0048)
- **`libs/civis`** = MECCANISMO: neutro, zero dipendenze POSIX.
- **`flavours/posix/libr`** = PERSONALITÀ POSIX: appare SOLO qui.
- Gate: `rg "posix::" libs/civis/src kernel/src` → zero match.
- La PAL si adatta a Velordo, mai il contrario (nessuna syscall nasce per std/POSIX).

### 7.2 Stack cardo 16 KiB + LTO
Con LTO l'inlining fonde i frame e sfonda la guardia. Regola: buffer grandi in
`Box`, handler/seed/livelli btree/volume `#[inline(never)]`.

### 7.3 Kernel heap riservato
4 MiB riservati nel bitmap fisico (`phys_mem::reserve`). Senza, i frame finiscono
ai processi → corruzione free-list.

### 7.4 Seriale raw vs console
`send_raw` (non `send`) sul percorso raw: "byte in = byte sul filo". `send()`
espande backspace in "BS SP BS" — comportamento da console, non adatto al mirror
seriale raw (D5/ADR-0062).

### 7.5 Cardo single-threaded
Un driver che blocca cardo si deadlocka. Per questo il display va in **diretta**
(IPC per nome col driver, niente device FS).

---

## 8. Gate di test (regressione boot)

```
[testfs] PASS 5/5
[testsarca] PASS 64/64
[posixtests] PASS 4/4
[identistest] PASS 11/11
[usertests] PASS 59/59
[threadtest] PASS 12/12
```

Zero FAIL/PANIC/#.* FAULT. Suite FAT32 fuori dal gate (`scripts/run-fat-tests.sh`).

Unit test host (FUORI gate, millisecondi): `./scripts/run-unit-tests.sh` — 22 test
(term rasterizer/cursore/surface, valloc, blake2s).

---

## 8. ADR principali (riferimento)

| ADR | Titolo | Area | Stato |
|-----|--------|------|-------|
| 0005 | Microkernel architecture | Architettura | Fatto |
| 0006 | Per-process TSS | IPC/Sched | Fatto |
| 0008 | IPC per nome + registry | IPC | Fatto |
| 0009 | IPC asincrono | IPC | Fatto |
| 0010 | Process lifecycle | Sched | Fatto |
| 0020 | Kernel higher-half | Memory | Fatto |
| 0025 | Mechanism vs personality | Architettura | Fatto |
| 0026 | Hardening layer 1 | Security | Fato |
| 0027 | Hardening layer 2 (manifest) | Security | Fato |
| 0040 | Driver code sharing (vela) | Drivers | Fato |
| 0041 | Mechanism/Personality split | Architettura | Fato |
| 0047 | PAL std Velordo (S1.3) | Compatibility | Fato |
| 0048 | No POSIX inside kernel | Architecture | Fato |
| 0049 | ArcaFS design | FS | Fato |
| 0055/0058 | User management | Auth | Fato |
| 0059 | FAT32 fuori dal gate | Testing | Fato |
| 0060 | Display architecture (D3) | Video | Fato |
| 0061 | Modeset protocol (D4) | Video | Fato |
| 0062 | Login gate + console (D5) | Auth/Video | Fato |

---

## 9. Come navigare il codice

### Per un problema di scheduling:
`kernel/src/ordo/sched.rs` → `queue.rs` (code) → `exec.rs` (dispatch) → `ipc.rs`
(blocking/waking) → `lifecycle.rs` (exit/kill) → `ps.rs` (process state).

### Per un problema IPC:
`kernel/src/relay/channels.rs` → `kernel/src/syscall/ipc.rs` → `ordo/sched/ipc.rs`.

### Per filesystem:
`userland/cardo/src/server.rs` (loop) → `handlers.rs` (dispatch) → `ramfs.rs` /
`fat32/` / `arca.rs` (backend) → `libs/civis/src/fs/ring.rs` (client ring ops).

### Per video/terminale:
`userland/gpu/src/` (driver virtio-gpu) → `libs/term/src/disp.rs` (protocollo) →
`userland/console/src/` (console server) → `libs/term/src/raster.rs` (font).

### Per IPC client-side (userland):
`libs/civis/src/ipc.rs` (send/recv) → `libs/civis/src/fs/ring.rs` (ring frame ops) →
`libs/vfscli/src/ring.rs` (client helpers).

---

## 10. Build e test

```bash
./run.sh                          # Build + QEMU boot completo
cargo build --release              # Kernel solo (release)
RUN_DISPLAY=gtk ./run.sh           # VGA visibile in GTK window
cd docs && mdbook build            # Documentazione
./scripts/run-unit-tests.sh        # Unit test host (22 test, millisecondi)
timeout 150 ./run-tests.sh         # Regression suite completa
```

**Target**: `x86_64-unknown-none` (custom target in `targets/`). Boot PVH custom
(stub in `kernel/src/boot.asm`), niente GRUB.
