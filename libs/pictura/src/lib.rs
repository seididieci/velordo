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

/// L'alpha di un colore (0 = trasparente, 255 = opaco).
pub const fn alpha(c: Color) -> u8 {
    (c >> 24) as u8
}

/// Lo stesso colore con un altro alpha.
pub const fn with_alpha(c: Color, a: u8) -> Color {
    (c & 0x00FF_FFFF) | ((a as u32) << 24)
}

/// Composizione **source-over** non premoltiplicata: `src` sopra `dst`.
///
/// Gestisce anche un `dst` trasparente (una toppa che si costruisce dal
/// nulla): il risultato conserva l'alpha di `src` e il suo colore. Con
/// `src` opaco ritorna `src` (il percorso veloce che tiene identici i
/// disegni gia' esistenti).
fn blend_over(dst: Color, src: Color) -> Color {
    let sa = alpha(src) as u32;
    if sa == 0xFF {
        return src;
    }
    if sa == 0 {
        return dst;
    }
    let da = alpha(dst) as u32;
    // `out_a * 255`, cosi' i denominatori restano interi.
    let out_a_num = sa * 255 + da * (255 - sa);
    if out_a_num == 0 {
        return 0;
    }
    let mut out = 0u32;
    for i in 0..3 {
        let s = ((src >> (8 * i)) & 0xFF) as u32;
        let d = ((dst >> (8 * i)) & 0xFF) as u32;
        let v = (s * sa * 255 + d * da * (255 - sa)) / out_a_num;
        out |= v.min(255) << (8 * i);
    }
    out | ((out_a_num / 255) << 24)
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

    /// Un pixel, se dentro. Con `alpha < 255` il colore si **compone** sopra
    /// quello gia' presente (source-over non premoltiplicato): e' cosi' che
    /// una toppa puo' lasciar vedere il wallpaper (ADR-0099, addendum).
    /// `alpha == 0` non tocca niente.
    pub fn put(&mut self, x: usize, y: usize, c: Color) {
        if x >= self.w || y >= self.h {
            return;
        }
        let o = (y * self.w + x) * 4;
        let a = alpha(c);
        if a == 0 {
            return;
        }
        if a == 0xFF {
            self.data[o..o + 4].copy_from_slice(&c.to_le_bytes());
            return;
        }
        let dst = u32::from_le_bytes([
            self.data[o],
            self.data[o + 1],
            self.data[o + 2],
            self.data[o + 3],
        ]);
        self.data[o..o + 4].copy_from_slice(&blend_over(dst, c).to_le_bytes());
    }

    /// Riempie un rettangolo (il percorso opaco resta quello di sempre: un
    /// `copy_from_slice` per pixel, nessun blend).
    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: Color) {
        if w == 0 || h == 0 || alpha(c) == 0 {
            return;
        }
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        if x >= x1 || y >= y1 {
            return;
        }
        if alpha(c) != 0xFF {
            for yy in y..y1 {
                for xx in x..x1 {
                    self.put(xx, yy, c);
                }
            }
            return;
        }
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

    /// Gradiente **verticale** (uno per riga): `top` in cima, `bottom` in
    /// fondo. I canali si interpolano in linea retta, **alpha compreso**:
    /// e' l'area sfumata dei grafici (il fondo del chart).
    pub fn vgradient(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        top: Color,
        bottom: Color,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        for yy in 0..h {
            let t = if h > 1 {
                (yy as u32 * 255) / (h as u32 - 1)
            } else {
                0
            };
            self.fill(x, y + yy, w, 1, lerp_color(top, bottom, t));
        }
    }

    /// Rettangolo con **angoli arrotondati** di raggio `r`, clippato alla
    /// tela. Il bordo del cerchio e' antialiasato con 4×4 sub-campioni: con
    /// un canvas **trasparente** i pixel fuori dal raggio restano trasparenti
    /// (e la toppa lascia vedere il wallpaper); con un fondo opaco si fondono
    /// con esso.
    pub fn round_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        r: usize,
        c: Color,
    ) {
        if w == 0 || h == 0 || alpha(c) == 0 {
            return;
        }
        let r = r.min(w / 2).min(h / 2);
        if r == 0 {
            self.fill(x, y, w, h, c);
            return;
        }
        // Bande piene: centro, poi sopra/sotto fra i due angoli.
        self.fill(x, y + r, w, h - 2 * r, c);
        self.fill(x + r, y, w - 2 * r, r, c);
        self.fill(x + r, y + h - r, w - 2 * r, r, c);
        // I quattro angoli, con la copertura dei sub-campioni.
        for j in 0..r {
            for i in 0..r {
                let cov = corner_coverage(i, j, r);
                self.put_cov(x + i, y + j, c, cov);
                self.put_cov(x + w - 1 - i, y + j, c, cov);
                self.put_cov(x + i, y + h - 1 - j, c, cov);
                self.put_cov(x + w - 1 - i, y + h - 1 - j, c, cov);
            }
        }
    }

    /// Barra **a capsula**: fondo arrotondato + riempimento proporzionale
    /// (`permille` 0..=1000) anch'esso arrotondato. E' la barra dei pannelli
    /// (memoria, carico): a celle resta leggibile perche' il riempimento e'
    /// comunque una frazione orizzontale.
    pub fn round_bar(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        permille: u32,
        bg: Color,
        fg: Color,
    ) {
        self.round_rect(x, y, w, h, h / 2, bg);
        let full = ((w as u32 * permille.min(1000)) / 1000) as usize;
        if full > 0 {
            let r = (h / 2).min(full / 2);
            self.round_rect(x, y, full, h, r, fg);
        }
    }

    /// Polilinea spessa `t` pixel (i segmenti sono delimitati dai punti):
    /// e' la **linea** del grafico. Il glow sono piu' passate con alpha
    /// decrescente, fatte dal chiamante.
    pub fn polyline(&mut self, points: &[(usize, usize)], t: usize, c: Color) {
        for pair in points.windows(2) {
            self.line(pair[0].0, pair[0].1, pair[1].0, pair[1].1, t.max(1), c);
        }
    }

    /// Un pixel con copertura `cov` (0..=16): moltiplica l'alpha del colore.
    fn put_cov(&mut self, x: usize, y: usize, c: Color, cov: u32) {
        if cov == 0 {
            return;
        }
        let a = (alpha(c) as u32 * cov.min(16)) / 16;
        self.put(x, y, with_alpha(c, a as u8));
    }
}

/// Interpolazione lineare fra due colori (`t` 0..=255), alpha compreso.
fn lerp_color(a: Color, b: Color, t: u32) -> Color {
    let t = t.min(255);
    let mut out = 0u32;
    for i in 0..4 {
        let ca = (a >> (8 * i)) & 0xFF;
        let cb = (b >> (8 * i)) & 0xFF;
        let v = (ca * (255 - t) + cb * t + 127) / 255;
        out |= v.min(255) << (8 * i);
    }
    out
}

/// Copertura (0..=16) del pixel `(i, j)` di un angolo arrotondato di raggio
/// `r`, misurato **verso l'interno** dallo spigolo: il centro del cerchio
/// sta a `(r, r)`. Le distanze sono in ottavi di pixel, quindi il confronto
/// e' intero (niente `sqrt`).
fn corner_coverage(i: usize, j: usize, r: usize) -> u32 {
    let r8 = 8 * r as i32;
    let r2 = r8 * r8;
    let mut inside = 0;
    for sy in 0..4 {
        let dy = 8 * j as i32 + (2 * sy + 1) - r8;
        for sx in 0..4 {
            let dx = 8 * i as i32 + (2 * sx + 1) - r8;
            if dx * dx + dy * dy <= r2 {
                inside += 1;
            }
        }
    }
    inside
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

    fn parts(c: Color) -> (u8, u8, u8) {
        (((c >> 16) & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, (c & 0xFF) as u8)
    }

    #[test]
    fn alpha_si_compone_col_fondo() {
        assert_eq!(alpha(with_alpha(FG, 7)), 7);
        let mut c = Canvas::new(2, 1, BG);
        c.put(0, 0, with_alpha(FG, 128));
        let p = c.pixel(0, 0).unwrap();
        assert_eq!(alpha(p), 255, "il fondo e' opaco");
        assert_eq!(parts(p), (128, 0, 0), "rosso al 50% su nero");
        // Alpha zero: il pixel non si tocca.
        c.put(1, 0, with_alpha(FG, 0));
        assert_eq!(c.pixel(1, 0), Some(BG));
        // Il riempimento segue la stessa regola.
        c.fill(0, 0, 2, 1, with_alpha(FG, 0));
        assert_eq!(c.pixel(0, 0), Some(p));
    }

    #[test]
    fn gradiente_verticale_alpha_compreso() {
        let mut c = Canvas::new(1, 3, 0);
        c.vgradient(0, 0, 1, 3, rgb(255, 0, 0), rgb(0, 0, 255));
        assert_eq!(c.pixel(0, 0), Some(rgb(255, 0, 0)));
        assert_eq!(c.pixel(0, 2), Some(rgb(0, 0, 255)));
        let (r, _, b) = parts(c.pixel(0, 1).unwrap());
        assert!(r > 100 && b > 100, "meta' strada: r={} b={}", r, b);
        // Un gradiente verso il trasparente conserva l'alpha (area del chart).
        let mut d = Canvas::new(1, 3, 0);
        d.vgradient(0, 0, 1, 3, with_alpha(FG, 200), with_alpha(FG, 0));
        assert_eq!(alpha(d.pixel(0, 0).unwrap()), 200);
        assert_eq!(alpha(d.pixel(0, 2).unwrap()), 0);
    }

    #[test]
    fn angoli_arrotondati_e_trasparenza() {
        let mut c = Canvas::new(10, 6, 0);
        c.round_rect(0, 0, 10, 6, 3, FG);
        // Fuori dal raggio il pixel resta **trasparente** (il wallpaper si vede).
        assert_eq!(c.pixel(0, 0), Some(0));
        // Centro e fascia piena.
        assert_eq!(c.pixel(5, 3), Some(FG));
        assert_eq!(c.pixel(5, 0), Some(FG));
        // Bordo del cerchio: copertura parziale (antialias), non 0 ne' 255.
        let a = alpha(c.pixel(0, 1).unwrap());
        assert!(a > 0 && a < 255, "alpha di bordo = {}", a);
        // Su un fondo opaco l'angolo si fonde invece di restare scoperto.
        let mut d = Canvas::new(10, 6, BG);
        d.round_rect(0, 0, 10, 6, 3, FG);
        assert_eq!(d.pixel(0, 0), Some(BG));
    }

    #[test]
    fn barra_a_capsula_proporzionale() {
        let base = rgb(1, 2, 3);
        let mut c = Canvas::new(20, 8, base);
        c.round_bar(0, 0, 20, 8, 500, BG, FG);
        assert_eq!(c.pixel(5, 4), Some(FG));
        assert_eq!(c.pixel(15, 4), Some(BG));
        // L'angolo del track non e' dipinto: resta il fondo della tela.
        assert_eq!(c.pixel(0, 0), Some(base));
        // 0 per mille: solo il track.
        let mut d = Canvas::new(20, 8, base);
        d.round_bar(0, 0, 20, 8, 0, BG, FG);
        assert_eq!(d.pixel(0, 4), Some(BG));
        assert_eq!(d.pixel(19, 4), Some(BG));
    }

    #[test]
    fn polilinea_attraversa_i_punti() {
        let mut c = Canvas::new(5, 5, BG);
        c.polyline(&[(0, 4), (2, 2), (4, 0)], 1, FG);
        assert_eq!(c.pixel(0, 4), Some(FG));
        assert_eq!(c.pixel(2, 2), Some(FG));
        assert_eq!(c.pixel(4, 0), Some(FG));
        assert_eq!(c.pixel(0, 0), Some(BG));
    }
}
