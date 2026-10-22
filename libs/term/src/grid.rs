//! Modello a celle del terminale (D3/D4).
//!
//! La griglia e' **il modello**: indipendente dalla surface (pixel o VGA
//! testuale). Ogni mutazione segna la zona sporca: le surface ridisegnano
//! solo quella (una cella per un tasto, tutto per uno scroll).
//!
//! La cella tiene un **codepoint** (u32), non un byte: le icone Nerd (PUA)
//! sono codepoint multi-byte in UTF-8 (D3).
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

/// Una cella della griglia: il carattere e i colori (indici VGA 0-15).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Codepoint Unicode (0 = spazio).
    pub cp: u32,
    /// Colore foreground (0-15, palette VGA).
    pub fg: u8,
    /// Colore background (0-15, palette VGA).
    pub bg: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            cp: b' ' as u32,
            fg: 0x0F,
            bg: 0x00,
        }
    }
}

impl Cell {
    /// La cella e' "vuota" (solo spazio: i colori non contano).
    pub fn is_blank(&self) -> bool {
        self.cp == b' ' as u32
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

    /// Scrive un codepoint nella cella (col,row) e segna la zona sporca.
    pub fn put(&mut self, col: usize, row: usize, cp: u32, fg: u8, bg: u8) {
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
    /// righe piu' vecchie vengono perse. Il cursore finisce in fondo.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        // 1) Le righe, dalla piu' vecchia: scrollback + griglia corrente.
        let mut lines: Vec<Vec<Cell>> = Vec::with_capacity(self.scrollback.len() + self.rows);
        for row in self.scrollback.iter() {
            lines.push(trim_row(row));
        }
        for r in 0..self.rows {
            lines.push(trim_row(&self.cells[r * self.cols..(r + 1) * self.cols]));
        }
        // 2) Via le righe vuote in coda (il testo finisce dove finiva).
        while let Some(last) = lines.last() {
            if last.is_empty() {
                lines.pop();
            } else {
                break;
            }
        }

        // 3) Spezza le righe troppo lunghe alla nuova larghezza.
        let mut flat: Vec<Vec<Cell>> = Vec::new();
        for line in lines.iter() {
            if line.is_empty() {
                flat.push(Vec::new());
                continue;
            }
            let mut i = 0;
            while i < line.len() {
                let end = (i + new_cols).min(line.len());
                flat.push(line[i..end].to_vec());
                i = end;
            }
        }

        // 4) La nuova griglia: le ultime `new_rows` righe.
        let start = flat.len().saturating_sub(new_rows);
        let mut cells = alloc::vec![Cell::default(); new_cols * new_rows];
        for (dst_row, src) in flat[start..].iter().enumerate() {
            for (c, cell) in src.iter().enumerate() {
                if c < new_cols {
                    cells[dst_row * new_cols + c] = *cell;
                }
            }
        }
        let last_len = flat.last().map(|l| l.len()).unwrap_or(0);

        self.cols = new_cols;
        self.rows = new_rows;
        self.cells = cells;
        // Il contenuto ora e' nella griglia: lo scrollback si svuota (le righe
        // piu' vecchie che non ci stanno sono perse).
        self.scrollback.clear();
        self.cursor = (last_len.min(new_cols.saturating_sub(1)), new_rows - 1);
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
