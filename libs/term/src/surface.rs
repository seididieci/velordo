//! Le surface del terminale (D3): dove il modello mette i caratteri.
//!
//! Due implementazioni, **un** modello:
//! - `Pixel`: framebuffer BGRA (virtio-gpu) — il glifo arriva dal font
//!   rasterizzato, con antialiasing.
//! - `VgaText`: il buffer testo 0xB8000 — il carattere ci mette il BIOS
//!   (font hardware 8x16, niente TTF): il fallback quando il device pixel
//!   non c'e'.
//!
//! I colori sono truecolor **RGBA packed** (`0xRRGGBBAA`) provenienti dalla
//! cella; la surface li converte in pixel BGRA per il framebuffer.

extern crate alloc;

use crate::font::Font;

/// Palette ANSI a 16 colori (RGB), sincronizzata con `grid::color`.
///
/// Slot 0–7: colori base. Slot 8–15: colori bright.
pub const PALETTE: [(u8, u8, u8); 16] = [
    (0x0B, 0x0D, 0x14), // 0 BLACK  (blu-notte, non nero puro)
    (0xFF, 0x5C, 0x70), // 1 RED
    (0x55, 0xD6, 0xA7), // 2 GREEN
    (0xF5, 0xC7, 0x6A), // 3 YELLOW
    (0x5B, 0x7C, 0xFF), // 4 BLUE
    (0xC4, 0x5C, 0xFF), // 5 MAGENTA
    (0x42, 0xD9, 0xF5), // 6 CYAN
    (0xE8, 0xEB, 0xF2), // 7 WHITE
    (0x85, 0x8D, 0xA3), // 8 GRAY (bright black)
    (0xFF, 0x84, 0x93), // 9 BRIGHT_RED
    (0x7B, 0xE8, 0xBC), // 10 BRIGHT_GREEN
    (0xFF, 0xDA, 0x85), // 11 BRIGHT_YELLOW
    (0x8B, 0xA3, 0xFF), // 12 BRIGHT_BLUE
    (0xD9, 0x91, 0xFF), // 13 BRIGHT_MAGENTA
    (0x7A, 0xE7, 0xF7), // 14 BRIGHT_CYAN
    (0xFF, 0xFF, 0xFF), // 15 BRIGHT_WHITE
];

/// Il colore (indice VGA) come pixel BGRA (byte in memoria: B, G, R, A).
pub fn bgra(idx: u8) -> u32 {
    let (r, g, b) = PALETTE[(idx & 0x0F) as usize];
    u32::from_le_bytes([b, g, r, 0xFF])
}

/// Converte un colore RGBA packed (`0xRRGGBBAA`) nel formato pixel BGRA
/// del framebuffer (byte in memoria: B, G, R, A).
pub fn rgba_to_bgra(c: u32) -> u32 {
    let r = (c >> 24) as u8;
    let g = ((c >> 16) & 0xFF) as u8;
    let b = ((c >> 8) & 0xFF) as u8;
    let a = (c & 0xFF) as u8;
    u32::from_le_bytes([b, g, r, a])
}

/// Estrae il canale alpha (0..255) da un colore RGBA packed.
pub fn alpha(c: u32) -> u32 {
    c & 0xFF
}

/// Trova l'indice VGA (0-15) piu' vicino a un colore truecolor RGB.
/// Ignora l'alpha: per il fallback VGA testuale non esiste trasparenza.
pub fn nearest_vga(c: u32) -> u8 {
    let r = (c >> 24) as i32;
    let g = ((c >> 16) & 0xFF) as i32;
    let b = ((c >> 8) & 0xFF) as i32;
    let mut best: u8 = 0;
    let mut best_dist: i32 = i32::MAX;
    for (i, (pr, pg, pb)) in PALETTE.iter().enumerate() {
        let dr = r - (*pr as i32);
        let dg = g - (*pg as i32);
        let db = b - (*pb as i32);
        let dist = dr * dr + dg * dg + db * db;
        if dist < best_dist {
            best_dist = dist;
            best = i as u8;
        }
    }
    best
}

/// Impacchetta RGBA in `0xRRGGBBAA`.
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32)
}

/// Copia un rettangolo di pixel BGRA da `src` a `dst`.
///
/// `src` e `dst` sono buffer lineari BGRA con i rispettivi stride (byte per
/// riga). Il rettangolo è `(x, y, w, h)` in pixel.
pub fn blit_bgra(
    dst: *mut u8,
    dst_stride: usize,
    src: *const u8,
    src_stride: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) {
    unsafe {
        for yy in 0..h {
            let src_row = src.add(((y + yy) as usize) * src_stride + (x as usize) * 4);
            let dst_row = dst.add(((y + yy) as usize) * dst_stride + (x as usize) * 4);
            core::ptr::copy_nonoverlapping(src_row, dst_row, w as usize * 4);
        }
    }
}

/// Palette xterm 256 colori: un indice (0..255) -> truecolor RGBA.
///
/// - 0..=15: i 16 colori VGA standard.
/// - 16..=231: cubo 6x6x6 (livelli 0, 95, 135, 175, 215, 255).
/// - 232..=255: scale di grigi da 8 a 238.
pub fn color256(n: u8) -> u32 {
    match n {
        0..=15 => {
            let (r, g, b) = PALETTE[n as usize];
            rgba(r, g, b, 0xFF)
        }
        16..=231 => {
            let n = n as u32 - 16;
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let r = LEVELS[((n / 36) % 6) as usize];
            let g = LEVELS[((n / 6) % 6) as usize];
            let b = LEVELS[(n % 6) as usize];
            rgba(r, g, b, 0xFF)
        }
        232..=255 => {
            let g = 8u8.saturating_add((n as u32 - 232) as u8 * 10);
            rgba(g, g, g, 0xFF)
        }
    }
}

/// Geometria della griglia (colonne/righe, cella e origine).
#[derive(Clone, Copy, Debug)]
pub struct Geom {
    /// Colonne.
    pub cols: usize,
    /// Righe.
    pub rows: usize,
    /// Larghezza della cella (px).
    pub cell_w: u32,
    /// Altezza della cella (px).
    pub cell_h: u32,
    /// Ascender del font in px (la base della linea dentro la cella).
    pub ascent_px: i32,
    /// Origine X della griglia nel framebuffer (px): il resto orizzontale
    /// diviso a meta' (D4: il terminale resta centrato nella finestra).
    pub origin_x: u32,
    /// Origine Y della griglia nel framebuffer (px): il resto verticale
    /// diviso a meta' (sopra e sotto).
    pub origin_y: u32,
}

impl Default for Geom {
    fn default() -> Self {
        Geom {
            cols: 1,
            rows: 1,
            cell_w: 8,
            cell_h: 16,
            ascent_px: 13,
            origin_x: 0,
            origin_y: 0,
        }
    }
}

impl Geom {
    /// La geometria di una superficie pixel: celle intere, resto centrato.
    ///
    /// `cols = floor(W/cell_w)`, `rows = floor(H/cell_h)` e l'origine mette
    /// la griglia al centro dello spazio rimasto (D4).
    pub fn for_surface(width: u32, height: u32, cell_w: u32, cell_h: u32, ascent_px: i32) -> Geom {
        let cell_w = cell_w.max(1);
        let cell_h = cell_h.max(1);
        let cols = (width / cell_w) as usize;
        let rows = (height / cell_h) as usize;
        let used_w = cols as u32 * cell_w;
        let used_h = rows as u32 * cell_h;
        Geom {
            cols: cols.max(1),
            rows: rows.max(1),
            cell_w,
            cell_h,
            ascent_px,
            origin_x: (width.saturating_sub(used_w)) / 2,
            origin_y: (height.saturating_sub(used_h)) / 2,
        }
    }
}

/// Surface pixel: un framebuffer lineare BGRA.
pub struct Pixel {
    /// Base del framebuffer (VA del processo).
    pub ptr: *mut u8,
    /// Larghezza in pixel.
    pub width: u32,
    /// Altezza in pixel.
    pub height: u32,
    /// Byte per riga.
    pub stride: u32,
    /// Geometria della griglia (dal font).
    pub geom: Geom,
}

impl Pixel {
    /// Scrive un pixel BGRA.
    pub unsafe fn put(&mut self, x: u32, y: u32, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let off = (y as usize * self.stride as usize) + (x as usize * 4);
        unsafe {
            core::ptr::write_volatile(self.ptr.add(off) as *mut u32, color);
        }
    }

    /// Legge un pixel BGRA dal framebuffer.
    unsafe fn get(&self, x: u32, y: u32) -> u32 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        let off = (y as usize * self.stride as usize) + (x as usize * 4);
        unsafe { core::ptr::read_volatile(self.ptr.add(off) as *const u32) }
    }

    /// Riempie un rettangolo con un colore opaco.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        for yy in y..(y + h) {
            for xx in x..(x + w) {
                unsafe { self.put(xx, yy, color) };
            }
        }
    }

    /// Riempie un rettangolo blendando `color` con alpha `a` sopra i pixel
    /// gia' presenti (usato per il background trasparente).
    fn fill_rect_blend(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32, a: u32) {
        for yy in y..(y + h) {
            for xx in x..(x + w) {
                let dest = unsafe { self.get(xx, yy) };
                unsafe { self.put(xx, yy, blend(dest, color, a)) };
            }
        }
    }

    /// Riempie **tutta** la superficie con un colore opaco (BGRA): il fondo
    /// di pagina, compresi i margini fuori dalla griglia.
    pub fn fill(&mut self, color: u32) {
        let words = (self.stride as usize * self.height as usize) / 4;
        for i in 0..words {
            let off = i * 4;
            unsafe { core::ptr::write_volatile(self.ptr.add(off) as *mut u32, color) };
        }
    }

    /// Compone `n` pixel BGRA con alpha **non premoltiplicato** sopra la
    /// superficie a partire da (x, y). L'alpha 0 lascia il pixel com'e': lo
    /// sfondo trasparente dell'immagine non deve dipingere (il fondo di
    /// pagina resta visibile).
    pub fn blend_bgra_span(&mut self, x: u32, y: u32, n: u32, src: &[u8]) {
        for i in 0..n {
            let px = x + i;
            if px >= self.width {
                break;
            }
            let s = (i as usize) * 4;
            if s + 4 > src.len() {
                break;
            }
            let a = src[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let rgb = u32::from_le_bytes([src[s], src[s + 1], src[s + 2], 0xFF]);
            let v = if a == 0xFF {
                rgb
            } else {
                blend(unsafe { self.get(px, y) }, rgb, a)
            };
            unsafe { self.put(px, y, v) };
        }
    }
}

/// Blend lineare fra background e foreground (copertura 0..255).
fn blend(bg: u32, fg: u32, cov: u32) -> u32 {
    let mut out = [0u8; 4];
    for i in 0..3 {
        let b = (bg >> (8 * i)) & 0xFF;
        let f = (fg >> (8 * i)) & 0xFF;
        let v = (b * (255 - cov) + f * cov) / 255;
        out[i] = v as u8;
    }
    out[3] = 0xFF;
    u32::from_le_bytes(out)
}

/// Surface testuale: il buffer testo della VGA (0xB8000), celle 8x16.
pub struct VgaText {
    /// Base del buffer testo (VA del processo).
    pub ptr: *mut u8,
    /// Colonne.
    pub cols: usize,
    /// Righe.
    pub rows: usize,
}

/// La surface attiva del terminale.
pub enum Surface {
    /// Framebuffer pixel (virtio-gpu).
    Pixel(Pixel),
    /// Buffer testo VGA (fallback).
    VgaText(VgaText),
}

impl Surface {
    /// Geometria della griglia attiva.
    pub fn geom(&self) -> Geom {
        match self {
            Surface::Pixel(p) => p.geom,
            Surface::VgaText(v) => Geom {
                cols: v.cols,
                rows: v.rows,
                cell_w: 8,
                cell_h: 16,
                ascent_px: 13,
                origin_x: 0,
                origin_y: 0,
            },
        }
    }

    /// Mette un codepoint in una cella (col,row) con colori truecolor.
    /// `cursor` disegna il cursore software (sottolineatura). Nel path VGA
    /// testuale il cursore e' quello hardware (CRTC): il flag e' ignorato.
    /// `l0` se presente è (dati, larghezza, altezza, offset_x, offset_y) del
    /// layer 0 BMP: i pixel sovrapposti vengono composti sopra lo sfondo
    /// (rispettandone l'alpha) prima di disegnare il glifo, cosi' il testo
    /// resta sopra l'immagine.
    pub fn put_char(
        &mut self,
        col: usize,
        row: usize,
        cp: u32,
        fg: u32,
        bg: u32,
        cursor: bool,
        font: Option<&mut Font>,
        l0: Option<(&[u8], u32, u32, u32, u32)>,
    ) {
        match self {
            Surface::Pixel(p) => {
                let cw = p.geom.cell_w;
                let ch = p.geom.cell_h;
                // L'origine centra la griglia nel framebuffer (D4).
                let x = p.geom.origin_x + (col as u32) * cw;
                let y = p.geom.origin_y + (row as u32) * ch;
                let bg_color = rgba_to_bgra(bg);
                let fg_color = rgba_to_bgra(fg);
                let bg_a = alpha(bg);
                // Background: opaco -> fill diretto; altrimenti la cella
                // riparte dal **fondo di pagina** e ci si fonde sopra il bg
                // richiesto (il terminale non ha altri layer sotto: partire
                // dal pixel precedente lascerebbe il carattere vecchio).
                if bg_a == 0xFF {
                    p.fill_rect(x, y, cw, ch, bg_color);
                } else {
                    p.fill_rect(x, y, cw, ch, rgba_to_bgra(crate::grid::color::BLACK));
                    p.fill_rect_blend(x, y, cw, ch, bg_color, bg_a);
                }
                // Copia i pixel del layer 0 sopra lo sfondo dove la cella
                // sovrappone il BMP: cosi' l'immagine resta visibile dietro
                // al testo (e il testo non cancella il BMP). L'alpha
                // dell'immagine viene rispettato: lo sfondo trasparente del
                // BMP lascia vedere lo sfondo della cella.
                if let Some((l0_data, l0_w, l0_h, l0_ox, l0_oy)) = l0 {
                    let img_left = l0_ox;
                    let img_right = l0_ox + l0_w;
                    let img_top = l0_oy;
                    let img_bottom = l0_oy + l0_h;
                    let overlap_x_start = x.max(img_left);
                    let overlap_x_end = (x + cw).min(img_right);
                    let overlap_y_start = y.max(img_top);
                    let overlap_y_end = (y + ch).min(img_bottom);
                    if overlap_x_start < overlap_x_end && overlap_y_start < overlap_y_end {
                        let n = overlap_x_end - overlap_x_start;
                        for sy in overlap_y_start..overlap_y_end {
                            if sy >= p.height {
                                break;
                            }
                            let sy0 = (sy - l0_oy) as usize;
                            if sy0 >= l0_h as usize {
                                break;
                            }
                            let src_off = sy0 * l0_w as usize * 4
                                + ((overlap_x_start - l0_ox) as usize) * 4;
                            let bytes = n as usize * 4;
                            if src_off + bytes > l0_data.len() {
                                break;
                            }
                            p.blend_bgra_span(
                                overlap_x_start,
                                sy,
                                n,
                                &l0_data[src_off..src_off + bytes],
                            );
                        }
                    }
                }
                if let Some(bmp) = font.and_then(|f| f.glyph(cp)) {
                    let baseline = y as i32 + p.geom.ascent_px;
                    for r in 0..bmp.h {
                        for c in 0..bmp.w {
                            let cov = bmp.bits[r * bmp.w + c] as u32;
                            if cov == 0 {
                                continue;
                            }
                            let px = (x as i32 + bmp.xoff + c as i32) as u32;
                            let py = (baseline + bmp.yoff + r as i32) as u32;
                            if px >= p.width || py >= p.height {
                                continue;
                            }
                            // Il pixel di destinazione e' quello realmente
                            // presente: bg fuso, oppure il layer 0 copiato
                            // sopra. Blendare su `bg_color` (invece del
                            // pixel vero) lasciava un alone nero attorno ai
                            // glifi sopra l'immagine.
                            let dest = unsafe { p.get(px, py) };
                            let mixed = blend(dest, fg_color, cov);
                            unsafe { p.put(px, py, mixed) };
                        }
                    }
                }
                if cursor {
                    p.fill_rect(x, y + ch - 2, cw, 2, fg_color);
                }
            }
            Surface::VgaText(v) => {
                if col >= v.cols || row >= v.rows {
                    return;
                }
                // Fallback VGA: mappa truecolor all'indice VGA piu' vicino.
                // L'alpha viene ignorato (nessuna trasparenza su VGA).
                let fg_vga = nearest_vga(fg);
                let bg_vga = nearest_vga(bg);
                let ch = if cp <= 0xFF { cp as u8 } else { b'?' };
                let attr = (fg_vga & 0x0F) | ((bg_vga & 0x0F) << 4);
                let off = (row * v.cols + col) * 2;
                unsafe {
                    core::ptr::write_volatile(v.ptr.add(off), ch);
                    core::ptr::write_volatile(v.ptr.add(off + 1), attr);
                }
            }
        }
    }

    /// Pulisce tutta la surface (colore di background truecolor).
    pub fn clear(&mut self, bg: u32) {
        match self {
            Surface::Pixel(p) => {
                let w = p.width;
                let h = p.height;
                let bg_a = alpha(bg);
                if bg_a == 0xFF {
                    p.fill_rect(0, 0, w, h, rgba_to_bgra(bg));
                } else {
                    p.fill_rect_blend(0, 0, w, h, rgba_to_bgra(bg), bg_a);
                }
            }
            Surface::VgaText(v) => {
                let bg_vga = nearest_vga(bg);
                for i in 0..(v.cols * v.rows) {
                    unsafe {
                        core::ptr::write_volatile(v.ptr.add(i * 2), b' ');
                        core::ptr::write_volatile(v.ptr.add(i * 2 + 1), bg_vga << 4);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Regressioni invisibili a occhio, trovate sul framebuffer vero:
    //! - la convenzione dei colori (`0xRRGGBBAA`) letta al contrario dava
    //!   sfondi semi-trasparenti che si accumulavano (un rosso scuro);
    //! - l'offset di riga del layer 0 senza `* 4` copiava bene la prima riga
    //!   del BMP e sbavava tutte le successive (striscia verticale).

    use super::*;
    use crate::grid::color;

    /// Framebuffer BGRA di prova 32x16 (griglia 4x2 di celle 8x8 a 0,0).
    fn surface(buf: &mut [u32]) -> Surface {
        Surface::Pixel(Pixel {
            ptr: buf.as_mut_ptr() as *mut u8,
            width: 32,
            height: 16,
            stride: 32 * 4,
            geom: Geom {
                cols: 4,
                rows: 2,
                cell_w: 8,
                cell_h: 8,
                ascent_px: 6,
                origin_x: 0,
                origin_y: 0,
            },
        })
    }

    /// Immagine 8x8 dove ogni pixel e' unico (riga nel B, colonna nel G):
    /// qualunque offset sbagliato di riga o di colonna si vede subito.
    /// Byte in memoria: B, G, R, A.
    fn immagine() -> alloc::vec::Vec<u8> {
        let mut img = alloc::vec::Vec::new();
        for y in 0..8u32 {
            for x in 0..8u32 {
                img.extend_from_slice(&[y as u8, x as u8, 0x40 + y as u8, 0xFF]);
            }
        }
        img
    }

    /// Un `put_char` di una cella sopra il layer 0 copia il BMP riga per riga
    /// e lo lascia intatto (il testo non cancella l'immagine). Vale sia con
    /// background opaco sia trasparente.
    #[test]
    fn layer0_copiato_riga_per_riga() {
        let img = immagine();
        for bg in [color::BLACK, rgba(0, 0, 0, 0)] {
            let mut fb = alloc::vec![0u32; 32 * 16];
            let mut s = surface(&mut fb);
            // Cella (1,1) = pixel (8,8): l'immagine (8x8) la ricopre esatta.
            s.put_char(1, 1, 0, color::WHITE, bg, false, None, Some((&img, 8, 8, 8, 8)));
            for y in 0..8usize {
                for x in 0..8usize {
                    let got = fb[(8 + y) * 32 + (8 + x)];
                    let want = u32::from_le_bytes([y as u8, x as u8, 0x40 + y as u8, 0xFF]);
                    assert_eq!(
                        got, want,
                        "bg={:#010x} pixel ({},{}) del layer 0",
                        bg, x, y
                    );
                }
            }
        }
    }

    /// L'alpha dell'immagine viene rispettato: lo sfondo trasparente del BMP
    /// (`a = 0`) lascia il colore di pagina, i pixel opachi sono l'immagine, i
    /// traslucidi sono il blend dei due.
    #[test]
    fn layer0_alpha_rispettato() {
        // 8x8 bianchi con alpha 0, 0x40, 0x80, 0xFF (poi si ripete).
        let alpha = [0u8, 0x40, 0x80, 0xFF];
        let mut img = alloc::vec::Vec::new();
        for _ in 0..8 {
            for x in 0..8 {
                img.extend_from_slice(&[0xFF, 0xFF, 0xFF, alpha[x % 4]]);
            }
        }
        let mut fb = alloc::vec![0u32; 32 * 16];
        let mut s = surface(&mut fb);
        s.put_char(1, 1, 0, color::WHITE, color::BLACK, false, None, Some((&img, 8, 8, 8, 8)));

        let bg = rgba_to_bgra(color::BLACK);
        let white = 0xFFFF_FFFF;
        for x in 0..8usize {
            let got = fb[8 * 32 + 8 + x];
            match alpha[x % 4] {
                0 => assert_eq!(got, bg, "alpha 0: resta il fondo di pagina"),
                0xFF => assert_eq!(got, white, "alpha 255: pixel dell'immagine"),
                a => assert_eq!(got, blend(bg, white, a as u32), "alpha {}", a),
            }
        }
    }

    /// I colori sono `0xRRGGBBAA`: l'alpha e' il byte **basso**, il rosso
    /// quello alto. Letto al contrario, `BLACK` risultava trasparente.
    #[test]
    fn colore_rrggbbaa() {
        let c = rgba(0x0B, 0x0D, 0x14, 0xFF);
        assert_eq!(c, 0x0B0D_14FF);
        assert_eq!(alpha(c), 0xFF, "alpha nel byte basso");
        assert_eq!(alpha(rgba(1, 2, 3, 0x80)), 0x80);
        // BGRA in memoria: B, G, R, A.
        assert_eq!(rgba_to_bgra(c), u32::from_le_bytes([0x14, 0x0D, 0x0B, 0xFF]));
        // Il fallback VGA legge gli stessi canali (niente alpha).
        assert_eq!(nearest_vga(rgba(0xFF, 0x5C, 0x70, 0xFF)), 1);
        assert_eq!(nearest_vga(color::WHITE), 7);
    }
}
