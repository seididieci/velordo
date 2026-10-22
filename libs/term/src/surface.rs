//! Le surface del terminale (D3): dove il modello mette i caratteri.
//!
//! Due implementazioni, **un** modello:
//! - `Pixel`: framebuffer BGRA (virtio-gpu) — il glifo arriva dal font
//!   rasterizzato, con antialiasing.
//! - `VgaText`: il buffer testo 0xB8000 — il carattere ci mette il BIOS
//!   (font hardware 8x16, niente TTF): il fallback quando il device pixel
//!   non c'e'.
//!
//! I colori sono indici VGA 0-15 (la palette e' la stessa del text mode:
//! il terminale non inventa una palette).

extern crate alloc;

use crate::font::Font;

/// Palette VGA a 16 colori (RGB).
pub const PALETTE: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // 0 nero
    (0x00, 0x00, 0xAA), // 1 blu
    (0x00, 0xAA, 0x00), // 2 verde
    (0x00, 0xAA, 0xAA), // 3 ciano
    (0xAA, 0x00, 0x00), // 4 rosso
    (0xAA, 0x00, 0xAA), // 5 magenta
    (0xAA, 0x55, 0x00), // 6 marrone
    (0xAA, 0xAA, 0xAA), // 7 grigio chiaro
    (0x55, 0x55, 0x55), // 8 grigio scuro
    (0x55, 0x55, 0xFF), // 9 blu chiaro
    (0x55, 0xFF, 0x55), // 10 verde chiaro
    (0x55, 0xFF, 0xFF), // 11 ciano chiaro
    (0xFF, 0x55, 0x55), // 12 rosso chiaro
    (0xFF, 0x55, 0xFF), // 13 magenta chiaro
    (0xFF, 0xFF, 0x55), // 14 giallo
    (0xFF, 0xFF, 0xFF), // 15 bianco
];

/// Il colore (indice VGA) come pixel BGRA (byte in memoria: B, G, R, A).
pub fn bgra(idx: u8) -> u32 {
    let (r, g, b) = PALETTE[(idx & 0x0F) as usize];
    u32::from_le_bytes([b, g, r, 0xFF])
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

    /// Riempie un rettangolo.
    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        for yy in y..(y + h) {
            for xx in x..(x + w) {
                unsafe { self.put(xx, yy, color) };
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

    /// Mette un codepoint in una cella (col,row) con i colori. `cursor`
    /// disegna il cursore software (sottolineatura). Nel path VGA testuale
    /// il cursore e' quello hardware (CRTC): il flag e' ignorato.
    pub fn put_char(
        &mut self,
        col: usize,
        row: usize,
        cp: u32,
        fg: u8,
        bg: u8,
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
                let bg_color = bgra(bg);
                p.fill_rect(x, y, cw, ch, bg_color);
                if let Some(bmp) = font.and_then(|f| f.glyph(cp)) {
                    let baseline = y as i32 + p.geom.ascent_px;
                    let fg_color = bgra(fg);
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
                            let mixed = blend(bg_color, fg_color, cov);
                            unsafe { p.put(px, py, mixed) };
                        }
                    }
                }
                if cursor {
                    p.fill_rect(x, y + ch - 2, cw, 2, bgra(fg));
                }
            }
            Surface::VgaText(v) => {
                if col >= v.cols || row >= v.rows {
                    return;
                }
                let ch = if cp <= 0xFF { cp as u8 } else { b'?' };
                let attr = (fg & 0x0F) | ((bg & 0x0F) << 4);
                let off = (row * v.cols + col) * 2;
                unsafe {
                    core::ptr::write_volatile(v.ptr.add(off), ch);
                    core::ptr::write_volatile(v.ptr.add(off + 1), attr);
                }
            }
        }
    }

    /// Pulisce tutta la surface (colore di background).
    pub fn clear(&mut self, bg: u8) {
        match self {
            Surface::Pixel(p) => {
                let w = p.width;
                let h = p.height;
                p.fill_rect(0, 0, w, h, bgra(bg));
            }
            Surface::VgaText(v) => {
                for i in 0..(v.cols * v.rows) {
                    unsafe {
                        core::ptr::write_volatile(v.ptr.add(i * 2), b' ');
                        core::ptr::write_volatile(v.ptr.add(i * 2 + 1), bg << 4);
                    }
                }
            }
        }
    }
}
