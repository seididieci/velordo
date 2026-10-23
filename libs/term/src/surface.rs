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
use crate::grid::color;

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
    let r = (c & 0xFF) as u8;
    let g = ((c >> 8) & 0xFF) as u8;
    let b = ((c >> 16) & 0xFF) as u8;
    let a = ((c >> 24) & 0xFF) as u8;
    u32::from_le_bytes([b, g, r, a])
}

/// Estrae il canale alpha (0..255) da un colore RGBA packed.
pub fn alpha(c: u32) -> u32 {
    (c >> 24) & 0xFF
}

/// Trova l'indice VGA (0-15) piu' vicino a un colore truecolor RGB.
/// Ignora l'alpha: per il fallback VGA testuale non esiste trasparenza.
pub fn nearest_vga(c: u32) -> u8 {
    let r = (c & 0xFF) as i32;
    let g = ((c >> 8) & 0xFF) as i32;
    let b = ((c >> 16) & 0xFF) as i32;
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
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16) | ((a as u32) << 24)
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
        _ => color::WHITE,
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
    unsafe fn put(&mut self, x: u32, y: u32, color: u32) {
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
    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
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
    pub fn put_char(
        &mut self,
        col: usize,
        row: usize,
        cp: u32,
        fg: u32,
        bg: u32,
        cursor: bool,
        font: Option<&mut Font>,
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
                // Background: opaco -> fill diretto; trasparente -> blend
                // con il pixel gia' presente (layer 0 / sfondo immagine).
                if bg_a == 0xFF {
                    p.fill_rect(x, y, cw, ch, bg_color);
                } else {
                    p.fill_rect_blend(x, y, cw, ch, bg_color, bg_a);
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
                            // Se il bg era trasparente, il pixel di
                            // destinazione e' già bg fuso col layer 0;
                            // altrimenti e' bg_color. In entrambi i casi
                            // blendiamo il foreground sopra.
                            let dest = if bg_a == 0xFF {
                                bg_color
                            } else {
                                unsafe { p.get(px, py) }
                            };
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
