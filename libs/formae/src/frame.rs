//! `Frame` — il frame di una TUI: una tela a **celle** (`tabula`) piu' le
//! **toppe** di pixel (`pictura`) che stanno sotto ai glifi.
//!
//! Non fa IPC: l'app manda le toppe al server (`sess::patch`) e poi il
//! differenziale delle celle (`Screen::present`). L'ordine conta: prima i
//! pixel (il fondo delle card), poi il testo.

use alloc::vec::Vec;

use pictura::{Canvas as Pixels, Color};
use tabula::{Canvas as Cells, Style};

use crate::geom::{CellRect, Metrics, PxRect};
use crate::target::Target;

/// Una toppa pronta da mandare: il rettangolo in celle e i pixel.
pub struct Patch {
    pub rect: CellRect,
    pub canvas: Pixels,
}

/// Il frame in composizione.
pub struct Frame {
    cells: Cells,
    metrics: Metrics,
    patches: Vec<Patch>,
}

impl Frame {
    /// Una tela `cols × rows` con le metriche date.
    pub fn new(metrics: Metrics) -> Frame {
        Frame {
            cells: Cells::new(metrics.cols as usize, metrics.rows as usize),
            metrics,
            patches: Vec::new(),
        }
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// La tela a celle (testo, cursore, pulizia).
    pub fn cells(&mut self) -> &mut Cells {
        &mut self.cells
    }

    /// La tela a celle in sola lettura.
    pub fn cells_ref(&self) -> &Cells {
        &self.cells
    }

    /// Nuova misura (un resize **scarta** il differenziale: si ridisegna).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.metrics.cols = cols;
        self.metrics.rows = rows;
        self.cells.resize(cols as usize, rows as usize);
        self.patches.clear();
    }

    /// Inizio frame: la tela a celle e le toppe del frame precedente sono
    /// gia' state mandate. Si riparte **vuoti** — la tela va azzerata, o un
    /// testo che si **accorcia** ("name" → "cpu") lascia la coda vecchia
    /// (il differenziale non sa che quelle celle sono diventate vuote). Il
    /// `diff` poi manda solo la differenza (comprese le celle svuotate).
    pub fn begin(&mut self) {
        self.patches.clear();
        self.cells.clear();
    }

    /// Disegna una card/toppa nel rettangolo di celle `rect`. La superficie
    /// parte **trasparente**: un pixel non dipinto lascia vedere il wallpaper
    /// (angoli arrotondati, ombre) e i glifi scritti sopra restano leggibili.
    ///
    /// Un rect fuori dalla griglia viene **saltato** (e' un errore di layout
    /// dell'app, non un caso da aggiustare qui: il server rifiuterebbe la
    /// toppa intera).
    pub fn patch(&mut self, rect: CellRect, f: impl FnOnce(&mut PatchTarget<'_>)) {
        if rect.is_empty()
            || rect.right() > self.metrics.cols
            || rect.bottom() > self.metrics.rows
        {
            return;
        }
        let px = rect.px(&self.metrics);
        if px.is_empty() {
            return;
        }
        let mut canvas = Pixels::new(px.w as usize, px.h as usize, 0);
        {
            let cells = &mut self.cells;
            let mut t = PatchTarget {
                canvas: &mut canvas,
                cells,
                metrics: self.metrics,
                origin: rect,
                origin_px: px,
            };
            f(&mut t);
        }
        self.patches.push(Patch { rect, canvas });
    }

    /// Le toppe di questo frame, da mandare **prima** del testo.
    pub fn patches(&self) -> &[Patch] {
        &self.patches
    }

    /// I byte del differenziale delle celle (vuoto se niente e' cambiato).
    pub fn diff(&mut self) -> &[u8] {
        self.cells.diff()
    }
}

/// Il `Target` di **una toppa**: i pixel vanno nella sua superficie locale, il
/// testo nella tela a celle della sessione (alla cella che contiene il punto).
pub struct PatchTarget<'a> {
    canvas: &'a mut Pixels,
    cells: &'a mut Cells,
    metrics: Metrics,
    /// Cella di origine della toppa (per scrivere il testo nella tela giusta).
    origin: CellRect,
    /// Origine della toppa in pixel: i componenti disegnano in coordinate
    /// assolute (quelle della sessione), questa le riporta alla superficie
    /// locale della toppa (0..canvas.w, 0..canvas.h).
    origin_px: PxRect,
}

impl PatchTarget<'_> {
    /// I pixel disegnati finora (per i test e per chi vuole ispezionare).
    pub fn canvas(&self) -> &Pixels {
        self.canvas
    }

    /// Il rettangolo (in celle) che questa toppa occupa.
    pub fn origin(&self) -> CellRect {
        self.origin
    }

    /// La tela a celle dove finisce il testo.
    pub fn cells(&self) -> &Cells {
        self.cells
    }

    /// Rette un rect assoluto alla superficie locale della toppa. Un rect
    /// gia' locale (x=0) e' invariante; un rect fuori dalla toppa viene
    /// clippato a zero dal chiamante (`Canvas` clippa gia' ai bordi).
    fn local(&self, r: PxRect) -> PxRect {
        PxRect {
            x: r.x.saturating_sub(self.origin_px.x),
            y: r.y.saturating_sub(self.origin_px.y),
            ..r
        }
    }
}

impl Target for PatchTarget<'_> {
    fn grid(&self) -> (u32, u32) {
        (self.metrics.cell_w, self.metrics.cell_h)
    }

    fn size_px(&self) -> (u32, u32) {
        (self.canvas.w as u32, self.canvas.h as u32)
    }

    fn measure(&self, s: &str, _st: Style) -> (u32, u32) {
        (
            s.chars().count() as u32 * self.metrics.cell_w,
            self.metrics.cell_h,
        )
    }

    fn text(&mut self, x: u32, y: u32, s: &str, st: Style) -> u32 {
        let cw = self.metrics.cell_w.max(1);
        let ch = self.metrics.cell_h.max(1);
        // `x`/`y` sono **assoluti** (come per le primitive pixel): si toglie
        // l'origine della toppa, o per una card non in (0,0) il testo finirebbe
        // spostato del doppio (bug visto sul monitor: "MEM" fuori dalla card).
        let cx = x.saturating_sub(self.origin_px.x) / cw;
        let cy = y.saturating_sub(self.origin_px.y) / ch;
        if cx >= self.origin.cols as u32 || cy >= self.origin.rows as u32 {
            return 0;
        }
        // Il testo non esce dalla toppa: tronca per **caratteri** (il
        // monospazio della TUI rende 1 carattere = 1 cella; una GUI misurera'
        // in pixel con `measure`).
        let room = (self.origin.cols as u32 - cx) as usize;
        let text: alloc::string::String = s.chars().take(room).collect();
        let n = self.cells.text_styled(
            self.origin.col as usize + cx as usize,
            self.origin.row as usize + cy as usize,
            &text,
            st,
        );
        n as u32 * cw
    }

    fn fill(&mut self, r: PxRect, c: Color) {
        let r = self.local(r);
        self.canvas.fill(r.x as usize, r.y as usize, r.w as usize, r.h as usize, c);
    }

    fn border(&mut self, r: PxRect, t: u32, c: Color) {
        let r = self.local(r);
        self.canvas.rect(
            r.x as usize,
            r.y as usize,
            r.w as usize,
            r.h as usize,
            t.max(1) as usize,
            c,
        );
    }

    fn vgradient(&mut self, r: PxRect, top: Color, bottom: Color) {
        let r = self.local(r);
        self.canvas.vgradient(
            r.x as usize,
            r.y as usize,
            r.w as usize,
            r.h as usize,
            top,
            bottom,
        );
    }

    fn round(&mut self, r: PxRect, radius: u32, c: Color) {
        let r = self.local(r);
        self.canvas.round_rect(
            r.x as usize,
            r.y as usize,
            r.w as usize,
            r.h as usize,
            radius as usize,
            c,
        );
    }

    fn bar(&mut self, r: PxRect, permille: u32, bg: Color, fg: Color) {
        let r = self.local(r);
        self.canvas.round_bar(
            r.x as usize,
            r.y as usize,
            r.w as usize,
            r.h as usize,
            permille,
            bg,
            fg,
        );
    }

    fn line(&mut self, a: (u32, u32), b: (u32, u32), t: u32, c: Color) {
        let a = (a.0.saturating_sub(self.origin_px.x), a.1.saturating_sub(self.origin_px.y));
        let b = (b.0.saturating_sub(self.origin_px.x), b.1.saturating_sub(self.origin_px.y));
        self.canvas
            .line(a.0 as usize, a.1 as usize, b.0 as usize, b.1 as usize, t.max(1) as usize, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bar::Bar;
    use crate::chart::Chart;
    use crate::fmt::percent;

    fn toppa(metrics: Metrics, rect: CellRect, f: impl FnOnce(&mut PatchTarget)) -> Pixels {
        let mut frame = Frame::new(metrics);
        frame.patch(rect, f);
        frame.patches.into_iter().next().unwrap().canvas
    }

    #[test]
    fn bar_riempie_proporzionalmente_senza_buchi() {
        let metrics = Metrics::new(127, 36, 10, 22);
        let rect = CellRect::new(2, 4, 40, 3);
        let p = toppa(metrics, rect, |t| {
            let mut bar = Bar::new(0xFF17_1C2A, 0xFF6D_4AFF);
            bar.set(640, 1000);
            bar.draw(t, rect.px(&metrics));
        });
        // Il centro della toppa e' dipinto (track o fill).
        assert_ne!(p.pixel(p.w / 2, p.h / 2), Some(0));
        // Il 64% e' fill: a meta' altezza c'è un confine fill->track.
        let px = rect.px(&metrics);
        let confine = (px.w * 640 / 1000) as usize;
        let mid = (px.h / 2) as usize;
        let prima = p.pixel(confine.saturating_sub(1), mid).unwrap();
        let dopo = p.pixel(confine.min(px.w.saturating_sub(1) as usize), mid).unwrap();
        assert_ne!(prima, dopo, "confine al 64% della larghezza");
    }

    #[test]
    fn chart_area_sotto_la_curva() {
        let metrics = Metrics::new(127, 36, 10, 22);
        let rect = CellRect::new(2, 8, 40, 5);
        let p = toppa(metrics, rect, |t| {
            let chart = Chart::new(&[0, 100, 200, 300, 400], 0xFFA7_8BFA);
            chart.draw(t, rect.px(&metrics));
        });
        let px = rect.px(&metrics);
        let last = (px.w - 1) as usize;
        let mid = (px.h / 2) as usize;
        // L'ultima colonna e' il massimo: l'area copre fino in fondo.
        assert_ne!(p.pixel(last, mid), Some(0), "ultima colonna colorata");
        // La prima colonna vale zero: trasparente in alto e a meta' altezza.
        assert_eq!(p.pixel(0, 0), Some(0));
        assert_eq!(p.pixel(0, mid), Some(0));
    }

    #[test]
    fn stesso_widget_due_metriche_proporzionale() {
        for m in [
            Metrics::new(47, 20, 10, 22),
            Metrics::new(128, 36, 10, 22),
        ] {
            let rect = CellRect::new(0, 0, m.cols / 2, 3);
            let p = toppa(m, rect, |t| {
                let mut bar = Bar::new(0xFF17_1C2A, 0xFFA7_8BFA);
                bar.set(500, 1000);
                bar.draw(t, rect.px(&m));
            });
            let px = rect.px(&m);
            let meta = (px.w / 2) as usize;
            let mid = (px.h / 2) as usize;
            // A meta' altezza: prima meta' fill, seconda meta' track.
            assert_ne!(
                p.pixel(meta.saturating_sub(1), mid),
                p.pixel(meta.min(px.w.saturating_sub(1) as usize), mid),
                "50% a metà rect"
            );
        }
    }

    #[test]
    fn percent_troncata() {
        let mut b = [0u8; 8];
        assert_eq!(percent(996, 0, &mut b), 3);   // "99%"
        assert_eq!(percent(14, 1, &mut b), 4);    // "1.4%"
    }

    #[test]
    fn testo_in_una_toppa_non_in_origine() {
        // La toppa e' alla colonna 63 (card MEM): il testo a x assoluta 4 px
        // dentro la toppa deve finire nella cella 63, non 63+0 (offset doppio).
        let m = Metrics::new(128, 36, 10, 22);
        let mut frame = Frame::new(m);
        let rect = CellRect::new(63, 2, 40, 4);
        let px = rect.px(&m);
        frame.patch(rect, |t| {
            t.text(px.x + 4, px.y + 2, "MEM", Style::fg(0xFFFF_FFFF));
            t.text(px.x + 4, px.y + 2 * m.cell_h, "4%", Style::fg(0xFFFF_FFFF));
        });
        let cells = frame.cells_ref();
        assert_eq!(cells.cell(63, 2), 'M');
        assert_eq!(cells.cell(64, 2), 'E');
        assert_eq!(cells.cell(65, 2), 'M');
        assert_eq!(cells.cell(63, 4), '4');
        // Niente finisce nella colonna 126 (il doppio offset di prima).
        assert_eq!(cells.cell(126, 2), ' ');
    }

    #[test]
    fn testo_che_si_accorcia_pulisce_la_coda() {
        // "name" -> "cpu": la cella della 'e' deve diventare spazio, o resta
        // la coda vecchia ("cpue"). E' il motivo per cui `begin()` azzera la
        // tela: il differenziale non sa che una cella e' diventata vuota.
        let m = Metrics::new(20, 2, 10, 22);
        let mut frame = Frame::new(m);
        frame.begin();
        frame.cells().text_styled(0, 0, "name", Style::DEFAULT);
        let d1 = frame.diff().to_vec();
        assert!(d1.windows(4).any(|w| w == b"name"), "d1 = {:?}", d1);
        frame.begin();
        frame.cells().text_styled(0, 0, "cpu", Style::DEFAULT);
        let d2 = frame.diff().to_vec();
        assert!(
            d2.windows(4).any(|w| w == b"cpu "),
            "il diff deve cancellare la 'e': {:?}",
            d2
        );
        assert_eq!(frame.cells_ref().cell(3, 0), ' ');
    }
}
