//! Geometria dei componenti: **celle** per il layout dell'app, **pixel** per
//! il disegno (ADR-0097/0099, `formae`).

use pictura::Color;

/// Un rettangolo in **pixel**, nelle coordinate del target (per una toppa:
/// locali alla toppa; per una GUI: della superficie).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl PxRect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> PxRect {
        PxRect { x, y, w, h }
    }

    pub const fn right(&self) -> u32 {
        self.x + self.w
    }

    pub const fn bottom(&self) -> u32 {
        self.y + self.h
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Il rettangolo ristretto di `n` pixel per lato (mai sotto zero).
    pub fn inset(self, n: u32) -> PxRect {
        let d = n.min(self.w / 2).min(self.h / 2);
        PxRect {
            x: self.x + d,
            y: self.y + d,
            w: self.w - 2 * d,
            h: self.h - 2 * d,
        }
    }

    /// Spostato di `dx`, `dy`.
    pub fn translate(self, dx: i32, dy: i32) -> PxRect {
        PxRect {
            x: (self.x as i64 + dx as i64).max(0) as u32,
            y: (self.y as i64 + dy as i64).max(0) as u32,
            ..self
        }
    }
}

/// Un rettangolo in **celle**: e' lo spazio in cui ragiona un'app di
/// terminale (la tile, una card, una riga).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellRect {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

impl CellRect {
    pub const fn new(col: u16, row: u16, cols: u16, rows: u16) -> CellRect {
        CellRect {
            col,
            row,
            cols,
            rows,
        }
    }

    pub const fn right(&self) -> u16 {
        self.col + self.cols
    }

    pub const fn bottom(&self) -> u16 {
        self.row + self.rows
    }

    pub const fn is_empty(&self) -> bool {
        self.cols == 0 || self.rows == 0
    }

    /// Lo stesso rettangolo in pixel, secondo le metriche del display.
    pub fn px(&self, m: &Metrics) -> PxRect {
        PxRect {
            x: self.col as u32 * m.cell_w,
            y: self.row as u32 * m.cell_h,
            w: self.cols as u32 * m.cell_w,
            h: self.rows as u32 * m.cell_h,
        }
    }

    pub fn inset(self, n: u16) -> CellRect {
        let d = n.min(self.cols / 2).min(self.rows / 2);
        CellRect {
            col: self.col + d,
            row: self.row + d,
            cols: self.cols - 2 * d,
            rows: self.rows - 2 * d,
        }
    }

    /// Due rettangoli affiancati: il primo largo `left` celle, il secondo il
    /// resto (se `left` sborda, il secondo e' vuoto).
    pub fn split_h(self, left: u16) -> (CellRect, CellRect) {
        let left = left.min(self.cols);
        let right = self.cols - left;
        (
            CellRect::new(self.col, self.row, left, self.rows),
            CellRect::new(self.col + left, self.row, right, self.rows),
        )
    }

    /// Due rettangoli impilati: il primo alto `top` celle.
    pub fn split_v(self, top: u16) -> (CellRect, CellRect) {
        let top = top.min(self.rows);
        let bottom = self.rows - top;
        (
            CellRect::new(self.col, self.row, self.cols, top),
            CellRect::new(self.col, self.row + top, self.cols, bottom),
        )
    }
}

/// La misura del display: celle della griglia e pixel di una cella.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Metrics {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

impl Metrics {
    pub const fn new(cols: u16, rows: u16, cell_w: u32, cell_h: u32) -> Metrics {
        Metrics {
            cols,
            rows,
            cell_w,
            cell_h,
        }
    }

    /// Tutta la griglia.
    pub const fn full(&self) -> CellRect {
        CellRect::new(0, 0, self.cols, self.rows)
    }

    /// Tutta la superficie, in pixel.
    pub fn px(&self) -> PxRect {
        PxRect::new(0, 0, self.cols as u32 * self.cell_w, self.rows as u32 * self.cell_h)
    }
}

/// Il tratto di un bordo (per `Theme` e per i widget che ne hanno bisogno).
pub type Thickness = u32;

/// Ricolora `c` con l'alpha dato (utile per le aree dei grafici).
pub const fn faded(c: Color, a: u8) -> Color {
    pictura::with_alpha(c, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn celle_in_pixel_e_clippate() {
        let m = Metrics::new(128, 36, 10, 22);
        let r = CellRect::new(10, 5, 12, 3).px(&m);
        assert_eq!((r.x, r.y, r.w, r.h), (100, 110, 120, 66));
        // Il bordo di una cella non e' mai a meta' cella.
        assert!(r.x % m.cell_w == 0 && r.y % m.cell_h == 0);
        // Colonna oltre il bordo: convertita ugualmente (la toppa e' clippata
        // dal chiamante: nessun pixel esce).
        let fuori = CellRect::new(120, 0, 10, 1).px(&m);
        assert_eq!(fuori.x, 1200);
        assert_eq!(fuori.w, 100);
    }

    #[test]
    fn split_non_perde_celle() {
        let r = CellRect::new(0, 0, 100, 30);
        let (l, rr) = r.split_h(47);
        assert_eq!((l.cols, rr.cols), (47, 53));
        assert_eq!(l.right(), rr.col);
        let (a, b) = r.split_v(5);
        assert_eq!((a.rows, b.rows), (5, 25));
        assert_eq!(a.bottom(), b.row);
        // split oltre: vuoto.
        let (l, rr) = r.split_h(200);
        assert_eq!((l.cols, rr.cols), (100, 0));
    }

    #[test]
    fn metrica_tutto_la_griglia() {
        let m = Metrics::new(10, 5, 10, 22);
        let full = m.full();
        assert_eq!((full.cols, full.rows), (10, 5));
        assert_eq!((m.px().w, m.px().h), (100, 110));
    }
}
