# ADR-0060: Console virtuale — il driver `gpu` e il servizio `console`

## Status

Accepted. Implementata (D3, 21 ottobre 2026). Dipende da ADR-0056 (L0a),
ADR-0057 (L0b), ADR-0058/0059 (il FAT esce dal boot).

## Context

Dopo L0b il servizio `gpu` faceva **due** cose: driver del device
`virtio-gpu` *e* console (VGA testuale, banner, editing, `/dev/console`).
Il video era quindi fermo al livello "il driver parla col device", e la
console non poteva crescere verso un desktop senza crescere dentro il driver.

Nel frattempo il percorso di reale è cambiato due volte:

- il **FAT è uscito dal boot** (ADR-0059): i servizi vivono come oggetti
  ArcaFS e il root è un volume solo → il font TTF non può stare in un
 'immagine FAT, deve essere un **oggetto del volume root**;
- l'utente ha chiesto un **layer intermedio** ("in futuro avremo un desktop
  environment") con tre ruoli distinti: il driver, la console, la shell.

Il vincolo tecnico che condiziona tutto: **virtio-gpu scrive nei pixel del
guest**, e il device richiede un **buffer fisicamente contiguo**
(`virtio_gpu_create_mapping_iov()` traduce l'indirizzo di
`ATTACH_BACKING` nello address space PCI e lo assume contiguo). Quindi il
framebuffer non può stare nel normale heap user: serve memoria device
(contigua, fisica, con la VA in mano per disegnare).

## Decision

### 1. Layering: driver → console → shell

```
Shell (POSIX/nativa) ──testo──► /dev/console ──► console (servizio)
                                                    │ modello a celle, UTF-8
                                                    │ font TTF, surface
                                                    ▼ /dev/display
                                                  gpu (driver): virtio-gpu
```

- **`gpu` = driver puro**: enumera `virtio-gpu-pci`, programma BAR/MSI-X,
  crea la risorsa 2D, la aggancia, imposta lo scanout, e pubblica
  **`/dev/display`**. Non sa cosa sono caratteri, celle o font: non
  disegna nulla (il codice di testo che stava in `gpu` **non è stato
  spostato**: è stato *estratto* in una libreria).
- **`console` = console server**: modello a celle, decoder UTF-8, font
  rasterizzato a runtime, le surface, e il device di testo `/dev/console`.
- **`libs/term`** (nuovo crate, no_std): `term::disp` (il protocollo
  display) + `term::{grid,utf8,font,ttf,raster,surface}` (il terminale).
  Il driver linka **solo** `term::disp`; il rasterizer non entra nel suo
  binario. `term` non conosce il FS: l'I/O del font è un trait `Source`
  fornito da chi lo usa.

### 2. Il protocollo display: IPC diretto col driver

Il driver e la console **si parlano in diretta** (IPC per nome, `Service::Gpu`),
non via un device FS. La prima stesura prevedeva `/dev/display` (un device
come `/dev/console`): e' stata abbandonata perche' il cardo e' single-threaded
e un driver che lo blocca (per fare una scrittura) mentre serve un cliente si
**deadlocka` — e la console serve `/dev/console` *e* parla col driver. I ring
client e server vivono inoltre nella stessa finestra `CLI_*`: un processo che
e' client e server insieme li sovrascrive. Il canale diretto evita entrambi i
problemi.

| Tag | Richiesta | Reply |
|---|---|---|
| `GPU_DISPLAY_QUERY` (0x75) | — | `w0` = backend, `w1` = geometria impacchettata |
| `GPU_DISPLAY_PRESENT` (0x76) | `w0` = fisico del buffer, `w1` = geometria | `w0` = 0/-1 |
| `GPU_DISPLAY_FLUSH` (0x77) | — | `w0` = 0/-1 |

- `backend`: `VgaText` (nessun device pixel) o `VirtioGpu`.
- La geometria sta in un registro 64 bit (`width | height<<16 | stride<<32 |
  format<<48`): i registri IPC sono quattro, i payload passerebbero per i
  ring (che sono condivisi).
- `PRESENT`: il driver crea la risorsa 2D (BGRA), le aggancia al buffer
  **del client** e la imposta come scanout. In QEMU `SET_SCANOUT` fa
  `qemu_console_resize`: **il guest sceglie la risoluzione** e la finestra
  host la segue.
- `FLUSH`: nel path non-virgl di QEMU il `RESOURCE_FLUSH` **non copia i
  pixel**: la copia la fa `RESOURCE_TRANSFER_TO_HOST_2D` (dal buffer del
  guest alla risorsa) e il flush dice solo alla superficie di rinfrescarsi.
  Il driver fa quindi `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`, in quest'ordine,
  a ogni refresh: senza, lo schermo resta nero.

### 3. I pixel sono della console

La console alloca il buffer device e lo renderizza; il driver lo *presenta*.
Quindi:

- nessuna mappatura cross-process (niente nuova classe di frame nel kernel);
- il driver non scrive mai nei pixel;
- la console è l'unica a scrivere lo schermo → è già il modello di un
  compositor (il DE di domani cresce qui, non nel driver).

### 4. Memoria device: DMA a N slot

`SYS_DMA_ALLOC` (49) prende un secondo argomento, lo **slot**:

```
dma_alloc(pages, slot) -> rax = phys | rdi = pagine | rsi = VA dello slot
```

- **slot 0** = staging (path storico di `block`): ≤ `DMA_PAGES_MAX` (4)
  pagine, VA `USER_DMA_VA` invariata, aperto a tutti come prima;
- **slot ≥ 1** = *device buffer* (il framebuffer): ≤
  `DMA_DEVICE_PAGES_MAX` (1024 pagine = 4 MiB) e **grant riservato all'owner
  di `Service::Console`** — come il grant MMIO di ADR-0057: alzare il bound
  a 4 MiB per qualunque processo sarebbe una regressione di hardening
  (ADR-0026). `DMA_SLOT_ANY` prende il primo slot libero.
- N slot per address space (`DMA_SLOTS = 8`), record come i ring: free a
  teardown/exec, mai ereditato dal figlio-copia.
- L'area VA (`USER_DMA_BASE`, 4 MiB per slot) sta dopo le finestre piccole e
  dopo quella MMIO.

### 5. Il font: TrueType a runtime

- **Maple Mono NF** (SIL OFL 1.1, attribuzione in `third_party/`), versione
  "Regular": advance 0.6 em **uniforme** (anche per le icone Nerd), em box
  1.32 em, 3499 glifi PUA raggiungibili **solo** dalla subtable `cmap`
  **formato 12** → il rasterizer implementa formato 4 *e* 12.
- Il file sta nel volume root come oggetto (`ns:font.ttf` per le letture a
  range dalla console, `sys:font.ttf` per l'oggetto) — non nel binario, non
  nel FAT (che non esiste più).
- **Il font e' letto tutto in RAM all'avvio** (2.4 MiB), non a pezzi durante
  il servizio: la console non puo' fare I/O sul FS mentre serve
  `/dev/console` (il cardo la aspetta: una lettura FS li' dentro
  deadlockerebbe — console e cardo si bloccherebbero a vicenda). Con il font
  in RAM il rasterizer non tocca piu' il FS e le icone Nerd (fuori dal range
  ASCII) restano disponibili.
- **`libs/term::ttf`**: parser sfnt (`head`, `maxp`, `hhea`/`hmtx`, `cmap`
  4/12, `loca`, `glyf` semplici e composti).
- **`libs/term::raster`**: rasterizzazione con antialiasing in **punto
  fisso** (16.16, nessun float, nessuna libm): 4 sub-scanline per riga,
  copertura orizzontale frazionaria. Il rendering è **deterministico** (stesso
  byte a ogni boot).
- **Dimensione fissa 16 px**: la cella deriva dalle metriche del font
  (advance 9.6 → 10 px, em box 21.1 → 22 px) e la **griglia** dalla
  risoluzione: `cols = W / cell_w`, `rows = H / cell_h` → **128×36** a
  1280×800, 64×21 a 640×480. Cambiando risoluzione la griglia si
  ricalcola da sola, il font resta com'è.
- **UTF-8**: la cella è un **codepoint** (`u32`), non un byte: le icone Nerd
  (PUA, 3 byte UTF-8) si disegnano nel framebuffer. Nella surface VGA
  testuale un codepoint > 0xFF diventa `?`.

### 6. Le surface: un modello, due backend

| Surface | Quando | Come disegna |
|---|---|---|
| `Pixel` | c'è un device virtio-gpu | il glifo TTF rasterizzato, blending AA sul BGRA |
| `VgaText` | niente device pixel (o font illeggibile, o buffer contiguo non ottenibile) | char+attr nel buffer testo 0xB8000 (80×25, font hardware) |

La surface VGA testuale è il **fallback**: è il comportamento di sempre, e
non costa nulla (il codice esistente della VGA, con il cursore CRTC, vive
nella console). **Non** si è scelto il VGA *grafico*: servirebbe un
framebuffer lineare sul VGA, cioè un trampolino real-mode + VBE LFB (o
l'hack dei registri 640×480×32): un progetto a parte, per un path che
serve solo quando virtio-gpu non c'è.

### 7. Il modello: celle, dirty, cursore

- La griglia (80×25 o 128×36…) è **il modello**: identico per le due
  surface. Ogni mutazione (put, scroll, clear) segna la zona sporca: le
  surface ridisegnano solo quella.
- Il parser ESC (editing: ←, →, cancella-fino-a-fine-riga) e il
  form-feed sono nel modello, non nella surface.
- Nel path pixel il cursore è **software** (sottolineatura disegnata dalla
  surface); nel path VGA resta quello **hardware** (porte CRTC, che la
  console dichiara nei suoi `io_ranges`).

## Consequences

- **`/dev/console` cambia di padrone**: lo serve `console`, non `gpu`. Il
  contratto del device (tag `DEV_*`, ring) non cambia: `porta` (l'eco),
  `identis-login` e i programmi scrivono come prima. La supervisione in
  `rector` ha una entry per `Service::Console` (nuovo, id 12).
- **La finestra VGA resta vuota** nel path pixel: il testo va nella
  finestra del device virtio-gpu (il display "vero"). Chi guardava la VGA
  per il prompt del login ora guarda l'altra finestra. La VGA resta il
  fallback (e il banner del kernel non ci scrive comunque: il kernel logga
  solo su seriale).
- **La shell POSIX scrive ancora su seriale**: il terminale grafico mostra
  login ed eco, non la sessione. Il "console server" che replica
  `/dev/console` sul seriale, e il routing POSIX stdout→device (l'hook
  `persona`), sono un lavoro a parte (rinviato): oggi ogni client che vuole
  il transcript seriale duplica a mano, come prima.
- **La syscall 49 cambia ABI** (arg2 = slot): `block` continua a usare lo
  slot 0 e non cambia; i chiamanti nuovi usano `dma_alloc_slot`.
- **Il grant del device buffer** è chiuso (owner `Service::Console`): un
  processo qualsiasi non può monopolizzare 4 MiB contigui.
- **Il rendering è verificabile**: la console logga il BLAKE2s-256
  dell'**intero framebuffer** (deterministico: il rasterizer è in punto
  fisso). Il confronto col host (stesso crate, stessa sequenza) dà la stessa
  immagine **pixel per pixel** (verifica cellula per cellula: la cella del
  banner combacia riga per riga).
- Il font è **ridistribuito** nel repo (`third_party/`, OFL 1.1):
  l'attribuzione è dovuta e va mantenuta.

## Relations

- **ADR-0005** (microkernel): il driver e la console sono due servizi; il
  kernel fa solo routing e i grant.
- **ADR-0006** (TSS per-processo): `gpu` ha il conf PCI (`0xCF8-0xCFF`),
  `console` il cursore CRTC (`0x3D4-0x3D5`).
- **ADR-0025/0041** (meccanismo/personalità): `libs/term` è meccanismo
  neutro (nessun POSIX); il routing stdout→device resta alla personalità.
- **ADR-0026** (hardening): grant del device buffer, limiti dichiarati.
- **ADR-0057** (grant MMIO): lo stesso modello di grant chiuso, applicato
  alla memoria device.
- **ADR-0059** (FAT fuori dal boot): da qui il font come oggetto ArcaFS.

## Implementazione

- `syscall-numbers`: `SYS_DMA_ALLOC` (arg2 = slot), `DMA_SLOT_ANY`,
  `DMA_SLOT_DEVICE`, `DMA_DEVICE_PAGES_MAX`, `DMA_SLOTS`, `DMA_SLOT_SPAN`,
  `USER_DMA_BASE`, `dma_slot_va()`, `Service::Console = 12`, `DEV_DISPLAY`.
- `kernel/src/arc/vmm_user/dma.rs`: N slot per address space + grant.
- `kernel/src/syscall/mem.rs` + `dispatch.rs`: `sys_dma_alloc(pages, slot)`.
- `kernel/src/syscall/service.rs`: `Service::Console` nel lookup/nome.
- `libs/civis`: `dma_alloc_slot`, re-export delle costanti.
- `libs/term` (nuovo): `disp`, `grid`, `utf8`, `font`, `ttf`, `raster`,
  `surface`.
- `libs/vela/src/gpu.rs`: i comandi virtio-gpu delle risorse (`CREATE_2D`,
  `ATTACH_BACKING`, `SET_SCANOUT`, `FLUSH`, `UNREF`) e i formati.
- `userland/gpu`: il driver (solo il protocollo display), `virtio.rs` con
  `submit()` a lunghezze variabili e `transfer_to_host_2d()`.
- `userland/console` (nuovo): il console server.
- `userland/rector`: i manifest (`SVC_GPU`, `SVC_CONSOLE`), l'ordine di
  boot (driver → console), la supervisione, i pinning del manifest.
- `scripts/build-userland.sh`, `scripts/arca-tool.sh` (seed del console e
  del font), `third_party/` (TTF + licenza).

Validazione: gate invariato (testfs 5/5, testsarca 64/64 con il flake noto
di `append 512KiB`, posixtests 4/4, identistest 11/11, usertests 58/58,
threadtest 12/12), `scripts/test-shell-all.sh` 10/10, marker
`[console] font: MapleMono-NF 16px cell=10x22`,
`[console] surface: pixel 1280x800 grid=128x36`, `[gpu] scanout: 1280x800`,
fallback `VIRTIO_GPU=0` → `[console] surface: vga-text 80x25`, e il
**dump dello schermo del device** (monitor QEMU: `screendump ... <device>`)
mostra il terminale letibile.

## Appendice: due deadlock evitati (le trappole del percorso)

1. **Il cardo single-threaded**: un servizio che serve `/dev/console` e
   vuole *anche* scrivere su un device FS (o fare I/O) si blocca: il cardo
   lo aspetta per la reply, il servizio aspetta il cardo. Da qui la scelta
   del canale IPC diretto col driver e del font letto tutto in RAM.
2. **Le finestre ring `CLI_*` sono una per processo**: chi è client e
   server insieme (la console) le sovrascrive. Il canale diretto non usa i
   ring.

E una lezione del device: nel path non-virgl di QEMU il `RESOURCE_FLUSH`
**non copia i pixel** (li copia il `TRANSFER_TO_HOST_2D`): senza il
transfer, lo schermo resta nero anche se il rendering è corretto.

---
*Registro delle modifiche: primo redatto il 21 ottobre 2026 per D3 (console
virtuale: driver `gpu` + servizio `console` + font TTF a runtime).*
