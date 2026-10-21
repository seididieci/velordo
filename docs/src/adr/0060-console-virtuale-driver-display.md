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

### 2. `/dev/display`: il device del driver

Protocollo minimo, riuso del `DEV_*` esistente (nessun tag IPC nuovo):

| Operazione | Frame | Contenuto |
|---|---|---|
| `DEV_OPEN` | reply | descrittore: `[backend: u8][w: u32][h: u32][stride: u32][format: u32][flags: u32]` |
| `DEV_WRITE` | request | `PRESENT(phys, w, h, stride, format)` (32 B) |
| `DEV_READ` | reply | il descrittore |

- `backend`: `VgaText` (nessun device pixel) o `VirtioGpu`.
- `PRESENT`: il driver crea la risorsa 2D (BGRA), le aggancia al buffer
  **del client** e la imposta come scanout. In QEMU `SET_SCANOUT` fa
  `qemu_console_resize`: **il guest sceglie la risoluzione** e la finestra
  host la segue.

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
  dell'ultima riga della griglia (dove sta il banner). Il valore è
  deterministico e **identico a quello calcolato sul host** con lo stesso
  codice (`libs/term`): il rendering nel guest è bit-identico a quello del
  rasterizer di riferimento.
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
- `userland/gpu`: il driver (solo `/dev/display`), `virtio.rs` con
  `submit()` a lunghezze variabili.
- `userland/console` (nuovo): il console server.
- `userland/cardo`: `dev_type("display")`.
- `userland/rector`: i manifest (`SVC_GPU`, `SVC_CONSOLE`), l'ordine di
  boot (driver → console), la supervisione, i pinning del manifest.
- `scripts/build-userland.sh`, `scripts/arca-tool.sh` (seed del console e
  del font), `third_party/` (TTF + licenza).

Validazione: gate invariato (testfs 5/5, testsarca 64/64 con il flake noto
di `append 512KiB`, posixtests 4/4, identistest 11/11, usertests 58/58,
threadtest 12/12), `scripts/test-shell-all.sh` 10/10, marker
`[console] font: MapleMono-NF 16px cell=10x22`,
`[console] surface: pixel 1280x800 grid=128x36`, `[gpu] scanout: 1280x800`,
fallback `VIRTIO_GPU=0` → `[console] surface: vga-text 80x25`, e il BLAKE2s
della riga del banner **identico** al valore calcolato sul host.

---
*Registro delle modifiche: primo redatto il 21 ottobre 2026 per D3 (console
virtuale: driver `gpu` + servizio `console` + font TTF a runtime).*
