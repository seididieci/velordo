# ADR-0099: Toppe di pixel nelle celle — `SESS_PATCH`, vita = testo

## Status

**Accepted** (2026-11-26). Implementata **2a** (griglia con segmenti, ABI,
libreria `pictura`, unit test) e **2b** (la prima GTUI: `usr/bin/gtui`, log con
i rami disegnati + barra + sparkline, e la prova che le toppe vivono quanto il
loro testo). Gate invariato; `run-tui-tests.sh` PASS con quattro check nuovi.

Dipende da ADR-0097 (modello grafico), ADR-0098 (schermo intero e notifica di
resize), ADR-0089/0090 (sessioni, scrollback).

## Contesto

ADR-0097 ha deciso che una cella può contenere un glifo **o** una toppa di
pixel, e che la toppa **scorre** con il suo testo. Restavano da decidere tre
cose che, prese male, costringono a riscriverle:

1. **quanto vive** una toppa (budget? eviction?);
2. **dove sta** nella griglia (e come scorre e va in cronologia);
3. **chi** paga il conto in memoria, e cosa succede quando finisce.

## Decisione

### 1. La toppa vive quanto il suo testo. Nessuna eviction.

Un segmento è **della riga**: quando la riga esce dalla griglia va nello
scrollback *con lui*; quando la cronologia scarta la riga (limite
`scrollback_max`, che esisteva già) i suoi pixel si liberano. Nessun contatore
separato, nessuna politica che scelga quali toppe valgono: **un log lungo con
poche toppe per riga non perde niente**.

L'unico tetto è `PATCH_MAX_BYTES` (8 MiB per sessione) e si controlla
**all'ingresso** di una toppa nuova: oltre il tetto la toppa viene **rifiutata**
(`!0`, l'app decide), **mai** una già presente viene tolta. È un guardrail
contro un `ThirdParty` che riempie 1000 righe di immagini, non un budget.

Altre due guardie sono strutturali: la toppa deve stare nella griglia e avere
una misura coerente con le metriche (`cells*cell_w × rows*cell_h`).

### 2. Il segmento sta nella riga; la griglia non conosce il font

```
Grid
 ├─ cells:    Vec<Cell>            (Copy, intatto)
 ├─ segments: Vec<Vec<Segment>>    una lista per riga
 └─ scrollback: VecDeque<Row { cells, segments }>
```

`Segment { col, cells, px_w, px_h, bitmap, alt }`: la griglia tratta
`px_w`/`px_h` come **dati opachi** — le metriche le ha solo chi ha il font (la
console), che le valida e deriva dal numero di celle. Così `term` resta senza
dipendenze dal font e i suoi unit test sono pixel-free.

Le operazioni che spostano righe (`scroll_cells`, `scroll_model_n`,
`scroll_up*`) portano i segmenti con sé (una `rotate_left`). **Il resize
(`Grid::resize`) li scarta tutti**: il reflow sposta le celle di colonna e di
riga, quindi un'ancora a celle non ha più senso — meglio perderli che disegnarli
dove non c'entrano. Il resize è **notificato** (ADR-0098), quindi l'app
ridisegna. `clear` (`ESC[2J`) libera tutto.

### 3. Sotto i glifi

Il render disegna, per riga: **fondo** (banda del wallpaper) → **toppe** →
**glifi**. Una cella vuota mostra la toppa; un glifo la copre. Conseguenze:
niente marcatore nella cella, niente split quando il testo scrive sopra, e
l'esempio d'uso (i rami di un `git log` disegnati in pixel, con il testo nelle
colonne accanto) funziona senza casi speciali. Per le immagini inline l'app
lascia vuote le celle che copre.

Il blit passa dalla **vista** della tile (`Pixel::view`): il clip è il rect
della tile, una toppa non può scrivere nella vicina. Nel fallback VGA le toppe
non si disegnano (non ci sono pixel).

### 4. `alt-text`: la toppa ha una forma testuale

`SESS_DUMP` rende l'`alt` della toppa al posto delle celle che copre
(imbottito/troncato a `cells`), e una riga con una toppa è "con contenuto"
anche se le celle sono vuote. Serve a due invarianti che non vogliamo perdere:
il **dump** (e quindi un futuro copia-incolla) di una schermata con grafica
resta leggibile, e il mirror seriale non finge che la riga sia vuota.

### 5. ABI: tre tag, zero shm

| tag | nome | semantica |
|---|---|---|
| `0xB8` | `SESS_METRICS` | → `w0` = larghezza cella in px, `w1` = altezza. Il client **non può indovinare** i pixel di una cella: li sa solo il font. |
| `0xB9` | `SESS_PATCH_BEGIN` | `w0 = x \| y<<16`, `w1 = cols \| rows<<16 \| alt_len<<32`; alt nella metà OUT → `0`/`!0`. Uno staging per sessione. |
| `0xBA` | `SESS_PATCH_DATA` | `w0 = offset`, `w1 = len`, byte nella metà OUT → `0`/`!0`. **Commit implicito** a `offset+len == atteso`; offset fuori ordine o overflow ⇒ `!0` e staging scartato. |

Niente memoria condivisa: il bitmap viaggia nei 2048 B della mailbox. Per una
toppa da poche celle sono una o due andate; per un'immagine grande servirà la
shm (dichiarato fuori scope: un'immagine a schermo intero sarebbero ~230
andate).

Permessi: **come `SESS_WRITE`** (attaccato o privilegiato). Una toppa è
contenuto, non un diritto nuovo.

### 6. `SESS_STATS` misura, non decide

`w1` = focus nei bit 0..8, **byte delle toppe** nei bit 8.. . Serve ai test
("quando la riga esce, i byte scendono") e agli strumenti; nessuna decisione
del server si basa su quel numero (il tetto si controlla all'ingresso).

## Conseguenze

- Il **monitor** (desiderata #3) e un log con rami disegnati si scrivono con le
  stesse primitive, e la grafica scorre nella cronologia come il testo.
- Il costo in memoria è **quello che l'app chiede**, per il tempo in cui il
  testo vive; nessun `Vec` di pixel sopravvive alla sua riga.
- `resize` e toppe non convivono: dopo un reflow le toppe spariscono (e l'app
  lo sa). È l'unico posto in cui "perdere" è la risposta giusta.
- `Cell` resta `Copy`: la toppa è un vicino di riga, non un campo della cella.

## Trappole pagate (2b)

- **La cella vuota ridipingeva il fondo sopra la toppa.** Il render disegna il
  fondo (banda), poi le toppe, poi le celle: una cella *vuota* ripassava il suo
  fondo e cancellava la toppa. Il grafo del log si vedeva solo dove c'era
  testo. Fix: `blank_over_patch` — una cella vuota sopra una toppa si salta
  (il cursore no: deve restare visibile). L'ha trovato lo **screendump**: il
  CRC delle righe cambiava (i pixel c'erano) ma i colori attesi non si
  vedevano.
- **`ESC[2J` non muove il cursore** (giusto: e' un terminale vero, ADR-0098).
  Chi ridisegna da capo deve mandare anche `ESC[H`, o disegna dal fondo e la
  prima riga scorre via.

## Trappole pagate

- **Il commit non disegna da solo.** `SESS_PATCH_DATA` che completa la toppa
  non produce nessun evento: senza un `render()+flush` **nel handler**, i
  pixel sono nella griglia e mai sullo schermo. L'ha detto il CRC della riga
  (identico prima e dopo) — non il conteggio dei byte, che era giusto.
- **L'ordine del render**: fondo → toppe → glifi. Mettendo le toppe prima della
  banda di background la si vede sparire (la banda le copre).
- **`J`/`K` con default sbagliato** (vedi ADR-0098) ha insegnato la regola
  generale: nei `Segment` la griglia non deve presumere nulla che non le sia
  stato detto (px opachi, metriche alla console).

## Test

- `libs/term` (host): la toppa **scorre** con la riga e **muore** quando la
  cronologia la scarta (limite 2 righe); riga fuori griglia / toppa più larga
  rifiutate; il reflow la scarta; più toppe sulla stessa riga (l'ultima vince).
- `libs/littera` (host): staging (uno per volta, offset sbagliato, commit solo
  a misura esatta), **tetto solo-rifiuto** (la toppa nuova oltre il tetto è
  rifiutata, quella presente resta), alt-text nel dump, `ESC[2J` che libera.
- `libs/pictura` (host): `fill`/`clip`, bordo, barra proporzionale, sparkline
  scalata, linea con spessore — pixel per pixel.
- `scripts/run-tui-tests.sh` (fuori dal gate): `fullscreen` con `p` disegna una
  toppa 12×1 celle (10560 byte) e il **CRC della riga cambia** (i pixel sono
  arrivati al framebuffer), poi il resto del giro (tasti, modeset, uscita).
