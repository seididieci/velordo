# ADR-0061: modeset e host-sync — chi comanda la risoluzione del display

## Status

Accepted. Implementata (D4, 22 ottobre 2026). Dipende da ADR-0057 (grant
MMIO), ADR-0060 (console virtuale), ADR-0058/0059 (FAT fuori dal boot).

## Context

D3 ha reso il terminale grafico reale (font TrueType rasterizzato a runtime,
modello a celle, framebuffer virtio-gpu) ma la **risoluzione era una scelta
dell'host**, non del guest:

- la console presentava una risorsa della misura che il *device* riportava
  (`GET_DISPLAY_INFO` → la misura dell'UI);
- il **config change** MSI-X (implementato in L0b) non era mai stato
  **esercitato**: in headless nessun resize scatta l'evento, e la prova
  manuale con GTK non era mai stata fatta;
- al config change il driver **loggava e basta**: la console continuava a
  presentare la risorsa vecchia (il terminale restava a 1280×800 e QEMU lo
  scalava nella console nuova);
- un cambio di risoluzione **non era possibile**: la risorsa virtio ha
  dimensione fissa, e il buffer device (contiguo, per il device) non poteva
  essere riallocato — la DMA API alloccava ma **non liberava**;
- il modello non aveva **scrollback**: al re-adattamento il testo sarebbe
  andato perso.

In più, il device ha una semantica che conviene fissare per iscritto: il
`GET_DISPLAY_INFO` riporta la misura **dell'host** (QEMU la aggiorna solo
quando l'UI si muove), mentre la risorsa presentata dal guest può avere
**qualsiasi** misura (QEMU la scala nella console).

## Decision

### 1. La policy: il guest sceglie, l'host insiste

La console tiene una **misura desiderata**:

```
desired: Option<(w, h)>     // None = "segue l'host" (default)
```

- **Avvio**: `desired = None` → la console prende la misura del device.
- **Il guest comanda** (`CONSOLE_SET_MODE(w, h)`): la console presenta una
  risorsa di quella misura → il device ridimensiona la console → **la
  finestra dell'host la segue** (in QEMU `SET_SCANOUT` fa
  `qemu_console_resize`).
- **L'host insiste** (config change MSI-X): il driver rilegge la misura del
  device e **notifica** la console (`GPU_DISPLAY_CHANGED`); la console
  **cede** (`desired = None`) e si riadatta alla misura dell'host. Da lì
  segue l'host, finché il guest non rimette una misura desiderata.

In pratica: **una volta che l'host ha toccato la finestra, l'host comanda**;
il guest può rientrare nel controllo con una nuova richiesta esplicita.

### 2. Il protocollo

| Tag | Direzione | Contenuto |
|---|---|---|
| `GPU_DISPLAY_QUERY` (0x75) | console → driver | `w0` = backend, `w1` = geometria **del device** (l'host) |
| `GPU_DISPLAY_PRESENT` (0x76) | console → driver | `w0` = fisico, `w1` = geometria impacchettata |
| `GPU_DISPLAY_FLUSH` (0x77) | console → driver | — |
| `GPU_DISPLAY_CHANGED` (0x78) | **driver → console** | `w1` = la nuova geometria del device (notifica) |
| `CONSOLE_QUERY` (0x79) | client → console | `w0` = colonne, `w1` = righe della griglia |
| `CONSOLE_SET_MODE` (0x7A) | client → console | `w0` = w, `w1` = h (0 = "segue l'host") |
| `CONSOLE_STATE` (0x7B) | client → console | `w0` = celle non vuote, `w1` = CRC32 del testo |

- `GPU_DISPLAY_CHANGED` è **fire-and-forget** (`send_async`): il driver non
  deve aspettare la console, che può essere occupata a servire un cliente.
  Per questo **non** va risposto (una risposta a una notifica rimbalzerebbe
  driver e console a vicenda): i due servizi rispondono solo alle richieste
  (`req_id == 0`).
- `CONSOLE_*` è l'API del terminale: serve ai test e, domani, sarà la porta
  del desktop environment (chi chiede una risoluzione, chi chiede lo stato).

### 3. `SYS_DMA_FREE` (62): la DMA API deve poter liberare

Il ri-adattamento ha bisogno di un **buffer nuovo** (lo stride della risorsa
deve combaciare con i dati: una risorsa 800×600 con stride 5120 copierebbe
righe non contigue → rotto). La DMA API alloccava ma non liberava → nuova
syscall:

```
dma_free(slot) -> 0 | -1
```

- libera le pagine contigue dello slot e **smonta il mapping** della finestra
  dello slot (la VA non deve restare mappata su pagine riusate);
- gli slot ≥ 1 restano dell'owner di `Service::Console` (stesso grant
  dell'alloc, ADR-0057/0060): nessuna regressione di hardening (ADR-0026).

### 4. Il modello: scrollback e riflow

- `Grid` tiene uno **scrollback** bounded (1000 righe): lo `scroll_up()` ci
  spinge la riga che esce.
- `Grid::resize(new_cols, new_rows)` (D4) rilegge il contenuto
  (scrollback + griglia) come un **flusso** di righe e lo rimette nella nuova
  griglia: le righe più lunghe della nuova larghezza **si spezzano** (reflow
  da terminale, non da paragrafo), le righe più vecchie che non stanno
  vengono perse. Il cursore finisce in fondo.
- Il testo **sopravvive** al ri-adattamento: è la proprietà verificata dal
  test (lo CRC del testo resta identico).

### 5. Il layout: celle intere, resto centrato

Il font è **fisso a 16 px** (cella 10×22): la griglia usa solo le celle che
ci stanno **per intero** e il resto della risoluzione è spazio morto
**centrato**:

```
cols     = floor(W / cell_w)      righe   = floor(H / cell_h)
origin_x = (W - cols*cell_w) / 2  origin_y = (H - rows*cell_h) / 2
```

- 1280×800 → 128×36 celle = 1280×792 → 4 px sopra, 4 px sotto.
- 1024×768 → 102×34 celle = 1020×748 → 2 px ai lati, 10 px sopra/sotto.

L'origine è un attributo della geometria (`Geom::origin_x/origin_y`): il
terminale appare **centrato** nella finestra a qualunque risoluzione. La
surface VGA testuale ha origine (0,0) (la sua griglia è già esatta: 80·8 =
640, 25·16 = 400).

### 6. Il ri-adattamento è transazionale

La console alloca il buffer nuovo nello **slot alternato** (1 ↔ 2) e
**libera il vecchio solo dopo** che il driver ha presentato il nuovo: un
ri-adattamento fallito (buffer non ottenibile, `PRESENT` rifiutato) lascia
tutto com'è — la surface non deve mai puntare a un buffer liberato.

### 7. Tre cose del device che il driver deve sapere

1. **La risorsa ha dimensione fissa**: per cambiare misura va rifatta. E
   l'`UNREF` di una risorsa **in uso** non va: il driver stacca prima lo
   scanout (`SET_SCANOUT` con `resource_id = 0`), poi rilascia (`UNREF`),
   poi crea (`CREATE_2D`), aggancia (`ATTACH_BACKING`) e rimette
   (`SET_SCANOUT`).
2. **Le struct uapi contano i padding**: `virtio_gpu_resource_unref` è
   hdr(24) + resource_id(4) + **padding(4)** = 32 byte; senza il padding il
   device leggeva un id spazzatura e rifiutava ogni comando.
3. **`GET_DISPLAY_INFO` riporta la misura dell'host**, non quella del guest:
   il device aggiorna `req_state` solo quando l'UI si muove. Quindi
   "l'host ha seguito il guest" si vede solo con una UI reale (GTK).

## Consequences

- **Il terminale si può ridimensionare**: un client (oggi il test, domani il
  DE) chiede una risoluzione e la finestra dell'host la segue.
- **Il testo non si perde** al ri-adattamento (scrollback + reflow).
- **Una nuova syscall** (62) e un nuovo permesso (il free degli slot
  device): il kernel resta neutro, il grant è chiuso.
- **Il fallback VGA non ha modeset**: la modalità testo è fissa (80×25) →
  `CONSOLE_SET_MODE` risponde -1 e il terminale resta su quello che è.
- **Il config change MSI-X** è ora un percorso con effetti (non solo un
  log): il driver notifica, la console cede e si riadatta. La prova del
  resize **reale** della finestra resta **manuale** (GTK): in headless
  l'UI non si muove e l'evento non scatta.
- Il re-adattamento **non** è gratis: una nuova risorsa, un nuovo buffer
  (4 MiB ricontiguiati) e un ridisegno completo della griglia.

## Relations

- **ADR-0057** (grant MMIO): lo stesso modello di grant chiuso, qui per la
  memoria device.
- **ADR-0060** (console virtuale): il driver e la console, il modello a
  celle, il font; qui la risoluzione.
- **ADR-0026** (hardening): il grant degli slot device e il bound delle
  pagine.
- **ADR-0059** (FAT fuori dal boot): il font vive nel volume root.

## Implementazione

- `syscall-numbers`: `SYS_DMA_FREE = 62`, `GPU_DISPLAY_CHANGED` (0x78),
  `CONSOLE_QUERY/SET_MODE/STATE` (0x79-0x7B).
- `kernel/src/arc/vmm_user/dma.rs`: `dma_slot_pages`, `free_dma_slot`;
  `vma.rs`: `unmap_user_range` `pub(crate)`.
- `kernel/src/syscall/mem.rs`: `sys_dma_free` (grant + free + smonto).
- `libs/civis`: `dma_free_slot`.
- `libs/term/src/grid.rs`: scrollback, `resize` (reflow), `text_crc`;
  `surface.rs`: `Geom::origin_x/origin_y`, `Geom::for_surface` (celle intere,
  resto centrato).
- `userland/gpu`: notifica del config change, `UNREF` prima di rifare la
  risorsa, `RESOURCE_UNREF` a 32 byte.
- `userland/console`: `resize_to` (transazionale), `set_mode`, i tag
  `CONSOLE_*`, la gestione di `GPU_DISPLAY_CHANGED`.
- `testland/usertests/src/t_display.rs`: il test t63.

Validazione: gate invariato + il **nuovo test t63** (modeset → la griglia si
ricalcola, il device non insiste, il testo sopravvive, la risoluzione
iniziale viene ripristinata), `test-shell-all.sh` 10/10, e la prova manuale
con GTK (resize della finestra → il terminale si riallinea: il config change
MSI-X percorso davvero, per la prima volta).

---
*Registro delle modifiche: primo redatto il 22 ottobre 2026 per D4 (modeset e
host-sync: la risoluzione del display).*
