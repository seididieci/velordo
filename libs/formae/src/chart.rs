//! `Chart` — il grafico a **area** con linea e alone.
//!
//! La parte che nessuno vuole riscrivere: una serie di campioni diventa
//! un'area sfumata (dal colore dell'accento al trasparente) piu' una linea
//! che resta leggibile anche a celle. Le primitive sono del [`Target`], il
//! componente non sa se sta disegnando in una toppa della TUI o in una
//! finestra grafica.

use pictura::{with_alpha, Color};

use crate::geom::PxRect;
use crate::target::Target;

/// Un grafico a area.
pub struct Chart<'a> {
    /// Campioni dal **piu' vecchio** al **piu' recente**.
    pub samples: &'a [u64],
    /// Il fondo scala; `0` = automatico (massimo dei campioni, almeno 1).
    pub max: u64,
    /// Colore della linea.
    pub accent: Color,
    /// Il colore dell'area in cima (di norma l'accento con alpha).
    pub area_top: Color,
    /// Il colore dell'area in fondo (di norma trasparente).
    pub area_bottom: Color,
}

impl<'a> Chart<'a> {
    /// Un grafico con l'area derivata dall'accento.
    pub fn new(samples: &'a [u64], accent: Color) -> Chart<'a> {
        Chart {
            samples,
            max: 0,
            accent,
            area_top: with_alpha(accent, 0x60),
            area_bottom: 0,
        }
    }

    /// Fondo scala esplicito (per una percentuale: `1000`).
    pub fn with_max(mut self, max: u64) -> Self {
        self.max = max;
        self
    }

    /// Disegna nel rettangolo `r` (coordinate del target).
    pub fn draw<T: Target + ?Sized>(&self, t: &mut T, r: PxRect) {
        if r.is_empty() || r.w < 2 || r.h < 2 || self.samples.is_empty() {
            return;
        }
        let n = self.samples.len();
        let max = if self.max > 0 {
            self.max
        } else {
            self.samples.iter().copied().max().unwrap_or(0).max(1)
        };
        let x_at = |i: usize| -> u32 {
            if n <= 1 {
                r.right() - 1
            } else {
                r.x + (i as u64 * (r.w as u64 - 1) / (n as u64 - 1)) as u32
            }
        };
        let y_at = |v: u64| -> u32 {
            let h = r.h - 1;
            let rel = (v.min(max) as u128 * h as u128 / max as u128) as u32;
            r.y + h - rel
        };

        // Area: una colonna per volta, dal punto alto della curva al fondo.
        // Il gradiente riparte dalla **curva**, cosi' il colore e' denso sotto
        // la linea e sparisce in fondo (come nella reference).
        for i in 0..n {
            let x0 = x_at(i);
            let x1 = if i + 1 < n { x_at(i + 1) } else { r.right() };
            let y0 = y_at(self.samples[i]);
            let y1 = if i + 1 < n {
                y_at(self.samples[i + 1])
            } else {
                y0
            };
            let span = x1.saturating_sub(x0).max(1);
            for k in 0..span {
                let x = x0 + k;
                if x >= r.right() {
                    break;
                }
                let dy = (y1 as i64 - y0 as i64) * k as i64 / span as i64;
                let top = (y0 as i64 + dy).max(r.y as i64) as u32;
                if top < r.bottom() {
                    t.vgradient(
                        PxRect::new(x, top, 1, r.bottom() - top),
                        self.area_top,
                        self.area_bottom,
                    );
                }
            }
        }

        // Alone (opzionale, spento se trasparente) e linea.
        if pictura::alpha(self.accent) == 0xFF {
            let glow = with_alpha(self.accent, 0x40);
            for i in 0..n.saturating_sub(1) {
                t.line(
                    (x_at(i), y_at(self.samples[i])),
                    (x_at(i + 1), y_at(self.samples[i + 1])),
                    3,
                    glow,
                );
            }
        }
        for i in 0..n.saturating_sub(1) {
            t.line(
                (x_at(i), y_at(self.samples[i])),
                (x_at(i + 1), y_at(self.samples[i + 1])),
                1,
                self.accent,
            );
        }
    }
}
