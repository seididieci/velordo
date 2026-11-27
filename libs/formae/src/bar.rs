//! `Bar` — la barra proporzionale (memoria, carico, capacita').
//!
//! Il widget decide **solo** la proporzione e i colori; la forma (capsula
//! arrotondata nella TUI, barra piena in una GUI) e' una scelta del
//! [`Target`](crate::Target). Cosi' la stessa barra non va riscritta.

use pictura::Color;

use crate::geom::PxRect;
use crate::target::Target;

/// Una barra da 0 a 1000 permille.
#[derive(Clone, Copy, Debug)]
pub struct Bar {
    pub permille: u32,
    pub track: Color,
    pub fill: Color,
}

impl Bar {
    pub const fn new(track: Color, fill: Color) -> Bar {
        Bar {
            permille: 0,
            track,
            fill,
        }
    }

    pub fn set(&mut self, value: u64, max: u64) {
        self.permille = if max == 0 {
            0
        } else {
            ((value.min(max) as u128 * 1000) / max as u128) as u32
        };
    }

    pub fn draw<T: Target + ?Sized>(&self, t: &mut T, r: PxRect) {
        if r.is_empty() {
            return;
        }
        t.bar(r, self.permille, self.track, self.fill);
    }
}
