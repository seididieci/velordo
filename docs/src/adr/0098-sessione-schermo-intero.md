# ADR-0098: Sessione a schermo intero — alt screen, CSI con parametri, resize notificato

## Status

**Accepted** (2026-11-26). Implementata (passo 1 dei desiderata). Gate
invariato; `scripts/run-tui-tests.sh` PASS (fuori dal gate); unit host
`littera` 21.

Dipende da ADR-0089/0090 (sessioni e ritiro di `/dev/console`), ADR-0091
(input come capability), ADR-0092 (tile), ADR-0097 (modello grafico).

## Contesto

La sessione era un terminale **a flusso**: si scrive, si va a capo, si scorre.
Le sequenze ANSI supportate erano il minimo per colori e per l'editor della
shell (`ESC[D`/`ESC[C` di **una** cella, `ESC[K` dal cursore, `ESC[2J` **non**
implementato). Un'applicazione a schermo intero (un editor, un monitor) ha
bisogno di: posizionare il cursore in assoluto, cancellare intervalli, contare
i movimenti, nascondere il cursore, e **sapere** che la geometria e' cambiata.

E c'era una lacuna architetturale: il resize **non lo chiede l'app** — lo
decidono il display (modeset, D4) e il DE (layout, D8e). Finora nessuno lo
diceva all'app.

## Decisione

### 1. CSI con parametri, in `littera` (host-testabile)

`A/B/C/D` con conteggio (`ESC[3D`), `E/F`, `G`/`d` (colonna/riga assoluta),
`H`/`f` (CUP 1-based), `J` (0/1/2), `K` (0/1/2), `?25h/l` (cursore),
`?1049h/l` (**schermata alternativa**). I modi privati (`?`) non finiscono piu'
a schermo come testo: `ESC[?25l` prima stampava `25l`.

Errore trovato scrivendo i test: `J`/`K` prendono un **modo** il cui default e'
`0`, non un conteggio il cui default e' `1`. Con il default sbagliato `ESC[K`
cancellava dal **principio** della riga (e il test storico della shell, che si
aspetta "non cancella niente col cursore a fine riga", falliva). Il parser ora
distingue i due casi.

### 2. La schermata alternativa non ha scrollback

`ESC[?1049h` mette da parte la griglia principale (con il suo scrollback) e ne
crea una vuota **senza** scrollback: e' una tela, non una cronologia. Uscendo,
la principale torna com'era — testo, scrollback e posizione del cursore. La
principale **si riadatta** anche mentre e' da parte (al resize), o uscendo si
troverebbe una griglia di misura sbagliata.

### 3. Il resize diventa una **notifica**

- `littera::Session::resize` alza `resized` (solo se la misura cambia davvero).
- `SESS_INFO` riporta il flag in `w1` **bit 2** e lo consuma: "la geometria e'
  cambiata dall'ultima domanda".
- L'hint `SESS_INPUT_EVENT` porta `w1 = 1` quando c'e' un resize in sospeso, e
  il server lo manda a **ogni** cambio di misura (modeset, `TILE_COMMIT`,
  `SESS_RESIZE`): non serve un tag nuovo, e un hint perso non perde il resize
  (il flag resta finche' non viene consumato, come per i tasti).

Un'app a schermo intero quindi si sveglia, chiede `SESS_INFO`, vede che la
misura e' cambiata e ridisegna. Nessun polling.

### 4. `civis::sess::Screen`: la base per un'app a schermo intero

`open()` (lettore dichiarato, eco spento, tela alternativa, cursore nascosto,
schermo pulito), `size()`, `poll_resize()`, `at(col,row)`, `clear()`,
`cursor(bool)`, `write(bytes)`, `close()` (tela principale e cursore). Non e' un
secondo modello: sono le sequenze ANSI scritte sulla sessione del processo, che
ha gia' scrollback, danno e mirror seriale.

**Il frame va solo sullo schermo** (`write_screen`, `SESS_WRITE.w1 = 1`): il
mirror seriale resta il transcript dell'output, non dei ridisegni. I marker di
stato di un'app di test vanno sul seriale con `print_string`.

## Conseguenze

- La shell e il login non cambiano: le sequenze nuove sono un sovrainsieme di
  quelle che usavano gia' (e `ESC[K`/`ESC[D` senza parametri si comportano come
  prima, dopo la correzione del default).
- Un'app a schermo intero non puo' piu' "perdere" un resize: la notifica e'
  esplicita e il flag e' persistente finche' non lo consuma.
- Il costo e' nella parte giusta: il parser e' in `littera` (unit test host,
  millisecondi), non nel server. Niente syscall, niente tag nuovi.

## Test

- `libs/littera` (host): `csi_conteggi_e_posizione` (movimenti con conteggio,
  CUP, clip), `ed_el_e_cursore_nascosto` (EL/ED, modi privati che non finiscono
  a schermo), `schermo_alternativo_conserva_la_principale` (andata/ritorno,
  idempotenza), `resize_segnala_il_cambiamento_una_volta` (flag consumato, e la
  principale da parte si riadatta).
- `scripts/run-tui-tests.sh` (fuori dal gate, `usr/bin/fullscreen`): l'app
  prende la tela (127x36), i tasti muovono la stella, **un modeset chiesto
  dall'app** la ridimensiona a 102x34 e lei si riadatta da sola (il resize non
  lo ha chiesto lei), `q` esce e la shell **ridisegna il prompt** — la tela
  principale era messa da parte, non distrutta.

## Addendum (2026-11-27): il frame è l'unità di disegno

**Status**: Accepted, implementato.

### Il problema, misurato

`Screen::at`/`write` erano **una `SESS_WRITE` ciascuno**, e il server fa
`parse → render → flush` **a ogni** feed. Il `draw()` di `fullscreen` (bordo
disegnato cella per cella) costava **~660 IPC per un tasto**, cioè ~660 render e
660 flush sul display: il frame si vedeva arrivare a pezzi, **partendo dallo
schermo vuoto** (`ESC[2J` una cella alla volta). Il lampeggio visto a schermo
non era un problema grafico, era la granularità dell'API.

### La decisione

Due strati, entrambi necessari:

1. **Il meccanismo bufferizza** (`libs/civis/src/sess.rs`): `at`/`write`/`clear`/
   `cursor` **accodano** i byte in un `Vec`, `present()` li manda con una (o
   poche, `MAX_CHUNK` = 2048 B) `SESS_WRITE`. **Una feed → un render → un
   flush**: gli stati intermedi non esistono. `close()` presenta il pendente
   prima dell'epilogo e `Drop` presenta un frame rimasto appeso (dimenticare
   `present` non lascia lo schermo muto in silenzio).
2. **L'app differenzia** (`libs/tabula`): un canvas a celle con **copia ombra**;
   `diff()` ritorna solo le celle cambiate (`CUP` per fetta + caratteri). L'app
   scrive il suo frame nel canvas (memoria, nessuna IPC) e manda il
   differenziale. `fullscreen`: da **~660 IPC / 3551 B** a **1 IPC / 33-34 B**
   per un tasto, primo frame intero 1564 B.

`civis` **non** dipende da `tabula`: il canvas produce byte, `Screen` li
bufferizza. Meccanismo e aiuto restano separati (il server non cambia, e la
reply di `SESS_WRITE` continua a significare "è sullo schermo" — i test ci
contano).

### Limiti dichiarati

- **Una cella = un carattere** (larghezza 1). La larghezza di un glifo la sa solo
  il **font** (`glyph_width_cells`, lato console): indovinarla con una tabella
  nel canvas disallineerebbe il modello dallo schermo — ed è la classe di bug
  appena pagata (coordinate). Chi deve disegnare un glifo largo scrive con
  `Screen::write` e conta le celle da sé.
- Il canvas **non** è un toolkit (ADR-0097): niente widget, niente layout,
  niente stile. Solo una superficie di caratteri con la copia ombra.
- Il **resize** azzera le due copie del canvas: dopo un resize si ridisegna
  (l'app lo sa, il resize è notificato).
- Il server **non** raggruppa le write in una finestra: la garanzia "quando
  `SESS_WRITE` ritorna, il frame è a video" resta com'è. Il raggruppamento è
  compito del client (ed è ciò che rende il frame atomico).
- `gtui` (testo + toppe) **non** passa dal canvas: scrive righe di testo e
  toppe, quindi il suo ridisegno (al resize) resta una trentina di feed. È
  un'app di testo, non una tela; se il repaint desse fastidio si bufferizza
  anche lì (debito dichiarato, non un difetto del meccanismo).

### Trappola pagata: il throttle "bulk" del server

Il frame **non si vedeva finché non si premeva un tasto**. `feed_session` nella
console tratta ogni feed **> 256 B** come output di un comando (`bulk`) e ne
**differisce** render+flush ogni 8 write, perché lo scroll di un blocco si
ricompone invece di spostare i pixel (T4). Il primo frame di un'app a schermo
intero è 1564 B: finiva nel modello e restava invisibile finché non arrivava la
write *successiva* (il primo tasto) a fare il flush.

Fix: il throttle vale **solo per il testo** (`mirror`), non per un feed
`screen_only` (`SESS_WRITE.w1 = 1`): una tela manda un frame, non un blocco di
output, e non ha nessun "prompt" che lo flushi dopo. Regola generale: **il
differimento è una politica per il testo che scorre, mai per il disegno.**

### Test

- `libs/tabula` (host, 12): primo frame intero, nessun cambio → zero byte, fette
  consecutive in un solo `CUP`, celle non adiacenti (il cursore che si sposta),
  riga che si accorcia, `clear`, fuori tela, testo che si ferma al bordo, UTF-8
  (bordo e frecce), `resize` che riparte da zero, `fill`, misure minime.
- `scripts/run-tui-tests.sh` (**18 check**): "un tasto = un frame piccolo in una
  feed" (`< 200` byte, `1` chunk), "il primo frame è intero, i successivi
  differenziali" (marker `[fullscreen] frame bytes=N chunks=M`) e "**il primo
  frame è a video senza tasti**" — quest'ultimo con uno **screendump**: la
  fascia in alto a sinistra deve avere pixel chiari (bordo + titolo; senza frame
  è wallpaper, 0). Due trappole di misura: il colore del bordo **non** si può
  cercare esatto (i glifi sono antialiasati sul fondo) e lo screendump va preso
  sulla superficie virtio-gpu (`FB_WINDOW=1`), o prende la VGA — l'altra console,
  "morta" nel path pixel.

### Trappola pagata: il fondo di una cella ridisegnata da sola

In una tile **spostata** (a destra o sotto l'inizio dell'immagine), le celle che
l'app lasciava dietro di sé — la **scia della stella** in `fullscreen` —
mostravano il **colore di pagina** invece del wallpaper.

Il percorso: ogni scrittura dell'app muove il cursore della sessione, e
`render_cursor` ridisegna la cella vecchia e quella nuova con `bg_ready = false`,
cioè dal percorso **legacy** `pagina + layer 0`. Lì l'origine della tile si
applica così:

```rust
let ox = ox.saturating_sub(rx);   // rx = tile.rect.x
let oy = oy.saturating_sub(ry);   // ry = tile.rect.y
```

L'immagine è centrata sullo **schermo** (es. `oy = 29`), la tile in basso
comincia a `ry = 494`: `29 - 494` è **negativo** e `saturating_sub` lo porta a
`0`. L'immagine non "continua dall'alto": **riparte dall'angolo della tile**, e
alla riga del cursore si vedeva la sua zona scura — indistinguibile dalla
pagina. Funzionava solo con la tile in `(0, 0)`: la terza volta, in questo
progetto, che un'origine applicata con l'aritmetica sbagliata produce lo stesso
genere di difetto.

**Decisione (non solo fix): il fondo di una cella ha una sola fonte, la bg
layer.** `draw_cell_at`, quando la banda non è già stata copiata e il fondo è
quello di default (`bg == BLACK`), copia **la cella dal wallpaper**
(`restore_cell_from_bg`, coordinate schermo) e prosegue come se la banda ci
fosse: `disegno inchiostro`. Il percorso `pagina + layer 0` resta solo dove non
c'è una bg layer pixel (fallback VGA) e per i `bg` espliciti con alpha (dove la
composizione dell'alpha del colore è l'unica cosa che sa il risultato). Così
spariscono gli offset da traslare — la classe di bug, non l'istanza.

**Prova d'invarianza** (T4, come t64): il frame raggiunto **per differenziale**
deve essere *identico* a quello di un **repaint da zero**. `fullscreen` ha un
hook `r` (`CONSOLE_REPAINT`, non conta come tasto e non cambia il frame), lo
script fa due screendump e confronta i pixel della tile: `0` diversi. La stessa
prova in `run-tui-tests.sh` (fuori dal gate), più i due check sulla stella:
**un passo = una cella** e il **clamp nel modello** (dal bordo non si
"memorizzano" righe: prima `j` scendeva di 2 e `k` di 1, e la posizione vera
usciva dalla tela mentre il disegno restava incollato al bordo).
