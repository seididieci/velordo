//! Il **look** in un posto solo: i token visivi della dashboard (pagina,
//! card, testo, accento, track, soglie). Una decisione, non un caso
//! sparso nei widget.

use pictura::{rgb, with_alpha, Color};

use crate::geom::faded;
use crate::Style;

/// Converte un colore `pictura` (`0xAARRGGBB`) nel colore di **cella** del
/// terminale (`0xRRGGBBAA`, la convenzione di `term::grid::color`).
pub const fn cell_color(c: Color) -> u32 {
    let r = (c >> 16) & 0xFF;
    let g = (c >> 8) & 0xFF;
    let b = c & 0xFF;
    let a = (c >> 24) & 0xFF;
    (r << 24) | (g << 16) | (b << 8) | a
}

/// I colori dell'interfaccia.
///
/// I valori sono la linea guida della reference (btop-like) riportata in
/// pixel: superficie scura, testo quasi bianco, accento viola, track blu
/// notte. Non sono una specifica: se un colore non si legge a celle, si
/// cambia qui.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Fondo della superficie (wallpaper/pagina).
    pub page: Color,
    /// Superficie di una card (la toppa).
    pub card: Color,
    /// Testo principale.
    pub text: Color,
    /// Testo secondario (etichette, header).
    pub dim: Color,
    /// Accento (linea del grafico, selezione).
    pub accent: Color,
    /// Track delle barre.
    pub track: Color,
    pub ok: Color,
    pub warn: Color,
    pub crit: Color,
}

impl Theme {
    /// L'opacita' di un pannello (card): ~84%. Traslucido, cosi' il wallpaper
    /// del compositor traspare attenuato sotto il monitor (non un fondo opaco
    /// tutto suo).
    pub const PANEL_ALPHA: u8 = 0xD6;

    /// Il tema scuro della dashboard.
    pub const NIGHT: Theme = Theme {
        page: rgb(0x07, 0x07, 0x0C),
        // Blu scuro "in tema", traslucido: il monitor e' un overlay sul
        // wallpaper, non un pannello opaco.
        card: with_alpha(rgb(0x0E, 0x12, 0x22), Theme::PANEL_ALPHA),
        text: rgb(0xE8, 0xEA, 0xF0),
        dim: rgb(0x8A, 0x93, 0xA6),
        accent: rgb(0xA7, 0x8B, 0xFA),
        track: rgb(0x17, 0x1C, 0x2A),
        ok: rgb(0x55, 0xD6, 0xA7),
        warn: rgb(0xF5, 0xC7, 0x6A),
        crit: rgb(0xFF, 0x5C, 0x70),
    };

    /// Il colore di una soglia: sotto 700 permille ok, sotto 900 warn, poi
    /// critico.
    pub fn level(&self, permille: u32) -> Color {
        if permille >= 900 {
            self.crit
        } else if permille >= 700 {
            self.warn
        } else {
            self.ok
        }
    }

    /// L'area del grafico: l'accento che sfuma nel trasparente (verso il
    /// fondo della card).
    pub fn area_top(&self) -> Color {
        faded(self.accent, 0x60)
    }

    pub const fn area_bottom(&self) -> Color {
        0x0000_0000
    }

    /// La penna del testo (`tabula::Style`) da un colore del tema.
    pub fn pen(&self, fg: Color) -> Style {
        Style::new(cell_color(fg), Style::DEFAULT.bg)
    }

    /// La penna con un fondo esplicito: sopra una toppa il fondo colorato
    /// **copre** il pixel (serve solo per un'evidenziazione voluta).
    pub fn pen_on(&self, fg: Color, bg: Color) -> Style {
        Style::new(cell_color(fg), cell_color(bg))
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::NIGHT
    }
}
