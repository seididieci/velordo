# ADR-0100: `formae` — componenti di disegno condivisi fra TUI e GUI

## Status

**Accepted** (2026-11-27). Implementati **M1** (`tabula` a colori, `pictura`
con alpha/gradiente/arrotondati/polilinea, blit delle toppe alpha-aware) e
**M2** (`libs/formae`: geometria, tema, `Target`, `Frame`, `Chart`, `Bar`,
`History`, `fmt`, con unit test host). **M3** e' il primo consumatore vero
(`usr/bin/monitor`); **M4** e' `scripts/run-monitor-tests.sh`.

Dipende da ADR-0097 (modello grafico), ADR-0098 (schermo intero, il frame e'
l'unita' di disegno), ADR-0099 (toppe di pixel nelle celle).

## Contesto

ADR-0097/0099 hanno dato il **meccanismo** del disegno: una cella puo'
contenere un glifo **o** una toppa di pixel, e la toppa scorre con il suo
testo. Mancava il livello **componenti**: una barra, un grafico a area, una
tabella, i formati dei numeri. Il primo consumatore e' il monitor di sistema
(una dashboard a 1 Hz), ma lo stesso codice dovra' servire alla futura
**finestra grafica** — dove il font non sara' necessariamente monospazio.

Il rischio da evitare e' quello classico: scrivere i componenti "per la TUI"
(posizioni in celle, testo a colpi di `col/row`) e doverli riscrivere per la
GUI, oppure scriverli "per la GUI" (tutto in pixel) e perdere l'allineamento a
colonne che rende leggibile un terminale.

## Decisione

### 1. Il confine e' un `Target` (single source)

`libs/formae` e' **puro** (`no_std`, nessuna dipendenza da `civis`, workspace
escluso, unit test host come `pictura`/`tabula`). I componenti disegnano solo
attraverso il trait `Target`:

```rust
pub trait Target {
    fn grid(&self) -> (u32, u32);          // cella del font (TUI) o (1,1) GUI
    fn size_px(&self) -> (u32, u32);
    fn measure(&self, s: &str, st: Style) -> (u32, u32);
    fn snap_x(&self, x: u32) -> u32;       // default: giu' alla griglia
    fn text(&mut self, x: u32, y: u32, s: &str, st: Style) -> u32;
    fn fill(&mut self, r: PxRect, c: Color);
    fn border(&mut self, r: PxRect, t: u32, c: Color);
    fn vgradient(&mut self, r: PxRect, top: Color, bottom: Color);
    fn round(&mut self, r: PxRect, radius: u32, c: Color);
    fn bar(&mut self, r: PxRect, permille: u32, bg: Color, fg: Color);
    fn line(&mut self, a: (u32, u32), b: (u32, u32), t: u32, c: Color);
}
```

Oggi c'e' **un'implementazione**: `PatchTarget`, che disegna i pixel in una
toppa `pictura` e scrive il testo nella tela a celle `tabula`. Domani ci sara'
`GuiTarget`, sulla superficie della finestra.

### 2. Layout in **celle**, disegno in **pixel**, testo **misurato**

- L'app ragiona in celle (`CellRect`, il suo spazio naturale) e converte con
  `CellRect::px(metrics)` prima di chiamare un componente.
- Un componente riceve un `PxRect` e disegna solo in pixel.
- Il testo **non** si posiziona a colpi di `col/row`: si chiede `measure` e si
  scrive a una `x` in pixel. `snap_x` e' l'unico punto in cui esiste la
  nozione di "cella", e nella GUI e' l'identita' (griglia `(1,1)`).

Cosi' la stessa `Bar`/`Chart` e' cella-allineata nella TUI (perche'
`measure` e' `caratteri × cella`) e proporzionale in una GUI con font vero —
senza riscrivere il componente. Non e' un toolkit: **immediate mode**, nessun
albero di widget, nessun event loop, nessuna invalidazione; lo stato (storia,
selezione, ordinamento) lo possiede l'app.

### 3. Il frame e' dell'app

`formae::Frame` compone le **toppe** (`pictura`) e il **differenziale** delle
celle (`tabula`). Non fa IPC: l'app manda prima le toppe (`sess::patch`), poi
il differenziale (`Screen::present`). L'ordine conta: i pixel sono il fondo
delle card, i glifi ci stanno sopra.

### 4. Colore per cella (M1)

`tabula` memorizza `(char, fg, bg)`; `diff` emette un `SGR` **solo al cambio**
di stile e chiude con `ESC[0m`, cosi' il colore non cola nel prompt dopo
un'app a schermo intero. Un'app monocroma (tutte le celle di default) produce
**esattamente** i byte di prima: nessun `SGR`, nessun reset — la regressione
di `fullscreen`/`gtui` lo prova.

### 5. Toppe con alpha (M1)

Il blit della console (`term::surface::Pixel::blit`) compone quando la riga ha
alpha < 255; una riga tutta opaca si copia come sempre (percorso veloce, byte
identici). E' cosi' che un angolo arrotondato o un'ombra lasciano vedere il
wallpaper sotto. La regola che ne segue: un testo sopra una toppa ha `bg` di
default (solo inchiostro); un `bg` colorato **copre** la toppa, ed e' voluto
solo per un'evidenziazione.

### 6. Contenuto (volutamente poco)

| elemento | perche' condividerlo |
|---|---|
| `PxRect` / `CellRect` / `Metrics` | geometria unica |
| `Theme` (+ `cell_color`) | i token del look in un posto solo |
| `Target` / `Frame` / `PatchTarget` | il confine; la TUI lo implementa con `tabula` + `pictura` |
| `Chart` | area con gradiente + linea + alone (la parte che nessuno vuole riscrivere) |
| `Bar` | track + fill proporzionale (la forma la sceglie il target) |
| `History<N>` | serie storica ad anello, sopravvive ai resize |
| `fmt` | tick→`hh:mm:ss`, permille→`%`, frame→MiB/GiB (puri, testati) |

**Fuori**: card con bordo (c'e' il bordo della tile), tabella (la calcola
l'app, tutta testuale), titoli/valori come widget (sono testo + `measure`).

## Conseguenze

### Positive

- Il monitor usa la stessa `Chart`/`Bar`/`History` che usera' la GUI: il
  confine `Target` e' gia' provato da un consumatore vero.
- Le app monocrome restano byte per byte identiche (regressione TUI).
- I colori del testo sbloccano dashboard, editor e ogni app futura senza
  aspettare la GUI.
- `formae` si prova sull'host in millisecondi (12 unit test, due metriche).

### Negative

- Disegnare per card = **piu' toppe per frame** (una per card). Nel monitor
  sono 2-3 a 1 Hz: misurato in `patches=`/`bytes=` del marker seriale. Se
  pesasse, la prima leva e' accorpare chart+barra della stessa card; mai una
  toppa a schermo intero.
- Due convenzioni di colore convivono: `pictura` (`0xAARRGGBB`, framebuffer) e
  `tabula`/term (`0xRRGGBBAA`, celle). La conversione e' `theme::cell_color`,
  in un punto solo.
- `measure`/`snap_x` sono un'astrazione che nella TUI e' banale: il costo e'
  pagato ora per non riscriverla dopo.

### Neutral

- Nessuna modifica al kernel, nessuna syscall/tag nuova.
- `libs/formae` dipende solo da `pictura` e `tabula` (entrambi puri).

## Alternative considerate

- **Componenti dentro `pictura`**: rifiutato — `pictura` e' il livello
  primitive (pixel), i componenti hanno bisogno del testo misurato e del
  canvas a celle.
- **Toolkit retain-mode (albero di widget + invalidazione)**: rifiutato —
  ADR-0097 lo esclude; per una dashboard immediate-mode e' piu' semplice e
  non impone un modello di proprieta' dei widget.
- **Layout a pixel puri**: rifiutato — perderebbe l'allineamento a colonne che
  rende leggibile la TUI; la GUI lo recupera con `measure`, non con un secondo
  layout.
- **Tabella come widget grafico**: rifiutato (per ora) — e' testo su celle, e
  il monitor la calcola da se' (larghezze, allineamento, selezione).

## Riferimenti

- ADR-0097 — superficie, celle e toppe
- ADR-0098 — sessione a schermo intero; il frame e' l'unita' di disegno
- ADR-0099 — toppe di pixel nelle celle
- `libs/formae/src/*`, `libs/pictura/src/lib.rs`, `libs/tabula/src/lib.rs`
- `userland/monitor`, `scripts/run-monitor-tests.sh`
