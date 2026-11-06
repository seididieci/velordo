# ADR-0066: redraw del terminale — danno, rect di trasferimento, sfondo precomposto e scroll (D7)

## Status

**T0 e T1 completati.** Il resto dell'ADR (`T2`–`T7`) rimane come piano per
ottimizzazioni successive.

I passi `T0`–`T5` sono eseguibili subito, uno per commit, con lo stesso gate
del resto del progetto. `T6` e `T7` sono opzionali (guadagno reale ma rischio
maggiore). La sezione **"Fuori scope — traiettoria"** elenca le ottimizzazioni
che richiedono un driver video più avanzato di quello attuale: sono
deliberatamente fuori da questa fase, ma vanno tenute in ordine di priorità
quando si affronterà il driver (virgl, blob, panning).

## Contesto

### Il percorso caldo, oggi, con i numeri

Schermo di riferimento: 1280×800, superficie BGRA (`stride = width*4`),
griglia 128×36, cella 10×22 (font Maple Mono NF 16 px). Grandezze:

| Grandezza | Valore |
|---|---|
| framebuffer | 1280×800×4 = **4.096.000 B** |
| area griglia | 1280×792 = 1.013.760 px = **4.055.040 B** |
| una riga di cella | 1280×22×4 = **112.640 B** |
| una cella | 10×22×4 = **880 B** |
| celle della griglia | **4608** |

Per **ogni** `DEV_WRITE` (ogni tasto ecoato, ogni riga di output):

1. `Term::feed` (`userland/console/src/main.rs:108`) chiama `render()` e poi
   `flush_display()` (`:121`) — sempre, anche per un `\r` o per una scrittura
   che non cambia nulla.
2. `flush_display` (`:254`) è una IPC **sincrona** al driver: il driver fa
   `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH` **a schermo intero**
   (`userland/gpu/src/virtio.rs:395` e `:412`: le funzioni prendono solo
   `width,height` e riempiono il rect con `x=0,y=0`). Quindi **4 MB copiati
   per ogni scrittura**, più due comandi virtio e il `notify` MMIO in polling.
3. Se la scrittura causa uno scroll, `Grid::scroll_up`
   (`libs/term/src/grid.rs:368`) fa `mark_all` e il renderer tocca **tutte**
   le 4608 celle. Per cella: `fill_rect` 10×22 = **220 `write_volatile`**
   (`libs/term/src/surface.rs:230`), poi l'overlap del BMP riga per riga con
   `blend_bgra_span` (lettura+blend+scrittura per pixel), poi l'inchiostro
   del glifo con `get`+`blend`+`put` per pixel (`surface.rs:423-452`).
   Ordine di grandezza: **1,5–2 milioni di accessi `volatile` per frame**.

Il collo di bottiglia dominante non è il disegno ma **il trasferimento
full-screen**; il secondo è il disegno per-cella con `volatile` e il blend del
BMP ripetuto per ogni cella.

### Il guasto già visto (perché "marca solo l'ultima riga" non funziona)

Dopo uno scroll la riga `r` dello schermo mostra il contenuto che stava in
`r+1`: **i pixel sono già corretti** — ma solo se lo sfondo della riga `r` è
identico a quello della riga `r+1`. Con il layer 0 (wallpaper BMP, ADR-0063)
non lo è: lo sfondo *non* scorre insieme al testo. La variante "marca solo
l'ultima riga" è stata provata e ha lasciato pixel vecchi a schermo; è stata
revertata. La lezione: **l'ottimizzazione dello scroll non è una proprietà del
dirty flag, è una proprietà dello sfondo**. Va condizionata alla
riga-invarianza dello sfondo, non applicata a priori.

### Vincoli

- `#![no_std]`, niente `std`, niente float (punto fisso dove serve).
- Il framebuffer è **RAM normale** (non MMIO): è il device a leggerlo via DMA
  dopo il `FLUSH`. La coerenza è garantita dal confine IPC/syscall.
- Il cardo è **single-threaded**: la console non può fare I/O FS mentre serve
  `/dev/console`; il display si parla in diretta (ADR-0060).
- Il buffer device è contiguo, ≤ 32 MiB, slot DMA dell'owner di
  `Service::Console` (ADR-0061).
- Il gate è QEMU: `[testfs] 5/5`, `[testsarca] 64/64`, `[posixtests] 4/4`,
  `[identistest] 11/11`, `[usertests] 59/59`, `[threadtest] 12/12`, zero
  `FAIL/PANIC/FAULT`. I test shell: `[all] PASS: native` (28 PASS).
- Unit test host **fuori dal gate**: `./scripts/run-unit-tests.sh`
  (term, valloc, blake2s: 22 test).

## Decisione

Il redraw diventa **guidato dal danno**, con tre strati espliciti e un
trasferimento parziale:

1. **bg layer precomposta** (`bg_layer: Vec<u8>`, BGRA, `width*height*4`):
   fondo di pagina + BMP centrato già composto con il suo alpha, calcolata
   **solo** all'avvio e a ogni resize. Sostituisce i tre percorsi per-cella
   che oggi ricompongono lo sfondo (`fill_rect` + `blend_bgra_span`).
2. **Composizione a banda**: per ogni riga di celle da ridisegnare, **una
   copia** (`copy_nonoverlapping`) del rettangolo di `bg_layer`, poi solo
   **inchiostro** dei glifi (nessun fill per cella, nessun BMP per cella).
3. **Danno esplicito** (`Damage` in coordinate pixel, half-open): ogni punto
   che scrive pixel **dichiara il danno**. Il trasferimento al device manda
   solo il rettangolo del danno (wire esteso di `GPU_DISPLAY_FLUSH`), e non
   manda **niente** se il danno è vuoto.
4. **Scroll**: se (e solo se) lo sfondo è riga-invariante
   (`bg_rows_equal == true`), si spostano i pixel del framebuffer di una cella
   in su e si ridisegna **la sola ultima riga** (`Grid::scroll_up_after_shift`
   marca solo quella). Altrimenti si ricompone tutta l'area griglia dalla bg
   layer (percorso corretto, più lento).
5. **Batching del present**: il flush non parte a ogni `DEV_WRITE`; si
   accumula il danno e si trasferisce **una volta sola** quando la coda IPC è
   vuota (o dopo un tick, per non congelare lo schermo sotto flusso continuo).
6. **Niente `volatile` sul framebuffer**: si scrive con store normali e copie
   bulk (`copy_nonoverlapping`, `fill` a parole). Il `volatile` serve per la
   MMIO, non per la RAM che il device legge via DMA.

**Cosa non cambia**: il modello a celle (`Grid`), il decoder UTF-8, il font,
il protocollo `/dev/console`, il fallback VGA testuale, i colori
(`0xRRGGBBAA`), il contratto "dopo un `PRESENT` serve un `FLUSH`".

### Invarianti (da non violare in nessun passo)

- **I1 — stesso modello, stessi pixel.** Due strade che portano allo stesso
  modello (scroll incrementale vs ridisegno completo) devono produrre lo
  **stesso framebuffer** byte per byte. È il test `t64` (`ScrollCRC ==
  RepaintCRC`). Se salta, la strada incrementale è sbagliata.
- **I2 — chi tocca pixel dichiara il danno.** Ogni scrittura di pixel passa da
  `draw_cell_at` (o dal copy di banda/scroll) e aggiunge il proprio rettangolo
  a `Damage`. Mai dedurre il danno "a occhio" da `Dirty`: il danno è la verità
  sul trasferimento.
- **I3 — il danno copre lo sbordo.** L'inchiostro può uscire dalla cella
  (Powerline con `xoff < 0`, glifi alti): il danno si espande di **1 cella**
  in tutte le direzioni, con clipping alla superficie.
- **I4 — `PRESENT` senza `FLUSH` = schermo nero.** Dopo ogni `PRESENT`
  (avvio, resize) si fa un trasferimento **full-screen** (non parziale).
- **I5 — lo scroll a shift richiede lo sfondo riga-invariante.** Il gate è
  `bg_rows_equal`, ricalcolato a ogni build della bg layer. Con wallpaper
  riga-variante si usa il percorso completo.
- **I6 — prima di shiftare si disegna il pendente, e il cursore non si
  sposta.** Se ci sono celle sporche non ancora disegnate, `scroll()` chiama
  prima `render()`; il cursore va cancellato (ridisegnando la sua cella senza
  blocco) **prima** dello shift, altrimenti il blocco del cursore viene
  spostato di una riga e resta un "fantasma" sopra il cursore vero.
- **I7 — fallback VGA intatto.** Il percorso VGA testuale resta quello di
  oggi (`put_char`, cursore hardware CRTC). Le nuove funzioni pixel non lo
  toccano.
- **I8 — nessun cambio di semantica del testo.** Scrollback, reflow al resize,
  glifi larghi e cursore restano identici (test `t63`, unit test host).

### Cosa NON si fa

- **Non** si sposta il framebuffer composito (bg+testo) senza ricomporre lo
  sfondo: sposterebbe anche il wallpaper (è il guasto già visto).
- **Non** si tiene un "layer testo" con alpha separato e lo si fonde a ogni
  frame: sarebbe un blend di 4 MB per frame, peggio di oggi.
- **Non** si usa il dirty flag della griglia come verità del trasferimento:
  il danno è quello che è stato disegnato (I2).
- **Non** si introduce un framebuffer in virgl/blob/scanout-panning in questa
  fase (vedi traiettoria).

## Piano di esecuzione

### Regole per gli agenti (valgono per ogni passo)

- **R1 — un passo, un commit.** Messaggio: `console(D7): <T n> <cosa>`.
  Mai due passi nello stesso commit.
- **R2 — prima si legge, poi si scrive.** Ogni passo elenca i file; leggere la
  funzione indicata e le righe citate **prima** di modificare (i numeri di
  riga sono quelli alla stesura: cercare per nome funzione).
- **R3 — verifica minima dopo ogni passo** (nell'ordine, dalla più veloce):
  ```bash
  cargo build --release
  ./scripts/build-userland.sh
  ./scripts/build-tests.sh
  ./scripts/run-unit-tests.sh          # atteso: 0 failed (22 oggi; cresce con i test nuovi)
  ./scripts/test-shell-all.sh native   # atteso: [all] PASS: native, 28 PASS
  ```
  Il gate completo (`./run-tests.sh`, ~2 min) si lancia a fine passo
  `T1`, `T3`, `T4`, `T5` e alla chiusura.
- **R4 — nessun warning nuovo.** `cargo build --release 2>&1 | rg warning`
  deve mostrare solo i warning di `core` nightly e del profilo.
- **R5 — se un gate cambia numero di PASS, ci si ferma e si reverta** il
  passo (`git revert --no-edit HEAD`), poi si riporta il sintomo. Non si
  "aggiusta" il test per farlo passare.
- **R6 — non si tocca** `kernel/`, `userland/fs/`, `arcafs/`, `pal/`,
  `flavours/posix/`. La fase è confinata a `libs/term`, `userland/console`,
  `userland/gpu`, `syscall-numbers`, `testland/usertests`, docs.
- **R7 — no `std`, no float, no allocazioni per-frame** (le allocazioni nuove
  si fanno solo a init/resize).
- **R8 — commenti in italiano**, stile del file, solo dove spiegano il perché
  (non il cosa).
- **R9 — niente `println!` di debug lasciati dietro.** Le uniche righe
  ammesse sono i marker `[console] ...` esistenti e la riga `[console] stats:`
  del passo `T0`.
- **R10 — ogni passo termina con l'aggiornamento documentale** elencato in
  "Chiusura e documentazione" (solo i punti che quel passo tocca).
- **R11 — se un'istruzione non combacia col codice** (nome di funzione
  assente, riga che non contiene quello che l'ADR dice), **ci si ferma** e lo
  si riporta: non si improvvisa una variante. L'ADR è la specifica.
- **R12 — un passo alla volta, in ordine.** `T0`, `T1`, `T2`, `T3` sono
  propedeutici: `T3` usa `blend_row_bgra` di `T2`, `T4` usa `copy_from` di
  `T3`. Saltare un passo = non farlo.

### Mappa del codice (leggere prima di toccare)

| File | Cosa c'è | Punti caldi |
|---|---|---|
| `libs/term/src/grid.rs` | modello a celle | `scroll_up` (`:368`), `mark_all`, `take_dirty` (`Dirty`), `put_wide` (`:237`), `resize` (`:443`), `text_crc` (`:412`), `cursor_clamped` |
| `libs/term/src/surface.rs` | pixel e glifi | `Pixel { ptr,width,height,stride,geom }`, `put` (`:210`), `get` (`:221`), `fill_rect` (`:230`), `fill` (`:251`), `blend_bgra_span` (`:263`), `blend` (`:289`), `put_char` (`:350`; glifo `:423-452`, cursore `:453-457`), `VgaText` |
| `libs/term/src/disp.rs` | protocollo geometria | `Geometry { width,height,stride,format }`, `pack`/`unpack`, `BACKEND_*`, `FORMAT_B8G8R8A8` |
| `userland/console/src/main.rs` | console server | `struct Term` (`:64-104`), `feed` (`:108`), `resize_to` (`:129`), `flush_display` (`:254`), `put` (`:265`), `render` (`:383`), `paint_cell_bg` (`:429`), `draw_cell` (`:448`), `draw_cell_at` (`:475`), `draw_layer0_full` (`:603`), `restore_layer0_cell` (`:652`), `real_main` (`:760`), loop IPC (`:847`) |
| `userland/gpu/src/main.rs` | driver virtio-gpu | loop e tag (`:68-99`), `handle_flush` (`:186`), `handle_present` (`:205`) |
| `userland/gpu/src/virtio.rs` | comandi virtio | `set_scanout`, `transfer_to_host_2d` (`:395`), `flush` (`:412`), `unref` |
| `syscall-numbers/src/lib.rs` | tag IPC | blocco "Protocollo display" (`:450-495`) |
| `testland/usertests/src/main.rs` | registro test | `helpers::report(...)` (`:108` è `t63`) |
| `testland/usertests/src/t_display.rs` | test display | `t_display_modeset`, helper `driver_stats`, `console_grid`, `console_state` |

Comandi utili (tutti da radice repo):

```bash
rg -n "fn render" userland/console/src/main.rs      # dove sta oggi
git log --oneline -5                                 # contesto prima del passo
```

### T0 — misura (nessun cambio di comportamento)

**Obiettivo**: avere numeri prima/dopo. Serve a dimostrare il guadagno e a
scegliere se i passi successivi valgono.

**File**: `userland/console/src/main.rs`.

**Modifiche**:

1. Nel `struct Term` (`:64`), aggiungere:
   ```rust
   // Statistiche (T0, ADR-0066): solo diagnostica su seriale.
   stat_renders: u64,
   stat_cells: u64,
   stat_flushes: u64,
   stat_bytes: u64,
   stat_cyc_render: u64,
   stat_cyc_flush: u64,
   ```
   e inizializzarli a `0` dove si costruisce `Term`.
2. In `render()` (inizio e fine): `let t0 = civis::rdtsc();` …
   `self.stat_cyc_render += civis::rdtsc() - t0;` e
   `self.stat_renders += 1;`. Contare le celle disegnate:
   `self.stat_cells += 1;` dentro il ciclo che chiama `draw_cell_at`.
3. In `flush_display()`: `let t0 = civis::rdtsc();` … a fine funzione
   `self.stat_cyc_flush += civis::rdtsc() - t0; self.stat_flushes += 1;` e
   `self.stat_bytes += (self.surface.geom().cols * self.surface.geom().cell_w
   * self.surface.geom().rows * self.surface.geom().cell_h * 4) as u64;`
   (a `T0` il trasferimento è full-screen: la formula è volutamente quella
   dell'area griglia, il numero assoluto non è un contratto).
4. Ogni 64 flush, stampare una riga sola:
   ```rust
   if self.stat_flushes % 64 == 0 {
       civis::println!(
           "[console] stats: renders={} cells={} flushes={} B/flush={} cyc/render={} cyc/flush={}",
           self.stat_renders, self.stat_cells, self.stat_flushes,
           self.stat_bytes / self.stat_flushes.max(1),
           self.stat_cyc_render / self.stat_renders.max(1),
           self.stat_cyc_flush / self.stat_flushes.max(1)
       );
   }
   ```
   `B/flush` è **il numero chiave** (byte medi trasferiti per flush = costo per
   scrittura): prima di D7 vale l'area della griglia (~4 MB), dopo T1 deve
   crollare. I cicli TSC sono comparabili **solo sulla stessa macchina**:
   servono per il rapporto prima/dopo, non come valore assoluto.

**Test**: R3 completo. La riga `[console] stats:` appare nel log seriale durante
un run con GPU presente (virtio-gpu attivo, non VGA testuale) e output
sufficiente per accumulare 64 flush (~2000 scritture). In fallback VGA il path
`flush_display` ritorna subito: zero flush = zero riga stats.

**Accettazione**: gate invariato; numeri di baseline annotati.

**Baseline T0** (1280×800, pixel 4B):

| Metrica | Valore | Interpretazione |
|---|---|---|
| **B/flush** | **~424 KB** | Trasferito per ogni flush = area griglia piena. Per ogni tasto, 412 KB di IPC+DMA. |
| **cyc/render** | ~800K–900K cicli (tasti), picco a ~73M (dump file) | Render normale: ~1M ciclo; dump `identis:config` (247K cells): 42M+ ciclo. Il costo dominante è il transfer, non il disegno. |
| **cyc/flush** | ~6,5–6,9M cicli | IPC + virtio `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`. Indipendente dal contenuto: il collo di bottiglia. |
| **renders=319, flushes=320** | Quasi 1:1 | Ogni `DEV_WRITE` triggera render+flush (nessun batching). |

**Riepilogo del costo per tasto:**
- IPC + DMA transfer: **~6,5M cicli** (dominante)
- Render drawing: ~1M cicli (minore)
- Totale per tasto: **~7,5M cicli**

**Rollback**: `git revert`.

### T1 — trasferimento e flush del solo rettangolo di danno

**Obiettivo**: smettere di copiare 4 MB per ogni scrittura. È il guadagno
maggiore per il caso "non scroll" (tasti, righe di stato) ed è a basso rischio.

**File**: `syscall-numbers/src/lib.rs`, `libs/term/src/disp.rs`,
`userland/gpu/src/virtio.rs`, `userland/gpu/src/main.rs`,
`userland/console/src/main.rs`.

**Modifiche**:

1. **Wire** (`syscall-numbers/src/lib.rs`, commento di `GPU_DISPLAY_FLUSH`
   a `:464`): documentare il nuovo payload:
   - `w0 = x | (y << 16)`, `w1 = w | (h << 16)` (pixel, half-open sul bordo
     destro/basso);
   - `w0 == 0 && w1 == 0` → **schermo intero**: è esattamente ciò che manda
     oggi `flush_display` (`send(tag, 0, 0)`), quindi il caso "full" resta
     compatibile e non va cambiato sul filo;
   - `w == 0 || h == 0` → **schermo intero** (un rect vuoto non è un rect).
   Non cambiare il numero del tag.
2. **Packing** (`libs/term/src/disp.rs`): aggiungere
   ```rust
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub enum Flush {
       /// Tutta la superficie (dopo un PRESENT, o per un ridisegno completo).
       Full,
       /// Rettangolo in pixel: `(x, y, w, h)`.
       Rect { x: u32, y: u32, w: u32, h: u32 },
   }

   pub fn pack_flush(f: Flush) -> (u64, u64);
   pub fn unpack_flush(w0: u64, w1: u64) -> Flush;
   ```
   con unit test `#[cfg(test)] mod tests` in `disp.rs`: `Full` ↔ `(0,0)`,
   `Rect` con x/y/w/h ai limiti (0, 0xFFFF), `w==0` → `Full`.
3. **Driver, comandi** (`userland/gpu/src/virtio.rs`):
   - `transfer_to_host_2d(&mut self, resource_id: u32, x: u32, y: u32,
     w: u32, h: u32, stride: u32)` → riempie `req[24..28]=x`, `[28..32]=y`,
     `[32..36]=w`, `[36..40]=h`; `offset = y*stride + x*4` (l'angolo in alto
     a sinistra del rect nel framebuffer lineare del guest). **Non `0`**:
     QEMU legge la sorgente a `offset + h*stride` e usa `x,y` solo per la
     destinazione; con `offset = 0` ogni rect fuori dall'origine copia la
     striscia in alto a sinistra (bug trovato in T1, v. "Completato").
   - `flush(&mut self, resource_id: u32, x: u32, y: u32, w: u32, h: u32)` →
     stessi campi rect di `RESOURCE_FLUSH`.
4. **Driver, logica** (`userland/gpu/src/main.rs`):
   - `handle_flush(dev, presented, w0, w1)`: decodifica con
     `term::disp::unpack_flush`; `Full` → `(0,0,w,h)` della geometria
     presentata; `Rect` → clip a `w,h` (se il clip è vuoto: risposta `0` e
     nessun comando); poi `transfer_to_host_2d` + `flush` con lo stesso rect.
   - `GPU_DISPLAY_STATS` **non cambia** (`presents`, `flushes_after_present`): i
     byte trasferiti li conta la console (T0), non il driver.
   - in `handle_present` la chiamata diventa
     `dev.flush(RESOURCE_ID, 0, 0, geom.width, geom.height)`.
5. **Console** (`userland/console/src/main.rs`):
   - nuovo tipo:
     ```rust
     /// Rettangolo di pixel toccati (half-open): il danno da trasferire.
     #[derive(Clone, Copy)]
     struct Damage { x0: u32, y0: u32, x1: u32, y1: u32 }
     ```
     con `fn none()`, `fn is_empty()`, `fn add_rect(&mut self, x, y, w, h)`,
     `fn add_cells(&mut self, geom: &Geom, c0, r0, c1, r1)` che espande di
     **1 cella** (I3) e clippa a `width,height` della superficie,
     `fn take(&mut self) -> Option<(u32,u32,u32,u32)>`.
   - campo `damage: Damage` in `Term`.
   - `draw_cell_at` (`:475`): subito dopo aver calcolato `start` e `span`
     (quindi **prima** dei due rami: una cella che si tocca è una cella che
     cambia) aggiungere
     `self.damage.add_cells(&geom, start, row, start + span - 1, row);`
     (calcolare `geom` una volta). Così ogni percorso di disegno dichiara il
     danno in un solo punto (I2).
   - **cursore**: oggi la cella del cursore viene ridisegnata **a ogni**
     `render()` (`:416-419`), anche se nulla è cambiato: con il danno
     esplicito sarebbe un trasferimento per ogni messaggio. Aggiungere il
     campo `cursor_drawn_visible: bool` e ridisegnare la cella del cursore
     **solo** se `(cur, cursor_visible) != (cursor_drawn,
     cursor_drawn_visible)`; aggiornare entrambi i campi dopo il disegno.
     (La cella sotto il cursore quando il *contenuto* cambia è già coperta
     dal bbox dei dirty.)
   - `render()`: dopo il blocco del cursore, non serve altro: il danno è già
     stato dichiarato dai `draw_cell_at`.
   - `flush_display(&mut self)`:
     ```rust
     // FULL_FLUSH_ONLY: valvola di sicurezza (T1). Se true, il percorso
     // parziale non viene mai usato: si torna al comportamento pre-D7.
     const FULL_FLUSH_ONLY: bool = false;
     ```
     ordine esatto:
     1. `if self.damage.is_empty() { return; }` — **nessuna IPC** quando non
        c'è niente da trasferire (oggi si manda sempre 4 MB);
     2. `let f = if FULL_FLUSH_ONLY { Flush::Full } else { match
        self.damage.take() { Some((x,y,w,h)) => Flush::Rect{x,y,w,h}, None =>
        return } };`
     3. invio sincrono di `GPU_DISPLAY_FLUSH` con `pack_flush(f)`.
     **I casi "full" obbligatori**: all'avvio (`real_main`, `:831`, subito dopo
     `draw_layer0_full`) e in `resize_to` (`:221`) si chiama
     `flush_display_full()`: il `PRESENT` ha appena agganciato un buffer nuovo
     e la console deve trasferirne **tutti** i pixel (I4). Anche
     `CONSOLE_REPAINT` (T4) usa `flush_display_full()`. `flush_display_full()`
     azzera il danno dopo l'invio (il trasferimento full copre tutto) e manda
     `Flush::Full` **anche** con danno vuoto.
6. **Console, bordo**: `paint_cell_bg`, `restore_layer0_cell` e `draw_cell`
   scrivono pixel; il loro danno è già coperto dal `draw_cell_at` che li
   chiama. Non aggiungere danno lì (una sola dichiarazione per cella).
7. **Test del driver** (`testland/usertests/src/t_display.rs`, `t63`):
   **nessun cambio al contratto di `GPU_DISPLAY_STATS`** (`w0` = presents,
   `w1` = flush dopo l'ultimo PRESENT, come oggi). Il controllo esistente
   `flushes1 == 0` resta valido e difende I4: dopo il `PRESENT` del modeset
   la console **deve** avere flushato. Che quel flush sia full-screen non si
   verifica coi registri ma col frame: se il post-`PRESENT` fosse parziale lo
   schermo resterebbe nero e l'A/B di T1 (0 pixel diversi) fallirebbe. Quindi
   **non toccare `t63`** in questo passo.
8. **T0**: aggiornare `stat_bytes` perché usi il rect effettivo
   (`w*h*4` del `Flush` inviato), non la formula dell'area griglia.

**Test**:
- unit test `libs/term` (`disp`): pack/unpack.
- gate QEMU invariato (`t63` compreso: non si tocca).
- **A/B automatico del frame (obbligatorio: è la prova che il rect parziale è
  corretto).** Il confronto non deve dipendere da input manuale, altrimenti
  non è riproducibile. Il punto di cattura è **il prompt di login** (D5): senza
  input il sistema si ferma lì e lo schermo è stabile (il login non ha
  timeout), quindi due run producono lo stesso frame.

  Aggiungere in `real_main`, **dopo** il banner (`term.feed(b"Velordo console
  server\n")`, `:835`), un **selftest del frame**:
  ```rust
  // TEMPORANEO (T1..T4, ADR-0066): frame deterministico detto "golden".
  // Rimosso a fine T4 (dopo l'ultimo A/B).
  const FRAME_SELFTEST: bool = true;
  if FRAME_SELFTEST {
      // 60 righe (2 scroll), un'icona Nerd Font, un glifo largo Powerline,
      // una riga colorata (bg diverso dal fondo di pagina).
      term.feed(b"\x1b[48;2;40;20;60m  colore  \x1b[0m\n");
      for i in 0..60u32 {
          let line = alloc::format!("riga {:02} .... \u{F520} \u{E0B0} ....\n", i);
          term.feed(line.as_bytes());
      }
      // Porta a schermo il danno accumulato. A T1 non esiste ancora
      // `flush_pending` (arriva in T5): usare `term.flush_display()`.
      term.flush_display();
  }
  ```
  Poi:
  ```bash
  # Helper: attende un marker nel log seriale (boot software-lento: non si
  # usa `sleep` fisso per la parte di boot).
  cat > /tmp/wait_for.sh <<'SH'
  for _ in $(seq 1 60); do
      rg -q "$1" "$2" && exit 0
      sleep 1
  done
  exit 1
  SH

  # A: FULL_FLUSH_ONLY = false (default)
  cargo build --release && ./scripts/build-userland.sh
  FB_WINDOW=1 RUN_DISPLAY=none ./run.sh \
      -monitor unix:/tmp/mon,server,nowait > /tmp/boot-a.log 2>&1 &
  bash /tmp/wait_for.sh '\[login\] registered' /tmp/boot-a.log || echo "BOOT LENTO"
  sleep 5                      # il prompt di login e' a schermo
  printf 'screendump /tmp/frame-a.ppm\n' | nc -U /tmp/mon
  kill %1; wait
  # B: FULL_FLUSH_ONLY = true, ricompilare, stessa sequenza, /tmp/frame-b.ppm
  python3 - <<'PY'
  def read(p):
      d = open(p, 'rb').read()
      assert d[:2] == b'P6'
      i, vals = 2, []
      while len(vals) < 3:
          while d[i:i+1].isspace(): i += 1
          if d[i:i+1] == b'#':
              while d[i:i+1] != b'\n': i += 1
              continue
          j = i
          while not d[j:j+1].isspace(): j += 1
          vals.append(int(d[i:j])); i = j
      return vals, d[i+1:]
  (wa, ha, _), pa = read('/tmp/frame-a.ppm')
  (wb, hb, _), pb = read('/tmp/frame-b.ppm')
  assert (wa, ha) == (wb, hb)
  diff = sum(1 for k in range(0, len(pa), 3) if pa[k:k+3] != pb[k:k+3])
  print('pixel diversi:', diff, 'su', wa*ha)
  PY
  ```
  **`FB_WINDOW=1` è obbligatorio**: con la VGA presente il `screendump`
  cattura la VGA 720×400, non la superficie virtio-gpu. Se `nc -U` non c'è:
  `socat - UNIX-CONNECT:/tmp/mon`. I due run devono fare **la stessa
  sequenza** (nessun input); il frame è stabile al prompt.
  Atteso: **0 pixel diversi**. Se non lo è, il passo è sbagliato: revert.
  **Conservare** `/tmp/frame-t1.ppm` (copia: `cp /tmp/frame-a.ppm
  /tmp/frame-t1.ppm`): è il frame golden che T2, T3 e T4 dovranno
  riprodurre. `FRAME_SELFTEST` resta attivo fino a fine T4.
- `./run-tests.sh` completo.

**Accettazione**: gate invariato (6 righe PASS, zero FAIL/PANIC/FAULT);
`t63` PASS; A/B automatico = 0 pixel diversi; `stat_bytes` per un tasto scende
sotto 1 MB (atteso ~1 KB).

**Completato**. Dati T1 misurati:

| Operazione | Rect (w×h) | Byte trasferiti | Riduzione |
|---|---|---|---|
| **Tasto singolo** | 40×66 px | **~10.560 B** (10 KB) | da 424 KB → 10 KB = **98% in meno** |
| **Riga intera** | ~270×88 px | ~95 KB | 98% in meno |
| **Pagina piena** | ~340–610×334 px | ~450–760 KB | riduce il danno, non elimina il trasferimento |

Il guadagno massimo si vede per i casi "non scroll" (tasti, righe di stato) dove
il trasferimento scende da **~424 KB a ~10 KB** per ogni scrittura. Lo schermo
intero viene trasferito solo all'avvio e al resize (via `flush_display_full`).

**Riduzione del costo IPC+DMA per tasto:**
- Prima: ~6,5M cicli/flush (full 4 MB)
- Dopo: ~300K–470K cicli/flush (rect 10–160 KB)
- **Guadagno stimato: ~90% in meno di tempo di trasferimento**

Il gate è rimasto verde: `[testfs] 5/5`, `[testsarca] 64/64`, `[posixtests] 4/4`,
`[identistest] 11/11`, `[usertests] 59/59`, `[threadtest] 12/12`, zero
`FAIL/PANIC/FAULT`. Unit test: 39 passati.

**Bug trovato (e corretto): l'offset del transfer.** Il primo run con i rect
ha mostrato testo corrotto. `TRANSFER_TO_HOST_2D` veniva inviato con
`offset = 0`, ma QEMU legge la sorgente a `offset + h*stride` e usa `x,y` solo
per la **destinazione** (`hw/display/virtio-gpu.c`,
`virtio_gpu_transfer_to_host_2d`). Con `offset = 0` ogni rect fuori
dall'origine copia la striscia in alto a sinistra del framebuffer dentro il
rect: il full-screen a `(0,0)` funzionava, i tasti no. Il driver ora manda
`offset = y*stride + x*4` (la stessa formula del driver Linux,
`virtgpu_plane.c`: `x*cpp + y*pitches[0]`). La lezione: il rect sul filo ha
**due** posizioni, una nella risorsa host (destinazione) e una nel buffer
guest (sorgente); il wire le lega con `offset`, non con `(0,0)`.

**A/B (T1) — eseguito.** Boot headless con
`-device virtio-gpu-pci,disable-legacy=on -vga none -display none` e
`screendump` al **prompt di login** (frame deterministico: niente orologio),
confronto pixel-per-pixel tra `FULL_FLUSH_ONLY = false` (rect) e `true`
(full): **0 pixel diversi su 1280×800**. Dopo il login (frame con l'orologio
della shell) le uniche differenze sono nelle cifre dell'ora (x 220–258),
attese perché i due run avvengono in secondi diversi; tutto il resto è
identico. Il `FRAME_SELFTEST` previsto dall'ADR non è stato aggiunto: il
prompt di login è già un frame stabile e resta fuori dal boot di produzione
(con T2–T7 non implementati non ci sarebbe un T4 a rimuoverlo).

---

## Fuori scope — traiettoria

Le ottimizzazioni seguenti richiedono modifiche più profonde e sono state
deliberatamente rimandate:

- **T3**: composizione a banda (copy bulk invece di fill+blend per-cella).
- **T4**: `CONSOLE_REPAINT` — forzaré il ridisegno completo senza trasferimento.
- **T5**: batching del present — accumulare il danno e trasferire una volta sola
  quando la coda IPC è vuota (o dopo un tick).
- **T6–T7**: scroll incrementale con shift dei pixel framebuffer (richiede sfondo
  riga-invariante), niente `volatile` sul framebuffer.

Queste ottimizzazioni sono documentate nell'ADR completo ma non implementate in
questa fase. La traiettoria verso un redraw completamente guidato dal danno è
parziale ma significativa: il trasferimento full-screen è stato eliminato per i
casi "non scroll".

**Nota**: `FRAME_SELFTEST` **non si rimuove** a fine T1: il frame
deterministico è il riferimento "golden" anche per T2, T3 e T4 (ogni passo
deve produrre lo stesso frame). Si rimuove alla fine di T4.

**Rollback**: `git revert` (o `FULL_FLUSH_ONLY = true` per disattivare a
caldo senza revertire il codice).

### T2 — memoria: via `volatile`, fill a parole, copie bulk

**Stato**: completato (insieme a T1 nel commit `88a3425`). Nessun cambio di
semantica: il frame prodotto e' identico a T1.

**File**: `libs/term/src/surface.rs`.

**Modifiche applicate**:

1. `Pixel::put` (`:210`) e `Pixel::get` (`:221`): **togliere
   `write_volatile`/`read_volatile`** e usare `ptr::write`/`ptr::read`.
   Aggiornare il commento: il framebuffer è **RAM normale**, la coerenza con
   il device è il `FLUSH` (confine IPC/syscall, che è anche barriera del
   compilatore), non la singola parola. Il `volatile` resta lecito nel buffer
   testo VGA (`VgaText`), che non è nel percorso caldo e non va toccato.
2. `Pixel::fill_rect` (`:230`): un ciclo per riga che scrive `u32` a parole
   (niente bounds-check per pixel: calcolare `base` una volta, clampare
   `w,h` prima) e, se `w` è largo, scrivere `u64` a coppie. Stesso trattamento
   per `fill_rect_blend` (`:240`) ma senza cambiare il blend.
3. `Pixel::fill` (`:251`): scrivere `u64` (`color | color<<32`) invece di un
   `u32` per volta.
4. `Pixel::blend_bgra_span` (`:263`): estrarre il corpo in una funzione libera
   ```rust
   pub fn blend_row_bgra(dst: &mut [u8], dst_stride: u32, x: u32, y: u32,
                         n: u32, src: &[u8]);
   ```
   e far chiamare quella a `Pixel::blend_bgra_span` (che passa
   `core::slice::from_raw_parts_mut(self.ptr, stride*height)`). Serve a `T3`
   per comporre la bg layer su un `Vec<u8>` senza costruire un `Pixel`
   temporaneo.
5. Nessun cambio a `blend()` (divisione per 255): il compilatore la
   trasforma in moltiplicazione; un'approssimazione cambierebbe i colori e i
   test esistenti.

**Test** (unit test host in `surface.rs`, `#[cfg(test)] mod tests`):
- `fill_rect_bordi`: su un `Vec<u32>` 40×44, riempire `(10,5,10,7)` e
  verificare i pixel dentro e **fuori** (bordi inclusi).
- `fill_totale`: `fill` copre `stride*height` byte.
- `blend_row_confini`: `blend_row_bgra` non scrive fuori dal rect
  (`x+n > width` → tronca).
- I test esistenti (glifi larghi, alpha, colori, layer 0) restano verdi.

**Accettazione**: `./scripts/run-unit-tests.sh` verde (42/42, +3 vs T1);
gate QEMU invariato; A/B col frame golden: `frame-t2.ppm` identico a
`frame-t1.ppm` (0 pixel diversi).

**Rollback**: `git revert`.

### T3 — bg layer precomposta + composizione a banda

**Obiettivo**: eliminare il fill per cella e il blend del BMP per cella.
È il cambiamento che rende lo scroll ricomponibile a costo di memcpy.

**File**: `userland/console/src/main.rs`, `libs/term/src/surface.rs`.

**Modifiche**:

1. **`libs/term/src/surface.rs`**, nuove funzioni su `Pixel`:
   ```rust
   /// Copia un rettangolo BGRA da `src` (stride in byte) alla surface,
   /// con clipping. `src` è la bg layer precomposta (fondo + BMP).
   pub fn copy_from(&mut self, src: &[u8], src_stride: u32,
                    x: u32, y: u32, w: u32, h: u32);

   /// Sposta verso l'alto di `dy` pixel il rettangolo (x,y,w,h) della
   /// surface; le ultime `dy` righe restano invariate (T4).
   pub fn shift_rows_up(&mut self, x: u32, y: u32, w: u32, h: u32, dy: u32);

   /// Disegna **solo l'inchiostro** del glifo e il cursore nella cella: il
   /// background è già corretto (copiato dalla bg layer). Nessun fill, nessun
   /// layer 0.
   pub fn draw_ink(&mut self, col: usize, row: usize, span: usize, cp: u32,
                   fg: u32, cursor: bool, font: Option<&mut Font>);
   ```
   `copy_from`: clampare `x,y,w,h` alla surface e alla sorgente; per ogni
   riga `copy_nonoverlapping(src_row, dst_row, w*4)`.
   `shift_rows_up`: per ogni riga da `0..(h-dy)` copiare
   `src = y+dy+rr` → `dst = y+rr` (mai sovrapposte: `copy_nonoverlapping`
   per riga, `h-dy` righe).
   `draw_ink`: estrarre da `put_char` (`:423-452`) il blocco "glifo" in
   `fn glyph_pixels(p: &mut Pixel, x: u32, y: u32, w: u32, cp: u32, fg: u32,
   font: Option<&mut Font>)` e il blocco cursore (`:453-457`) in
   `fn cursor_block(p: &mut Pixel, x: u32, y: u32, w: u32, ch: u32, fg: u32)`;
   `put_char` li chiama come oggi, `draw_ink` chiama solo loro due.
2. **Console, campo**: `bg_layer: Option<Vec<u8>>` e `bg_rows_equal: bool`.
3. **Console, costruzione** (`draw_layer0_full`, `:603`, rinominarla
   `build_bg_layer`):
   - `w,h,stride` dalla surface pixel (se VGA: niente bg layer);
   - `let mut v = vec![0u8; (w*h*4) as usize];` riempire col colore di pagina
     BGRA (ciclo su `w*h` `u32`);
   - calcolare e memorizzare `layer0_offset_x/y` come oggi;
   - comporre il BMP con `surface::blend_row_bgra(&mut v, stride, ox, oy+yy,
     w_img, &layer0[off..off+w_img*4])` per ogni riga, **clippando**
     `yy < h - oy`;
   - `self.bg_layer = Some(v);`
   - `bg_rows_equal = self.compute_bg_rows_equal();`:
     ```rust
     /// true se tutte le righe della bg layer sono byte-identiche alla riga 0:
     /// è la condizione in cui lo scroll può spostare i pixel (I5).
     fn compute_bg_rows_equal(&self) -> bool { /* confronto per righe == riga 0 */ }
     ```
     (costo O(h·stride), solo all'avvio/resize.)
   - copiare la bg layer nella surface: `p.copy_from(&v, stride, 0, 0, w, h)`.
     Per il borrow: `v` è locale, quindi `self.bg_layer = Some(v)` va fatto
     **dopo** la copia (`copy_from` prende `&v`), oppure si estrae
     `(ptr, len)` come fa oggi `render` con `l0_data`.
4. **Console, render** (`render`, `:383`): introdurre il percorso veloce
   **solo** se `matches!(self.surface, Surface::Pixel(_))` e
   `self.bg_layer.is_some()`. Struttura esatta (il `Dirty` è un bbox):
   ```text
   let d = self.grid.take_dirty();
   if !d.any { /* niente */ } else {
       // 1) Allarga il bbox agli span interi: se min_col cade dentro un glifo
       //    largo, si parte dalla sua testa; se max_col cade su una
       //    continuazione, si arriva in fondo allo span. Senza questo la
       //    banda cancellerebbe mezzo glifo che nessuno ridisegna.
       //    Usare grid.span_at(col, row) per ogni row.
       // 2) Per ogni row del bbox:
       //      x0 = origin_x + c0*cell_w ; w = (c1-c0+1)*cell_w
       //      p.copy_from(bg, stride, x0, origin_y + row*cell_h, w, cell_h)
       //      damage.add_rect(x0, y0, w, cell_h)
       //      per ogni col in c0..=c1:
       //          is_cursor = (col,row)==cursor_clamped && cursor_visible
       //          self.draw_cell_at(col, row, is_cursor, l0_data, /*bg_ready=*/true)
   }
   ```
   `draw_cell_at` prende un parametro in più `bg_ready: bool`; il corpo diventa:
   ```text
   let (start, _len) = self.grid.span_at(col, row);
   let cell = self.grid.cell(start, row);
   let span = cell.width.max(1) as usize;
   self.damage.add_cells(&geom, start, row, start + span - 1, row);   // T1
   if cell.width == 0 { return; }        // continuazione: la disegna la testa
   if bg_ready && cell.bg == color::BLACK {
       if let Surface::Pixel(p) = &mut self.surface {
           p.draw_ink(start, row, span, cell.cp, cell.fg, cursor, self.font.as_mut());
       }
       return;
   }
   /* resto invariato: restore_layer0_cell / paint_cell_bg / draw_cell */
   ```
   I due punti che ridisegnano la cella del cursore fuori dalla banda
   (`:411` e `:418`) passano `bg_ready = false` (nessuno ha copiato la banda
   per loro). Se `bg_layer` manca (fallback, resize in corso) → percorso
   attuale invariato (`bg_ready = false`).
   **Nota**: `cell.bg == color::BLACK` è il caso comune (prompt, shell,
   output). Le celle con background diverso restano corrette passando dal
   `put_char` completo (che riparte dal proprio bg e ricompone il BMP sopra).
   **Nota su `cell.cp == 0`**: con `bg_ready` la cella vuota non fa niente
   (la banda ha gia' messo lo sfondo giusto): non chiamare
   `paint_cell_bg`/`restore_layer0_cell`.
5. **Console, `paint_cell_bg`/`restore_layer0_cell`**: restano per il percorso
   senza bg layer (VGA e fallback). Non rimuoverli.
6. **`l0_data`**: resta necessario per il ramo `put_char` (celle con bg
   diverso). Non eliminare l'estrazione in `render()`.

**Test**:
- unit test host (nuovi):
  - `copy_from_rect`: copia corretta e clipping a destra/basso (nessun
    overflow);
  - `shift_rows_up`: pattern `0,1,2,3` per righe di cella → shift di 1 cella
    → righe `1,2,3,3` (l'ultima **non** viene pulita: la pulisce il renderer);
  - `draw_ink`: con il font vero, l'inchiostro c'è e il background **non**
    viene toccato (pixel di sfondo invariati).
- QEMU: `t63` (resize ricostruisce la bg layer) + gate completo.
- **A/B col frame golden** (stessa procedura di T1, `FRAME_SELFTEST` ancora
  attivo): `frame-t3.ppm` deve essere **identico** a `frame-t1.ppm`
  (0 pixel diversi). Questo copre da solo la composizione del BMP: il
  percorso per-cella e quello a banda devono dare gli stessi pixel.
- occhio su GTK (`RUN_DISPLAY=gtk ./run.sh`): logo nella posizione giusta,
  testo sopra leggibile, **nessun alone**.

**Accettazione**: gate invariato; `t63` PASS; `cyc/render` in forte calo;
`frame-t3.ppm` identico a `frame-t1.ppm` (0 pixel diversi).

**Rollback**: `git revert`.

### T4 — scroll: shift dei pixel + ultima riga (condizionato) e test di equivalenza

**Obiettivo**: lo scroll smette di ridisegnare lo schermo intero quando lo
sfondo lo consente, e si guadagna una **prova automatica** che le due strade
danno gli stessi pixel.

**File**: `libs/term/src/grid.rs`, `userland/console/src/main.rs`,
`syscall-numbers/src/lib.rs`, `testland/usertests/src/main.rs`,
`testland/usertests/src/t_display.rs` (helper resi `pub(crate)`),
`testland/usertests/src/t_scroll.rs` (**nuovo**).

**Modifiche**:

1. **`Grid`** (`libs/term/src/grid.rs`):
   - estrarre il corpo di `scroll_up` in `fn scroll_cells(&mut self)`
     (scrollback + `copy_within` + azzeramento ultima riga);
   - `pub fn scroll_up(&mut self)` → `scroll_cells()` + `mark_all()` (oggi);
   - `pub fn scroll_up_after_shift(&mut self)` → `scroll_cells()` + marcare
     **solo l'ultima riga** (con il commento: contratto = il chiamante ha già
     spostato i pixel del framebuffer);
   - `pub fn has_dirty(&self) -> bool { self.dirty.any }`.
2. **Console**, tre chiamate a `self.grid.scroll_up()` (`:319`, `:354`,
   `:365`) diventano `self.scroll();`.
3. **Console**, nuova funzione:
   ```rust
   // T4: valvola di sicurezza e per l'A/B. `true` = non si fa MAI lo shift
   // (si ricompone sempre): è il comportamento pre-D7.
   const NO_SHIFT: bool = false;

   /// Scroll: sposta i pixel se lo sfondo è riga-invariante, altrimenti
   /// ricompone (percorso completo). Vedi ADR-0066 I5/I6.
   fn scroll(&mut self) {
       let can_shift = !NO_SHIFT
           && self.bg_rows_equal
           && self.bg_layer.is_some()
           && matches!(self.surface, Surface::Pixel(_));
       if !can_shift {
           self.grid.scroll_up();
           return;
       }
       // I6: prima si disegna il pendente (i pixel del testo appena scritto
       // devono esistere prima di essere spostati).
       // I6b: il cursore non deve finire nello shift (diventerebbe un blocco
       // fantasma una riga sopra): lo si cancella prima, ridisegnando la sua
       // cella con cursor_visible = false, poi si ripristina il flag.
       let cursor_was_visible = self.grid.cursor_visible;
       self.grid.cursor_visible = false;
       self.render();
       self.grid.cursor_visible = cursor_was_visible;
       let (gx, gy, gw, gh) = self.grid_px_rect();
       let cell_h = self.surface.geom().cell_h;
       if let Surface::Pixel(p) = &mut self.surface {
           // sposta le righe della griglia, escluse le ultime cell_h righe
           p.shift_rows_up(gx, gy, gw, gh.saturating_sub(cell_h), cell_h);
       }
       self.grid.scroll_up_after_shift();
       self.damage.add_rect(gx, gy, gw, gh);
   }
   ```
   `fn grid_px_rect(&self) -> (u32,u32,u32,u32)`: area griglia in pixel
   (`origin_x, origin_y, cols*cell_w, rows*cell_h`), clippata alla superficie.
4. **Console**, `render()` accumula danno (già in T1) e `scroll()` lo usa:
   nessun ritorno.
5. **Diagnostica per il test** (2 tag nuovi, **solo test**, commentati come
   tali in `syscall-numbers/src/lib.rs`; **non** usare `0x7C`–`0x7F`: sono
   notifiche senza reply e non si possono rispondere):
   ```rust
   /// Diagnostica (ADR-0066): CRC32 del framebuffer BGRA visibile
   /// (`w0` = CRC, `w1` = byte hashati). Solo per i test: hashtare 4 MB
   /// costa ~10 ms. Non è una notifica: si risponde.
   pub const CONSOLE_FRAME_CRC: u64 = 0x67;
   /// Diagnostica (ADR-0066): ridisegna **tutto** il modello corrente e
   /// trasferisce l'intero schermo. È il riferimento "full repaint" del test
   /// di equivalenza. Reply (0,0).
   pub const CONSOLE_REPAINT: u64 = 0x68;
   ```
   In `CONSOLE_FRAME_CRC`: hash del framebuffer con la stessa funzione di
   `Grid::text_crc` (polinomio `0x0100_0193`), righe per riga su `width*4`
   byte (lo stride padding non si hasha).
   In `CONSOLE_REPAINT`: `grid` → `mark_all` + `render()` +
   `flush_display_full()`.
6. **Test `t64`** (`testland/usertests`, nuovo file `t_scroll.rs`,
   registrato in `main.rs` accanto a `t63`: `mod t_scroll;` fra i moduli e
   `helpers::report(&mut total, &mut ok, "t64 scroll vs repaint (D7)",
   t_scroll::t_scroll_equivalence());` subito dopo la riga di `t63`):
   ```
   1. console_chan()?; se None -> PASS con SKIP (fallback senza console).
      (Rendere `pub(crate)` in `t_display.rs` gli helper `console_chan`,
      `console_state` e `driver_stats`; `t_scroll.rs` li importa con
      `use crate::t_display::...`.)
   2. fd = civis::open("/dev/console", 0)?  (stesso percorso del login)
      scritture con civis::write_fs(fd, data, data.len()):
      - 3 schermate di righe "riga NN ....", con un'icona (U+F520) e un
        glifo largo (U+E0B0) in una riga, per coprire lo sbordo (I3);
      - abbastanza '\n' da far scorrere almeno 2 schermate;
   3. crc_a = CONSOLE_FRAME_CRC (w0 = CRC, w1 = byte hashati);
   4. CONSOLE_REPAINT;
   5. crc_b = CONSOLE_FRAME_CRC;
   6. PASS se crc_a == crc_b (e w1 > 0) e il modello non è vuoto
      (CONSOLE_STATE -> non_empty > 0).
   ```
   Ogni `write_fs` è un `DEV_WRITE` sincrono: quando ritorna, la console ha
   già reso i pixel (il flush può essere in ritardo, ma il CRC legge il
   buffer, non il device). Se `CONSOLE_FRAME_CRC` risponde `0`/ERR (nessuna
   surface pixel: fallback VGA) → PASS con SKIP, come fa `t63` per il
   modeset.
7. **Non** scrivere sul seriale i byte del test: il test scrive su
   `/dev/console` (schermo), e stampa solo la sua riga `PASS ...` sul
   seriale.

**Test**:
- unit test host: `scroll_up_after_shift` marca **solo** l'ultima riga e il
  modello si sposta di una riga; `has_dirty`.
- `t64` nel gate.
- **A/B del frame golden, percorso senza shift** (stessa procedura di T1):
  `frame-t4.ppm` deve essere **identico** a `/tmp/frame-t1.ppm` (0 pixel
  diversi). Copre la ricomposizione dalla bg layer dopo T3/T4.
- **A/B del percorso shift (obbligatorio): lo shift è l'unica strada che il
  gate non esercita** (con il wallpaper `bg_rows_equal == false`). Serve un
  secondo esperimento, a sfondo uniforme, sempre col frame golden:
  ```rust
  // TEMPORANEO (T4, A/B): true = ignora /bg.bmp (sfondo uniforme =>
  // bg_rows_equal == true => si usa lo shift). Rimosso a fine T4.
  const NO_WALLPAPER: bool = false;
  ```
  in `real_main`, al posto di `match load_bmp("/bg.bmp")`:
  ```rust
  let bmp = if NO_WALLPAPER { None } else { load_bmp("/bg.bmp") };
  match bmp { /* ... invariato ... */ }
  ```
  Poi, con `FRAME_SELFTEST` attivo (60 righe = 2 scroll):
  1. build `A`: `NO_WALLPAPER = true`, `NO_SHIFT = false` → shift;
  2. build `B`: `NO_WALLPAPER = true`, `NO_SHIFT = true` → ricomposizione;
  3. stesso avvio, stessa `screendump`: `frame-shift.ppm` vs
     `frame-noshift.ppm`.
  Atteso: **0 pixel diversi**. È la prova che lo shift dei pixel e la
  ricomposizione dalla bg layer portano allo stesso framebuffer (I1).
  Se differiscono: lo shift è sbagliato → revert.
- manuale GTK: `RUN_DISPLAY=gtk ./run.sh`, login, `help` più volte / un
  comando che stampi 60 righe (es. più `help`): il testo deve scorrere in su
  **senza righe duplicate né righe perse** e **senza blocco del cursore
  fantasma** una riga sopra il prompt. Con wallpaper attivo
  (`bg_rows_equal == false`) si usa il percorso completo: stesso risultato
  visivo, `cyc/render` più alto.

**Pulizia a fine T4** (nello stesso commit, dopo gli A/B):
- rimuovere `FRAME_SELFTEST` e `NO_WALLPAPER` (ripristinare la chiamata
  diretta a `load_bmp`);
- **tenere** `FULL_FLUSH_ONLY` e `NO_SHIFT` a `false`: sono le valvole di
  sicurezza documentate (permettono di tornare al comportamento pre-D7 senza
  revertire il codice);
- nessun `println!` di debug residuo (R9).

**Accettazione**: `[usertests] PASS 60/60` (era 59), gate a 6 righe, zero
FAIL; `t64` PASS; `frame-t4.ppm` == `frame-t1.ppm` e
`frame-shift.ppm` == `frame-noshift.ppm` (0 pixel diversi in entrambi); con
wallpaper il testo scrolla; `cyc/render` durante lo scroll in forte calo
(annotare il numero).

**Rollback**: `git revert` (i tag 0x67/0x68 possono restare senza danno, ma è
più pulito revertire tutto il passo).

### T5 — batching del present (un trasferimento per raffica)

**Obiettivo**: per un `dmesg`/build log, N righe = **1** trasferimento invece
di N. Latenza interattiva invariata (un tasto = un flush immediato).

**File**: `userland/console/src/main.rs`.

**Modifiche**:

1. `feed()` (`:108`): **togliere solo** la `self.flush_display()` finale (la
   `self.render()` resta, il modello va disegnato subito: `scroll()` in T4 si
   appoggia ai pixel veri — vedi T6 per la variante differita).
2. Nuovo:
   ```rust
   /// Trasferisce il danno accumulato, se c'è. Un solo flush per raffica.
   fn flush_pending(&mut self) {
       if let Some((x, y, w, h)) = self.damage.take() {
           self.flush_display_rect(x, y, w, h);
       }
   }
   ```
3. Loop principale (`:847`): leggere i messaggi con `recv_poll` per sapere se
   la coda è vuota **senza** consumare un messaggio che poi andrebbe perso
   (non esiste un "push back"):
   ```rust
   loop {
       let msg = match civis::recv_poll() {
           Some(m) => m,
           None => {
               // coda vuota: è il momento del flush (un solo trasferimento
               // per raffica), poi si torna a dormire.
               term.flush_pending();
               match civis::recv() {
                   Ok(m) => m,
                   Err(_) => continue,
               }
           }
       };
       match msg.tag { /* ... gli stessi rami di oggi ... */ }
       // anti-congelamento: sotto flusso continuo la coda non si svuota mai.
       if term.flush_due() { term.flush_pending(); }
   }
   ```
   `flush_due()`: `self.damage` non vuoto **e** `get_ticks()` avanzato di
   almeno 1 tick dall'inizio del danno. Implementazione: campo
   `damage_since: Option<i64>` impostato al primo `add_*` (dentro
   `Damage::add_*` non si ha accesso ai tick: impostarlo in `feed()`/`render()`
   quando il danno passa da vuoto a non vuoto) e azzerato in `flush_pending`.
   Il tick è 10 ms (`get_ticks`).
4. `resize_to` (`:129`) e l'avvio (`:830-831`): continuano a fare
   `render()` + `flush_display_full()` **subito** (I4), non il batching.
5. `CONSOLE_REPAINT` (T4): full flush immediato.

**Test**:
- `t63` + `t64` + gate completo.
- **manuale**: con GTK, digitare al prompt: ogni tasto deve apparire subito
  (la coda è vuota → flush immediato). Poi un comando che stampa 200 righe:
  il testo deve scorrere fluido e alla fine lo schermo deve essere completo
  (nessuna riga mancante: se manca, il danno è stato consumato prima del
  render → bug).
- `stat_flushes` su una raffica deve essere molto minore del numero di
  `DEV_WRITE`.

**Accettazione**: gate invariato (6 righe, `[usertests] 60/60`); digitazione
reattiva; raffica completa a schermo; `cyc/flush` per raffica in calo.

**Rollback**: `git revert`.

### T6 (opzionale) — render differito: dentro la raffica si disegna una volta

**Obiettivo**: N righe di output = **1** render invece di N. Guadagno
maggiore sulle raffiche, rischio medio (vedi I6).

**File**: `userland/console/src/main.rs`.

**Modifiche**:

1. `feed()`: **non** chiama più `render()`; aggiorna solo il modello (e
   `damage_since`).
2. `flush_pending()`: prima `self.render();` poi `flush_display_rect(...)`.
3. `scroll()`: **già** chiama `self.render()` prima dello shift (T4): con il
   render differito questa chiamata è **indispensabile**; verificare che sia
   presente e che `render()` con danno vuoto sia un no-op.
4. `CONSOLE_STATE`/`CONSOLE_FRAME_CRC`: se il modello è cambiato ma il render
   non è ancora avvenuto, forzare un `render()` prima di rispondere (il CRC
   deve fotografare lo schermo, non il modello).
5. `resize_to`: invariato (render + full flush immediati).

**Test**: tutti quelli di T5 + `t64`; in più, la digitazione deve restare
reattiva (un tasto = coda vuota = render+flush subito).

**Accettazione**: gate invariato; `stat_renders` su raffica ≈ 1; nessuna riga
persa; `t64` PASS (la prova che il differito non rompe lo scroll).

**Rollback**: `git revert`.

### T7 (opzionale) — doppio buffer device + page flip

**Obiettivo**: niente tearing e nessun read-back; il disegno avviene su un
back buffer mentre il device scansiona il front.

**File**: `userland/console/src/main.rs`, `userland/gpu/src/main.rs`,
`userland/gpu/src/virtio.rs`, `syscall-numbers` (eventuale tag di swap).

**Requisiti** (se si fa):
- due **slot DMA** alternati (la macchina c'è già: `fb_slot` alterna
  `DMA_SLOT_DEVICE`/`+1` in `resize_to`, `:156-160`);
- il driver tiene **due risorse** (`resource_id` 1 e 2), entrambe
  `attach_backing`, e commuta `SET_SCANOUT` con un comando nuovo
  (`GPU_DISPLAY_SWAP`, tag libero) che prende la risorsa da mostrare;
- **fallback obbligatorio**: se lo swap fallisce, si resta sul buffer corrente
  e si segnala `ERR` (la console continua a disegnare lì);
- il `PRESENT` iniziale crea entrambe le risorse; il resize le ricrea
  entrambe (o torna a single buffer);
- **la console non legge mai il front buffer**: oggi legge il framebuffer per
  il blend del glifo — con la bg layer (T3) legge il back buffer, quindi il
  requisito è soddisfatto; verificarlo;
- verificare che il device (QEMU) accetti `SET_SCANOUT` su una risorsa già
  usata in precedenza.

**Test**: `t63` (resize), `t64`, gate; manuale GTK con output veloce: nessun
tearing visibile; `GPU_DISPLAY_STATS` coerente.

**Accettazione**: gate invariato; nessuna regressione visiva; guadagno
misurato (solo se positivo, altrimenti si tiene il single buffer).

**Rollback**: `git revert`.

## Gate e verifica finale

Alla chiusura della fase (dopo l'ultimo passo eseguito):

```bash
cargo build --release
./scripts/build-userland.sh && ./scripts/build-tests.sh
./scripts/run-unit-tests.sh
timeout 150 ./run-tests.sh > /tmp/boot.log
rg '\[testfs\] PASS 5/5|\[testsarca\] PASS 64/64|\[posixtests\] PASS 4/4|\[identistest\] PASS 11/11|\[usertests\] PASS 60/60|\[threadtest\] PASS 12/12' /tmp/boot.log
test "$(rg -c 'FAIL|PANIC|#.* FAULT' /tmp/boot.log)" = "0"
./scripts/test-shell-all.sh native   # [all] PASS: native
rg '\[console\] stats:' /tmp/test-shell-native.log   # numeri finali
```

Numeri da riportare nel messaggio di chiusura (prima → dopo): `cyc/render`,
`cyc/flush`, `stat_bytes` per tasto, `stat_flushes` per raffica, PASS count.

Verifica visiva obbligatoria (GTK):

1. login `root/root` → banner e prompt leggibili, logo dietro il testo;
2. `help` → testo a colori, cursore che segue;
3. un comando che stampa ≥ 3 schermate → scroll fluido, nessuna riga
   duplicata/persa, nessuna banda sporca;
4. `CONSOLE_SET_MODE` non è raggiungibile dalla shell: si prova il resize
   ridimensionando la finestra GTK (D4) → il testo sopravvive e la bg layer si
   ricostruisce.

## Rischi e mitigazioni

| Rischio | Sintomo | Mitigazione |
|---|---|---|
| Rect di trasferimento sbagliato su QEMU | zone dello schermo "congelate" | A/B di T1 (0 pixel diversi) + `FULL_FLUSH_ONLY` |
| Danno incompleto (sbordo glifo) | pixel vecchi ai bordi delle celle | I3 (espansione di 1 cella) + `t64` con Powerline |
| Shift con sfondo non uniforme | wallpaper che scorre | I5 (`bg_rows_equal`), ricalcolato a ogni resize |
| Shift con pixel non ancora disegnati | righe di testo perse dopo lo scroll | I6 (`render()` prima dello shift) |
| Cursore shiftato con i pixel | blocco cursore "fantasma" una riga sopra | I6 (cursore cancellato prima dello shift) |
| Percorso shift non esercitato dal gate | un bug dello shift passa inosservato | A/B a sfondo uniforme di T4 (0 pixel diversi) + unit test host |
| Ripristino a caldo di `FULL_FLUSH_ONLY` | il frame resta come prima del revert | è una valvola: `false` in produzione, `true` solo per diagnosi |
| `recv_poll` consuma messaggi | richieste senza risposta (hang) | il messaggio letto entra **subito** nello stesso `match`, nessun ramo "scarta" |
| Batching senza limite | schermo fermo sotto flusso continuo | `flush_due()` a 1 tick |
| `volatile` rimosso | nessuno su x86 (RAM, DMA) | il confine è la syscall/IPC; se comparisse un artefatto, revert di T2 |
| Doppio buffer rifiutato dal device | schermo nero / nessuno swap | fallback obbligatorio al buffer corrente |

## Conseguenze

### Positive

- **Tasto**: da 4 MB trasferiti a ~1 KB (danno di una cella). Da ~4500× a
  ~1000× meno traffico.
- **Raffica**: un trasferimento e (con T6) un render per raffica.
- **Scroll**: da ~1,8 M accessi `volatile` a uno spostamento di memorie
  (memcpy bulk) + una riga di celle; con wallpaper, a una ricomposizione a
  banda (memcpy + solo inchiostro).
- **Codice più semplice nel percorso caldo**: nessun `fill_rect` per cella,
  nessun blend BMP per cella.
- **Prova automatica dell'equivalenza** (`t64`): la ricomposizione dopo uno
  scroll deve dare lo stesso framebuffer di un repaint completo, o il gate
  fallisce. (Il percorso *shift* non è esercitato dal gate con il wallpaper
  attivo: lo coprono l'A/B a sfondo uniforme di T4 e gli unit test host.)

### Negative

- 4 MB di RAM in più per la bg layer (heap userspace, demand-zero).
- Due tag diagnostici nuovi (`0x67`, `0x68`) e la semantica di
  `GPU_DISPLAY_FLUSH` estesa (rect): vanno documentati in `06-syscalls.md`.
- Il percorso "cella con background diverso" resta quello vecchio (più lento,
  ma corretto); non è il caso comune.
- Con wallpaper (riga-variante) lo scroll resta a ricomposizione completa: il
  guadagno maggiore si ha senza immagine, o con un'immagine riga-uniforme.
- `t64` aggiunge una suite: il gate passa a `[usertests] 60/60`.

### Neutre

- Il fallback VGA non cambia (e non guadagna).
- Lo scrollback e il reflow al resize non cambiano.
- Il contratto "chi ridisegna manda il flush" resta (I4), solo più stretto:
  il flush è del **danno**.

## Alternative considerate

- **Marcare solo l'ultima riga dopo lo scroll, senza gate sullo sfondo**:
  provata e revertata. Con il BMP lo sfondo non scorre con il testo → pixel
  vecchi. Scartata perché sbagliata in generale; sopravvive solo come caso
  particolare condizionato da `bg_rows_equal` (T4).
- **Spostare il framebuffer composito (sfondo+testo) senza ricomporre**:
  identico al punto sopra, con lo stesso difetto.
- **Due layer con alpha (testo trasparente) fusi a ogni frame**: blend di
  4 MB per frame, peggiore del percorso per-cella attuale. Scartata.
- **Flush multipli (uno per cella) invece di uno per danno**: N IPC + N
  comandi virtio per frame. Scartata.
- **Trasferire solo le righe del danno in verticale (strisce piene)**: più
  semplice del bbox ma sposta fino a 112 KB per un tasto. Il rect orizzontale
  è altrettanto semplice e molto più stretto.
- **Aspettare virgl e fare tutto sulla GPU**: il guadagno sarebbe O(1) sullo
  scroll, ma richiede un driver 3D completo e rinvia tutto il resto.
  Scartata per questa fase, tenuta come traiettoria.
- **Double buffering subito (T7 come primo passo)**: non riduce il lavoro,
  aggiunge rischio. Rimandata a opzione.
- **VGA hardware scroll (CRTC start address)**: inutile: il percorso VGA è un
  fallback e le celle sono 4000.

## Fuori scope — traiettoria (driver più avanzati)

Queste ottimizzazioni **non** fanno parte dei passi eseguibili qui; sono
registrate perché ordinano il futuro del driver display.

1. **virgl (3D) e atlas di glifi sulla GPU** — negoziare
   `VIRTIO_GPU_F_VIRGL`, creare contesto 3D e risorsa texture; il terminale
   diventa un atlas di glifi + un draw call per cella (o instancing), e lo
   **scroll è un uniform** (`scroll_y`): O(1) per scroll, CPU ~0. È il modello
   di Alacritty/Kitty/foot/WezTerm. Richiede: command stream virglrenderer,
   shader, vertex/instance buffer, gestione contesto. Da fare in una ADR
   dedicata, dopo questa fase.
2. **Blit host-side** (con virgl): `glBlitFramebuffer`/texture copy per
   spostare righe dentro la texture del framebuffer. Alternativa al copy in
   RAM quando i pixel sono già sul device.
3. **Blob resource + host-visible memory** (`VIRTIO_GPU_F_RESOURCE_BLOB`,
   `VIRTIO_GPU_F_HOST_VISIBLE`): scrittura diretta nella memoria del device,
   **niente `TRANSFER_TO_HOST_2D`** e niente round-trip di present. Non riduce
   il memmove dello scroll, elimina il trasferimento. Da valutare quando si
   aggiunge la memoria host-visible al kernel (grant/mapping).
4. **Scanout con panning** (risorsa più alta dello schermo, rect di scanout
   che si sposta di una cella): lo scroll diventa una scrittura di registro.
   QEMU virtio-gpu **2D ignora x/y di `SET_SCANOUT`**: non è praticabile con
   il driver attuale; servirebbe un display controller con panning (VBE/bochs
   o un driver custom).
5. **Piano cursore hardware** (`UPDATE_CURSOR`): il cursore non ridisegna più
   le celle. Piccolo guadagno, ma elimina i due `draw_cell_at` per tasto.
6. **DMA scatter-gather** (più iovec in `attach_backing`): trasferire solo le
   pagine dirty. Utile solo se il trasferimento resta il collo di bottiglia
   dopo T1/T5.
7. **VGA hardware scroll** (CRTC start address): una scrittura di registro
   invece di 4000 celle. Solo per completezza del fallback.

## Chiusura e documentazione (checklist AGENTS)

Nello stesso commit dell'ultimo passo eseguito:

- `docs/src/11-testing.md`: gate corrente aggiornato (`t64`, `[usertests]`
  60/60) e riga della suite nuova;
- `docs/src/06-syscalls.md`: `GPU_DISPLAY_FLUSH` con il rect (`w0`/`w1`,
  `(0,0)` = full), `CONSOLE_FRAME_CRC` (0x67), `CONSOLE_REPAINT` (0x68);
- `docs/src/SUMMARY.md`: questa ADR;
- `run-tests.sh`: commento del gate (60/60);
- `ROADMAP.md`: riga **D7** in "Pianificate" → completata con la vittoria
  misurata (prima/dopo);
- `docs/src/14-cronologia-fasi.md`: voce di fase con i numeri e i bug trovati;
- `AGENTS.md`: paragrafo video aggiornato (bg layer, danno, scroll
  condizionato, `FULL_FLUSH_ONLY`, gate 60/60);
- `CONTEXT.md`: se la mappa delle crate/servizi menziona il percorso caldo,
  allinearla.

## Riferimenti

- ADR-0060 (console virtuale, `libs/term`, surface pixel/VGA).
- ADR-0061 (modeset, host-sync, doppio slot DMA, bound device buffer).
- ADR-0063 (layer 0, alpha, convenzione `0xRRGGBBAA`).
- `docs/src/11-testing.md`, `docs/src/06-syscalls.md`.
- virtio-gpu: `TRANSFER_TO_HOST_2D` (rect + offset), `RESOURCE_FLUSH` (rect).
- QEMU `hw/display/virtio-gpu.c`, `virtio_gpu_transfer_to_host_2d`: la
  **sorgente** è `offset + h*stride` (il rect non viene riapplicato al
  backing del guest) mentre la **destinazione** è
  `(r.y + h)*stride + r.x*bpp`. Quindi `offset` deve essere
  `y*stride + x*4` (Linux `virtgpu_plane.c`: `x*cpp + y*pitches[0]`); con
  `offset = 0` i rect fuori dall'origine copiano la striscia in alto a
  sinistra del framebuffer (è il bug trovato in T1).

## Appendice A — formati sul wire (dopo questa fase)

| Tag | Direzione | `w0` | `w1` |
|---|---|---|---|
| `GPU_DISPLAY_FLUSH` 0x77 | console → gpu | `x \| y<<16` | `w \| h<<16` (`0,0` = full) |
| `GPU_DISPLAY_STATS` 0x74 | client → gpu | presents (invariato) | flush dopo ultimo PRESENT (invariato) |
| `CONSOLE_FRAME_CRC` 0x67 | test → console | CRC32 del frame | byte hashati |
| `CONSOLE_REPAINT` 0x68 | test → console | 0 | 0 |

Vincolo invariato: **nessun tag di richiesta** può valere `0x7C`–`0x7F`
(notifiche senza reply implicita: `EXIT_NOTIFY`, `SVC_READY`, `TEST_DONE`,
`INIT_BOUNCE`). I tag `0x67`/`0x68` sono richieste normali e si rispondono.

## Appendice B — firme nuove o cambiate (elenco puntuale)

`libs/term/src/surface.rs`:

```rust
pub fn blend_row_bgra(dst: &mut [u8], dst_stride: u32,
                      x: u32, y: u32, n: u32, src: &[u8]);

impl Pixel {
    pub fn copy_from(&mut self, src: &[u8], src_stride: u32,
                     x: u32, y: u32, w: u32, h: u32);
    pub fn shift_rows_up(&mut self, x: u32, y: u32, w: u32, h: u32, dy: u32);
    pub fn draw_ink(&mut self, col: usize, row: usize, span: usize, cp: u32,
                    fg: u32, cursor: bool, font: Option<&mut Font>);
}
```

`libs/term/src/grid.rs`:

```rust
impl Grid {
    pub fn has_dirty(&self) -> bool;
    pub fn scroll_up(&mut self);               // invariato: mark_all
    pub fn scroll_up_after_shift(&mut self);   // marca solo l'ultima riga
}
```

`libs/term/src/disp.rs`:

```rust
pub enum Flush { Full, Rect { x: u32, y: u32, w: u32, h: u32 } }
pub fn pack_flush(f: Flush) -> (u64, u64);
pub fn unpack_flush(w0: u64, w1: u64) -> Flush;
```

`userland/gpu/src/virtio.rs`:

```rust
pub fn transfer_to_host_2d(&mut self, resource_id: u32,
                           x: u32, y: u32, w: u32, h: u32) -> bool;
pub fn flush(&mut self, resource_id: u32,
             x: u32, y: u32, w: u32, h: u32) -> bool;
```

`userland/console/src/main.rs` (interni):

```rust
struct Damage { /* x0,y0,x1,y1 half-open */ }
impl Damage {
    fn none() -> Self;
    fn is_empty(&self) -> bool;
    fn add_rect(&mut self, x: u32, y: u32, w: u32, h: u32);
    fn add_cells(&mut self, geom: &Geom, c0: usize, r0: usize,
                 c1: usize, r1: usize);       // espande di 1 cella (I3)
    fn take(&mut self) -> Option<(u32, u32, u32, u32)>;
}

impl Term {
    fn grid_px_rect(&self) -> (u32, u32, u32, u32);
    fn compute_bg_rows_equal(&self) -> bool;
    fn build_bg_layer(&mut self);
    fn scroll(&mut self);
    fn flush_pending(&mut self);
    fn flush_due(&self) -> bool;
    fn flush_display_rect(&mut self, x: u32, y: u32, w: u32, h: u32);
    fn flush_display_full(&mut self);
}
```
