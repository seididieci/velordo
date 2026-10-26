# ADR-0063: lo sfondo immagine del terminale (layer 0)

## Status

Accepted. Implementata (Fase 60, 26 ottobre 2026). Dipende da ADR-0060
(console virtuale: modello a celle, surface pixel) e ADR-0061 (modeset e
host-sync: la risoluzione cambia a runtime).

## Context

Il terminale pixel disegnava il solo testo su un fondo di pagina
`#0B0D14`: la superficie fuori dalla griglia non veniva mai scritta (restava
il nero dell'allocazione) e non c'era modo di avere un'immagine di sfondo.
Il requisito: un logo **centrato** (`assets/bg.bmp`, 585x135, BITMAPV5 a 32
bit con maschere BGRA e canale alpha) **dietro** il testo — visibile dove il
testo e le celle non lo coprono, con il fondo di pagina corretto attorno.

Il primo tentativo (copia cruda del BMP nel framebuffer) ha prodotto tre
difetti indipendenti, tutti trovati sul framebuffer vero con `screendump`
headless:

1. **Le celle con testo cancellavano l'immagine.** Solo le celle vuote
   venivano "ripristinate" dal layer 0; una cella con un carattere veniva
   ridipinta di netto, immagine compresa.
2. **Sbavatura verticale.** L'offset sorgente per riga era
   `sy0 * w_img` invece di `sy0 * w_img * 4` (byte vs pixel): la **prima**
   riga tornava, tutte le successive leggevano qualche riga piu' in basso e
   il logo diventava una striscia. Un confronto limitato alla riga 0 dava
   "0 differenze" — il bug era invisibile proprio dove si guardava per primo.
3. **Sfondo rosso scuro.** Il colore era documentato come `0xRRGGBBAA` ma
   le quattro funzioni (`rgba`, `rgba_to_bgra`, `alpha`, `nearest_vga`) lo
   decodificavano al contrario (`0xAARRGGBB`): `BLACK` risultava
   semi-trasparente, il fondo veniva ri-blendato a ogni cella e accumulava
   verso `(49, 0, 0)`.

In piu', l'antialiasing dei glifi blendava contro `bg_color` invece che
contro il **pixel di destinazione** vero (immagine compresa): alone nero
attorno alle lettere sopra il logo.

## Decision

Lo sfondo del terminale e' uno **stack di strati**, dal basso:

1. **colore di pagina** — `rgba_to_bgra(color::BLACK)` (`#0B0D14`) su
   **tutta** la superficie, margini della griglia compresi;
2. **layer 0** — il BMP centrato, **composto con il suo alpha** (alpha non
   premoltiplicato);
3. **glifo** — blendato contro il pixel realmente presente.

Regole:

- **L'alpha dell'immagine conta.** Un pixel `a = 0` non dipinge: resta il
  colore di pagina (lo sfondo del BMP e' trasparente, non "quasi nero");
  `a = 255` scrive il pixel; i valori intermedi sono il blend dei due. Il
  logo resta identico (i suoi pixel sono quasi tutti opachi), ma la banda
  dell'immagine **non si vede piu'** come rettangolo.
- **Una sola primitiva di compositing** (`Pixel::blend_bgra_span`, in
  `libs/term`): la usano i tre percorsi — fondo pieno (`draw_layer0_full`),
  cella vuota (`restore_layer0_cell`), cella con carattere (`put_char`). Il
  testo non ha un percorso "speciale": ricompone la cella (fondo → immagine)
  e poi disegna il glifo sopra, cosi' non serve ridisegnare lo schermo.
- **Gli offset si ricalcolano sulla superficie corrente**: all'avvio e a
  **ogni resize** (D4). Il resize sostituisce il buffer device: senza un
  ridisegno del fondo l'immagine resterebbe alla posizione della
  risoluzione precedente e i margini neri.
- **La convenzione dei colori e' un invariante**: `0xRRGGBBAA` (rosso nel
  byte alto, alpha nel byte **basso**). Le funzioni di conversione e
  `Cell::bg_transparent` la rispettano.

## Consequences

- Il percorso per cella resta O(celle sporche): il fill dell'intera
  superficie avviene solo all'avvio e al resize (un `screendump` di
  1280x800 = 4 MB).
- La verifica non e' "a occhio": il frame deve coincidere con il BMP
  **composto** (alpha su `#0B0D14`). Misura: 0 pixel diversi su 78.975
  (scarto massimo 0), margini `(11, 13, 20)`.
- Regressioni coperte da **unit test host** (fuori dal gate) in
  `libs/term/src/surface.rs`: offset di riga del layer 0 (riga per riga),
  alpha della composizione (0 / 0x40 / 0x80 / 0xFF), convenzione dei
  colori. Il gate QEMU resta a 6 righe (nessuna suite nuova): il resize e'
  gia' coperto da `t63` (modeset + rifiuto oltre-bound).
- Il BMP e' letto **tutto in RAM** all'avvio (come il font): niente I/O FS
  durante il servizio (il cardo e' single-threaded).

## Lezioni

- **Una convenzione di colore sbagliata non crasha**: produce un colore
  plausibile ma sbagliato (qui, un accumulo verso il rosso). Va fissata con
  un test, non con l'occhio.
- **Un `* 4` mancante in uno stride si nasconde nella prima riga**: il
  confronto "riga 0 identica" e' la trappola; il test deve coprire righe
  diverse.
- **Il blend deve leggere il pixel di destinazione vero**: qualunque
  scorciatoia (`bg_color` noto) e' sbagliata appena sotto il glifo c'e' un
  altro strato.
