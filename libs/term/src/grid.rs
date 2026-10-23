//! Modello a celle del terminale (D3/D4).
//!
//! La griglia e' **il modello**: indipendente dalla surface (pixel o VGA
//! testuale). Ogni mutazione segna la zona sporca: le surface ridisegnano
//! solo quella (una cella per un tasto, tutto per uno scroll).
//!
//! La cella tiene un **codepoint** (u32), non un byte: le icone Nerd (PUA)
//! sono codepoint multi-byte in UTF-8 (D3).
//!
//! I colori sono truecolor **RGBA packed** (`0xRRGGBBAA`, alpha opaco per
//! default). Il background puo' avere alpha < 0xFF per trasparire verso il
//! layer 0 (sfondo immagine); il foreground resta opaco per ora (alpha sui
//! caratteri rinviata a quando ci saranno le immagini).
//!
//! D4: la griglia ha uno **scrollback** (le righe che escono dal basso) e
//! sa **ridimensionarsi** (`resize`): il testo viene rilegto come un flusso e
//! rimesso nella nuova griglia (le righe troppo lunghe si spezzano: reflow
//! da terminale, non da paragrafo). Serve al re-adattamento al resize del
//! display: il terminale si riallinea **senza perdere il testo**.

extern crate alloc;

use alloc::vec::Vec;

/// Righe di scrollback tenute (oltre quelle della griglia).
const SCROLLBACK_MAX: usize = 1000;

/// Colori truecolor predefiniti (RGBA packed, `0xRRGGBBAA`, alpha opaco).
///
/// Gli slot 0–15 sono la palette ANSI convenzionale: molti programmi li
/// usano per nome. Gli slot 8–15 sono i "bright" corrispondenti.
/// `BLACK` non e' nero puro: il blu-notte #0B0D14 rende l'interfaccia
/// piu' morbida.
pub mod color {
    // Base (slot ANSI 0–7).
    pub const BLACK: u32        = 0x0B0D_14FF;
    pub const RED: u32          = 0xFF5C_70FF;
    pub const GREEN: u32        = 0x55D6_A7FF;
    pub const YELLOW: u32       = 0xF5C7_6AFF;
    pub const BLUE: u32         = 0x5B7C_FFFF;
    pub const MAGENTA: u32      = 0xC45C_FFFF;
    pub const CYAN: u32         = 0x42D9_F5FF;
    pub const WHITE: u32        = 0xE8EB_F2FF;

    // Bright (slot ANSI 8–15).
    pub const GRAY: u32         = 0x858D_A3FF;
    pub const BRIGHT_RED: u32   = 0xFF84_93FF;
    pub const BRIGHT_GREEN: u32 = 0x7BE8_BCFF;
    pub const BRIGHT_YELLOW: u32 = 0xFFDA_85FF;
    pub const BRIGHT_BLUE: u32  = 0x8BA3_FFFF;
    pub const BRIGHT_MAGENTA: u32 = 0xD991_FFFF;
    pub const BRIGHT_CYAN: u32  = 0x7AE7_F7FF;
    pub const BRIGHT_WHITE: u32 = 0xFFFF_FFFF;

    pub const DEFAULT_FG: u32   = WHITE;
    pub const DEFAULT_BG: u32   = BLACK;
}

/// Una cella della griglia: il carattere e i colori truecolor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Codepoint Unicode (0 = spazio).
    pub cp: u32,
    /// Colore foreground truecolor (RGBA packed, `0xRRGGBBAA`).
    /// Opaco per ora: alpha sui caratteri e' rinviata.
    pub fg: u32,
    /// Colore background truecolor (RGBA packed, `0xRRGGBBAA`).
    /// Alpha < 0xFF = trasparente verso il layer 0 (sfondo immagine).
    pub bg: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            cp: b' ' as u32,
            fg: color::DEFAULT_FG,
            bg: color::DEFAULT_BG,
        }
    }
}

impl Cell {
    /// La cella e' "vuota" (solo spazio: i colori non contano).
    pub fn is_blank(&self) -> bool {
        self.cp == b' ' as u32
    }

    /// Il background della cella e' trasparente (alpha < 0xFF).
    pub fn bg_transparent(&self) -> bool {
        (self.bg >> 24) & 0xFF < 0xFF
    }
}

/// Zona sporca (inclusiva): `None` = tutto pulito.
#[derive(Clone, Copy, Default)]
pub struct Dirty {
    pub min_col: usize,
    pub min_row: usize,
    pub max_col: usize,
    pub max_row: usize,
    pub any: bool,
}

impl Dirty {
    /// Nuova zona sporca vuota.
    pub fn none() -> Self {
        Dirty {
            min_col: usize::MAX,
            min_row: usize::MAX,
            max_col: 0,
            max_row: 0,
            any: false,
        }
    }

    /// Segna la cella (col,row) come sporca.
    pub fn mark(&mut self, col: usize, row: usize) {
        if !self.any {
            self.any = true;
            self.min_col = col;
            self.max_col = col;
            self.min_row = row;
            self.max_row = row;
            return;
        }
        if col < self.min_col {
            self.min_col = col;
        }
        if col > self.max_col {
            self.max_col = col;
        }
        if row < self.min_row {
            self.min_row = row;
        }
        if row > self.max_row {
            self.max_row = row;
        }
    }

    /// Segna tutto.
    pub fn mark_all(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.mark(0, 0);
        self.mark(cols - 1, rows - 1);
    }
}

/// La griglia del terminale: le celle, il cursore, lo scrollback, la zona
/// sporca.
pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    /// Le righe uscite dal basso della griglia (dalla piu' vecchia).
    scrollback: Vec<Vec<Cell>>,
    /// Posizione del cursore (colonna, riga).
    pub cursor: (usize, usize),
    /// Cursore visibile.
    pub cursor_visible: bool,
    dirty: Dirty,
}

impl Grid {
    /// Nuova griglia `cols×rows` (spazi, fg 0x0F su bg 0x00).
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut cells = Vec::with_capacity(cols * rows);
        for _ in 0..(cols * rows) {
            cells.push(Cell::default());
        }
        Grid {
            cols,
            rows,
            cells,
            scrollback: Vec::new(),
            cursor: (0, 0),
            cursor_visible: true,
            dirty: Dirty::none(),
        }
    }

    /// Colonne della griglia.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Righe della griglia.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// La cella (col,row).
    pub fn cell(&self, col: usize, row: usize) -> Cell {
        if col >= self.cols || row >= self.rows {
            return Cell::default();
        }
        self.cells[row * self.cols + col]
    }

    /// Scrive un codepoint nella cella (col,row) con colori truecolor e segna
    /// la zona sporca.
    pub fn put(&mut self, col: usize, row: usize, cp: u32, fg: u32, bg: u32) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        let cell = Cell { cp, fg, bg };
        let idx = row * self.cols + col;
        if self.cells[idx] != cell {
            self.cells[idx] = cell;
            self.dirty.mark(col, row);
        }
    }

    /// Pulisce tutta la griglia (azzera le celle, cursore a casa).
    pub fn clear(&mut self) {
        for c in self.cells.iter_mut() {
            *c = Cell::default();
        }
        self.cursor = (0, 0);
        self.dirty.mark_all(self.cols, self.rows);
    }

    /// Scorri in alto di una riga: la riga che esce va nello **scrollback**
    /// (D4: serve a conservare il testo al re-adattamento).
    pub fn scroll_up(&mut self) {
        if self.rows == 0 {
            return;
        }
        let first: Vec<Cell> = self.cells[..self.cols].to_vec();
        self.scrollback.push(first);
        if self.scrollback.len() > SCROLLBACK_MAX {
            self.scrollback.remove(0);
        }
        self.cells.copy_within(self.cols.., 0);
        let last = self.rows - 1;
        for col in 0..self.cols {
            self.cells[last * self.cols + col] = Cell::default();
        }
        self.dirty.mark_all(self.cols, self.rows);
    }

    /// La zona sporca e la azzera (la surface la consuma).
    pub fn take_dirty(&mut self) -> Dirty {
        let d = self.dirty;
        self.dirty = Dirty::none();
        d
    }

    /// Riga e colonna del cursore, limitati alla griglia.
    pub fn cursor_clamped(&self) -> (usize, usize) {
        (
            self.cursor.0.min(self.cols.saturating_sub(1)),
            self.cursor.1.min(self.rows.saturating_sub(1)),
        )
    }

    /// Numero di celle non vuote (il contenuto del terminale).
    pub fn non_empty(&self) -> usize {
        self.cells.iter().filter(|c| !c.is_blank()).count()
    }

    /// CRC32 del testo (solo i codepoint, nell'ordine): serve da prova
    /// automatica che il testo e' sopravvissuto a un re-adattamento.
    pub fn text_crc(&self) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for c in self.cells.iter() {
            if c.is_blank() {
                continue;
            }
            let mut v = c.cp;
            for _ in 0..4 {
                crc ^= (v & 0xFF) as u32;
                crc = crc.wrapping_mul(0x0100_0193);
                v >>= 8;
            }
        }
        crc
    }

    /// **Ridimensiona** la griglia conservando il testo (D4).
    ///
    /// Il contenuto (scrollback + griglia) viene rilegto come un flusso di
    /// righe e rimesso nella nuova griglia: le righe piu' lunghe della nuova
    /// larghezza si spezzano (reflow da terminale). Se il testo non sta, le
    /// righe piu' vecchie vengono perse.
    ///
    /// **Il cursore mantiene la sua posizione** (riga e colonna): il
    /// contenuto viene ancorato in modo che la riga del cursore resti
    /// dov'era. Prima finiva *sempre* in fondo e il testo era ancorato al
    /// basso: crescendo la finestra tutto il testo scendeva di N righe e il
    /// prompt finiva sul bordo — il terminale "saltava" invece di adattarsi.
    /// Ora la finestra che cresce allarga il terminale e il contenuto resta
    /// fermo; quando il testo non sta piu' (finestra che si restringe) il
    /// fondo e' ancorato al basso e il cursore resta visibile.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        // Il cursore, nel flusso delle righe (scrollback + griglia): la riga
        // che occupa e la colonna (la colonna puo' essere oltre la fine del
        // testo: il cursore a capo).
        let cursor_row = self.cursor.1.min(self.rows.saturating_sub(1));
        let cursor_col = self.cursor.0;
        let cursor_src = self.scrollback.len() + cursor_row;

        // 1) Le righe, dalla piu' vecchia: scrollback + griglia corrente.
        let mut lines: Vec<Vec<Cell>> = Vec::with_capacity(self.scrollback.len() + self.rows);
        for row in self.scrollback.iter() {
            lines.push(trim_row(row));
        }
        for r in 0..self.rows {
            lines.push(trim_row(&self.cells[r * self.cols..(r + 1) * self.cols]));
        }
        // 2) Via le righe vuote in coda (il testo finisce dove finiva), MA
        //    **non oltre la riga del cursore**: il cursore sta spesso su
        //    una riga vuota (dopo l'ultimo testo) e deve restare li' —
        //    senza questo la sua riga sparirebbe dal flusso e il cursore
        //    finirebbe in fondo (il comportamento di prima).
        while lines.len() > cursor_src + 1 {
            match lines.last() {
                Some(last) if last.is_empty() => {
                    lines.pop();
                }
                _ => break,
            }
        }
        // Se la riga del cursore era vuota e in coda, e' stata rimossa: il
        // cursore finisce in fondo all'ultima riga (il default sotto).
        let cursor_live = cursor_src < lines.len();

        // 3) Spezza le righe troppo lunghe alla nuova larghezza, tenendo il
        //    posto del cursore (indice piatto della riga + colonna).
        let mut flat: Vec<Vec<Cell>> = Vec::new();
        let mut cursor_flat: Option<(usize, usize)> = None;
        for (src_idx, line) in lines.iter().enumerate() {
            if line.is_empty() {
                flat.push(Vec::new());
                // Riga **vuota**: puo' essere quella del cursore (il
                // cursore sta spesso sotto l'ultimo testo). La colonna non
                // si taglia (non c'e' testo): il cursore resta dov'e'.
                if cursor_live && src_idx == cursor_src {
                    cursor_flat = Some((flat.len() - 1, cursor_col));
                }
                continue;
            }
            let mut i = 0;
            while i < line.len() {
                let end = (i + new_cols).min(line.len());
                flat.push(line[i..end].to_vec());
                if cursor_live && src_idx == cursor_src {
                    // Il cursore puo' essere oltre la fine del testo della
                    // riga: in quel caso resta in coda all'ultimo pezzo.
                    let col = cursor_col.min(line.len());
                    if col >= i && col <= end {
                        cursor_flat = Some((flat.len() - 1, col - i));
                    }
                }
                i = end;
            }
        }

        // 4) La nuova griglia: il contenuto ancorato in modo che il cursore
        //    resti sulla sua riga (l'alto se la finestra cresce, il basso se
        //    il testo non sta piu').
        let (flat_cursor, cursor_col) = cursor_flat.unwrap_or_else(|| {
            let last = flat.len().saturating_sub(1);
            let col = flat.last().map(|l| l.len()).unwrap_or(0);
            (last, col)
        });
        let max_start = flat.len().saturating_sub(new_rows);
        // Il cursore deve restare nella griglia (il suo offset piatto non
        // puo' scendere sotto le ultime `new_rows` righe) e si vuole la sua
        // riga di prima: il contenuto e' ancorato di conseguenza.
        let min_start = (flat_cursor + 1).saturating_sub(new_rows);
        let start = flat_cursor
            .saturating_sub(cursor_row)
            .max(min_start)
            .min(max_start);
        let mut cells = alloc::vec![Cell::default(); new_cols * new_rows];
        for (dst_row, src) in flat[start..].iter().enumerate() {
            for (c, cell) in src.iter().enumerate() {
                if c < new_cols {
                    cells[dst_row * new_cols + c] = *cell;
                }
            }
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.cells = cells;
        // Il contenuto ora e' nella griglia: lo scrollback si svuota (le righe
        // piu' vecchie che non ci stanno sono perse).
        self.scrollback.clear();
        // Il cursore: stessa riga (dove il testo lo lascia), colonna seguita
        // nel reflow.
        self.cursor = (
            cursor_col.min(new_cols.saturating_sub(1)),
            flat_cursor - start,
        );
        self.dirty.mark_all(new_cols, new_rows);
    }
}

/// La riga tagliata all'ultimo carattere non vuoto.
fn trim_row(row: &[Cell]) -> Vec<Cell> {
    let mut end = row.len();
    while end > 0 && row[end - 1].is_blank() {
        end -= 1;
    }
    row[..end].to_vec()
}

#[cfg(test)]
mod tests {
    //! Il cursore al re-adattamento (D4, host, `cargo test`).
    //!
    //! Il terminale che cresce deve **allargarsi**, non far saltare il
    //! testo: prima il cursore finiva sempre in fondo e il contenuto era
    //! ancorato al basso, quindi al resize la finestra tutto scendeva di N
    //! righe e il prompt finiva sul bordo.

    use super::*;
    use alloc::string::String;

    /// Mette `s` nella riga `row` dalla colonna 0.
    fn put_str(g: &mut Grid, row: usize, s: &str) {
        for (i, b) in s.bytes().enumerate() {
            g.put(i, row, b as u32, color::WHITE, color::BLACK);
        }
    }

    /// Il testo della riga (senza gli spazi finali).
    fn row_str(g: &Grid, row: usize) -> String {
        let mut s = String::new();
        for col in 0..g.cols() {
            let c = g.cell(col, row);
            s.push(if c.cp == 0 {
                ' '
            } else {
                char::from_u32(c.cp).unwrap_or('?')
            });
        }
        s.trim_end().to_string()
    }

    /// Il cursore resta sulla stessa riga (e colonna) quando la griglia
    /// cresce: il testo non scivola.
    #[test]
    fn cursore_mantiene_la_riga() {
        let mut g = Grid::new(10, 5);
        put_str(&mut g, 0, "abc");
        put_str(&mut g, 1, "def");
        put_str(&mut g, 2, "ghi");
        g.cursor = (3, 2); // subito dopo "ghi"
        g.resize(10, 8);
        assert_eq!(g.cursor, (3, 2), "il cursore deve restare dove era");
        assert_eq!(row_str(&g, 0), "abc");
        assert_eq!(row_str(&g, 2), "ghi");
    }

    /// Il cursore su una riga **vuota** (dopo l'ultimo testo: il caso del
    /// banner, senza prompt) resta sulla sua riga: la riga vuota non
    /// sparisce dal flusso.
    #[test]
    fn cursore_su_riga_vuota() {
        let mut g = Grid::new(10, 5);
        put_str(&mut g, 0, "abc");
        g.cursor = (3, 4); // tre righe sotto l'ultimo testo
        g.resize(10, 9);
        assert_eq!(g.cursor, (3, 4), "il cursore resta sulla sua riga");
        assert_eq!(row_str(&g, 0), "abc");
    }

    /// Crescendo la griglia il contenuto resta **in alto** (prima era
    /// ancorato al basso: la riga 0 finiva in fondo).
    #[test]
    fn il_testo_non_scivola() {
        let mut g = Grid::new(10, 5);
        put_str(&mut g, 0, "abc");
        g.cursor = (3, 0);
        g.resize(10, 9);
        assert_eq!(row_str(&g, 0), "abc", "il testo resta dov'era");
        assert_eq!(row_str(&g, 6), "", "il fondo e' vuoto, non il testo");
        assert_eq!(g.cursor, (3, 0));
    }

    /// Il reflow spezza la riga: il cursore resta **sullo stesso testo**
    /// ("j" resta sotto il cursore), non a capo della riga.
    #[test]
    fn reflow_il_cursore_resta_sul_testo() {
        let mut g = Grid::new(10, 3);
        put_str(&mut g, 0, "abcdefghij");
        g.cursor = (9, 0); // sull'ultima lettera
        g.resize(5, 3);
        assert_eq!(row_str(&g, 0), "abcde");
        assert_eq!(row_str(&g, 1), "fghij");
        assert_eq!(g.cursor, (4, 1), "il cursore resta su 'j'");
    }

    /// Restringendo la griglia il cursore resta **visibile** (l'ultima
    /// riga): il testo che non sta viene ancorato al basso.
    #[test]
    fn restringendo_il_cursore_resta_visibile() {
        let mut g = Grid::new(10, 10);
        for r in 0..10 {
            put_str(&mut g, r, "0123456789");
        }
        g.cursor = (2, 9);
        g.resize(10, 4);
        assert!(g.cursor.1 < g.rows(), "il cursore deve restare nella griglia");
        assert_eq!(g.cursor, (2, 3), "il cursore finisce sull'ultima riga");
        assert_eq!(row_str(&g, 3), "0123456789");
    }
}
