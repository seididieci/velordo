//! `Target` — l'unico confine fra i componenti e chi disegna (ADR-0097).
//!
//! Un componente riceve un [`PxRect`] e disegna solo con queste primitive.
//! Nella TUI il target e' una **toppa** (`pictura`) piu' la tela a celle
//! (`tabula`); in una GUI sara' la superficie della finestra. Il testo non si
//! posiziona "a colonne": si chiede `measure` e si scrive a una x in pixel
//! (`snap_x` e' l'unico punto in cui la cella esiste, e nella GUI e' identita').

use pictura::Color;
use tabula::Style;

use crate::geom::PxRect;

pub trait Target {
    /// La "griglia di layout": la cella del font nella TUI, `(1, 1)` in una
    /// GUI a pixel puri.
    fn grid(&self) -> (u32, u32);

    /// La misura della superficie di disegno, in pixel.
    fn size_px(&self) -> (u32, u32);

    /// Quanto e' largo/alto `s` con lo stile `st`, **secondo il font vero**.
    fn measure(&self, s: &str, st: Style) -> (u32, u32);

    /// Arrotonda una x alla griglia (verso il basso: mai a meta' cella).
    /// Il default e' l'identita' quando la griglia e' `(1, 1)`.
    fn snap_x(&self, x: u32) -> u32 {
        let g = self.grid().0.max(1);
        (x / g) * g
    }

    /// Scrive `s` con l'angolo in alto a sinistra a `(x, y)` (arrotondando
    /// alla griglia) e ritorna l'avanzamento in pixel.
    fn text(&mut self, x: u32, y: u32, s: &str, st: Style) -> u32;

    fn fill(&mut self, r: PxRect, c: Color);

    /// Bordo di `t` pixel verso l'interno.
    fn border(&mut self, r: PxRect, t: u32, c: Color);

    /// Gradiente verticale (alpha compreso).
    fn vgradient(&mut self, r: PxRect, top: Color, bottom: Color);

    /// Rettangolo con angoli arrotondati di raggio `radius`.
    fn round(&mut self, r: PxRect, radius: u32, c: Color);

    /// Barra proporzionale (`permille` 0..=1000): il target decide lo stile
    /// (capsula nella TUI, piena in una GUI).
    fn bar(&mut self, r: PxRect, permille: u32, bg: Color, fg: Color);

    /// Segmento spesso `t` pixel.
    fn line(&mut self, a: (u32, u32), b: (u32, u32), t: u32, c: Color);
}
