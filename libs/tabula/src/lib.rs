//! `tabula` — canvas a **celle** con differenziale (ADR-0098, addendum).
//!
//! Un'applicazione a schermo intero disegna in **celle**, non in byte: scrive il
//! suo frame in questo canvas e chiede [`Canvas::diff`], che ritorna solo i byte
//! delle celle **cambiate** (un `CUP` per fetta, poi i caratteri). Il frame
//! intero si compone in memoria, quindi non esiste nessuno stato intermedio da
//! vedere: e' il contrario di un IPC per cella, che sul display fa un
//! `render`+`flush` per cella (il "lampeggio").
//!
//! Perche' non e' un toolkit: nessun widget, nessun layout, nessuno stile. E'
//! una superficie di caratteri con una copia ombra — il minimo che serve perche'
//! il differenziale sia **del meccanismo** e non di ogni app (ADR-0097: niente
//! toolkit unificato; il disegno a schermo resta della console).
//!
//! **Una cella = un carattere** (larghezza 1). I glifi larghi non sono
//! supportati qui: la larghezza di un glifo la conosce solo il **font**
//! (`font.glyph_width_cells`, lato console) e indovinarla con una tabella
//! significherebbe disallineare il canvas dallo schermo — con un font
//! monospazio il glifo che il font non ha e' largo **1**. Chi deve disegnare un
//! glifo largo scrive con `Screen::write` e conta le celle da se'.
//!
//! Nessuna dipendenza dal sistema: si prova sull'host.

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

/// Una tela a celle con la **copia ombra** di quello che e' gia' sullo schermo.
pub struct Canvas {
    cols: usize,
    rows: usize,
    /// Il frame in composizione.
    cur: Vec<char>,
    /// Quello che il server ha gia' sullo schermo (l'ultimo `diff`/`present`).
    shown: Vec<char>,
    /// I byte dell'ultimo `diff` (riusato: nessuna allocazione per frame).
    out: Vec<u8>,
}

impl Canvas {
    /// Una tela `cols × rows` di celle vuote (il primo `diff` disegna tutto).
    pub fn new(cols: usize, rows: usize) -> Canvas {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Canvas {
            cols,
            rows,
            cur: vec![' '; cols * rows],
            shown: vec![' '; cols * rows],
            out: Vec::new(),
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// La misura `(colonne, righe)`.
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Il carattere nel frame corrente (`' '` fuori dalla tela).
    pub fn cell(&self, col: usize, row: usize) -> char {
        if col >= self.cols || row >= self.rows {
            return ' ';
        }
        self.cur[row * self.cols + col]
    }

    /// Svuota la tela (**nel frame**, non sullo schermo): chi ridisegna da capo
    /// azzera e riscrive, e il `diff` manda solo cio' che cambia davvero.
    pub fn clear(&mut self) {
        self.cur.fill(' ');
    }

    /// Scrive un carattere in `(col, row)`. Fuori tela non fa niente.
    pub fn set(&mut self, col: usize, row: usize, ch: char) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        self.cur[row * self.cols + col] = ch;
    }

    /// Scrive una stringa da `(col, row)` e ritorna le colonne occupate.
    pub fn text(&mut self, col: usize, row: usize, s: &str) -> usize {
        let mut c = col;
        for ch in s.chars() {
            if c >= self.cols {
                break;
            }
            self.set(c, row, ch);
            c += 1;
        }
        c - col
    }

    /// Riempie un rettangolo di `ch` (clippato alla tela).
    pub fn fill(&mut self, col: usize, row: usize, w: usize, h: usize, ch: char) {
        for r in row..row.saturating_add(h).min(self.rows) {
            for c in col..col.saturating_add(w).min(self.cols) {
                self.cur[r * self.cols + c] = ch;
            }
        }
    }

    /// Cambia misura **scartando** le due copie: dopo un resize si ridisegna
    /// (il reflow non e' compito del canvas).
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.cur = vec![' '; cols * rows];
        self.shown = vec![' '; cols * rows];
    }

    /// I byte del **differenziale** rispetto all'ultima chiamata: un `CUP` per
    /// fetta di celle cambiate, poi i caratteri. Vuoto se niente e' cambiato.
    /// Aggiorna la copia ombra (il prossimo `diff` riparte da qui).
    pub fn diff(&mut self) -> &[u8] {
        self.out.clear();
        for r in 0..self.rows {
            let base = r * self.cols;
            let mut c = 0;
            while c < self.cols {
                if self.cur[base + c] == self.shown[base + c] {
                    c += 1;
                    continue;
                }
                let start = c;
                while c < self.cols && self.cur[base + c] != self.shown[base + c] {
                    c += 1;
                }
                self.push_cup(r + 1, start + 1);
                for k in start..c {
                    let ch = self.cur[base + k];
                    self.push_char(ch);
                }
                for k in start..c {
                    self.shown[base + k] = self.cur[base + k];
                }
            }
        }
        &self.out
    }

    /// `ESC[<row>;<col>H` (1-based), scritto in decimale senza `format!`.
    fn push_cup(&mut self, row: usize, col: usize) {
        self.out.extend_from_slice(b"\x1b[");
        push_usize(&mut self.out, row);
        self.out.push(b';');
        push_usize(&mut self.out, col);
        self.out.push(b'H');
    }

    fn push_char(&mut self, ch: char) {
        let mut tmp = [0u8; 4];
        let s = ch.encode_utf8(&mut tmp);
        self.out.extend_from_slice(s.as_bytes());
    }
}

fn push_usize(out: &mut Vec<u8>, mut v: usize) {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(tmp[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn s(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn il_primo_diff_disegna_tutto() {
        let mut c = Canvas::new(3, 2);
        c.text(0, 0, "abc");
        assert_eq!(s(c.diff()), "\x1b[1;1Habc");
    }

    #[test]
    fn nessun_cambio_nessun_byte() {
        let mut c = Canvas::new(4, 2);
        c.text(1, 1, "ok");
        assert_eq!(s(c.diff()), "\x1b[2;2Hok");
        assert_eq!(c.diff().len(), 0);
    }

    #[test]
    fn fette_consecutive_in_un_solo_cup() {
        let mut c = Canvas::new(8, 1);
        c.text(2, 0, "ciao");
        assert_eq!(s(c.diff()), "\x1b[1;3Hciao");
        assert_eq!(c.diff().len(), 0);
    }

    #[test]
    fn celle_non_adiacenti_sono_fette_distinte() {
        // Il caso del cursore che si sposta: la cella vecchia si cancella, la
        // nuova si scrive — due fette, non un ridisegno.
        let mut c = Canvas::new(8, 1);
        c.set(1, 0, '*');
        assert_eq!(s(c.diff()), "\x1b[1;2H*");
        c.set(1, 0, ' ');
        c.set(4, 0, '*');
        assert_eq!(s(c.diff()), "\x1b[1;2H \x1b[1;5H*");
    }

    #[test]
    fn una_riga_che_si_accorcia_manda_gli_spazi() {
        let mut c = Canvas::new(6, 1);
        c.text(0, 0, "abc");
        let _ = c.diff();
        // Chi scrive "a" piu' corto deve anche cancellare il resto: li conta
        // l'app, il canvas manda gli spazi come qualunque altro carattere.
        c.set(1, 0, ' ');
        c.set(2, 0, ' ');
        assert_eq!(s(c.diff()), "\x1b[1;2H  ");
    }

    #[test]
    fn clear_azzera_il_frame_e_manda_spazi() {
        let mut c = Canvas::new(3, 1);
        c.text(0, 0, "abc");
        let _ = c.diff();
        c.clear();
        assert_eq!(s(c.diff()), "\x1b[1;1H   ");
        assert_eq!(c.diff().len(), 0);
    }

    #[test]
    fn fuori_tela_non_fa_niente() {
        let mut c = Canvas::new(2, 2);
        c.set(5, 0, 'x');
        c.set(0, 9, 'x');
        assert_eq!(c.diff().len(), 0);
        assert_eq!(c.cell(5, 0), ' ');
        assert_eq!(c.cell(0, 0), ' ');
    }

    #[test]
    fn il_testo_si_ferma_al_bordo() {
        let mut c = Canvas::new(4, 1);
        let n = c.text(2, 0, "abcdef");
        assert_eq!(n, 2);
        assert_eq!(s(c.diff()), "\x1b[1;3Hab");
    }

    #[test]
    fn biforcazione_e_caratteri_utf8() {
        // I caratteri del bordo e le frecce sono a 1 cella e viaggiano in UTF-8.
        let mut c = Canvas::new(4, 1);
        c.text(0, 0, "┌─┐✓");
        let out = s(c.diff());
        assert_eq!(out, "\x1b[1;1H┌─┐✓");
        assert!(out.len() > 1 + "abc".len());
    }

    #[test]
    fn il_resize_riparte_da_zero() {
        let mut c = Canvas::new(4, 2);
        c.text(0, 0, "ab");
        let _ = c.diff();
        c.resize(6, 3);
        assert_eq!(s(c.diff()).len(), 0, "il canvas nuovo e' vuoto a video");
        c.text(5, 2, "z");
        assert_eq!(s(c.diff()), "\x1b[3;6Hz");
    }

    #[test]
    fn fill_riempie_il_rettangolo() {
        let mut c = Canvas::new(4, 2);
        c.fill(0, 0, 2, 1, '#');
        assert_eq!(s(c.diff()), "\x1b[1;1H##");
    }

    #[test]
    fn dimensioni_minime() {
        let c = Canvas::new(0, 0);
        assert_eq!(c.size(), (1, 1));
    }
}
