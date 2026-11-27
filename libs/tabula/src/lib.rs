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
//! **Colore per cella** (M1 di `formae`): ogni cella porta `(char, fg, bg)`;
//! `diff` emette un `SGR` solo quando lo stile **cambia** e chiude con
//! `ESC[0m`, cosi' il colore non cola nel prompt dopo un'app a schermo intero.
//! Un'app monocroma (tutte le celle di default) produce **esattamente** i byte
//! di prima: nessun `SGR`, nessun reset.
//!
//! Nessuna dipendenza dal sistema: si prova sull'host.

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

/// Colore RGBA impacchettato come `term::grid::color` (`0xRRGGBBAA`).
pub type Color = u32;

/// Foreground di default: lo stesso bianco morbido della griglia del terminale.
pub const DEFAULT_FG: Color = 0xE8EB_F2FF;
/// Background di default: lo stesso blu-notte della griglia del terminale.
pub const DEFAULT_BG: Color = 0x0B0D_14FF;

/// Lo stile di una cella: colore del glifo e del fondo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
}

impl Style {
    /// Bianco su blu-notte: lo stile con cui il terminale parte.
    pub const DEFAULT: Style = Style {
        fg: DEFAULT_FG,
        bg: DEFAULT_BG,
    };

    pub const fn new(fg: Color, bg: Color) -> Style {
        Style { fg, bg }
    }

    /// Solo il colore del glifo (fondo di default: sopra una toppa il fondo
    /// non si dipinge, vedi ADR-0099).
    pub const fn fg(fg: Color) -> Style {
        Style {
            fg,
            bg: DEFAULT_BG,
        }
    }

    pub const fn is_default(&self) -> bool {
        self.fg == DEFAULT_FG && self.bg == DEFAULT_BG
    }
}

/// Una cella: il carattere e il suo stile.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    st: Style,
}

impl Cell {
    const BLANK: Cell = Cell {
        ch: ' ',
        st: Style::DEFAULT,
    };
}

/// Una tela a celle con la **copia ombra** di quello che e' gia' sullo schermo.
pub struct Canvas {
    cols: usize,
    rows: usize,
    /// Il frame in composizione.
    cur: Vec<Cell>,
    /// Quello che il server ha gia' sullo schermo (l'ultimo `diff`/`present`).
    shown: Vec<Cell>,
    /// Lo stile della "penna": lo usano `set`/`text`/`fill`/`clear`.
    pen: Style,
    /// Lo stile SGR che resta attivo sullo schermo dopo l'ultimo `diff`.
    live: Style,
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
            cur: vec![Cell::BLANK; cols * rows],
            shown: vec![Cell::BLANK; cols * rows],
            pen: Style::DEFAULT,
            live: Style::DEFAULT,
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

    /// Lo stile corrente della penna.
    pub fn style(&self) -> Style {
        self.pen
    }

    /// Cambia lo stile della penna: `set`/`text`/`fill`/`clear` da qui in poi.
    pub fn set_style(&mut self, st: Style) {
        self.pen = st;
    }

    /// Il carattere nel frame corrente (`' '` fuori dalla tela).
    pub fn cell(&self, col: usize, row: usize) -> char {
        if col >= self.cols || row >= self.rows {
            return ' ';
        }
        self.cur[row * self.cols + col].ch
    }

    /// Lo stile della cella nel frame corrente (`None` fuori).
    pub fn style_at(&self, col: usize, row: usize) -> Option<Style> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        Some(self.cur[row * self.cols + col].st)
    }

    /// Svuota la tela (**nel frame**, non sullo schermo) con lo stile della
    /// penna: chi ridisegna da capo azzera e riscrive, e il `diff` manda solo
    /// cio' che cambia davvero.
    pub fn clear(&mut self) {
        self.cur.fill(Cell {
            ch: ' ',
            st: self.pen,
        });
    }

    /// Scrive un carattere in `(col, row)` con lo stile della penna. Fuori
    /// tela non fa niente.
    pub fn set(&mut self, col: usize, row: usize, ch: char) {
        self.set_styled(col, row, ch, self.pen);
    }

    /// Scrive un carattere con uno stile **esplicito** (non tocca la penna).
    pub fn set_styled(&mut self, col: usize, row: usize, ch: char, st: Style) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        self.cur[row * self.cols + col] = Cell { ch, st };
    }

    /// Scrive una stringa da `(col, row)` e ritorna le colonne occupate.
    pub fn text(&mut self, col: usize, row: usize, s: &str) -> usize {
        self.text_styled(col, row, s, self.pen)
    }

    /// Come [`Canvas::text`], con uno stile esplicito.
    pub fn text_styled(&mut self, col: usize, row: usize, s: &str, st: Style) -> usize {
        if row >= self.rows {
            return 0;
        }
        let mut c = col;
        for ch in s.chars() {
            if c >= self.cols {
                break;
            }
            self.cur[row * self.cols + c] = Cell { ch, st };
            c += 1;
        }
        c.saturating_sub(col)
    }

    /// Riempie un rettangolo di `ch` con lo stile della penna (clippato).
    pub fn fill(&mut self, col: usize, row: usize, w: usize, h: usize, ch: char) {
        let cell = Cell { ch, st: self.pen };
        for r in row..row.saturating_add(h).min(self.rows) {
            for c in col..col.saturating_add(w).min(self.cols) {
                self.cur[r * self.cols + c] = cell;
            }
        }
    }

    /// Riempie un rettangolo con uno stile esplicito (clippato).
    pub fn fill_styled(
        &mut self,
        col: usize,
        row: usize,
        w: usize,
        h: usize,
        ch: char,
        st: Style,
    ) {
        let cell = Cell { ch, st };
        for r in row..row.saturating_add(h).min(self.rows) {
            for c in col..col.saturating_add(w).min(self.cols) {
                self.cur[r * self.cols + c] = cell;
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
        self.cur = vec![Cell::BLANK; cols * rows];
        self.shown = vec![Cell::BLANK; cols * rows];
    }

    /// Forza il prossimo `diff` a emettere **tutte** le celle (repaint pieno):
    /// la copia ombra va a un carattere che nessuna cella usa. Serve alle app a
    /// bassa frequenza (il monitor a 1 Hz) che non vogliono dipendere dal
    /// differenziale per convergere.
    pub fn invalidate(&mut self) {
        self.invalidate_rows(self.rows);
    }

    /// Come [`Canvas::invalidate`], ma solo le prime `rows` righe. Un'app a
    /// schermo intero **non** deve invalidare l'ultima riga: scrivere l'ultima
    /// cella dell'ultima riga fa **scorrere** la griglia (`NeedScroll`), e un
    /// repaint pieno la farebbe scorrere a ogni frame (le toppe "salgono").
    pub fn invalidate_rows(&mut self, rows: usize) {
        let n = rows.min(self.rows) * self.cols;
        for cell in self.shown.iter_mut().take(n) {
            *cell = Cell {
                ch: '\u{0}',
                st: Style::DEFAULT,
            };
        }
    }

    /// I byte del **differenziale** rispetto all'ultima chiamata: un `CUP` per
    /// fetta di celle cambiate, poi i caratteri. Vuoto se niente e' cambiato.
    /// Aggiorna la copia ombra (il prossimo `diff` riparte da qui).
    ///
    /// Le fette si spezzano anche al cambio di **stile**: il `SGR` si manda
    /// solo quando serve, e lo stato che lasciamo allo schermo e' sempre
    /// quello di default (l'app non puo' "colorare" il prompt che segue).
    pub fn diff(&mut self) -> &[u8] {
        self.out.clear();
        let mut live = self.live;
        for r in 0..self.rows {
            let base = r * self.cols;
            let mut c = 0;
            while c < self.cols {
                if self.cur[base + c] == self.shown[base + c] {
                    c += 1;
                    continue;
                }
                let start = c;
                let st = self.cur[base + c].st;
                while c < self.cols
                    && self.cur[base + c] != self.shown[base + c]
                    && self.cur[base + c].st == st
                {
                    c += 1;
                }
                self.push_cup(r + 1, start + 1);
                if st != live {
                    self.push_sgr(st);
                    live = st;
                }
                for k in start..c {
                    let ch = self.cur[base + k].ch;
                    self.push_char(ch);
                }
                for k in start..c {
                    self.shown[base + k] = self.cur[base + k];
                }
            }
        }
        if !live.is_default() {
            self.out.extend_from_slice(b"\x1b[0m");
            live = Style::DEFAULT;
        }
        self.live = live;
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

    /// `ESC[0;38;2;R;G;B;48;2;R;G;Bm`: autosufficiente (parte dal reset),
    /// quindi non dipende dallo stato lasciato dal frame precedente.
    fn push_sgr(&mut self, st: Style) {
        self.out.extend_from_slice(b"\x1b[0;38;2;");
        for ch in [(st.fg >> 24) as u8, (st.fg >> 16) as u8, (st.fg >> 8) as u8] {
            push_usize(&mut self.out, ch as usize);
            self.out.push(b';');
        }
        self.out.extend_from_slice(b"48;2;");
        for (i, ch) in [
            (st.bg >> 24) as u8,
            (st.bg >> 16) as u8,
            (st.bg >> 8) as u8,
        ]
        .iter()
        .enumerate()
        {
            push_usize(&mut self.out, *ch as usize);
            if i < 2 {
                self.out.push(b';');
            }
        }
        self.out.push(b'm');
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

    const RED: Color = 0xFF5C_70FF;
    const BLUE_BG: Color = 0x1A20_30FF;

    #[test]
    fn il_colore_si_manda_solo_al_cambio_e_si_chiude() {
        let mut c = Canvas::new(4, 2);
        let red = Style::new(RED, DEFAULT_BG);
        c.set_styled(0, 0, 'a', red);
        c.set_styled(2, 0, 'b', red);
        // Due fette dello **stesso** stile: il secondo SGR non si rimanda.
        // `0;` rende la sequenza autosufficiente; il reset finale non lascia
        // il colore acceso sul prompt che segue (SGR sopravvive alle feed).
        assert_eq!(
            s(c.diff()),
            "\x1b[1;1H\x1b[0;38;2;255;92;112;48;2;11;13;20ma\x1b[1;3Hb\x1b[0m"
        );
        // Cella di default dopo il reset: nessun SGR da mandare.
        c.set(1, 1, 'c');
        assert_eq!(s(c.diff()), "\x1b[2;2Hc");
    }

    #[test]
    fn stili_diversi_spezzano_la_fetta() {
        let mut c = Canvas::new(4, 1);
        c.set_styled(0, 0, 'a', Style::fg(RED));
        c.set_styled(1, 0, 'b', Style::new(DEFAULT_FG, BLUE_BG));
        let out = s(c.diff());
        assert_eq!(
            out,
            "\x1b[1;1H\x1b[0;38;2;255;92;112;48;2;11;13;20ma\
             \x1b[1;2H\x1b[0;38;2;232;235;242;48;2;26;32;48mb\x1b[0m"
        );
    }

    #[test]
    fn app_monocroma_nessun_byte_di_colore() {
        let mut c = Canvas::new(4, 2);
        c.text(0, 0, "ciao");
        c.fill(0, 1, 4, 1, '-');
        let out = s(c.diff());
        assert!(!out.contains('\x1b') || out.matches('\x1b').count() == 2);
        // Solo i due CUP: nessun SGR.
        assert_eq!(out, "\x1b[1;1Hciao\x1b[2;1H----");
        assert_eq!(c.style(), Style::DEFAULT);
        assert_eq!(c.style_at(0, 1), Some(Style::DEFAULT));
        assert_eq!(c.style_at(9, 9), None);
    }

    #[test]
    fn clear_usa_lo_stile_della_penna() {
        let mut c = Canvas::new(2, 1);
        c.set_style(Style::new(RED, BLUE_BG));
        c.clear();
        let out = s(c.diff());
        assert!(out.contains("\x1b[0;38;2;255;92;112;48;2;26;32;48m"));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn invalidate_forza_repaint_pieno() {
        // Le app a bassa frequenza (il monitor) invalidano la copia ombra per
        // convergere sempre: il prossimo `diff` rimanda **tutte** le celle.
        let mut c = Canvas::new(3, 1);
        c.text(0, 0, "abc");
        assert_eq!(s(c.diff()), "\x1b[1;1Habc");
        assert_eq!(c.diff().len(), 0, "nessun cambio: zero byte");
        c.invalidate();
        assert_eq!(s(c.diff()), "\x1b[1;1Habc", "invalidate: repaint pieno");
        assert_eq!(c.diff().len(), 0);
    }

    #[test]
    fn invalidate_rows_non_tocca_l_ultima() {
        // Repaint pieno **tranne l'ultima riga**: scriverne l'ultima cella fa
        // scorrere la griglia (con il repaint a ogni frame le toppe salirebbero).
        let mut c = Canvas::new(4, 3);
        c.text(0, 2, "z"); // ultima riga
        let _ = c.diff();
        c.invalidate_rows(2); // righe 0,1 (non la 2)
        c.text(0, 0, "a");
        let out = s(c.diff());
        assert!(out.contains('a'), "la riga 0 si rimanda: {:?}", out);
        assert!(!out.contains('z'), "l'ultima riga NON si rimanda: {:?}", out);
    }

    #[test]
    fn riga_fuori_tela_non_panica() {
        // Una cella fuori tela e' un no-op (il monitor sbagliava la riga e
        // faceva index-out-of-bounds: la guardia e' qui, non nell'app).
        let mut c = Canvas::new(4, 2);
        assert_eq!(c.text_styled(0, 99, "abc", Style::DEFAULT), 0);
        assert_eq!(c.set_styled(0, 99, 'x', Style::DEFAULT), ());
        c.fill_styled(0, 99, 4, 1, 'x', Style::DEFAULT);
        assert_eq!(c.diff().len(), 0);
        // Colonna fuori: tronca, non panica.
        assert_eq!(c.text_styled(3, 0, "abc", Style::DEFAULT), 1);
        assert_eq!(s(c.diff()), "\x1b[1;4Ha");
    }
}
