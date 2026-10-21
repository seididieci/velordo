//! Modello a celle del terminale (D3).
//!
//! La griglia e' **il modello**: indipendente dalla surface (pixel o VGA
//! testuale). Ogni mutazione segna la zona sporca: le surface ridisegnano
//! solo quella (una cella per un tasto, tutto per uno scroll).
//!
//! La cella tiene un **codepoint** (u32), non un byte: le icone Nerd (PUA)
//! sono codepoint multi-byte in UTF-8 (D3).

extern crate alloc;

use alloc::vec::Vec;

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

/// La griglia del terminale: le celle, il cursore, la zona sporca.
pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
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

    /// Scorri in alto di una riga (le celle dell'ultima riga sono azzerate).
    pub fn scroll_up(&mut self) {
        if self.rows == 0 {
            return;
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
}
