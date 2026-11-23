# ADR-0088: Finestre del compositor — protocollo `WIN_*` e `civis::win` (D8b)

## Status

**Accepted** (2026-11-23). **D8b implementata e verificata** (gate `[gate] PASS`,
t73 verde, zero `[munmap]`).

Dipende da ADR-0087 (compositor `musivum`, D8a), ADR-0060/0066 (console e
damage/rect flush) e ADR-0073 (shm ownership/destroy).

## Context

D8a ha innestato `musivum` in `console` con una sola tile `Direct` (lo
schermo) e zero finestre. Perché il DE serva a qualcosa, un processo deve
poter **disegnare in una finestra**: serve un protocollo client→console, un
modo per passare i pixel (memoria condivisa) e regole di vita (chi chiude la
finestra quando il processo muore).

Il vincolo dichiarato nel piano DE: il compositor vive dentro `console` finché
il grant DMA (`SYS_DMA_ALLOC` slot ≥ 1) è chiuso a `Service::Console`. Quindi
niente screen server separato in questa fase: il protocollo parla con
`Service::Console`.

## Decision

### 1. Tag `WIN_*` (0x94–0x9A), nessuna syscall nuova

| Tag | Semantica |
|---|---|
| `WIN_OPEN` 0x94 | `w0` = tile handle (0 = floating), `w1` = `w<<16\|h` → `w0` = win id, `w1` = shm_id |
| `WIN_COMMIT` 0x95 | `w0` = win id, `w1` = rect locale (0 = tutta) → blit + flush (D8b danneggia l'intera finestra) |
| `WIN_SET_RECT` 0x96 | solo floating (D8f): in D8b risponde `!0` |
| `WIN_SET_SURFACE` 0x97 | solo floating (D8f): in D8b risponde `!0` |
| `WIN_RAISE` 0x98 | solo floating (D8f): in D8b risponde `!0` |
| `WIN_CLOSE` 0x99 | libera lo shm e ridisegna il terminale sotto |
| `WIN_STATS` 0x9A | solo test: `(visibili, totali)` |

**Niente enum di intento**: layer e clip li decide l'**handle**. Con un handle
di tile la finestra nasce in `Layer::TileGuests`, con `clip` = rect della
tile; senza (0 = floating/overlay) in D8b è negata. La policy (classe ≥
`ThirdParty` + consenso per il floating, `System` per l'overlay screen-global)
arriva in D8f/D8g: il meccanismo è pronto, la decisione non è presa qui.

### 2. Superficie: la crea il server, il client la mappa RW

`WIN_OPEN` fa sì che il **server** (console) crei lo shm (`shm_create`) e lo
mappi in **sola lettura** per il blit; risponde con `shm_id`. Il client lo
mappa in **RW** con `shm_map` e ci disegna: le pagine sono le stesse, quindi
il compositor legge i pixel vivi del client. `WIN_CLOSE` e la morte
dell'owner (`EXIT_NOTIFY`) fanno **`munmap`** della mappatura del server
(lunghezza a pagine) e poi **distruggono** lo shm: senza il `munmap` il
riferimento della mappatura resterebbe appeso e i frame non verrebbero mai
liberati. Il client smappa la propria mappatura RW. Il gate `[munmap]` resta
la sentinella per il percorso dei client.

Limite di dimensione: lo shm del kernel è al massimo **64 frame** (256 KiB);
`WIN_OPEN` rifiuta una superficie più grande **prima** della syscall, con un
messaggio che dice perché.

### 3. Apertura non visibile; il primo `WIN_COMMIT` mostra

`open` crea la finestra con `visible: false`: aprire non deve dipingere un
rect nero sopra il terminale. Il primo `commit` la rende visibile, dichiara il
danno e flusha.

### 4. La tile `Direct` va ridisegnata su apertura/chiusura

La tile scrive **direttamente** nel buffer del device (`Backing::Direct`):
il compositor non sa ricostruire i pixel del terminale sotto una finestra.
Quindi, quando una finestra cambia area (open/close), `console` marca le
**celle** sotto il rect (`Grid::mark_rect`, nuovo) e ri-renderizza la tile
(`repaint_tile_area`), *poi* il compositor ricompone le finestre `Owned` sopra
(`flush_display`). `WIN_COMMIT` non ridisegna il terminale: il guest ha
disegnato sopra e lo copre. Invariante verificata da t73: dopo `close` il CRC
del frame torna **byte-identico** a prima dell'apertura.

### 5. `civis::win` — client nel meccanismo

`open`/`commit`/`set_rect`/`set_surface`/`raise`/`close`/`stats` +
`surface_create`. Errori: `WinError::{ServiceDead, Denied, Ipc}`. La reply è
quella implicita di `send` (sync), non un `recv` separato: `!0` in `w0` = rifiuto.

## Consequences

### Positive

- Un processo può disegnare pixel in una finestra senza syscall nuove e senza
  conoscere il compositor: solo il protocollo e la sua shm.
- Nessun leak shm: l'owner è il server, che chiude su `WIN_CLOSE` e su
  `EXIT_NOTIFY`.
- I pixel del client sono la verità (shm condivisa): zero copie in più del
  necessario, e il compositor fa un solo blit per commit.
- L'invariante "chiudi e torna com'era" è misurata (t73, CRC identico).

### Negative

- In D8b una sola tile (id `1`) e niente floating: 0x96–0x98 rifiutano.
- `repaint_tile_area` ri-renderizza le celle sotto la finestra a ogni
  open/close: costo proporzionale all'area della finestra, non allo schermo.
  Per commit frequenti il costo è zero (solo il blit).
- La superficie massima è 256 KiB (vincolo shm del kernel), quindi finestre
  grandi richiederanno il device buffer o più shm (follow-up).

### Neutral

- L'id della tile (`1`) è deterministico in D8b; in D8c diventa un handle
  passato allo spawn (niente numero noto al cliente).
- `WIN_STATS` conta anche la tile (`Layer::Tiles`) fra le finestre totali: i
  test confrontano il **delta**, non il valore assoluto.

## Alternatives Considered

- **Il client crea lo shm e passa l'id al server**: avrebbe reso il client
  owner e quindi responsabile della distruzione; con la morte del client lo
  shm sarebbe stato liberato mentre il compositor lo legge ancora. Respinto:
  owner = server, che sa quando smettere di leggerlo.
- **`WIN_OPEN` con un enum (`guest`/`floating`/`overlay`)**: un enum è una
  dichiarazione che il server deve *credere*. L'handle è una capability che il
  server può *verificare*. Respinto.
- **Visibile subito all'apertura**: dipingeva un rect nero finché il client
  non disegnava. Respinto: il primo commit mostra.

## References

- `syscall-numbers/src/lib.rs` — tag `WIN_*` (0x94–0x9A)
- `libs/civis/src/win/mod.rs` — client `civis::win`
- `libs/musivum/` — compositor (ADR-0087)
- `userland/console/src/main.rs` — gestori `WIN_*`, `repaint_tile_area`,
  `close_window`, `close_windows_of`
- `libs/term/src/grid.rs` — `Grid::mark_rect`
- `testland/usertests/src/t_win.rs` — t73 (open/commit/close, CRC identico)
- `/home/work/.opencode/plan/d8-de-tiling.md` — piano DE (tile = sessione,
  livelli, clip, protocollo, torsioni §10)
