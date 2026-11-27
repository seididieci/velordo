# AGENTS.md - Istruzioni per Agenti AI

## Contesto del Progetto

Velordo è un **microkernel x86_64** scritto in Rust (ADR-0005). Architettura
microkernel: il kernel contiene scheduling, IPC, gestione della memoria e
routing degli interrupt; driver e servizi (console, file system, devfs, shell)
sono processi userspace che comunicano via IPC per nome con reply implicita,
supporto async e trasferimento dati zero-copy su ring SPSC per-processo. Lo
scheduler è unico RT a 32 priorità con Constant Bandwidth Server (CBS) per la
bandwidth reservation.

**Obiettivo**: un microkernel con isolamento dei servizi in userspace, IPC ad
alte prestazioni (per-nome, async, zero-copy) e CPU time garantito sotto carico.

**Orizzonte (dichiarato, non roadmap)**: self-hosting — un Velordo capace di
ricompilare se stesso. Non pianifica nulla da solo, ma ordina le priorita'
(storage veloce prima, servizi fuori dal kernel poi, personalita' per software
reale dopo). Stato e backlog vivono in `ROADMAP.md` (sorgente unica: tabella
completate, voci PIANIFICATE con scope e vittoria dichiarati, "Parcheggiate"
per le idee senza trigger); la storia dettagliata in
`docs/src/14-cronologia-fasi.md`.

## Stack Tecnico

| Componente | Scelta |
|------------|--------|
| Architettura | Microkernel (ADR-0005) |
| Language | Rust nightly (x86_64-unknown-none) |
| Bootloader | stub PVH custom (kernel/src/boot.asm), niente GRUB |
| Testing | QEMU (qemu-system-x86_64, `-kernel` + nota PVH) |
| Documentazione | mdbook |
| Target | x86_64 bare-metal |

NB: `vga.rs`/`serial.rs` sono DEBUG FACILITY temporanee in-kernel; migreranno
al console server userspace quando esiste l'IPC (ADR-0005 §3).

### File di contesto per agenti AI

- **`CONTEXT.md`** — mappa completa del progetto: struttura crate, ABI syscall,
  meccanismi IPC, architetture chiave, servizi utenti, vincoli critici, gate test.
  Leggere questo file prima dei sorgenti riduce drasticamente il consumo di token
  e contesto in nuove sessioni.
- **`AGENTS.md`** (questo file) — istruzioni operative: convenzioni, stato corrente,
  testing, pitfall, checklist fase.
- **`ROADMAP.md`** — stato fasi + pianificate + parcheggiate.

## Build Commands

```bash
# Build kernel + boot in QEMU (PVH)
./run.sh

# Build kernel only (release)
cargo build --release

# VGA visibile in locale
RUN_DISPLAY=gtk ./run.sh

# Build documentation
cd docs && mdbook build

# Serve documentation locally
cd docs && mdbook serve
```

## Coding Conventions

1. **Sempre** usare `#![no_std]` e `#![no_main]` nei moduli kernel
2. **Volatile** per tutti gli accessi MMIO (VGA, LAPIC, ecc.)
3. **Spin locks** per sincronizzazione (no std::sync)
4. **`hlt`** negli idle loop (mai `loop {}` vuoto)
5. **EOI** sempre dopo interrupt handlers
6. **Comments** solo quando necessario (il codice deve essere auto-esplicativo)
7. **Naming**: snake_case per funzioni/variabili, PascalCase per tipi
8. **Error handling**: usare `Result<T, E>` dove possibile, `unwrap()` solo in init
9. **Stratificazione meccanismo/personalita' (ADR-0025, ADR-0041)**: il
   MECCANISMO e' il crate `libs/civis` (neutro: sys/ipc/heap/task/fs/spawn/
   print/args, `error`); la PERSONALITA' POSIX e' `flavours/posix/libr`
   (`posix`/`stdio`/`fork`/`exec`). `Error` vive nel meccanismo
   (`civis::error`). Il confine e' una dipendenza Cargo e l'unico aggancio
   meccanismo→personalita' e' l'hook `civis::persona` (routing stdout,
   installato da `libr::entry!`). REGOLA: il meccanismo non usa mai POSIX
   (verifica `rg "posix::" libs/civis/src` → zero).
10. **Niente POSIX dentro (ADR-0048)**: POSIX appare SOLO come personalita'
    in `flavours/posix` (compatibilita'); mai nel kernel, nel meccanismo
    (`civis`, ABI syscall, PAL `std`) o nei servizi nativi. La PAL si adatta
    a Velordo, mai il contrario (nessuna syscall nasce per `std`/POSIX;
    noun test per le nuove). Gate: `rg "posix::" libs/civis/src kernel/src`
    → zero; `rg "os::unix" pal/` → zero. Bonificate le coincidenze
    (`SYS_SPAWN_COPY`, `OPEN_*`/`SEEK_*`, maniglie `CONSOLE_*`, `OsStr`
    proprio); residui documentati in ADR-0048 (numeri 1/2, ELF, verbi
    open/read/write, fd-protocol).

## File Structure

```
velordo/
├── kernel/         # Il kernel stesso
│   ├── src/
│   │   ├── main.rs     # Entry point Rust (rust_main)
│   │   ├── boot.asm    # Stub PM32 -> long mode (NASM)
│   │   ├── vga.rs      # VGA text mode
│   │   ├── serial.rs   # UART debug
│   │   └── ...         # Altri moduli
│   ├── Cargo.toml
│   └── linker.ld       # Include la nota PVH per il boot QEMU
├── libs/
│   ├── civis/          # meccanismo di sistema condiviso (ex libr): sys/ipc/heap/
│   │                   #   fs/spawn/print/args/net + hook `persona` (ADR-0041)
│   ├── identis-proto/  # protocollo IPC di Identis (ruoli, WHOAMI)
│   ├── musivum/        # compositor del DE (D8a, ADR-0087): layer, clip, danno
│   ├── littera/        # sessione di terminale (D8c1, ADR-0089): modello +
│   │                   #   ANSI + input, senza display
│   ├── sess-proto/     # layout della mailbox di sessione (D8c1, ADR-0089)
│   ├── pictura/        # primitive bitmap delle toppe (D8, ADR-0099)
│   ├── tabula/         # canvas a celle + differenziale (ADR-0098 addendum)
│   ├── formae/         # componenti di disegno TUI/GUI (ADR-0100): geometria,
│   │                   #   Theme, Target/Frame, Chart, Bar, History, fmt
│   ├── net/            # core rete puro (Ethernet/IPv6/NDP/TCP/SIIT/DNS)
│   ├── net-ep/         # formato degli slot dei ring endpoint (N7.0, ADR-0074)
│   ├── custos-proto/   # protocollo IPC di Custos (ADR-0080 fase 5)
│   ├── sig/            # firma Ed25519 delle immagini (ADR-0078 fase 2)
│   ├── tls/ tls-net/   # TLS 1.3 puro + newtype `TcpStream` (N6.2/N6.3)
│   └── vela/           # codice driver condiviso (hub/block/input/gpu)
├── flavours/
│   └── posix/          # personalita' POSIX (ADR-0041)
│       ├── libr/       # crate `libr`: errno/stdio/fork/exec/sock + `entry!` (hook)
│       ├── server/     # posix-server (userposix.bin)
│       ├── shell/      # shell POSIX (usershell.bin)
│       ├── cli/        # programmi lanciabili POSIX (runhello)
│       ├── build/      # output .bin del flavour (generato)
│       └── tests/      # suite test della personalita' (Fase 58.5)
├── syscall-numbers/    # Costanti syscall + costanti condivise (kernel+user)
├── targets/            # Target spec rustc custom (S1.0: x86_64-unknown-velordo.json per PAL std S1.3)
├── pal/                # PAL std Velordo (S1.3, ADR-0047): velordo/{new,apply.py} + build/ (generato, gitignored)
├── scripts/
│   ├── boot.asm        # MBR 16-bit (riserva, non usato dal path PVH)
│   ├── build_common.sh # build_one() condivisa (freestanding PIC)
│   ├── build-userland.sh  # servizi nativi -> userland/build (+ build-posix)
│   ├── build-posix.sh  # binari POSIX -> flavours/posix/build
│   ├── build-tests.sh  # binari test suite -> testland/build
│   ├── arca-tool.sh    # seed del volume (manifest/policy/anchor/sig + binari)
│   ├── sign-bins.sh    # firma per bucket + anchor (ADR-0078 fase 2)
│   └── putc16.inc
├── run.sh              # build userland + testland + kernel + QEMU (PVH)
├── userland/           # servizi NATIVI: init, block, cardo, gpu, kbd, vela,
│   │                   #   porta (servizio tastiera, D8d: niente device),
│   │                   #   vestigia, time, uptime, console (console server
│   │                   #   D3 + sessioni), custos (policy per identita',
│   │                   #   ADR-0080), monitor (dashboard TUI a 1 Hz sopra
│   │                   #   `formae`, ADR-0100);
│   │                   #   tools/arca (nativo, linka `libr` solo per
│   │                   #   l'entry/redirect)
│   └── build/          # output .bin dei servizi nativi
├── libs/term/          # terminale (D3, ADR-0060): protocollo display (`disp`)
│                       #   + modello a celle/UTF-8/font TTF rasterizzato/
│                       #   surface pixel|VGA testuale (no_std)
├── blake2s/            # BLAKE2s-256 proprio (RFC 7693, no_std) — P5
├── arcafs/             # casa ArcaFS (56.2a): tag/wire/formato condivisi guest/host — P5/A1/A2
├── tools/arca/         # tool host `arca create` (std, P5)
├── tools/sign/         # tool host: chiavi e firme delle immagini (std, ADR-0078)
│                       #   keys/*-test-only.key = chiavi di TEST (mai produzione)
├── testland/           # TEST SUITE + repro + demo (nessun binario "utente")
│   │                   #   demo, testfs, testfat, testsarca, hogheap, devreader,
│   │                   #   usertests (+ helper usertest-client/usertest-spin);
│   │                   #   srv/cli: demo storiche Fase 7 NON piu' buildate
│   │                   #   (basate su IPC per PID, rimosse in Fase 12)
│   └── build/          # output .bin dei test
├── docs/               # Documentazione mdbook
│   ├── src/            # Capitoli (mdbook src) + adr/ (decisioni, source unica)
│   │                   # + 14-cronologia-fasi.md (storia dettagliata per fase)
│   ├── book.toml
│   └── book/           # output build mdbook
├── ROADMAP.md          # Stato (tabella completate) + Pianificate + Parcheggiate
└── AGENTS.md           # Questo file
```

**Layout moduli (Cleanup)**: ogni crate ha `main.rs`/`lib.rs` sottile (solo attr, `mod`, import, `_start`/panic o re-export) + moduli tematici (`cardo`: mount/ramfs/ftable/rights/rings/handlers/server; `civis`: ipc/spawn/sys/print/tsc/fs/* + facade che riesporta tutti i path `civis::X`; `libr`: posix/stdio/exec/fork; kernel: `ordo/` (`sched/`+`aegis`), `relay/` (`channels`), `arc/` (`vmm_user/`+`phys_mem`+`heap`+`vmm`), `syscall/` con facade e `main.rs` intoccato); i figli usano `use super::*;` (+`use crate::*;` se annidati) e i cross-riferimenti sono path espliciti (`handlers::handle_open`), mai glob dai parent. Costanti/tag condivisi stanno in `syscall-numbers` via `civis`, mai duplicati nei crate.

## Stato corrente

Gate (un solo boot, ADR-0081): `[testfs] PASS 5/5` + `[testsarca] PASS 64/64` + `[posixtests] PASS 4/4` + `[identistest] PASS 12/12` + `[usertests] PASS 72/72` + `[threadtest] PASS 12/12` + `[shell-native] PASS 33/33` (la shell nativa gira nella **stessa** QEMU del gate: login via `sendkey` e `--attach-pid`, `scripts/test-shell-native.py`), zero FAIL/PANIC/FAULT; il gate stampa `[gate] PASS/FAIL` ed **esce != 0** (vedi `docs/src/11-testing.md`). Le fasi POSIX restano storiche dal login nativo (ADR-0062). La suite FAT32 e' **fuori dal gate** (ADR-0059): `scripts/run-fat-tests.sh`.

- **Video (D3, ADR-0060)**: tre ruoli — `gpu` = **driver** (virtio-gpu:
  BAR/MSI-X/risorsa 2D/scanout; pubblica `/dev/display`), `console` =
  **console server** (modello a celle, UTF-8, font TTF rasterizzato a
  runtime, surface pixel|VGA testuale; **sessioni di terminale** e mirror
  seriale, possiede i pixel), `shell` = testo. Il codice di testo vive in `libs/term` (no_std);
  il driver linka solo `term::disp`. Font **Maple Mono NF** (OFL 1.1, in
  `third_party/`) letto dal volume root come oggetto, rasterizzato in punto
  fisso; font fisso 16 px, cella 10x22, griglia dalla risoluzione (128x36 a
  1280x800). `SYS_DMA_ALLOC` prende lo **slot** (0 = staging invariato;
  >= 1 = device buffer contiguo <= 32 MiB, **grant chiuso** all'owner di
  `Service::Console` = 12). Fallback: VGA testuale (`VIRTIO_GPU=0`). Il
  display si parla in **diretta** (IPC per nome col driver: niente device
  FS — il cardo e' single-threaded: un driver che lo blocca mentre serve un
  cliente si deadlocka, e i ring `CLI_*` sono una finestra per processo). Il
  font e' letto tutto in RAM all'avvio (niente I/O FS durante il servizio).
  Nel path pixel la finestra VGA resta vuota ("Booting from ROM"): il
  terminale e' nella console del device virtio-gpu. **Il display di default
  e' il terminale pixel** (D5, ADR-0062): `run.sh` espone `FB_WINDOW`
  (`auto` = default: `-vga none` se si vuole un display grafico **e** il
  device virtio-gpu c'e'; `1` forza `-vga none`; `0` forza la VGA) — con la
  VGA presente QEMU apre la finestra della **console 0 = VGA**, cioe' della
  console *morta* nel path pixel, e il terminale (console 1) resta senza
  finestra: due console, una sola visibile, e quella visibile e' quella
  sbagliata. `RUN_DISPLAY=gtk ./run.sh` mostra **il terminale**;
  per la prova headless: il dump `screendump` del monitor (la superficie
  virtio-gpu; con la VGA presente il dump prende la VGA 720x400).
  **Rasterizzazione** (`libs/term::raster`, punto fisso 16.16): copertura
  esatta in orizzontale + 8 sub-scanline in verticale; **l'ultimo pixel
  parziale di uno span va contato** (`pa..=pb`: con `pa..pb` il testo si
  assottiglia e diventa irregolare, 61-97% dell'inchiostro vero) e la bbox
  usa il `ceil`. Guardia: unit test host fuori dal gate
  (`./scripts/run-unit-tests.sh`).
  **D4 (ADR-0061)**: la risoluzione e' una decisione — il guest comanda
  (`CONSOLE_SET_MODE`) e l'host insiste (config change MSI-X →
  `GPU_DISPLAY_CHANGED` → la console cede e si riadatta). `SYS_DMA_FREE=62`
  (la DMA API puo' liberare: il buffer device si rialloca), scrollback +
  `Grid::resize` (il testo sopravvive), layout centrato (celle intere,
  resto diviso a meta'). Il bound del device buffer si misura sull'host
  (`DMA_DEVICE_PAGES_MAX` = 8192 pagine = 32 MiB, copre 4K, con
  `DMA_SLOT_SPAN` a 32 MiB: le due costanti vanno assieme); oltre il bound
  la console rifiuta *prima* della syscall e tiene la risoluzione corrente
  (il device scala lo scanout). Il `PRESENT` non trasporta i pixel futuri:
  chi ridisegna deve anche **flushare** (`GPU_DISPLAY_STATS` conta i FLUSH
  dopo l'ultimo PRESENT; senza flush lo schermo resta nero fino al tasto).
  Test: `t63` nella suite (usertests 60/60, modeset a 1920x1088 + rifiuto
  oltre-bound + refresh verificato). Prova manuale GTK **fatta**: al resize
  il terminale si aggiorna subito, testo conservato. **D5**: al
  ri-adattamento il **cursore non salta piu' in fondo** (tiene la sua riga,
  il contenuto resta ancorato al testo): unit test host in `libs/term`.
  **D7 (ADR-0066)**: redraw guidato dal danno — T0/T1 (rect di trasferimento,
  `FULL_FLUSH_ONLY` come valvola), T2 (surface senza `volatile` per pixel,
  fill a parole, `blend_row_bgra`) e T3 (bg layer precomposta fondo+immagine,
  composizione a banda: `copy_from` + `draw_ink`, il fill/blend per cella
  sparisce). Frame al login identico a T1 (0 pixel diversi). **T4**: scroll
  a shift di pixel (`shift_rows_up`, solo l'ultima riga ridisegnata) con
  ricomposizione quando la bg non e' riga-invariante o il feed e' in blocco;
  `t64` confronta il CRC del frame dopo shift e dopo `CONSOLE_REPAINT`.
  Bulk: feed > 256 B differisce render+flush ogni 8 write, `hexdump` a
  blocchi da 4000 B, mirror seriale con budget 256 KiB, scrollback
  `VecDeque` (`hexdump /bg.bmp` 5,2 MB: ~38 min → 31 s).
  **Mirror seriale + routing stdout POSIX** (D8c2, ADR-0090): il default
  stdout dei programmi POSIX (`civis::println!` → `print::flush` → hook
  `civis::persona` → `libr::stdio::route_out`) e dei writer nativi
  (`console_write` di login/interpres/comandi) passa da `civis::sess::out`:
  sessione attestata → sessione 0 → seriale, e il **mirror seriale lo fa il
  server** (una sola scrittura per riga, non piu' device + seriale lato
  client). Il redirect (`>`) resta file-only (e non entra nel mirror: il file
  non e' il terminale). Fuori scope dichiarato: il sink PAL
  `std::io::stdout` (`write_console` → `SYS_WRITE`).
  Due trappole emerse dal gate: (a) il terminale **pieno** cambia
  l'invariante del re-adattamento — crescendo la finestra riappare lo
  scrollback, quindi `non_empty` **cresce** (t63: 900 → 1221) e la prova
  giusta e' la **coda** del flusso (`Grid::text_crc_last`; `CONSOLE_STATE`
  prende `w0` = limite); (b) la console ha due classi di peer (chi la monta
  e chi le scrive): `EXIT_NOTIFY` deve re-registrare **solo** per la morte
  del peer FS (`fs_peer_pid()`), o un test che esce la fa entrare in
  `fs_register` mentre cardo aspetta una write → deadlock (ADR-0086).
  Prossimo: il DE.
- **Rete (N1–N6.3, ADR-0068/0069/0072)**: stack nativo in userspace — `libs/net`
  core `no_std` 100% safe (Ethernet/IPv6/ICMPv6/NDP, SLAAC, RA guard,
  neighbor cache **STALE + NS di verifica** (non si butta la entry a 30 s:
  il MAC resta usabile, si scarta dopo 3 NS senza risposta), IID
  randomizzato, no DAD; **TCP reale** (N6.1: `tcp/conn.rs`, macchina a stati
  pura con 10 stati, finestra 2–4 segmenti, go-back-N RTO 1/2/4 s, solo
  in-order, FIN/RST/TIME_WAIT, listener + backlog 4); 54 unit test host
  IPv6/NDP/TCP/SIIT/DNS/parse), `userland/netdev` driver virtio-net (1 RX + 1 TX,
  64 descrittori, 2048 B, no offload, MSI-X, ring condiviso via shm),
  `userland/net` servizio (attach, RX a eventi N8, SLAAC, ping6 + endpoint a
  capability: `NET_EP_OPEN/TX/CLOSE/STATS`, tag `0x6F`/`0x73`/`0x81`/`0x82`,
  shm ring SPSC per endpoint; TCP: connessioni **nel core**, id di
  connessione, `EV_ACCEPT`/`EV_SYNACK`/`EV_FIN`/`EV_RST`, `NET_EP_TX` TCP = 0
  preso in carico), `userland/netecho` (echo UDP porta 7 + listener TCP porta
  8; l'handshake è del core, niente SYN+ACK applicativo),
  `userland/siit` (traduttore IPv4 SIIT/NAT64, `Service::Siit` slot 16,
  endpoint UDP porta 30000, tabella NAPT, API `SIIT_EP_OPEN/TX/CLOSE`,
  tag `0x84`/`0x85`/`0x86`), `userland/dns` (resolver, `Service::Dns`
  slot 17, API `DNS_EP_OPEN/TX/CLOSE/STATS`, tag `0x87`/`0x88`/`0x89`/`0x8A`,
  tabella statica `example.com` A e `ipv6.example.com` AAAA; **N6.0**:
  inoltro upstream **plain UDP** per i nomi fuori tabella, config
  `sys:dns.conf` (`upstream = auto|<ipv6>`, `port`, `mode = plain|dot|off`,
  `timeout_ms`), endpoint UDP effimero + single-flight + deadline + `txid`
  rimappato, SERVFAIL se l'upstream è configurato ma muto/invalido,
  NXDOMAIN solo senza upstream; **N6.3**: `mode = dot` reale (una connessione
  TLS per query, vedi sotto); `upstream = auto` usa il tag **`NET_ADDR`
  (0x8B)** di `net` (selettore link-local/global/router, 16 byte in due
  parole)).
  Kernel:
  `Service::Net=14`/`NetDev=15`/`Siit=16`/`Dns=17`/`Custos=18`/`NetBroker=19`,
  `MSI_VECTOR_NET=0x51`, finestra MMIO per servizio, grant DMA per NetDev,
  `SERVICE_COUNT=20`.
  I servizi partono solo con `net=1` in cmdline (run dedicato, fuori dal
  gate). Test: `./scripts/run-net-tests.sh` (passt, IPv6 reale): SLAAC +
  **echo reply** (N1), `[netudp] risultato: PASS` (N2: endpoint ephemeral →
  `::1:7` con validazione echo, server `netecho` su porta 7),
  `[net3] risultato: PASS` (N3: endpoint TCP → `::1:8`, validazione invio
  SYN accettato, server `netecho` TCP su porta 8), `[net4] risultato: PASS`
  (N4: endpoint UDP presso `siit` verso indirizzo IPv4 in formato SIIT
  `64:ff9b::/96`, validazione accettazione payload), `[dns] risultato: PASS`
  + `[dns] upstream: PASS` (N5/N6.0: query A per `example.com` →
  `93.184.216.34` e per `upstream.test` → `198.51.100.7` via responder
  Python sull'host su `[::]:5353`, seed di `sys:dns.conf` gated su
  `NET_DNS_UPSTREAM_PORT`).
  **N6.2 (ADR-0072, completato)**: `libs/tls` è un adattatore **puro**
  (niente `civis`: trasporto e orologio iniettati) sopra `embedded-tls`
  `=0.19.0` (`default-features = false`, features
  `rustpki`+`rsa`+`p384`+`alloc`: `webpki` è std-only, `rsa` serve alle
  catene RSA-4096, `p384` alle firme ECDSA-SHA384 — senza, le arm gated di
  `pki` rifiutano la catena DigiCert G3 di Quad9). Moduli:
  `transport` (trait + adapter `embedded_io`), `drbg` (ChaCha20, RDRAND con
  gate CPUID, fallback `rdtsc`, reseed 1 MiB), `trust` (`CertVerifier` con
  `TlsClock`, `CERT_SIZE` 4 KiB fail-loud, record 16640/16384), `session`.
  L'unico aggancio al sistema è la feature `civis-clock` (`SystemClock` su
  `civis::time::wall_secs()`): i 12 test host girano senza. Prova reale in
  QEMU: `testland/tlsprobe` (stack grande) + `scripts/run-tls-tests.sh`
  contro un server TLS sull'host (certificati da `tools/tls-test-certs`,
  server Python `ssl`, ALPN `dot`), catena **e** hostname verificati +
  record applicativo HTTP. **Bug TCP trovato dalla prova**: `next_tx`
  spediva dati in `SynSent` con ACK=0 (passt resetta; invisibile sul
  loopback perché il SYN+ACK è sincrono) — corretto in
  `libs/net/src/tcp/conn.rs` (dati solo in `Established`/`CloseWait`, FIN
  solo a buffer vuoto), regressione in
  `no_data_before_handshake_completes`. **Nota heap**: `ChaCha20Rng` ha
  align 16 e l'heap di civis accetta solo align ≤ 8 → il DRBG lo avvolge in
  un wrapper a align 8.
  **N6.3 (ADR-0072, completato, → Accepted)**: `dns` in `mode = dot` —
  `libs/tls-net` (`TcpStream` sull'endpoint `net` + `impl tls::Transport` +
  `SystemClock`) condiviso con `tlsprobe`; `libs/tls` resta puro (via la
  feature `civis-clock`) e un `&mut T: Transport` lascia il socket al
  chiamante. Framing RFC 7858 (`frame_tcp`/`tcp_prefix_len` in
  `libs/net::dns`), **una connessione per query ma endpoint persistente**
  (il ring si mappa una volta; `EV_ABORT` libera lo slot TCP e azzera
  `conn_id`). Config `sys:dns.conf` con `sni`,
  CA obbligatoria da `sys:ca.der`, `mode = dot` mal configurato o
  TLS/certificato invalido → **SERVFAIL** (mai NXDOMAIN, mai plaintext).
  Due invariant di `net` corretti: su RST l'endpoint CONNECT non viene più
  liberato (ring smappato per il client) e gli eventi stantii vanno scartati
  al reconnect. Prova in `scripts/run-tls-tests.sh` (un boot): `dot: PASS`
  ×2 (query + reconnect) + `dotfail: PASS` con certificato fuori trust.
  **N7.0 (ADR-0074, completato)**: il lato **client** del protocollo a
  endpoint era copiato in otto posti con tre politiche di backpressure e due
  modelli di cursore. Ora il formato è in `libs/net-ep` (puro, zero
  dipendenze, layout IPv6/IPv4, 6 unit test host) e il client nel meccanismo:
  `civis::net::{Endpoint, Stream}`. Invarianti unici: cursore TX **locale**
  (i contatori del ring TX del servizio sono consultivi), `w0 == 0` = preso
  in carico / `!0` = `Full` senza drop silenzioso / errore IPC = `Dead`; testa
  RX avanzata solo dopo la copia; `len` invalido consumato e saltato;
  `attach` non azzera i contatori; `close` idempotente. `libs/tls-net` è ora
  un newtype (orphan rule) su `civis::net::Stream`. Migrati `netecho`, `dns`,
  `testland/{netudp,net3,nettcp}`; `siit`, il server `net` e il lato client
  di `dns` usano il codec. Nessuna syscall/tag/ABI nuova. Il workspace root
  ha `exclude = ["libs/net-ep"]` (path dep con workspace proprio per i test
  host). Verifica: unit host 117/117, net tests PASS, TLS/DoT PASS, gate 6/6.
  **N7.1 (ADR-0076, completato)**: la personalità espone le socket sopra il
  client del meccanismo — `libr::sock` con
  `socket/connect/send/sendto/recv/recvfrom/close`, **zero syscall e kernel
  intatto**. Scelta dichiarata (b): fd `64..72` riservati alle socket, i fd
  < 64 restano ai file (fd table unificata = follow-up); solo `AF_INET6`
  (`AF_INET` → `EAFNOSUPPORT`, IPv4 via `siit` è un follow-up); errori come
  `-errno`. Invarianti: `Full` → `-EAGAIN` (mai drop silenzioso), attese
  sull'**evento** del servizio (`Endpoint::wait`, E2/ADR-0084), e **su TCP
  gli eventi di controllo non sono dati** (`SynAck` scartato; `Fin`→0 = EOF,
  `Rst`→`-ECONNRESET` — il primo giro del probe leggeva 0 byte e lo chiamava
  EOF). Vittoria: probe
  `testland/posixsock` (init lo spawana con `net=1` dopo `dnsprobe`) fa echo
  UDP `netecho:7` + TCP `netecho:8` con le sole API POSIX →
  `[posixsock] risultato: PASS` in `scripts/run-net-tests.sh`; lo stesso
  binario è in `usr/bin/posixsock.bin` per l'uso manuale dalla shell. Gate
  invariato (6/6, zero FAIL/PANIC/FAULT).
  **E2 (ADR-0084, completato)**: `NET_EP_EVENT` (0x8C) è l'hint coalescente
  che `net` manda al proprietario di un endpoint quando scrive slot nuovi nel
  suo ring RX (dati/accept/fin/rst/abort); `civis::net::{wait, pending}` +
  `Endpoint::{pending, wait}` dormono su `recv_timeout` (E1) con **una sola**
  attesa e il ring come verità. Migrati `netecho`, `dns` (loop + upstream),
  `siit`, `netbroker`, `Stream` (DoT/TLS) e `libr::sock::recvfrom`: **zero
  risvegli a tick** (`rg futex_wait libs/civis/src/net libs/tls-net` → 0,
  gate strutturale in `run-net-tests.sh`); nei servizi restano solo i retry
  di boot `service_lookup` (ADR-0067). **Trappola trovata a runtime**: la reply è
  implicita al messaggio corrente — leggere un hint con una reply in sospeso
  ne sposta il target (client appeso, reply spaiata): **si conferma prima
  (ack) e si pubblica dopo nel ring** (in `dns`), mai dormire con una reply
  aperta. Seconda trappola: `net::wait` che mette da parte un messaggio
  estraneo e **resta** addormentato = deadlock del broker servito ai client →
  `Event` significa "c'è qualcosa da fare" e i messaggi estranei vanno nello
  stash di `civis::ipc` (letti da `recv`/`recv_poll`, mai persi).
  **N8 (ADR-0085, completato)**: anche il **servizio** `net` è a eventi — il
  driver `netdev` manda `NET_RX_EVENT` (0x8D, fire-and-forget, canale appreso
  al `NET_ATTACH`) quando il ring RX condiviso passa da vuoto a non vuoto, e
  il servizio dorme su `recv_timeout(Net::next_deadline())` (retransmission
  TCP, TIME_WAIT, RS/echo, aging NDP; `None` → `0` = mai). Niente più
  `NET_POLL` a tick (tag **rimosso** dall'ABI: la guardia è il compilatore),
  kick `NET_TX` solo se `head != tail`, driver che drena il TX anche su IRQ.
  Corretto un bug latente: `Conn::rto_expired` chiudeva una connessione
  **inattiva** dopo `MAX_RETRIES` poll. CPU idle misurata **0,0%** (soglia
  5%, boot dedicato nello script). Trappole: il primo RS ha bisogno del suo
  kick; il confronto per il kick è head-vs-tail (non "ho scritto io"); le
  deadline devono coprire anche i byte bufferizzati e mai partiti.
  Prossimo: DoH (N6.4), poi VT-d.
- **Input a eventi (E0, ADR-0082)**: il device `/dev/input/keyboard` risponde
  subito (`0` = vuoto), quindi l'evento arriva per IPC: `porta` tiene i
  sottoscrittori (`INPUT_ATTACH`/`INPUT_DETACH`, tag 0x46/0x47) con focus
  **LIFO** e manda `INPUT_NOTIFY` (0x48, fire-and-forget, hint coalescente)
  al titolare quando decodifica tasti; il client dorme in `recv()`
  (`civis::input::wait`, su morte di porta una riattaccata poi uscita loud) e
  **drena** il device finche' `civis::input::read` non ritorna 0. **Nessuna
  modifica al kernel.** `interpres`, `identis-login` e `create-user` non
  pollano piu' (CPU al prompt 1,0% / 2,0% misurata; gate `< 5%` su entrambi i
  prompt); la shell POSIX resta a polling (personalita', debito dichiarato).
  E2 (eventi di rete) e' il seguito dichiarato nello stesso ADR.
- **`recv` con deadline (E1, ADR-0083)**: `SYS_RECV` (17) prende `arg1` = tick
  assoluto di scadenza (`0` = mai: i chiamanti esistenti non cambiano), alla
  scadenza `rax = -2` e **nessun messaggio consumato**; **la coda vince** (un
  messaggio arrivato mentre il timer scadeva si consegna sempre). La scadenza
  vive nel PCB (`Process::recv_deadline`) e la scandisce `on_tick` accanto ai
  futex (`tick_recv_deadlines`): nessuna tabella di timer, nessuno slot orfano,
  nessun PID riusato che eredita una deadline. Attenzione all'anti-deadlock di
  `pick_next()` (nessun altro runnable = resta `Ready` e ritorna `-1`): la
  syscall controlla l'orologio **prima** di bloccarsi, cosi' un timer scaduto
  non diventa un errore. Meccanismo: `civis::ipc::recv_timeout` (`RecvError::
  {Timeout, Dead}`), `task::Sleep` + `Receivable::deadline()` (l'executor dorme
  sulla deadline **minima** fra i task pending, `Join` propaga quella dei figli),
  `input::wait_until` (`Wait::{Event, Timeout, Dead}`; `wait()` =
  `matches!(wait_until(0), Wait::Event)`). Vittoria: l'orologio del prompt della
  shell **avanza mentre dorme** — repaint **in-place** di 8 cifre (`ESC[D`×13 +
  `HH:MM:SS` + `ESC[C`×5, **saldo zero**; il console server muove di UNA cella per sequenza,
  ignora i parametri CSI), solo con buffer vuoto e fuori continuazione, e **mai**
  il prompt intero (il gate conta i `" > "`); `get_time_str` restituisce
  esattamente 8 byte (il `\0` di prima finiva in una cella del prompt).
  `useruptime` usa `block_on(Sleep)` al posto del futex-timer. Test: t71 (scade;
  messaggio in gara non perso; 1 tick) e t72 (`Sleep` in `run`/`block_on` con
  misure di tick) → `usertests 68/68`; 33° check della shell (l'ora avanza al
  prompt fermo, forma esatta `ESC[D`×13 + ora + `ESC[C`×5). Guardia: `glyph_width_cells` di ✓ **e**
  ✗ = 1 cella (unit test host in `libs/term`).
- **Sessioni di terminale (D8c1, ADR-0089)**: il testo esce dalla console e
  diventa **sessione**. `libs/littera` (`no_std`, zero dipendenze dal sistema)
  = `Session` (griglia `term::Grid`, decoder UTF-8, parser ESC/CSI, coda di
  input, eco) + `Sessions` (`MAX_SESSIONS` 8, id = slot **stabile**, focus,
  attestazioni per pid, attachment per canale; sessione 0 = schermo con
  scrollback 1000, nuove 256). La console tiene il **display** (surface, font,
  danno, T4) e delega con `feed_session(idx, bytes)`; il render disegna solo la
  sessione in `focus` (helper `fgrid_*`). **Lo scroll e' di entrambi**: `put`
  non scrolla, ritorna `Step::NeedScroll`, chi disegna scrolla (modello +
  `shift_rows_up`) e richiama `put` con lo **stesso** codepoint; la sessione
  ricorda l'azione (`Resume::{Newline, Home, WideWrap}`) e la completa — la
  politica T4 (`can_shift`) resta identica. Endpoint: tag `SESS_*`
  (0x9B–0xAE, nessuna syscall/tag di servizio nuovo), capability = **mailbox**
  di una pagina shm (`libs/sess-proto`: OUT `0..2048`, IN `2048..4096`),
  creata dal server e mappata RW dal cliente (`civis::sess`, `Drop` che smappa
  a pagina). **Attribuzione per pid** (`SESS_WHOAMI` → `SYS_PEER_PID`), mai
  handle allo spawn ne' ambiente; **D8f**: se il pid non ha binding/owner la
  risoluzione **risale i padri** (`ps_info.parent`, max 16) — un comando
  lanciato in una tile parla con *quella* tile (prima finiva sulla 0) e resta
  attestazione (la catena la dice il kernel); `SESS_BIND` (≥ `ThirdParty`) per il DE;
  **D8f passo 2** (poi S3.3c/ADR-0094): l'**utente autenticato** vive nel
  **server** (`SESS_AUTH_SET` 0xAF, solo `System`, sulla sessione
  **attaccata**; `SESS_AUTH` 0xB7 in lettura per pid, solo `System`). I tag
  `0xAD`/`0xAE` (`SESS_TOKEN_SET`/`SESS_TOKEN`) sono **ritirati** (i numeri
  non si riusano): nessun token, nessun segreto che viaggia. `VELORDO_SESSION`
  non esiste, login scrive il **nome** sulla 0, il DE lo propaga alle tile
  nuove, `interpres`/`users`/`create-user` non leggono nulla: Identis/Custos
  risolvono il **pid** del chiamante (credenziale locale da `VERIFY`, oppure
  la sessione autenticata via `SESS_AUTH`); **nessuna variabile d'ambiente**
  (S3.3d/ADR-0095: il prompt la chiede al server, `impluvium`/`login` non
  passano piu' `USER`);
  `EXIT_NOTIFY` purga binding/sessioni del morto (mai la 0) con `munmap` **a
  pagina prima** di `shm_destroy`. `/dev/console` e' un **adattatore sulla
  sessione 0** (`DEV_WRITE` → `feed_session(0)`, `CONSOLE_*` → focus): il
  device non e' un secondo canale di testo. `WIN_OPEN` con `w0 = 0` = "la tile
  della mia sessione" (risolta per attestazione, ammessa solo se e' quella in
  focus). Limiti D8c1: focus solo 0 (tile multiple = D8e), `SESS_RESIZE`
  negato sulla sessione in focus (la misura e' del display). **Trappole**:
  `resize`/`echo` hanno la sessione **implicita** nell'attachment (in `w0` va
  la geometria/il flag, non l'id) e `dump_text` si ferma all'ultima riga con
  contenuto o al cursore (le righe vuote in fondo falsano la coda). Test: t74
  (`usertests 70/70`), unit host `littera` 14 + `sess-proto` 3.
- **Ritiro di `/dev/console` (D8c2, ADR-0090)**: il device di testo **non
  esiste piu'**. Login, shell nativa, comandi, personalita' POSIX e `t64`
  scrivono sulla **sessione**; `porta` non inoltra piu' testo (la tastiera
  resta un device di sola lettura: i tasti entreranno nelle sessioni in D8d).
  Decisioni: la mailbox e' **dell'attachment** (una per canale, non per
  sessione — un buffer condiviso si sovrascriverebbe fra il "client copia" e
  il "server copia"; il ring FS era gia' per processo) e `SESS_WRITE.w1` e'
  l'hint di mirror (`0` = entra nel log seriale, `1` = solo schermo: eco,
  ridisegni, test). Il mirror seriale e' del server (sessione in focus, nudo,
  1 MiB per sessione come rete di sicurezza; il budget *per comando* da
  256 KiB resta della shell). Ritiro **loud**: `DEV_CONSOLE` fuori dall'ABI
  (il numero non si riusa), niente `dev_type("console")`, niente
  `fs_register`, niente gestori `DEV_*`; `open("/dev/console")` →
  `ERR_NOTFOUND`, `write` sulla tastiera → `ERR` (frame comunque consumato).
  Il marker `[gate] boot: login-ready` (emesso da `identis-login` **dopo** il
  reply della write del prompt) sostituisce `[login] Nome utente:` nel
  contratto di `run-tests.sh` e `scripts/shell_harness.py`. Sparisce la
  trappola ADR-0086 (ri-registrare il mount su `EXIT_NOTIFY`). Gate invariato
  (70/70, 33/33) + asserzioni strutturali: nessun sorgente cita il device
  (tranne il test che ne prova il ritiro), niente `DEV_CONSOLE`, `t64` non in
  SKIP. **Trappole**: doppio mirror se il client non perde la sua copia nello
  stesso passo; l'eco non entra nel mirror; due punti di `run-tests.sh` piu'
  `wait_login_prompt` da cambiare insieme, e **mai** mentre il gate gira (bash
  rilegge il file per offset: modifica a meta' corsa = errore di sintassi).
- **I tasti sono della sessione (D8d, ADR-0091)**: la tastiera non e' piu' un
  device. `porta` serve i byte con **`INPUT_READ`** (0x49: diretto, sincrono,
  payload nei **registri** — niente ring e niente cardo) a chi ha la
  **capability** (`INPUT_ATTACH` ≥ `ThirdParty`: un `Untrusted` non legge i
  tasti di nessuno); `/dev/input/keyboard` e' **ritirato** (`DEV_KEYBOARD`
  fuori dall'ABI, il numero non si riusa) e resta solo `/dev/kbd` (scancode
  raw, lo legge `porta`). Il **focus owner** — la console in D8d, il DE in D8e
  — si sottoscrive (E0), a ogni hint **drena** e spinge i byte nella coda
  della sessione in focus; il lettore si dichiara con `SESS_READ` e viene
  svegliato dall'hint `SESS_INPUT_EVENT` (0xAA, coalescente: la coda e' la
  verita'). L'**eco resta al lettore** (login/`interpres`/shell POSIX hanno i
  loro editor: `input::attach` spegne l'eco del modello, `SESS_ECHO` e'
  l'opt-in per un consumatore senza editor). `civis::input` non cambia forma
  (`attach`/`read`/`wait`/`wait_until`/`detach`) — cambia dentro, e la shell
  POSIX perde `TERM_FD`. `civis::input::device` e' il lato del focus owner.
  **Trappole**: la **reply va prima dell'hint** (chi inietta e' spesso anche il
  lettore e un hint davanti alla reply fa fallire la sua `send` con
  `UnexpectedMsg` — per questo `ipc::wait_reply` mette gli hint nello stash,
  lezione E2); l'`EXIT_NOTIFY` della console va riconosciuto in `wait_until`
  (riattaccata una volta, o si dorme su una console morta); la console parte
  **prima** di `porta` (attach bounded a boot, poi un tentativo per volta nel
  loop con `recv_timeout` — mai un'attesa bloccante in un server); `t31`/`t74`
  citavano il device e vanno riscritti (o il ritiro passa con i test spenti).
  Test: t75 (`usertests 71/71`), unit host `littera` 16 + `sess-proto` 3 = 173.
- **Tile nel server (D8e1, ADR-0092)**: la console disegna N sessioni in N
  rettangoli. Una tile, nel server, e' `rect + sessione` (+ finestra `Direct` in
  `Layer::Tiles`); l'handle e' `sid + 1` (una tile *e'* una sessione).
  Protocollo `TILE_SET/COMMIT/INFO/DROP/CRC/SCREEN/SIZE` (0xB0–0xB6, solo >=
  `ThirdParty`): `TILE_SET` in **staging**, `TILE_COMMIT` applica in un render +
  flush solo; `w = 0` = tile nascosta (e' cosi' che si fa lo **zoom**);
  `TILE_CRC` prova "solo questa tile e' cambiata". **La geometria e' per tile**:
  `Geom::for_rect` (celle intere dentro il rect, origine in coordinate schermo) +
  `Geom::local` + `Pixel::view` — il clip per tile e' una **vista** (base
  spostata, `width`/`height` del rect), quindi non c'e' un check in piu' nel
  percorso caldo e nessun glifo/ospite puo' scrivere fuori. Il render e' per
  tile; il cursore del terminale e' **solo** della tile in focus. Con **una**
  tile il server si adatta da solo al modeset (D4, come D8c1) e il gate resta
  invariato; con N tile manda `TILE_SCREEN` al DE. **Trappole**: il render era
  "una griglia" in **sei** punti (`render`, `draw_cell_at`,
  `restore_layer0_cell`, `scroll_session`, `cells_rect`/`grid_px_rect` **e
  `feed_session`**, che rendeva solo se la sessione era in focus — trovato da
  `tileprobe`); `Pixel::put_char` e' di `Surface` (la vista si avvolge in
  `Surface::Pixel(view)`); il **layer 0** va traslato nell'origine della tile e
  `copy_from` legge `src` alle stesse coordinate (le bande si copiano sulla
  superficie intera); una tile **nascosta non ridimensiona** la sessione
  (`Geom::for_rect` di un rect vuoto darebbe 1x1 e distruggerebbe il testo allo
  zoom); lo shift T4 e' della **tile** che mostra la sessione. Prova fuori dal
  gate: `./scripts/run-desktop-tests.sh` (`desktop=1`, `tileprobe` in `usr` come
  `posixsock`); il refactor e1.1 e' stato verificato col **CRC del frame
  identico** (t64). Unit host 176 (+ `Geom::for_rect`/`view`, rect impacchettato).
- **Il DE esiste (D8e2, ADR-0092)**: `impluvium` (`sys/bin/impluvium.bin`) e'
  un processo. **D8f**: non e' piu' un servizio di boot — lo spawana il
  **login**, dentro la sessione autenticata (identita' nel server, niente env),
  ed e' il login a essere sorvegliato da init. Non registra un servizio,
  `Service::Impluvium` **non esiste**, kernel intatto.
  Possiede: **albero** (`libs/impluvium-tree`, `Leaf`/`Split{dir,ratio}`,
  **sezione aurea** `GOLDEN = 618`, `MIN_TILE`, somma esatta dell'asse), **layout**
  (`TILE_SET`/`TILE_COMMIT`), **focus** (geometrico, `hjkl`), **tasti** (e' il
  focus owner: legge `INPUT_*` da `porta` e inietta il resto con `SESS_INPUT`,
  mailbox sulla sessione in focus via `SESS_ATTACH`) e la **barra** (finestra
  `Overlay` con blocchi colorati, da due tile in su: con una tile il boot resta
  identico a D8d). Keybinding **Super**: `hjkl` focus, `%`/`"` split, `z` zoom,
  `Shift+q` chiudi. `porta` traccia `LWin`/`RWin` dal `KeyEvent` (che ha
  `state`; `pc-keyboard 0.7` **non** ha i modificatori GUI) e prefissa i tasti
  con `INPUT_SUPER` (0x1C) **nello stesso flusso** (due code = ordine non
  garantito). **Trappole**: `send` e' `Ok` anche su rifiuto (`w0 = !0`); i tag
  usati in un `match` devono essere **importati** o diventano binding
  catch-all (successo con `SESS_ATTACH` in console e con `DEV_WRITE` in porta);
  lo split che crea una sessione deve
  `SESS_CLOSE`rla se il layout lo rifiuta. **D8f**: la shell della tile 0 la
  crea il DE (`spawn_shell(0)` al boot), non piu' il login; le voci
  supervisionate senza servizio richiederebbero il **canale** di nascita o il
  supervisore le vedrebbe "assenti all'avvio" (non ce ne sono piu': il DE e'
  del login). Prova: gate (le 33 check della shell
  girano **attraverso il DE**) + `./scripts/run-desktop-tests.sh` (login via
  monitor, poi keybinding, `t76` che confronta i CRC delle due tile per il bordo).
  Unit host **183** (`impluvium-tree` 7).
- **Componenti e monitor (M1–M4, ADR-0100)**: la base grafica ha il suo livello
  riusabile. `libs/formae` (puro, niente `civis`) = geometria celle↔pixel,
  `Theme`, `Target`/`Frame` (testo **misurato** con `measure`/`snap_x`),
  `Chart` (area con gradiente), `Bar`, `History`, `fmt`. Primo consumatore
  `usr/bin/monitor`: CPU (grafico), RAM (barra), tabella processi, **1 Hz** su
  un `input::wait_until`, tasti `q/p/j/k/s/r`, responsive 47↔128. Prove:
  `scripts/run-monitor-tests.sh` (10 check, anche **pixel**: card dipinta +
  grafico che si muove). **Trappole**: il lettore si dichiara alla `SESS_READ`
  (dentro `input::read`), non con `attach` → il loop **drena prima** di
  attendere (come `fullscreen`); `PatchTarget::text` deve togliere l'origine in
  pixel o il testo di una card non in (0,0) finisce spostato del doppio;
  `tabula::text_styled` ha la guardia sulla riga (una riga in pixel invece di
  celle = index-out-of-bounds). Toppe alpha: `Pixel::blit` compone solo le righe
  con alpha < 255. La tabella e' testo su una **card a celle** (bg per cella),
  non una toppa. **Console**: in `render` il bbox delle celle si **espande alle
  toppe della riga**, o un render parziale (una cella cambiata) ribalta l'intera
  toppa e ridisegna solo il bbox, coprendo il testo delle card fuori dal bbox.
  **Difetti dal primo uso vero**: il delta di CPU per-processo usava il
  **totale globale** (`e.ticks - last_ticks` = negativo → 0 per tutti): ora
  `prev_ticks[pid]`; il campione scattava solo su `Timeout` ma la scadenza si
  spostava **anche su `Event`** (storia a singhiozzo, grafico fermo): ora si
  campiona **sull'orologio** (`now >= next_sample`) e si ridisegna solo se
  serve; una toppa **identica non si rispedisce** (`sent` confronta i byte).
- **Stato e futuro**: `ROADMAP.md` (sorgente unica: tabella completate 1-57, Pianificate, Parcheggiate).
- **Storia dettagliata**: `docs/src/14-cronologia-fasi.md` (log per fase: decisioni, bug trovati, lezioni, validazioni).
- **Personalità POSIX separata (Fase 58, ADR-0041, chiusa)**: meccanismo
  `libs/civis` (ex `libs/libr`), personalità `flavours/posix/libr`; binari in
  `flavours/posix/{server,shell,cli}` → `flavours/posix/build`, suite test in
  `flavours/posix/tests` → `flavours/posix/tests/build`. Servizi nativi in
  `userland/` (bins in `userland/build`).
- **Gestione utenti (ADR-0055/0058, Fase 3.1)**: il servizio `Identis`
  (`userland/identis`, `Service::Identis = 11`) detiene **da solo** il bucket
  `identis:config` (`nome:hash_hex` per riga) e parla col protocollo
  condiviso `libs/identis-proto` (framing chunked, magic `ACLI`/`ACUS`,
  codici `R_*`). **S3.3c (ADR-0094)**: **niente token**. `VERIFY` riuscito
  lega il **pid chiamante** all'utente (credenziale locale in RAM, mai
  trasmessa, purgata su `EXIT_NOTIFY`); le altre operazioni risolvono il
  chiamante per pid — credenziale locale **oppure** sessione autenticata
  (`SESS_AUTH` al server `console`, che risale i padri via `ps_info.parent`).
  `IDENTIS_WHOAMI` prende `w1` = pid (0 = il chiamante; un altro pid solo
  `System`): cosi' `Custos` risolve i ruoli del richiedente. `ADD_USER`
  richiede autorita' dell'identita' attestata (root per `root`). La
  **password non transita mai in chiaro**: il client hasha in locale
  (BLAKE2s-256) e manda solo l'hash. La transazione IPC e' **legata al
  canale**: un secondo client riceve `R_BUSY` e ritenta (non corrompe il
  buffer altrui). **Debiti dichiarati**: il bucket `identis:config` non e'
  ancora protetto in cardo, la provenienza del consenso (`usr:<nome>:consent`)
  porta un nome che il client dichiara (non decide nulla), manca `su`/cambio
  utente e il bucket home `usr:<nome>`.
- **Il login e' il gate (ADR-0062, D5)**: `userland/identis-login` prova 3
  volte e **la shell parte solo dopo l'autenticazione** (prima partiva
  comunque: login fallito = shell aperta). **D8f**: il login e' il
  **capo-sessione** — dopo l'autenticazione scrive il **nome** nella sessione 0
  (`SESS_AUTH_SET`), spawma il DE **senza ambiente**, aspetta la sua morte (= fine
  sessione), azzera l'identita' (`SESS_AUTH_SET` vuoto) ed esce 0. `init` lo supervisiona come un **getty**
  (`Service::Login = 13`, `OnDeath::LoginGate`): **uscita 0 = sessione
  conclusa → prompt subito** (contatore fallimenti azzerato); **uscita != 0 =
  accesso negato → backoff e tetto ai riavvii**. Il boot finisce quindi al
  **prompt** del login: il gate di regressione si chiude col timeout (le righe
  `PASS` sono il contratto). L'eco del backspace c'e' in **ogni** campo (nel
  campo username senza, si autenticava una stringa diversa da quella
  mostrata).
- **Diritti ereditari all'avvio (S3.4, ADR-0093)**: un figlio **non ha piu'
  diritti del padre**. `SpawnMeta.rights` (64 B, packed: floor di classe nei
  bit 0..8, maschera ops nei bit 8..40, `0` = eredita) e il PCB
  (`class_floor`, `ops_ceiling`): `create_user` applica
  `floor = max(padre, richiesto)` / `ops = padre & richiesta` e **rifiuta** se
  `floor <= class(immagine) <= classe_del_padre` non vale; `exec` **conserva**
  floor e ops (monotonia); `spawn_copy`/`create_thread` li copiano
  esplicitamente. `SYS_PEER_INFO` espone i diritti packed in `r10`.
  `cardo::policy::ceiling_for = policy & peer_info_ops` (fail-closed su `0`).
  La shell nativa lancia i programmi con floor `ThirdParty`: un `Untrusted`
  viene rifiutato dal **kernel**, un `exec` verso `System` non eleva.
  **Hardening**: `SYS_TRUST_IMAGE` e `SYS_SPAWN` per nome richiedono anche la
  classe `System` del chiamante (init degradato non riapre la trust table né
  spawana il TCB); `spawn_copy`/`create_thread` copiano `image_class` dal
  padre/leader. Test: `t77` nel gate (`usertests 72/72`: vie di creazione,
  bit riservati, `ceiling_for` con `rightshelper` `OPEN|READ`, fork floor/ops,
  gate strutturali) + `scripts/run-rights-tests.sh` fuori gate
  (`floorprobe` e `floorposix`: floor `ThirdParty`, `Untrusted` rifiutato,
  `ThirdParty` ammesso, `exec`/`spawn` di `System` negati). **Subtree**: cardo
  tiene `pid → subtree` e all'handshake risale i padri (`ps_info.parent`, bound
  16) per ereditare il **prefisso di path** (attestazione, non trasferimento);
  `R_RIGHTS_DROP` solo in restringimento. Test: `subtreehelper`+`subtreechild`
  in `t77` (padre restringe a `tmp`, il figlio legge dentro e non fuori).
  Trappole: **ogni** via di creazione (`create_user`/`exec_current`/
  `create_spawn_copy`/`create_thread`) va bonificata, o il default permissivo
  riapre la falla in silenzio; su cardo l'handshake con `ps_info` va in una
  funzione `#[inline(never)]` (firewall), o LTO sfonda lo stack di 16 KiB
  (`#PF` user-mode sotto la guardia).
- **Identita' di sessione per attestazione (S3.3c, ADR-0094)**: il **token
  bearer non esiste piu'**. I tag `0xAD`/`0xAE` sono **ritirati** (numeri non
  riusati); al loro posto `SESS_AUTH_SET` (0xAF: `w0` = len del nome nella
  meta' OUT, scrive l'utente sulla sessione **attaccata**, solo `System`,
  `len = 0` = deautentica) e `SESS_AUTH` (0xB7: `w0` = pid, `0` = la sessione
  attaccata → `w0` = 1/0, `w1` = len nella meta' IN, solo `System`).
  `littera::Session` ha `auth_user`/`auth_len`/`auth_by` (chi l'ha stabilita:
  alla sua morte l'identita' sparisce) e `purge_pid` la pulisce. **Due fonti,
  in ordine**: (1) credenziale locale in Identis (`VERIFY` riuscito lega il
  pid, mai trasmessa, purgata su `EXIT_NOTIFY`); (2) sessione autenticata
  (`SESS_AUTH`, che risale i padri) — la via normale: login scrive il nome
  sulla 0, il DE lo propaga alle tile, i comandi lo ereditano per
  attestazione. `IDENTIS_ADD_USER`/`LIST` perdono il token; `IDENTIS_WHOAMI`
  prende il **pid** in `w1` (0 = il chiamante, un altro pid solo `System`);
  `CUSTOS_GRANT`/`CONSENT` pure (Custos ricava il pid da `TXN_PID` e chiede i
  ruoli a Identis). `R_TOKEN_INVALID` → `R_NO_IDENTITY` (valore 2). Il gate
  resta 72/72 + 33/33 (negativi dentro `identistest` t2/t4/t10 e `t74`).
  **Trappole**: il fallback di **sessione** in Identis non e' opzionale (la
  shell non fa mai `VERIFY`: solo credenziale ⇒ `create-user` risponde
  `R_NO_IDENTITY`); in `impluvium::split` prima si legge la 0, poi ci si
  attacca alla tile e **poi** si scrive l'utente (invertendo finisce sulla
  tile sbagliata); `ensure_mailbox` ritorna l'**id della shm**, non un
  puntatore (usarlo come puntatore = `#PF` a `0x805` e il server muore:
  `sess_mb` e' il puntatore, `ensure_mailbox` serve solo a creare).
- **Credenziali riservate (ADR-0096)**: le credenziali sono il dato
  `sys:identis` (bucket `sys`, non piu' `ns:identis:config`). Scrittura:
  carve-out `identis_writer` (init o Identis, **anche con `RIGHTS_ALL`**).
  Lettura: `policy::read_allowed` in `cardo/src/server.rs` — `R_OPEN` del path
  e `R_OBJ_GET` di `(sys, identis)` solo Identis/init. Chi elenca usa
  `IDENTIS_LIST` (**pull a 4 byte**: `w0` = offset → `w1` = 4 byte di
  `nome:ruoli`, `R_DONE` 9 a fine elenco; mai l'hash): il push in N messaggi
  si perderebbe (coda 8 slot). `users` non apre piu' il bucket. Test:
  `identistest` 12/12 (t11 elenco senza hash, t12 lettura negata) +
  `foreign` bit8 (t57 → 511). Pattern riusabile da S3.2b.
- **Sessione a schermo intero (ADR-0098, ADR-0097)**: `littera` onora i
  parametri CSI (`ESC[nA/B/C/D`, `E/F`, `G/d`, `H/f`, `J`/`K` con **modo** di
  default 0, non conteggio), i modi privati (`?25h/l` cursore, `?1049h/l`
  **schermata alternativa senza scrollback**) e segnala il resize:
  `Session::resize` alza `resized`, `SESS_INFO.w1` bit 2 lo riporta e lo
  consuma, l'hint `SESS_INPUT_EVENT.w1 = 1` sveglia il lettore (mandato su
  modeset, `TILE_COMMIT` e `SESS_RESIZE`). `civis::sess::Screen` e'
  la base per un'app (alt screen, `at`, `clear`, `cursor`, `poll_resize`); il
  frame va **solo schermo** (`write_screen`), il mirror resta il transcript.
  **Il frame e' l'unita' di disegno (addendum ADR-0098)**: `at`/`write`/`clear`
  **accodano**, `present()` manda una `SESS_WRITE` (chunk 2048 B) = una feed, un
  render, un flush; `libs/tabula` (`Canvas` a celle con copia ombra) manda il
  **differenziale** (`diff()` = `CUP` per fetta + caratteri). REGOLA: mai un IPC
  per cella — un bordo disegnato cella per cella erano ~660 render+flush per
  tasto e il frame si vedeva arrivare a pezzi (il "lampeggio" di `fullscreen`;
  ora 1 IPC / ~33 B). Una cella = un carattere: la larghezza di un glifo la sa
  solo il font, non una tabella nel canvas. E il throttle **bulk** di
  `feed_session` (> 256 B = output di un comando, render differito) vale solo
  per il testo: un feed `screen_only` (`w1 = 1`) e' un frame e va reso subito
  (senza, il primo frame di un'app resta invisibile fino al primo tasto).
  **Alt screen: lo stato si salva *prima* di sostituirlo** — `enter_alt` legge
  cursore e `cursor_visible` dalla griglia vecchia (leggerli dopo il `replace`
  salva sempre `(0,0)`: all'uscita il prompt tornava in cima invece che sotto
  l'ultimo comando). **Il fondo di una cella ha una sola fonte, la bg layer**: anche fuori dalla
  banda (cursore, percorsi per-cella) si copia la cella dal **wallpaper** in
  coordinate schermo; il percorso legacy `pagina + layer 0` traslava l'origine
  della tile con `saturating_sub` e per una tile spostata (a destra o sotto
  l'inizio dell'immagine) **ripartiva dall'angolo della tile** (scia scura al
  posto del fondo). Prova d'invarianza: frame per differenziale == repaint da
  zero (`CONSOLE_REPAINT`).
  Demo `usr/bin/fullscreen` + `scripts/run-tui-tests.sh` (fuori dal gate: prova
  alt screen, tasti, toppa, modeset→riadattamento 127x36→102x34, ritorno al
  prompt) + **GTUI** `usr/bin/gtui` (log con i rami disegnati in pixel, barra,
  sparkline: le toppe restano nella cronologia e muoiono con le loro righe).
  Modello grafico (ADR-0097): la tile e' una **superficie di pixel**, il testo
  e' una vista a celle (`Geom::for_rect`), una cella contiene glifo **o**
  toppa; la libreria di disegno produce bitmap (una sola, `text` a due teste:
  cella vs font proporzionale); niente toolkit unificato.
  **Toppe (ADR-0099)**: `Segment` per riga che **vive quanto il suo testo**
  (nessuna eviction; tetto `PATCH_MAX_BYTES` 8 MiB **solo-rifiuto**
  all'ingresso), alt-text nel dump, `SESS_STATS.w1` le misura; render
  fondo → toppe → glifi con `blank_over_patch` (una cella vuota **non**
  ridipinge il fondo sopra la toppa); `Grid::resize` **conserva** le toppe se
  la **larghezza** non cambia (le righe non si spezzano) e le **scarta** se
  cambia (il re-wrap puo' spezzare una riga); l'app notificata **ridisegna**
  (`gtui`: `[gtui] resize WxH patches=N`, `N = 0` = non ridisegnato).
  **Coordinate**: il disegno passa dalla **vista** della tile
  (`Pixel::view` + `geom_local`, locali), il **danno** usa `geom` (schermo) —
  come `draw_cell_at`; sbagliarlo funziona solo nella tile in (0,0) e la
  guardia `[console] toppa fuori dalla tile` (una volta) lo dice. Il "terminale come app" resta una migrazione possibile
  perche' `littera` e' indipendente dal display.
- **Niente identita' in ambiente (S3.3d, ADR-0095)**: `USER` non esiste piu'.
  `identis-login` spawma il DE e `impluvium` spawma le shell **senza env**;
  il prompt chiede il nome a ogni riga (`civis::sess::auth()` →
  `SESS_AUTH(0)`), senza cache, con `forget_current` + un tentativo su errore
  (console ripartita). Costo: una IPC per prompt (CPU al prompt invariata). Il
  **nome** ora ha una sola fonte (il server): chi cambia l'identita' non deve
  inseguire copie. `su` come shell annidata **scartato** (ADR-0095
  §Alternative: consenso per azione, tile-sessione e classe dell'immagine
  coprono le tre esigenze); `runas` (identita' per albero + `exec`, niente
  shell) e' parcheggiato in ROADMAP con i suoi trigger.

## Important Notes

- **Non usare** `std` - solo `core` e `alloc`
- **Testare sempre** con QEMU prima di commit
- **Documentare** ogni decisione architetturale in ADR
- **Aggiornare** `ROADMAP.md` quando si chiudono o aggiungono fasi (stato +
  Pianificate/Parcheggiate); la storia dettagliata va in
  `docs/src/14-cronologia-fasi.md`, mai in questo file
- **Checklist di fine fase (docs anti-marcio)**: nello stesso commit della
  fase aggiornare `docs/src/11-testing.md` (gate corrente + riga test se la
  suite cresce), `docs/src/06-syscalls.md` (tabella numeri + wrapper se ci
  sono nuove syscall), `docs/src/SUMMARY.md` (se ADR/capitoli nuovi),
  `run-tests.sh` (commento gate), `ROADMAP.md` (riga stato) e
  `docs/src/14-cronologia-fasi.md` (voce di fase); i gate delle
  fasi passate restano snapshot storici (mai "corretti" al nuovo totale).
  Un solo gate corrente: `11-testing.md` + AGENTS Testing + `run-tests.sh`.
- **Crate consentite**: solo `no_std`-compatible
- **Kernel higher-half: FATTO (ADR-0020, Fase 27)** — kernel a `-2G+1M`
  (`0xFFFF_FFFF_8010_0000`, LMA 1M) + direct map `[0,64G)` a pagine 2M a
  `0xFFFF_8880_0000_0000`; `PML4[0] = 0` a runtime (NULL-deref faulta).
  Conversioni via `kernel/src/addr.rs` (`phys_to_virt`/`virt_to_phys`/
  `kern_*`); RSP0 e stack su VIRT alte. Dettagli e trappole in `04-memory.md`.
- **TSS per-processo** (ADR-0006): ogni processo ha il proprio TSS con I/O
  bitmap. I driver userspace dichiarano le proprie porte in `io_ranges`
  (`user_binary.rs::NAMED_BINARIES`); chi non ha range non tocca porte.
- **IPC per nome: registry + channel nel kernel** (ADR-0008, Fase 12): i peer
  non si indirizzano piu' per PID. Ogni processo parla su un **`Channel`**
  (coppia bidirezionale creata da `spawn` per i figli, o da `service_lookup`
  per i servizi registrati per nome con `service_register`). Il canale 0 = il
  parent. `civis` risolve `Fs`/`Console`/`Devfs` per nome. I messaggi viaggiano
  per channel_id; `reply` e' implicita al messaggio corrente (via `reply_chan`),
  mai per PID. La morte di un endpoint invalida i suoi canali e libera lo slot
  servizio → riavvio/riuso sicuri. Lo slot canale 0 NON si assegna mai (id 0 =
  sentinella `CHANNEL_PARENT`: assegnarlo faceva risolvere i messaggi al parent
  sbagliato).
- **Hardening (Fase 35, ADR-0026 + Fase 36, ADR-0027)**: cancelli per un avversario "programma
  locale malevolo" — (1) `kill` solo parent/init (i test di restart guidano il
  caos via `init_bounce`, non killano i server direttamente); (2) i servizi di
  sistema si registrano solo da figli di init (`Test` aperto per la suite);
  (3) `map_physical`/`map_in` solo frame del sistema (ring/scratch/VGA), mai
  RAM arbitraria; (4) `FS_REGISTER` solo prefix sotto `/dev/`, replace di un
  driver vivo solo da init-child o dallo STESSO binario (`SYS_PEER_PID` 46 per
  attribuire, `SYS_PEER_INFO` 47 per l'identita'). Strato 2 (Fase 36): hash
  FNV-1a nel PCB misurato allo spawn, manifest generato a build-time verificato
  da init pre-spawn; il kernel resta neutro (ADR-0025: POSIX e' personalità, non
  struttura). Il manifest esclude i binari che lo incorporano (rector/cardo:
  hash di sé = ciclo instabile, mai fixpoint). (6) **Identità crittografica e
  manifest come dato (ADR-0078, fase 1)**: `ImageId` = BLAKE2s-128
  (`syscall-numbers::ImageId`, una misura in `elf::load`, riusata da text
  table e PCB), `peer_info` a 128 bit (`rdi`+`rsi`); `FNV-1a` resta solo
  checksum ArcaFS. `sys:manifest` / `sys:policy` / `sys:test-policy` sono
  **dati** seedati da `arca-tool.sh` e letti a runtime (init fail-closed senza
  manifest, cardo dal proprio motore prima del READY). Corollario di build:
  `build_one` non riscrive `.bin` identici (temp fuori dalla dir) e
  `kernel/build.rs` elenca i singoli `.bin`, mai la dir `userland/build`
  (contiene `target/`) — senza, il kernel si ricompila comunque. (7) **Firma
  Ed25519 delle immagini (ADR-0078, fase 2)**: `libs/sig` (formato sidecar
  `[VSIG][key_id:8 LE][sig:64]`, dominio `VELORDO-IMAGE-V1\0`, verifica
  incrementale; `ed25519-compact` `no_std`), `tools/sign` host e
  `scripts/sign-bins.sh` agganciato a `seed_bin` (`sys`/`vela` → chiave
  `system`, `usr` → `thirdparty`). La firma e' sul **contenuto**: namespace
  `sig` indicizzato dal digest BLAKE2s-256 (`sig:<64 hex>`), una voce per
  immagine — copie in `sys`/`vela`/`usr`/`ns` condividono la firma, una copia
  manomessa no. ArcaFS resta v4 (il sidecar nel record e' la strada del v5).
  Anchor come dato `sys:anchor` (`key_id` ricalcolata dalla pubkey); verifica
  in cardo (`verify.rs`, cache su ImageId+key_id+sig) e `R_RIGHTS_EXECUTE`
  risponde `w0` = verdetto, **`w1` = `ImageClass`**. Firma obbligatoria in
  `sys`/`vela`/`usr`; altrove assente = `Untrusted`; sidecar malformato,
  chiave ignota o firma invalida = **sempre** negato (mai downgrade). `sig` e
  `vela` in `protected_bucket`. Chiavi di **test** in
  `tools/sign/keys/*-test-only.key` (override
  `VELORDO_SIGN_KEY_SYSTEM`/`_THIRDPARTY`); t66 nel gate (`usertests 63/63`).
  (8) **Classi di privilegio nel PCB (ADR-0079, fase 3)**: trust table nel
  kernel (32 voci) registrabile **solo** da cardo/init via
  `SYS_TRUST_IMAGE = 64`; il kernel **ricalcola** l'`ImageId` sui byte
  caricati allo spawn/exec e fa lookup → `image_class` nel PCB (default
  `Untrusted`; mai ereditata da exec). Cardo registra l'identità verificata in
  `R_RIGHTS_EXECUTE` (il path POSIX passa da `bucket="ns"` + key = path: il
  **VFS** detiene la verità sui path, `mandatory` vale anche per prefissi
  `bin/`/`usr/bin/`). `libr::exec`/shell verificano prima di eseguire
  (`libr::verify_exec`); init verifica prima di ogni `spawn_image` da disco
  (fail-closed). Gate strutturali su classe `System`: `service_register`
  (tranne `Test`), `dma_alloc` slot ≥ 1, `map_mmio`, `io_ranges`. Embedded e
  init sono `System` per costruzione. `peer_info` riporta la classe in `rdx`
  (`civis::peer_info_class`). **TOCTOU chiuso senza crypto nel kernel**: se i
  byte cambiano tra verifica e load l'id non combacia → `Untrusted` (t67).
  **Trappola**: il gate `service_register` richiede **lineage e** classe —
  usertests (figlio di init ma `tst` non firmato) è `Untrusted`.
  (9) **Porte I/O dal manifest (ADR-0079, fase 4)**: `sys:manifest` porta una
  terza colonna `<io>` (`-` o `start-end[,...]` esadecimale; tabella `IO` in
  `gen-service-hashes.sh`); cardo la parsa in `policy::manifest_io` e registra
  classe **e** porte insieme (`TrustIo` in `SYS_TRUST_IMAGE`). `create_user`
  prende i range dalla trust entry; **`SpawnMeta.io_ranges` è ignorato**
  (nessun chiamante si allarga le porte). `exec` riapplica le porte del nuovo
  contenuto. I binari **embedded** (`block`, ATA + PCI + BM: 6 range) tengono
  i range compilati — partono prima di cardo e la loro slice non è troncata a
  `TRUST_IO_MAX`. **`SYS_SPAWN` per nome è solo init** (prima chiunque poteva
  spawnare `block` e prendersi le porte ATA). t68: `ioprobe` firmato `system`
  con manifest `80-80` legge `0x80` (exit 0) e, dichiarando `0x2F8` in
  `SpawnMeta`, fa `#GP` (la dichiarazione è ignorata). **Trappola**: un
  binario di test fuori da `userland/build`/`flavours/posix/build` non entra
  nel manifest generato — la riga va aggiunta da `arca-tool.sh`
  (`manifest-io.txt`); init ignora la terza colonna, cardo la legge.
  Limiti: porte embedded compilate, revoca a caldo assente, conf PCI senza
  IOMMU (contenimento nullo, ADR-0026); file fuori dal namespace (FAT)
  restano `Untrusted`. **Trappola**: la `key_id`
  e' BE (`%016x`) in `sys:anchor` e LE nel sidecar — il parser usa
  `from_be_bytes` lato anchor. (5) **Bucket di fiducia
  protetti (ADR-0077, fase 0)**: `policy::write_allowed` in cardo, dopo il
  tetto e il subtree — admin ops (snapshot) solo con `ceiling == RIGHTS_ALL`;
  bucket `sys`/`usr`/`tst`/`ns`/`sig`/`vela` e path `bin/`, `usr/bin/`,
  `test/` solo con
  `RIGHTS_ALL` o init; carve-out per owner di servizio (`identis:` → Identis,
  bucket `log` → Vestigia), classificato all'handshake e purgato su
  `EXIT_NOTIFY`. Senza, il default `0x19F` (che include WRITE) lasciava a
  chiunque la riscrittura di `sys:ca.der` e dei binari. Catena
  identità/privilegi: ADR-0078 (fasi 1–2 fatte: digest + manifest come dato +
  firma Ed25519, verifica in cardo), ADR-0079 (fasi 3–4 fatte: classi
  System/ThirdParty/Untrusted nel PCB via `SYS_TRUST_IMAGE`, porte I/O dal
  manifest, gate strutturali, `SYS_SPAWN` solo init, t67/t68), ADR-0080
  (fase 5 fatta: `Custos` = `Service::Custos = 18`, unico writer di
  **`sys:caps`** — deviazione dichiarata da `sys:policy`, che resta la
  tabella ops di cardo; cardo legge `sys:caps` direttamente con il carve-out
  `caps_writer` = solo Custos/init **anche con `RIGHTS_ALL`**; ruoli multipli
  `nome:hash:ruolo[,ruolo...]` + `IDENTIS_WHOAMI` live; `net` fa bind delle
  porte note via `CUSTOS_QUERY` con cache per (identità, porta) e
  fail-closed; t69). **Fase 6 fatta**: `NetBroker` = `Service::NetBroker = 19`
  (`SERVICE_COUNT = 20`) con `BROKER_OPEN/TX/CLOSE/STATS` (`0x90`–`0x93`,
  client `civis::broker`, slot in `net-ep`), allowlist di porte + rate limit
  per identità + audit in Vestigia + uscita IPv4 via `siit`; **tetto
  strutturale per classe**: `net` (`NET_EP_OPEN`) e `siit` (`SIIT_EP_OPEN`)
  negano ogni endpoint a `Untrusted` (`peer_info_class`) — nessuna tabella
  può riaprirlo, il broker è l'unica strada. **Consenso per immagine**:
  `CUSTOS_AUTHORIZE=0x83` (`policy admin ∩ consenso`), `CUSTOS_CONSENT=0x84`
  (scrittura solo con identita' attestata), `usr:consent` (tabella autorevole,
  carve-out `consent_writer` in cardo) + `usr:<nome>:consent` (provenienza);
  shell nativa `consent <prog> on|off`; t70, `usertests 66/66`. I probe
  che parlano con `net` in diretta (`nettcp`, `tlsprobe`, `posixsock`) sono
  `usr`/`ThirdParty` (provano il trasporto, non la policy); `netudp`/`net3`/
  `net4` restano `tst`/`Untrusted` e passano **solo** dal broker.
- **IPC reply implicita**: la reply del server va al peer del canale del
  messaggio correntemente elaborato (fissato da `recv` in `reply_chan`), non
  all'ultimo `send`. Piu' client concorrenti su un server sono quindi
  supportati (fix 9.2.2 generalizzato ai canali). Un request-id esplicito lato
  server / `reply_to` e' rimandato: la Fase 13 (async) usa un request-id come
  campo interno del messaggio, senza toccare l'ABI dei registri.
- **IPC asincrono (Fase 13, ADR-0009)**: `send_async` (33) NON blocca e
  ritorna il `req_id` (>= 1); `recv_nonblock` (34) non blocca. Il `req_id` e'
  un campo INTERNO di `PendingMsg` (signed: >= 0 richiesta, < 0 risposta a
  `-req_id`), assegnato dal mittente via `req_next`. La reply del server resta
  implicita: il kernel alla `reply` guarda il target — `BlockedOnReply` →
  `reply_slot` (sync); altrimenti accoda una risposta con `req_id = -reply_req`
  (async). Trasparente a cardo/console/devfs. Vincoli primo passo: no mix
  sync/async in volo per processo; risposte FIFO (`wait_reply` non riordina);
  FS async = 1 op in volo (guard `FS_PENDING`: il formato frame del ring non ha
  lunghezza payload esplicita); reply async persa se la msg_queue del target
  (8 slot) e' piena (log nel kernel).
- **Process lifecycle (Fase 14, ADR-0010, implementata)**: exit/kill kernel-side
  in DUE tempi — (1) "morte logica" immediata (`Scheduler::terminate`): stato
  `Terminated`, release di CBS/servizi/canali, risveglio dei peer bloccati in
  `send` sincrono verso il morto (`waiting_pid`), cascata sulla discendenza,
  accodamento al reclaim; (2) teardown fisico differito (`drain_reclaim` a inizio
  `on_tick`): stack kernel, slot TSS, address space user (foglie PTE `owned`,
  vedi sotto) → poi notifica `EXIT_NOTIFY` a TUTTI i peer e PID nel free-set.
  Notifica DOPO il teardown: nei loop spawn/exit il pool non si esaurisce. Riuso: PID
  (`MAX_PIDS` = 128 concorrenti, processi + thread 1:1 nello stesso pool),
  slot TSS, canali (`None`) e server CBS (`None`).
  `kill(pid, code)` (35): killabile qualunque processo user tranne init/kernel/
  self. Morte di init → panic documentato. `usertestcli` ha i modi CHURN/KILLME/
   SRVDIE/SYNCWAIT/MNTDIE/OPENDIE/MAPHAMMER/FLOOD; suite 21/21 → 31/31 (t22 churn riuso+leak, t23
   kill+notifica, t24 notifica unificata async+sync, t25 morte driver +
   re-registrazione, t26 morte client senza close + smoke, t27 init-restart devfs,
   t28 restart cardo end-to-end, t29 map-flap isolation, t30 fairness sotto flood).
  **Notifica unificata (14.10)**: DOPO il
  teardown il kernel notifica TUTTI i peer (non solo il parent), ciascuno sul
  canale che li collegava (`die_peers` nel PCB, max 31); `wait_reply` ritorna
  `WaitReplyError::ServerDied{pid,code}` (mai attesa infinita), `fs_collect`
  filtra per canale (`wait_reply_chan`), `drain_stray` scarta senza reply.
  Semantica "UN peer e' morto" (notifiche stale filtrate per pid/canale);
  `Service::Test` = slot usa-e-getta per t24. Retry/init-restart rimandati.
- **Address space teardown e bit "owned" (Fase 14)**: le PTE delle pagine user
  hanno il bit AVL `0x200` (owned) se di proprieta' del processo (code copiato,
  stack user, ring, heap demand-zero). `map_physical`/`map_in` NON lo settano
  (pagine iniettate: VGA, ring di altri processi, scratch). `teardown_user_space`
  libera solo le foglie owned e le page-table private (entry PML4 diverse da
  quelle del kernel), mai i frame altrui.
- **Ring SPSC per-processo, niente piu' buffer FS** (Fase 10.2, sostituisce
  9.6): ogni processo alloca DUE pagine ring (syscall **`sys_ring_alloc` (26)**,
  che riusa il numero del vecchio `fs_buf_alloc`) mappate a `USER_FS_BUFFER`
  (request) e `USER_RESP_RING` (response), e le registra presso cardo con una
  IPC register-only (`FS_BUF_REG`). Ogni operazione FS = 1 frame nel request
  ring `[tag:4][w0:8][w1:8][payload]` + `send(FS_NOTIFY)`; cardo consuma
  SEMPRE l'intero frame (header + payload) e scrive 1 response frame
  `[result:8][w1:8][payload]` — **ECCEZIONE: per i WRITE remoti cardo NON
  consuma il frame** (dedicato `handle_write_remote`): il payload resta nel
  request ring e il driver (console/devfs) lo legge direttamente (mappato con
  `map_in` (27), mapper generico cross-process) avanzando la tail lui stesso.
  Per i READ remoti il driver scrive il response frame nella response ring del
  client (zero copie in ogni percorso). Ring a pagina singola: dati
  `[0x0000..0xFF8)` = 4088 B, head a `0xFF8`, tail a `0xFFC`; capacity reale
  4087 B (free = CAP-1) → civis splitta read/write > ~4000 B in piu' round
  trip. Il kernel NON e' nel percorso dati; slot (`fs_slots`) e syscall FS
  kernel-side (3-7, 23, 24) rimossi.
- **Registrazione driver via ring**: devfs/console si registrano con
  `FS_REGISTER` (0x30) scrivendo un frame `R_REGISTER` nel proprio request
  ring (NON il tag FS_NOTIFY); cardo legge il prefix dalla request ring del
  driver (primo elemento della coppia `(req, resp)` registrata — attenzione a
  non confonderlo col response ring).
- **Memoria condivisa: owner + destroy (Fase 30.1, ADR-0073)** — `shm_create`
  registra l'owner e prende il riferimento iniziale (`refs = 1`);
  `shm_destroy` (63) e' owner-only: marca `destroyed` (i `shm_map` successivi
  falliscono) e rilascia il ref owner; i frame restano vivi finche' l'ultimo
  `munmap`/teardown non scende a 0. Il teardown rilascia i ref owner del
  processo morto: creazione senza `shm_map` non e' piu' un leak. `net`/`dns`/
  `siit` `free_endpoint` lo chiamano; i client non distruggono (solo
  `munmap`).
- **Reattivita' shell**: a valle del boot i soli processi `Normal` sono i
  servizi interattivi (console, fs, devfs, shell), tutti bloccati in attesa IPC;
  init resta bloccato in `recv`, `useruptime` e' `Low`. Quantum scheduler = 2
  tick (20 ms). Ordine spawn di init: log+disk in parallelo, poi fs/time,
  flush al log, gpu (registra `Gpu`, kbd la raggiunge per nome), uptime/
  devfs, quindi i test in SEQUENZA (ognuno atteso
  fino a `TEST_DONE` sul canale di nascita), usershell per ultimo (interattivo).
- **I test girano in sequenza, la shell e' ultima**: usertestfs/usertestfat/
  testsarca/posixtests/usertests condividono la ramfs di cardo (path e file di
  lavoro) e l'output seriale; la sequenza rende PID e risultati deterministici.
  `posixtests` (`flavours/posix/tests`) precede `usertests` (t54 prima dei drop
  di diritti di t34). Con il buffer
  per-processo (9.6) la race della vecchia shared buffer e' eliminata (la suite
  t15 churn devfs concorrente gira davvero in parallelo). init spawa i test uno
  alla volta e attende il `TEST_DONE` (canale 0x7E) da ciascuno sul canale di
  nascita prima dello spawn successivo.
- **Processi Low e server idle**: i server Normal (fs/shell) in coda di `recv`
  vuota e senza altri runnable diventano `Ready` in tight-loop (fix anti-
  deadlock); una fascia `Low` non e' quindi schedulabile finche' girano. Per
  questo il test di priorita' e' **High vs Normal** (Low usato solo da
  `useruptime` in boot reale).
- **Robustezza scheduler (fix pre-esistenti)**: (1) i wait in userland NON fanno
  busy-loop su syscall (`get_ticks`) — che maschera gli interrupt (IF=0) e
  affama il timer — e le attese **idle** (prompt di input) dormono con
  `futex_wait` a deadline di 1 tick (ADR-0067: processo `Blocked`, PIT attivo,
  CPU ~0%); restano spin puri IF=1 solo i bound brevi e certi (`spin_esc` delle
  continuazioni ESC, `wait_fg`/`cancel_job` durante un comando foreground;
  `usertestspin`/`utcbstest` fanno batch da 512 spin puri tra due
  `get_ticks`); (2) `ipc_recv` controlla
  la coda e marca `Blocked` sotto lo STESSO lock (chiusa la race check-then-
  block / lost-wakeup); (3) i wait da IRQ (kbd) usano `pending_wake`: `wake`
  imposta il flag se il processo non e' ancora bloccato, `block_current` lo
  consuma e non si blocca; `block_current` ripristina `Ready` se non c'e'
  nessun altro runnable.
- **Read: solo i byte restituiti sono significativi**: ogni client ha una
  pagina FS propria (mai riusata da altri, Fase 9.6) → niente residui di slot/
  buffer condivisi che finivano nelle risposte di altri device (il vecchio
  bug `/dev/zero`).
- **Heap on-demand in civis** (single allocator): niente piu' `static [u8; N]`
  nei binari user. `civis/src/heap.rs` e' l'UNICO allocatore (free-list first-fit
  con split+coalescenza) ed espone `#[global_allocator]`; i crate user che
  alloccano non definiscono allocatori propri. L'heap parte vuoto a
  `USER_HEAP_BASE` (= `USER_STACK_TOP`) e cresce via la syscall **`sbrk` (25)**,
  che riserva solo VA (`heap_brk`): le pagine vengono materializzate **lazy** dal
  page-fault handler (demand-zero, come brk/mmap di Linux). Binari
  sensibilmente piu' piccoli (es. cardo 132→66 KiB).
- **Kernel heap riservato nel frame allocator**: la regione di 4 MiB del kernel
  heap deve essere marcata `used` nel bitmap fisico (`phys_mem::reserve` in
  `main.rs`). Senza questa riserva i frame della regione finivano ai processi e
  venivano sovrascritti → corruzione della free-list del kernel heap (alloc
  falliti o hang; il fix e' alla radice dei fallimenti dell'heap lazy).

## Testing

```bash
# Tile e DE (D8e, FUORI dal gate: il layout lo dispone solo >= ThirdParty)
./scripts/run-desktop-tests.sh   # tileprobe + keybinding del DE via monitor

# Monitor di sistema (M4, ADR-0100, FUORI dal gate: prende la tela della sessione)
./scripts/run-monitor-tests.sh   # 1 Hz senza tasti, pausa, sort, selezione, pixel, uscita

# Unit test host dei crate condivisi (FUORI dal gate: millisecondi, no boot)
./scripts/run-unit-tests.sh   # term, musivum (D8a), littera + sess-proto (D8c1), pictura + tabula (toppe e canvas a celle), formae (componenti: chart/barra/history/fmt, ADR-0100), net (IPv6/NDP/TCP/SIIT/DNS + framing RFC 7858), net-ep, broker-proto, valloc, syscall-numbers, impluvium-tree, blake2s, sig, tls: 238 test

# Test in QEMU (output seriale, esce con Ctrl-C)
./run.sh

# Verifica build
cargo build --release

# Build dei binari user (test suite inclusa)
./scripts/build-userland.sh && ./scripts/build-tests.sh

# Test selftest (feature flag)
cargo build --release --features selftest

# Verifica la mappa di memoria dinamica a diverse dimensioni RAM
# (QEMU -m 4G/16G/32G: la RAM sale sopra 4 GiB per il PCI hole)
timeout 6 qemu-system-x86_64 -m 4G -display none -serial stdio -no-reboot \
  -kernel target/x86_64-unknown-none/release/velord

# Produzione (default): niente test, shell subito usabile
timeout 60 ./run.sh > /tmp/boot.log

# Bench throughput su KVM (Fase 23, mai nel gate): 3 run di riferimento
./scripts/bench.sh > /tmp/bench.log
rg '\[bench\]' /tmp/bench-run1.log /tmp/bench-run2.log /tmp/bench-run3.log

# Gate di regressione (un solo boot, ADR-0081): 6 righe di suite + shell
# nativa, ZERO FAIL/PANIC/FAULT. Il prompt del login e' il segnale di fine
# suite; da li' il gate si attacca alla stessa QEMU e gira i 33 check della
# shell (`[shell-native] PASS 33/33`). Il gate stampa `[gate] PASS/FAIL` ed
# esce != 0: niente piu' timeout esterno che chiude il boot.
# (ADR-0059: la suite FAT32 e' fuori dal gate -> scripts/run-fat-tests.sh)
#   [testfs] PASS 5/5
#   [testsarca] PASS 64/64
#   [posixtests] PASS 4/4
#   [identistest] PASS 12/12
#   [usertests] PASS 72/72
#   [threadtest] PASS 12/12
#   [shell-native] PASS 33/33
# KVM=1 accelera (-enable-kvm -cpu host, opt-in; senza /dev/kvm resta TCG).
# Valvole: GATE_SHELL=0 (solo suite), GATE_TIMEOUT (600), SHELL_TIMEOUT (900).
timeout 1800 ./run-tests.sh > /tmp/boot.log 2>&1
rg '\[gate\] PASS' /tmp/boot.log
test "$(rg -c 'FAIL|PANIC|#.* FAULT' /tmp/boot.log)" = "0"
```

## Documentation

- Ogni ADR deve essere nel formato `docs/src/adr/NNNN-title.md` (source unica:
  il libro mdbook li legge da li'; nessuna copia altrove)
- Ogni nuovo componente deve avere documentazione in `docs/src/`
- Aggiornare il SUMMARY.md quando si aggiungono nuovi capitoli
- Usare mdbook per la documentazione pubblica

## Common Pitfalls

1. **Dimenticare EOI**: dopo ogni interrupt, mandare End of Interrupt al PIC
2. **Deadlock con spin locks**: un interrupt handler non può prendere un lock già preso
3. **Dimenticare volatile**: gli accessi MMIO devono essere volatile
4. **Stack alignment**: x86_64 richiede 16-byte alignment per SSE
5. **Busy waiting**: usare `hlt` invece di `loop {}` negli idle loop
6. **Stack cardo 16 KiB + LTO**: con `lto` (cardo) l'inlining fonde i frame
   del loop con le catene chiamate (btree/volume/seed/FAT) e sfonda la
   guardia (#PF user-mode). Regola: buffer grandi in `Box`, handler/seed e i
   livelli btree/volume `#[inline(never)]` (firewall). Vale per chi tocca
   `userland/fs` e `arcafs::btree`.
7. **DMA e cache settoriale (block)**: i path DMA di `DISK_READ`/`DISK_WRITE`
   devono fare fill della cache dopo `finish_dma`, o le letture raw cached
   (`DEV_READ`→`node_read`) servono dati stale mentre le scritture su disco
   sono fresche (fix 56.2c). Il PIO (`node_read_multi`/`node_write_multi`) lo
   fa già; i due path devono restare coerenti.
8. **Seriale: `send` vs `send_raw`**: il driver `uart_16550` espande il
   backspace (0x08/0x7F) in "BS SP BS" — comportamento da **console**.
   Sul percorso **raw** (`serial::_write_bytes`, da cui passa `sys_write`)
   il byte deve arrivare sul filo identico ("byte in = byte sul filo" = audit):
   lì si usa `send_raw` (D5/ADR-0062). Con `send()` la sequenza di
   cancellazione (3 byte) arrivava come 7 e il mirror seriale non combaciava
   col terminale.
9. **Attese di input in userland: eventi, non polling (E0/ADR-0082, ADR-0067)**:
   i lettori della tastiera (`interpres`, `identis-login`, `create-user`) si
   bloccano in `civis::input::wait()` (un `recv` sull'`INPUT_NOTIFY` di
   `porta`), mai in `spin_loop()` ne' in un `futex_wait` a 1 tick. Se
   `civis::input::read` ritorna 0 si chiama `wait`; il notify e' un **hint**,
   dopo ogni risveglio si **drena** il device finche' non ritorna 0 (mai
   mettere i byte nel notify: la coda async e' 8 slot). Restano a tick solo
   le attese senza un produttore che notifica (es. retry di servizio, attach
   durante un riavvio); lo spin puro solo per bound di microsecondi
   (continuazioni ESC) e comandi foreground (`wait_fg`/`cancel_job`). La fase
   shell `native` campiona la CPU di QEMU: prompt principale e `create-user`
   sotto il **5%** (un busy-wait e' ~100%; `base` resta a 50%).
10. **SSE2 userland e `mem*` (ADR-0071)**: lo userland compila con
    `targets/x86_64-unknown-velordo-user.json` (SSE2), il kernel resta
    `x86_64-unknown-none` soft-float. Con SSE2 LLVM **non** espande più
    `memcpy`/`memmove`/`memset`/`memcmp` inline: emette libcall, e i simboli
    sono nostri in `libs/civis/src/rt.rs` (bulk `rep movsb`/`rep stosb`,
    taglie piccole `movdqu`). Non abilitare
    `-Zbuild-std-features=compiler-builtins-mem`: la sua versione
    (`rep movsb`+`rep movsq`+`rep movsb`) è +46% su `testsarca` append 512 KiB
    sotto QEMU TCG. Conseguenza: **ogni crate user deve linkare `civis`**
    (senza, link fallisce con `undefined symbol: memcpy`). Se si aggiunge un
    crate che non linka `civis`, o si aggiunge la dipendenza o si forniscono
    i simboli. I target JSON sono pinnati: `-Zjson-target-spec` +
    `[unstable] json-target-spec = true` nei `.cargo/config.toml`.
    **Allineamento RSP**: con SSE2 ogni `movaps` su uno slot 16-allineato
    assume l'ABI SysV (RSP ≡ 8 mod 16 all'ingresso). I tre punti che calcolano
    l'RSP iniziale sono stati tutti corretti (thread: `(stack & !0xF) - 8`;
    `layout_argv`: `arr` arrotondato a 16 con `argc` dispari; main:
    `USER_STACK_TOP - 24`): se ne aggiungi un quarto, verifica la parità o lo
    userland muore con `#GP` code 139 — che **non** è `FAIL`/`PANIC`/`FAULT`,
    quindi il gate lo vede solo come riga `PASS` mancante.
