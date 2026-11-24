# ADR-0092: Tile nel server e `impluvium` — il DE esiste (D8e)

## Status

**Accepted** (2026-11-24). **D8e1 implementata e verificata** (`tileprobe` +
gate invariato). **D8e2 implementata**: `impluvium` nel boot (albero, layout,
chrome, tastiera al DE con i keybinding `Super+hjkl`/`%`/`"`/`z`/`Shift+q`,
supervisione per canale, kernel intatto). Lo **split avvia una shell vera**
nella sessione nuova (`SESS_CREATE` → verifica+`obj_get` → `spawn_image` →
`suspend` → `SESS_BIND` → `resume`) e l'uscita della shell chiude la tile.
Il **bordo della tile segue il focus** (accento sulla tile in focus, scuro
sulle altre: tmux-style) e la **barra non c'e'** piu': il DE non disegna nulla,
i 22 px tornano al testo. Resta per D8f titoli in testo, puntatore e floating.

Dipende da ADR-0087 (compositor `musivum`: layer, clip, danno), ADR-0088
(finestre `WIN_*`, ospiti clippati alla tile), ADR-0089 (sessioni `littera`),
ADR-0090 (il testo è una sessione), ADR-0091 (i tasti sono una capability).
Prepara il DE completo: dopo D8e il DE è un processo che *decide*.

## Context

Dopo D8d il sistema ha sessioni vere, testo vero, tasti veri — ma **una** tile:
la console disegna **solo la sessione in focus**, a schermo intero, e usa una
sola geometria (`self.surface.geom()`) in cinque punti del render. Non esiste
un processo che decida il layout, il focus, lo zoom.

Tre cose non stanno insieme con "una tile sola":

1. **N sessioni, un rettangolo.** `Session::resize` cambia la griglia di una
   sessione, ma il render disegna solo la focus: le altre esistono e non si
   vedono. La geometria è globale, non per sessione.
2. **Un DE senza albero non è un DE.** Split, focus direzionale, zoom sono
   *decisioni*: appartengono a chi ha l'autorità (ADR-0087 §10.1), non al
   server dei pixel.
3. **Chi riceve i tasti?** In D8d li inietta la console perché è l'unico che
   conosce il focus. Ma i tasti "globali" (split, zoom, focus) sono del DE: il
   focus owner deve essere lui.

## Decision

### 1. Una tile è `rect + sessione`; l'handle è `sid + 1`

Nel server una tile **non ha stato proprio**: è un rettangolo, una sessione e
una finestra `Backing::Direct` in `Layer::Tiles`. La torsione del piano DE
("una tile *è* una sessione") diventa letterale: `2^0` di indirezione in meno,
un solo posto dove la griglia prende la sua misura (dal rect), nessun id di
tile da inventare — l'handle è `sid + 1` (per questo `WIN_OPEN(0)` = "la tile
della mia sessione" continua a funzionare e `WIN_OPEN(1)` = la prima tile,
t73 compreso).

Protocollo (solo ≥ `ThirdParty`: il layout è un privilegio):

| Tag | Semantica |
|---|---|
| `TILE_SET` (0xB0) | `w0` = sessione, `w1` = `(x,y,w,h)` 4×u16; `w = 0` = tile **nascosta** (sessione viva, non disegnata: è così che si fa lo zoom) |
| `TILE_COMMIT` (0xB1) | applica lo **staging** in un render + flush solo (niente stati intermedi a schermo) |
| `TILE_INFO` (0xB2) | → `(tile visibili, handle in focus)` |
| `TILE_DROP` (0xB3) | toglie la tile (il rect torna al fondo); la sessione la chiude chi la possiede |
| `TILE_CRC` (0xB4) | → `(byte, crc)` del **rect** della tile: la prova "solo questa è cambiata" |
| `TILE_SCREEN` (0xB5) | console → DE, fire-and-forget: il display si è riadattato, rilancia il layout |
| `TILE_SIZE` (0xB6) | → `(w<<16|h)` del display **in pixel**: l'unità del layout |

Lo staging non è un vezzo: applicare N tile una per una renderebbe N volte e
mostrerebbe stati intermedi (tile sovrapposte, sessioni ridimensionate due
volte). `TILE_SET` scrive, `TILE_COMMIT` applica.

### 2. Geometria per tile, clip gratis

`Geom::for_rect(x, y, w, h, cell_w, cell_h, ascent)` = le stesse celle intere
della `for_surface`, contate **dentro** il rettangolo, con l'origine in
coordinate schermo (così il danno e `cells_rect` restano assoluti).
`Geom::local(x, y)` la riporta nella tile, per disegnare.

Il clipping per tile non ha codice nuovo: `Pixel::view(rect, geom)` è una
**vista** della superficie (base spostata, `width`/`height` del rect, stride
invariato) — e tutte le scritture a coordinate-cella (`put_char`, `draw_ink`,
`fill_rect`) hanno già il bounds-check su `width`/`height`. Un glifo al bordo
non colora il vicino, e non c'è un `if` in più nel percorso caldo.

Il render diventa: per ogni tile visibile, la sua banda di background (memcpy
dalla bg layer, coordinate schermo) e l'inchiostro **nella vista**; poi il
cursore — **solo** della tile in focus (negli altri sarebbe un blocco fantasma,
e con N tile ce ne sarebbero N).

### 3. Focus = tasti + cursore di una tile

`SESS_FOCUS` accetta qualunque sessione **che ha una tile** (prima solo la 0).
Niente `mark_all` + full flush: si ridisegnano le celle del cursore vecchio
(spento) e nuovo (acceso). `SESS_RESIZE` resta com'è: la misura di una sessione
dentro una tile la decide il **layout** (`TILE_COMMIT`), non il cliente.

### 4. Con una tile sola il server si adatta; con N decide il DE

Al modeset (D4) la console si riadatta: se c'è **una** tile, la riporta a
schermo intero e ridimensiona la sessione (comportamento D8c1/D8d — è ciò che
tiene in piedi `t63`/`t64` e il gate); se ce ne sono N, manda `TILE_SCREEN` al
DE e non tocca niente (le tile non sono sue). Il primo `TILE_COMMIT` elegge il
DE come **padrone del layout** (`tile_owner`).

### 5. `impluvium` (D8e2): il DE come processo

- **Albero**: `Leaf(sid)` / `Split { dir, ratio }`, rapporto in per-mille con
  `GOLDEN = 618` — il `dwindle` di Hyprland: la foglia che si splitta tiene il
  61,8%, la tile nuova il 38,2%, e i rapporti sopravvivono a resize e zoom
  perché il layout si ricalcola **dall'albero**, mai da pixel memorizzati.
  Clamp a `MIN_TILE` = 1 cella: su schermi piccoli il 38,2% non può fare zero.
- **Tasti**: il DE è il **focus owner**. Si sottoscrive a `porta`
  (`INPUT_ATTACH`, LIFO: è l'ultimo, quindi riceve lui), consuma i tasti
  *globali* e **inietta** gli altri nella sessione in focus con `SESS_INPUT`
  (nuovo tag: accoda nella coda di input + hint al lettore, ≠ `SESS_WRITE` che
  è testo). Per iniettare gli serve una mailbox su una sessione **nominata**:
  `SESS_ATTACH` (0xAB, ≥ `ThirdParty`) — che non lo dichiara *lettore* (quello
  resta `SESS_READ`).
- **Mappa tasti**: modificatore **Super**; `hjkl` = focus
  sinistra/sotto/sopra/destra (sway), `%`/`"` = split verticale/orizzontale
  (tmux), `z` = zoom, `Super+Shift+q` = chiudi. Il DE lega **caratteri**, non
  posizioni fisiche: `Super+%` arriva come `0x1C '%'`, `Super+Shift+q` come
  `0x1C 'Q'` — lo Shift è già nel carattere.
  `pc-keyboard 0.7` **non traccia i modificatori GUI** (`Modifiers` = shift,
  ctrl, alt_gr, caps, numpad): `porta` guarda il `KeyEvent` (che ha
  `state: Down|Up`) **prima** di decodificare, tiene `super_down` e prefissa i
  byte del tasto con `INPUT_SUPER` (0x1C). Un solo flusso = ordine garantito;
  il prefisso non arriva mai a una sessione (e se il DE non c'è, l'editor della
  shell ignora i control byte).
- **Spawn di una shell per foglia**: `SESS_CREATE` (non legata) → `spawn_image`
  (come `login`, fail-closed sulla firma) → **`suspend`** del figlio →
  `SESS_BIND(sid, pid)` → `resume`; se il bind fallisce la shell si **chiude**
  (mai appesa a session 0). Il `suspend` chiude la finestra in cui il figlio
  chiama `sess::current()` prima del bind — la bind è una IPC sincrona, quindi
  il padre si blocca e il figlio girerebbe davvero. Il layout e il focus si
  applicano **prima** dello spawn: la tile esiste già quando la shell scrive il
  suo banner. Alla morte della shell la tile si chiude (mai l'ultima).
- **L'eco non va nel vuoto**: `queue_input` ecoa solo se la sessione ha un
  **lettore dichiarato** (`reader_of`), oltre al flag. Una tile senza programma
  non mostra i tasti digitati (era il "testo fantasma" del primo split).
- **Chrome**: **il bordo della tile** (D8e3) — 2 px *dentro* il rect (la
  griglia è inset, così il bordo non copre testo; due tile adiacenti fanno una
  linea di 4 px), colore **accento** sulla tile in focus e scuro sulle altre.
  Lo disegna la **console** (rect e focus li conosce già: nessun tag nuovo) e
  con una tile sola/zoomata **non c'è** — il boot resta identico a D8d. La
  barra `Overlay` del primo giro è stata **tolta**: mangiava una riga di testo e
  il bordo fa lo stesso mestiere meglio. La capability `WIN_OPEN(!0) → Overlay`
  resta nel server (policy ≥ `ThirdParty`) come gancio per D8g.
- **Niente slot di servizio nuovo**: `Service::Impluvium` costringerebbe a
  toccare il kernel (`service_from_disc` + `service_name` + `SERVICE_COUNT`).
  Il DE non registra un servizio: `signal_ready` viaggia sul canale di nascita
  e init lo supervisiona **per canale** (`Supervised.svc` → `Option`). **Il
  kernel resta intatto.**

## Conseguenze

- **Zero kernel, zero servizi**: 6 tag `TILE_*` + 2 `SESS_*`, nessuna syscall.
  `musivum` non cambia (aveva già `Layer::Tiles`/`TileGuests` e il clip).
- **Il clip per tile è strutturale**: una vista, non una politica — nessun
  ospite e nessun glifo possono scrivere fuori dalla loro tile.
- **Zoom = layout**: nessuno stato in più nel server (le altre tile sono
  nascoste, le sessioni vivono).
- **Ospiti GUI**: con N tile il clip di D8b segue il rect della tile; "replace"
  e "overlay" sono la stessa cosa vista dal compositor. La **policy** (chi apre
  dove) resta D8g.
- **Prova**: `scripts/run-desktop-tests.sh` (`desktop=1`, `tileprobe` in `usr`
  come `posixsock`) — il layout lo dispone solo un'immagine ≥ `ThirdParty`,
  quindi non può stare in `usertests` (`tst`). Il gate resta a **una** tile e
  quelle 33 check della shell sono la prova che una tile disegna il terminale
  giusto; il refactor e1.1 è stato verificato con il **CRC del frame identico**
  (`0xe8264257`, 4 096 000 byte, 735 celle — t64).
- **Trappole pagate e annotate**:
  - il render è "una griglia" in **cinque** punti (`render`, `draw_cell_at`,
    `restore_layer0_cell`, `scroll_session`, `cells_rect`/`grid_px_rect`): se se
    ne dimentica uno il testo va nella tile sbagliata e il gate non lo vede
    (una tile sola). Per questo e1.1 è *solo* il refactor con una tile, e la
    prova è un CRC, non "sembra uguale";
  - `Pixel::put_char` è un metodo di `Surface`, non di `Pixel`: la vista si
    avvolge in `Surface::Pixel(view)`; e il **layer 0** (che è dello schermo) va
    traslato nell'origine della tile, o il BMP compare nel posto sbagliato;
  - `copy_from` legge `src` alle **stesse** coordinate: le bande di background
    si copiano sulla superficie intera (coordinate schermo), non sulla vista;
  - una tile **nascosta non ridimensiona la sessione**: `Geom::for_rect` di un
    rect vuoto darebbe 1×1 e distruggerebbe la griglia (e il testo) al primo
    zoom;
  - lo shift T4 è **della tile** che mostra la sessione, non "della sessione in
    focus": altrimenti un `scroll` in una tile non disegnata sposterebbe i
    pixel sbagliati;
  - il cursore del terminale è di **una** tile (quella in focus): disegnarlo in
    tutte Produce N blocchi fantasma e cambia il CRC di t64;
  - il **bordo** va disegnato **dopo** il render delle tile e **prima** del
    flush, e ridisegnato al cambio di focus: è l'unica cosa che dipende dal
    focus oltre al cursore (t76 lo prova confrontando i CRC di *entrambe* le
    tile);
  - il modeset con N tile non deve auto-adattare (le tile sono del DE):
    altrimenti il server e il DE si contendono i rect.
- Fuori scope dichiarato: puntatore e floating (D8f); titoli in testo nella
  barra (D8f); policy/consenso degli ospiti (D8g); alpha nel compositor
  (`musivum` è opaco per scelta).
