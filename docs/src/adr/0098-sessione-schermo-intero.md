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
