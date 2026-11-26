//! `pictura` — disegno in **bitmap** per le toppe del terminale (ADR-0097,
//! ADR-0099).
//!
//! Una toppa e' un buffer RGBA (BGRA in memoria, l'ordine del framebuffer) che
//! si disegna **sotto** le celle: le primitive qui sotto riempiono quel buffer,
//! non conoscono il terminale, il font ne' il server. Chi le usa decide dove
//! finisce il risultato (una toppa in modalita' testo, la superficie di una
//! finestra in modalita' grafica).
//!
//! Le primitive sono scelte per **degradare bene** in celle: barre, linee,
//! rettangoli e grafici a barre sono leggibili anche a 10×22 pixel, mentre una
//! curva fine no. Il testo non e' qui: in modalita' testo lo scrive l'app con
//! `sess::out`, in grafica arrivera' col rasterizzatore proporzionale.
//!
//! Nessuna dipendenza dal sistema: si prova sull'host, pixel per pixel.

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

/// Colore in formato framebuffer: `0xAARRGGBB`? No — BGRA in memoria.
///
/// Convenzione identica a `term::grid::color` / `term::surface`: un `u32`
/// impacchettato come **B** nei bit 0..8, **G** 8..16, **R** 16..24, **A**
/// 24..32.
pub type Color = u32;

/// Costruisce un colore opaco da R, G, B.
pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    (b as u32) | ((g as u32) << 8) | ((r as u32) << 16) | 0xFF00_0000
}

/// Una superficie BGRA su cui disegnare, con clip ai bordi.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    /// `w * h * 4` byte, BGRA.
    pub data: Vec<u8>,
}

impl Canvas {
    /// Tela `w×h` riempita di `bg`.
    pub fn new(w: usize, h: usize, bg: Color) -> Canvas {
        let mut data = vec![0u8; w * h * 4];
        let b = bg.to_le_bytes();
        for px in data.chunks_exact_mut(4) {
            px.copy_from_slice(&b);
        }
        Canvas { w, h, data }
    }

    /// Il colore del pixel `(x, y)` (`None` fuori).
    pub fn pixel(&self, x: usize, y: usize) -> Option<Color> {
        if x >= self.w || y >= self.h {
            return None;
        }
        let o = (y * self.w + x) * 4;
        Some(u32::from_le_bytes([
            self.data[o],
            self.data[o + 1],
            self.data[o + 2],
            self.data[o + 3],
        ]))
    }

    /// Il framebuffer da consegnare a `sess::patch`.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Un pixel, se dentro.
    pub fn put(&mut self, x: usize, y: usize, c: Color) {
        if x >= self.w || y >= self.h {
            return;
        }
        let o = (y * self.w + x) * 4;
        self.data[o..o + 4].copy_from_slice(&c.to_le_bytes());
    }

    /// Riempie un rettangolo.
    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: Color) {
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        let b = c.to_le_bytes();
        for yy in y..y1 {
            let base = (yy * self.w + x) * 4;
            for xx in x..x1 {
                let o = base + (xx - x) * 4;
                self.data[o..o + 4].copy_from_slice(&b);
            }
        }
    }

    /// Bordo di un rettangolo, `t` pixel di spessore (verso l'interno).
    pub fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, t: usize, c: Color) {
        if w == 0 || h == 0 {
            return;
        }
        let t = t.max(1).min(w).min(h);
        self.fill(x, y, w, t, c);
        self.fill(x, y + h - t, w, t, c);
        self.fill(x, y, t, h, c);
        self.fill(x + w - t, y, t, h, c);
    }

    /// Riga orizzontale.
    pub fn hline(&mut self, x: usize, y: usize, w: usize, c: Color) {
        self.fill(x, y, w, 1, c);
    }

    /// Riga verticale.
    pub fn vline(&mut self, x: usize, y: usize, h: usize, c: Color) {
        self.fill(x, y, 1, h, c);
    }

    /// Segmento (Bresenham), spesso `t` pixel: serve ai rami di un grafo.
    pub fn line(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, t: usize, c: Color) {
        let (mut x0, mut y0) = (x0 as i32, y0 as i32);
        let (x1, y1) = (x1 as i32, y1 as i32);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            if t <= 1 {
                self.put(x0 as usize, y0 as usize, c);
            } else {
                self.fill(x0 as usize, y0 as usize, t, t, c);
            }
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Barra di riempimento orizzontale: `permille` (0..=1000) del rettangolo.
    /// A celle si legge benissimo (e' la forma che degrada meglio).
    pub fn bar(&mut self, x: usize, y: usize, w: usize, h: usize, permille: u32, bg: Color, fg: Color) {
        self.fill(x, y, w, h, bg);
        let full = ((w as u32 * permille.min(1000)) / 1000) as usize;
        if full > 0 {
            self.fill(x, y, full, h, fg);
        }
    }

    /// Grafico a barre verticali (sparkline) di `values` nel rettangolo:
    /// ogni valore e' scalato sul massimo. Colonne da 1 px, una per valore.
    pub fn sparkline(&mut self, x: usize, y: usize, w: usize, h: usize, values: &[u64], c: Color) {
        if values.is_empty() || w == 0 || h == 0 {
            return;
        }
        let max = values.iter().copied().max().unwrap_or(0).max(1);
        let n = values.len().min(w);
        for (i, &v) in values[..n].iter().enumerate() {
            let bh = ((v as u128 * h as u128) / max as u128) as usize;
            let bh = bh.min(h).max(if v > 0 { 1 } else { 0 });
            if bh > 0 {
                self.fill(x + i, y + h - bh, 1, bh, c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BG: Color = rgb(0, 0, 0);
    const FG: Color = rgb(255, 0, 0);

    #[test]
    fn nuovo_e_pixel() {
        let c = Canvas::new(4, 3, BG);
        assert_eq!((c.w, c.h), (4, 3));
        assert_eq!(c.bytes().len(), 4 * 3 * 4);
        assert_eq!(c.pixel(0, 0), Some(BG));
        assert_eq!(c.pixel(4, 0), None);
        // BGRA in memoria: nero opaco, poi rosso puro = B=0, G=0, R=255, A=255.
        assert_eq!(&c.bytes()[0..4], &[0, 0, 0, 255]);
        let mut d = Canvas::new(1, 1, FG);
        d.put(0, 0, FG);
        assert_eq!(&d.bytes()[0..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn fill_e_clip() {
        let mut c = Canvas::new(4, 2, BG);
        c.fill(2, 0, 10, 10, FG); // sborda: clippa
        assert_eq!(c.pixel(2, 0), Some(FG));
        assert_eq!(c.pixel(3, 1), Some(FG));
        assert_eq!(c.pixel(1, 0), Some(BG));
        // Fuori non esplode.
        c.put(99, 99, FG);
    }

    #[test]
    fn bordo() {
        let mut c = Canvas::new(6, 4, BG);
        c.rect(0, 0, 6, 4, 1, FG);
        assert_eq!(c.pixel(0, 0), Some(FG));
        assert_eq!(c.pixel(5, 3), Some(FG));
        assert_eq!(c.pixel(5, 0), Some(FG));
        assert_eq!(c.pixel(0, 3), Some(FG));
        // Dentro resta il fondo.
        assert_eq!(c.pixel(1, 1), Some(BG));
        assert_eq!(c.pixel(4, 2), Some(BG));
    }

    #[test]
    fn barra_proporzionale() {
        let mut c = Canvas::new(10, 2, BG);
        c.bar(0, 0, 10, 2, 500, BG, FG);
        for x in 0..10 {
            let want = if x < 5 { FG } else { BG };
            assert_eq!(c.pixel(x, 0), Some(want), "colonna {}", x);
        }
        // Permille fuori scala: clampa.
        c.bar(0, 0, 10, 1, 5000, BG, FG);
        assert_eq!(c.pixel(9, 0), Some(FG));
    }

    #[test]
    fn sparkline_scala_sul_massimo() {
        let mut c = Canvas::new(3, 4, BG);
        c.sparkline(0, 0, 3, 4, &[0, 2, 4], FG);
        // 0 = niente, 2 = meta' (2 righe dal basso), 4 = tutta l'altezza.
        assert_eq!(c.pixel(0, 3), Some(BG));
        assert_eq!(c.pixel(1, 3), Some(FG));
        assert_eq!(c.pixel(1, 1), Some(BG));
        assert_eq!(c.pixel(2, 0), Some(FG));
        assert_eq!(c.pixel(2, 3), Some(FG));
    }

    #[test]
    fn linea_e_spessore() {
        let mut c = Canvas::new(5, 5, BG);
        c.line(0, 4, 4, 0, 1, FG);
        assert_eq!(c.pixel(0, 4), Some(FG));
        assert_eq!(c.pixel(4, 0), Some(FG));
        assert_eq!(c.pixel(2, 2), Some(FG));
        assert_eq!(c.pixel(0, 0), Some(BG));
        // Spessa: 2×2.
        let mut d = Canvas::new(5, 5, BG);
        d.line(0, 0, 0, 0, 2, FG);
        assert_eq!(d.pixel(1, 1), Some(FG));
    }
}
