# ADR-0097: Superficie, celle e toppe — il modello grafico del terminale

## Status

**Accepted** (2026-11-26) come **decisione di direzione**. Implementazione:
passo 1 fatto (ADR-0098, schermo intero), passi 2-4 da fare (canvas, toppe,
base grafica). Desiderata #1-#5 in `ROADMAP.md`.

Non introduce codice da sola: fissa le scelte che, se prese dopo, costano una
riscrittura.

## Contesto

Il DE ha **tile** (`TILE_*`), il compositore ha layer e danno (`musivum`), il
server di sessione ha una griglia di celle con scrollback (`littera`) e il
terminal e' gia' rasterizzato a pixel dal font (`term::Surface::Pixel`). Le
domande aperte erano: come convivono testo e grafica? un'app GUI e' una cosa
diversa da una testuale? il disegno lo fa la libreria o il server?

## Decisione

### 1. Una tile e' una **superficie di pixel**; il testo e' una vista derivata

```
tile = rect di pixel (il DE decide, musivum clippa)
 ├─ testo:   Geom::for_rect → celle intere + resto (padding) → glifi
 │           toppe: N×M celle = N*cell_w × M*cell_h px  (quantizzato, voluto)
 └─ GUI:     l'intero rect, origine (0,0), nessuna griglia, nessun padding
```

La griglia **non e' una proprieta' della superficie**: e' il modo in cui il
testo decide di stare in celle intere, perche' il font monospace lo impone.
Una GUI non chiama `Geom` e non eredita padding.

### 2. La toppa e' **contenuto della cella**, non un layer parallelo

Una cella contiene un glifo **oppure** una grafica. La toppa e' ancorata a un
rect di celle, scorre con il testo ed e' dipinta dallo **stesso** percorso
(lettura cella → pixel) che disegna i glifi. Se la toppa vivesse in un layer
separato ci sarebbero due renderer da tenere allineati, e il secondo marcirebbe.

Questo e' anche il motivo per cui il modello non ha "due backend": c'e' una
superficie, e due tipi di contenuto. Un'app che vuole essere bella in entrambi
i mondi usa primitive che degradano bene (barre, sparkline, frame), non
geometria fine.

### 3. La libreria di disegno disegna in **bitmap**, e ha una primitiva `text` a due teste

`rect/hline/bar/sparkline/frame/image` riempiono un buffer di pixel: identiche
in testo (dove il buffer e' grande quanto il rect di celle) e in GUI (dove e'
la superficie della finestra). L'unica primitiva che dipende dal mezzo e'
`text`: in testo si scrive una cella, in GUI si rasterizza a coordinate
arbitrarie con un **font proporzionale** (il modello a celle di `term::font`
non si usa).

### 4. Compositing **annidato**, non duplicato

Ogni livello compone il proprio contenuto (l'app la sua superficie, il server
di sessione la sua griglia, il compositore le finestre): e' lo schema normale
(Wayland docet). Non e' duplicazione **a patto che il codice sia uno** — il
pittore "contenuto di cella", il danno e la superficie stanno in `term`/
`musivum` e si usano a tutti i livelli.

### 5. Niente toolkit di widget unificato

Il livello **disegno** e' condivisibile; il livello **widget** no: ha layout,
albero di focus ed eventi, che un terminale non ha e una GUI non puo' evitare.
Unificarli dall'inizio produce un toolkit con un backend lossy e nessuna storia
buona. I widget arrivano nella fase grafica, sopra il backend a pixel; in
modalita' testo il "widget" e' testo o toppa, senza albero di eventi.

### 6. La porta per il "terminale come app GUI" resta aperta

`littera` e' `no_std` e indipendente dal display **apposta**: il giorno in cui
servisse (performance, testo proporzionale, scroll fluido) la sessione si
**sposta dal server all'app** e il server resta compositore + input. Non e' una
riscrittura, e' una migrazione.

**Regola di disciplina** che ne deriva: niente funzionalita' nuove nel server di
sessione che non avrebbero senso anche quando la sessione sara' un'app. Se una
feature richiede che il *server* sappia cosa l'app sta disegnando, va progettata
come contenuto (cella/toppa), non come servizio.

### 7. I limiti delle toppe, dichiarati

- **Budget per sessione** (contato in `SESS_STATS`, verificabile da un test):
  una schermata di toppe a pixel pieni costa 4 MB, una griglia di testo 73 KB.
- **Niente toppe nello scrollback**: quando escono, si liberano e le celle
  tornano spazi. (E' il punto in cui i terminali con immagini hanno sanguinato.)
- **Alt-text** nella toppa: il mirror seriale e il copia-incolla hanno una
  forma testuale anche per un'immagine (l'invariante "byte in = byte sul filo"
  non si rompe in silenzio).
- **La quantizzazione e' voluta**: in modalita' testo una toppa e' a celle, e il
  padding del tile non e' indirizzabile.

## Conseguenze

- Il **monitor** (desiderata #3) si scrive **una volta** con primitive che
  degradano: oggi in testo, domani in una tile grafica.
- Le immagini inline non sono un mondo a parte: sono lo stesso meccanismo delle
  toppe, quindi **scorrono** (a differenza delle finestre GUI).
- Il compositore e il server di sessione restano due cose distinte con un
  contratto chiaro (pixel + danno), non due implementazioni della stessa cosa.
- Un'app GUI e un'app testuale non sono due tipi di tile: sono due modi di
  riempire la stessa superficie. Il DE non dichiara una "modalita'": chi vuole
  il canvas apre una finestra grande quanto il tile, chi vuole testo scrive
  celle.

## Alternative scartate

- **Due backend della libreria (testo/pixel)**: e' la stessa idea vista dal
  lato sbagliato. La differenza non e' il *medium* (e' sempre pixel) ma il
  *contenuto* (celle + toppe vs superficie libera).
- **Un toolkit unico che "collassa" in glifi**: vedi §5.
- **Immagini come layer dietro la griglia** (sfondo): non serve ne' all'esempio
  git ne' all'immagine inline, che *occupano* le celle.
