# ADR-0087: Compositore del DE — crate `musivum` (D8a)

## Status

**Accepted** (2026-11-23). **D8a implementata e verificata** (gate `[gate] PASS`,
t64 verde, login frame identico, zero `[munmap]`).

## Context

La console (`userland/console`) oggi è anche il compositor del DE: costruisce
un fondo precomposto (pagina + `bg.bmp` centrato, T3) e lo ridisegna a ogni
frame per le celle di default. Il piano del DE nativo (D8–D8g) introduce
tile, sessioni e finestre; il meccanismo di composizione (rettangoli, clip,
livelli, danno) va isolato in un crate puro, senza policy, testabile sull'host.

Il nome scelto è `musivum` (mosaico, latino corretto; `musium` è una grafia
errata), crates.io libero. Il carattere (albero di split, sessioni, privilegi)
vive sopra, in `impluvium`/`littera` (fasi successive).

## Decision

### 1. `libs/musivum` — meccanismo puro, `no_std`, zero dipendenze

Modello: rettangoli half-open (`Rect`), unione del danno (`Damage`), surface
BGRA lineare (`Surface`), fondo del DE grande quanto lo schermo (`Wallpaper`
con `view` = fetta in coordinate locali), finestre (`Window`: id, owner,
rect, clip, `Backing` (Direct/Owned), livello, z, visibile) e il
`Compositor` che compone nel danno.

Livelli (`Layer`, dal fondo al cursore): `Background`, `Tiles`, `TileGuests`,
`Floating`, `Overlay`, `Cursor`. L'ordine di dichiarazione **è** la z globale
(deriva `Ord`); poi per `z` e per `id`.

L'algoritmo di composizione:
- i danni sono un bounding box (un solo rect da trasferire, D7);
- le finestre si disegnano **sempre** (anche sopra un'altra finestra);
- sotto di loro si sottrae l'area dal fondo, così il wallpaper resta
  scoperto solo dove nessuna finestra copre;
- `Backing::Direct` = pixel già nel buffer destinazione (D8a: la tile scrive
  nel device buffer); il compositor non li copia, li sottrae solo dal fondo;
- `Backing::Owned(surface)` = il compositor blitta il rect con clip;
- overflow di sicurezza: oltre `MAX_REMAINING` (64) rettangoli scoperti, il
  fondo non viene ridipinto (degradazione sicura, meglio un pixel vecchio).

Host unit tests: z-order, clipping ai bordi, unione del danno, finestra
fuori schermo, `Direct` preserva i pixel già disegnati.

### 2. Innesto in `userland/console` — zero finestre, pixel-identico

- `Term.bg_layer` sparisce; il fondo è il `wallpaper` del compositor
  (`musivum::Wallpaper`), costruito una volta da `build_bg_layer` (oggi:
  colore di pagina + `bg.bmp` centrato, `blend_row_bgra`).
- `Term.damage` sparisce; il danno è `comp.damage` (`musivum::Damage`).
- `rows_all_equal` sparisce (vive in `Wallpaper::rows_equal` per T4).
- `cells_rect` calcola il bbox pixel di uno span di celle con lo sbordo di 1
  cella (I3) e il clip alla superficie.
- All'avvio si apre una **sola tile** (`musivum::Layer::Tiles`,
  `Backing::Direct`), grande quanto lo schermo, che scrive direttamente nel
  buffer del device. Il compositor non compone nulla: sottrae l'area della
  tile dal fondo (coincide con tutto lo schermo). Il wallpaper resta la
  sorgente che la tile legge per comporre le proprie bande (T3).
- `flush_display` chiama `comp.compose(&dst)` e trasferisce il rect restituito;
  `flush_display_full` mantiene il full transfer all'avvio/al resize (I4).
- Fallback VGA testuale: il compositor esiste ma non si usa (`surface` VGA
  esce presto da `compose`).

Invariante di uscita: t64 (`crc after shift == crc after CONSOLE_REPAINT`),
frame al login identico, zero `[munmap]`, zero FAIL/PANIC/FAULT.

### 3. Wiring

- `libs/musivum` aggiunta a `CRATES` in `scripts/run-unit-tests.sh` (131 → 153
  test host, tutti verdi).
- `userland/console/Cargo.toml`: dipende da `musivum`.
- Nessuna nuova syscall, nessun cambio al kernel, nessun cambio al driver
  `gpu`: il grant DMA (`SYS_DMA_ALLOC` slot ≥ 1) è già chiuso a
  `Service::Console` (D3).

## Consequences

### Positive

- Meccanismo di composizione isolato e testabile sull'host (no boot).
- `console` resta pixel-identica: stessa precomposizione, stesso shift (T4),
  stesso damage/rect flush (D7).
- Risparmio di memoria con più tile: il wallpaper è uno solo, non un
  `bg_layer` per tile.
- Zero kernel change fino allo split eventuale dello screen server.
- `t64` verde e login frame identico (guardia di pixel-identità).

### Negative

- `musivum` è oggi usato solo da `console`; il valore pieno arriva con
  `impluvium`/`littera` (D8b+).
- L'algoritmo di sottrazione del fondo è O(n·m) nel peggiore caso; con una
  tile sola è banale.

### Neutral

- Il nome `musivum` (non `musium`) è stato verificato libero su crates.io.
- Nessuna nuova tag IPC: il protocollo finestre (`WIN_*`) arriva in D8b.

## Alternatives Considered

- **Blit con source-offset esplicito** invece di `Wallpaper::view`: respinto
  perché la fetta (ptr del rect + stride dello schermo) è più chiara e
  riutilizza la stessa logica di `Surface::blit_from` con coordinate locali.
- **Compositor dentro `console` invece di crate separato**: respinto per
  rispettare separazione meccanismo/personalità e permettere i test host.

## References

- `libs/musivum/` — crate (geometria, surface, wallpaper, window, compositor)
- `userland/console/src/main.rs` — innesto (comp, tile, cells_rect, build_bg_layer)
- `docs/src/adr/0060-console-virtuale-driver-display.md` — console/D3
- `docs/src/adr/0066-redraw-terminale-danno-rect-scroll.md` — T3/T4/D7
- `/home/work/.opencode/plan/d8-de-tiling.md` — piano DE (tile = sessione, 6 livelli, clip, protocollo)
