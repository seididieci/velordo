//! Il font TrueType a runtime (D3): caricamento, metriche, cache dei glifi.
//!
//! Il font non e' nel binario: sta nel volume (oggetto ArcaFS) e si legge a
//! pezzi. Il `Source` e' l'unica astrazione di I/O: il crate non sa cosa sia
//! un file (meccanismo puro, ADR-0025) — chi lo usa fornisce le letture.
//!
//! Rasterizzazione: `raster.rs` (punto fisso, nessun float). Il risultato e'
//! deterministico: stesso byte a ogni boot (il CRC del rendering e' un marker
//! stabile).

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::raster::{rasterize, Bitmap};
use crate::ttf::{self, Cmap, Outline, ParseError};

/// Cache dei glifi (slot diretti per codepoint: una collisione evicta).
const CACHE_SLOTS: usize = 256;

/// Fonte di byte per il file del font: il crate non conosce il FS.
pub trait Source {
    /// Legge fino a `buf.len()` byte a partire da `off`. Ritorna quanti
    /// byte ha letto (meno di `buf.len()` = fine file) o `None` su errore.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Option<usize>;
}

/// Metriche del font (in pixel, derivate dalla scala).
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    /// Larghezza della cella (px): l'advance del font.
    pub cell_w: usize,
    /// Altezza della cella (px): ascender - descender.
    pub cell_h: usize,
    /// Ascender in px (dalla base della linea).
    pub ascent_px: i32,
}

/// Un font caricato e pronto a rasterizzare.
pub struct Font {
    src: Box<dyn Source>,
    units_per_em: u16,
    num_glyphs: u16,
    ascent: i16,
    descent: i16,
    line_gap: i16,
    num_h_metrics: u16,
    index_to_loc: i16,
    hmtx: Vec<u8>,
    cmap: Cmap,
    loca: Vec<u8>,
    /// Offset della tabella `glyf` nel file.
    glyf_off: u64,
    /// Scala 16.16 (px per unita' del font).
    scale: i32,
    cache: Vec<Option<(u32, Bitmap)>>,
}

impl Font {
    /// Carica il font da una sorgente. `px` = la dimensione in pixel del
    /// font (l'altezza della cella segue le metriche del font).
    pub fn load(mut src: Box<dyn Source>, px: u32) -> Result<Font, ParseError> {
        // Header sfnt: versione + numero di tabelle.
        let mut head = [0u8; 12];
        if src.read_at(0, &mut head).ok_or(ParseError)? < 12 {
            return Err(ParseError);
        }
        let version = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        if version != 0x0001_0000 && version != 0x7472_7565 {
            return Err(ParseError); // 'OTTO' (CFF) non supportato
        }
        let num_tables = u16::from_be_bytes([head[4], head[5]]) as usize;
        if num_tables == 0 || num_tables > 64 {
            return Err(ParseError);
        }

        // Directory delle tabelle.
        let mut dir = alloc::vec![0u8; num_tables * 16];
        if src.read_at(12, &mut dir).ok_or(ParseError)? < dir.len() {
            return Err(ParseError);
        }
        let mut t_head = None;
        let mut t_maxp = None;
        let mut t_hhea = None;
        let mut t_hmtx = None;
        let mut t_cmap = None;
        let mut t_loca = None;
        let mut t_glyf = None;
        for i in 0..num_tables {
            let rec = &dir[i * 16..i * 16 + 16];
            let tag = &rec[0..4];
            let off = u32::from_be_bytes([rec[8], rec[9], rec[10], rec[11]]) as u64;
            let len = u32::from_be_bytes([rec[12], rec[13], rec[14], rec[15]]) as usize;
            match tag {
                b"head" => t_head = Some((off, len)),
                b"maxp" => t_maxp = Some((off, len)),
                b"hhea" => t_hhea = Some((off, len)),
                b"hmtx" => t_hmtx = Some((off, len)),
                b"cmap" => t_cmap = Some((off, len)),
                b"loca" => t_loca = Some((off, len)),
                b"glyf" => t_glyf = Some((off, len)),
                _ => {}
            }
        }
        let (head_off, _) = t_head.ok_or(ParseError)?;
        let (maxp_off, _) = t_maxp.ok_or(ParseError)?;
        let (hhea_off, _) = t_hhea.ok_or(ParseError)?;
        let (hmtx_off, hmtx_len) = t_hmtx.ok_or(ParseError)?;
        let (cmap_off, cmap_len) = t_cmap.ok_or(ParseError)?;
        let (loca_off, loca_len) = t_loca.ok_or(ParseError)?;
        let (glyf_off, _) = t_glyf.ok_or(ParseError)?;

        // `head`
        let mut head = alloc::vec![0u8; 54];
        if src.read_at(head_off, &mut head).ok_or(ParseError)? < 54 {
            return Err(ParseError);
        }
        let units_per_em = u16::from_be_bytes([head[18], head[19]]);
        let index_to_loc = i16::from_be_bytes([head[50], head[51]]);
        if units_per_em == 0 {
            return Err(ParseError);
        }

        // `maxp`
        let mut maxp = [0u8; 6];
        if src.read_at(maxp_off, &mut maxp).ok_or(ParseError)? < 6 {
            return Err(ParseError);
        }
        let num_glyphs = u16::from_be_bytes([maxp[4], maxp[5]]);

        // `hhea`
        let mut hhea = [0u8; 36];
        if src.read_at(hhea_off, &mut hhea).ok_or(ParseError)? < 36 {
            return Err(ParseError);
        }
        let ascent = i16::from_be_bytes([hhea[4], hhea[5]]);
        let descent = i16::from_be_bytes([hhea[6], hhea[7]]);
        let line_gap = i16::from_be_bytes([hhea[8], hhea[9]]);
        let num_h_metrics = u16::from_be_bytes([hhea[34], hhea[35]]);

        // `hmtx` (solo i primi num_h_metrics advance: il resto usa l'ultimo)
        let hmtx_len = hmtx_len.min(num_h_metrics as usize * 4);
        let mut hmtx = alloc::vec![0u8; hmtx_len];
        if hmtx_len > 0 && src.read_at(hmtx_off, &mut hmtx).ok_or(ParseError)? < hmtx_len {
            return Err(ParseError);
        }

        // `cmap`
        let mut cmap = alloc::vec![0u8; cmap_len];
        if src.read_at(cmap_off, &mut cmap).ok_or(ParseError)? < cmap_len {
            return Err(ParseError);
        }
        let cmap = Cmap::parse(&cmap)?;

        // `loca`
        let mut loca = alloc::vec![0u8; loca_len];
        if src.read_at(loca_off, &mut loca).ok_or(ParseError)? < loca_len {
            return Err(ParseError);
        }

        // Scala 16.16 (px per unita' del font).
        let scale = (((px as i64) << 16) / units_per_em as i64) as i32;

        let mut cache = Vec::with_capacity(CACHE_SLOTS);
        for _ in 0..CACHE_SLOTS {
            cache.push(None);
        }

        Ok(Font {
            src,
            units_per_em,
            num_glyphs,
            ascent,
            descent,
            line_gap,
            num_h_metrics,
            index_to_loc,
            hmtx,
            cmap,
            loca,
            glyf_off,
            scale,
            cache,
        })
    }

    /// Calcola la larghezza di un glifo in termini di celle della griglia.
    pub fn glyph_width_cells(&self, cp: u32) -> usize {
        let gid = self.glyph_index(cp);
        if gid == 0 {
            return 1;
        }
        let advance = self.advance_units(gid) as i64;
        // Usiamo lo scaling del font (similmente a metrics()) per trovare l'advance in pixel.
        let advance_px = (advance * self.scale as i64 + 0xFFFF) >> 16;
        // La cella base è derivata dall'advance dello spazio (GID 0).
        let space_advance = self.advance_units(0) as i64;
        let cell_w = ((space_advance * self.scale as i64 + 0xFFFF) >> 16) as i64;
        
        if advance_px <= 0 || cell_w <= 0 {
            return 1;
        }
        // Usiamo il ceil per coprire interamente l'advance in pixel.
        (((advance_px + cell_w - 1) / cell_w) as usize).max(1)
    }

    /// Metriche in pixel (la cella deriva dal font: larghezza = advance,
    /// altezza = ascender - descender).
    pub fn metrics(&self) -> Metrics {
        let advance = self.advance_units(0).max(1) as i64;
        let cell_w = ((advance * self.scale as i64 + 0xFFFF) >> 16) as usize;
        let box_h = (self.ascent as i64 - self.descent as i64).max(1);
        let cell_h = ((box_h * self.scale as i64 + 0xFFFF) >> 16) as usize;
        let ascent_px = ((self.ascent as i64 * self.scale as i64) >> 16) as i32;
        Metrics {
            cell_w: cell_w.max(1),
            cell_h: cell_h.max(1),
            ascent_px,
        }
    }

    /// Numero di glifi del font.
    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// Ascender del font (unita').
    pub fn ascent(&self) -> i16 {
        self.ascent
    }

    /// Descender del font (unita', negativo).
    pub fn descent(&self) -> i16 {
        self.descent
    }

    /// Spazio extra fra una riga e la successiva (unita' del font): non
    /// entra nella cella (che e' ascender - descender), serve per un
    /// eventuale line-height maggiore.
    pub fn line_gap(&self) -> i16 {
        self.line_gap
    }

    /// Advance del glifo 0 (in unita' del font): il font e' monospace, quindi
    /// l'advance e' lo stesso per tutti i glifi.
    fn advance_units(&self, gid: u16) -> u16 {
        if self.num_h_metrics == 0 {
            return self.units_per_em / 2;
        }
        let idx = (gid as usize).min(self.num_h_metrics as usize - 1);
        let off = idx * 4;
        if off + 2 > self.hmtx.len() {
            return self.units_per_em / 2;
        }
        u16::from_be_bytes([self.hmtx[off], self.hmtx[off + 1]])
    }

    /// Indice del glifo del codepoint (0 = assente).
    pub fn glyph_index(&self, cp: u32) -> u16 {
        self.cmap.glyph_index(cp)
    }

    /// Il glifo rasterizzato del codepoint (dalla cache o rasterizzato ora).
    pub fn glyph(&mut self, cp: u32) -> Option<&Bitmap> {
        let slot = (cp as usize) % CACHE_SLOTS;
        let hit = match &self.cache[slot] {
            Some((cached_cp, _)) => *cached_cp == cp,
            None => false,
        };
        if !hit {
            let gid = self.cmap.glyph_index(cp);
            if gid == 0 || gid >= self.num_glyphs {
                return None;
            }
            let outline = self.build_outline(gid, 0)?;
            let bmp = rasterize(&outline, self.scale);
            self.cache[slot] = Some((cp, bmp));
        }
        self.cache[slot].as_ref().map(|(_, b)| b)
    }

    /// Offset/lunghezza del glifo nella tabella `glyf`.
    fn glyph_range(&self, gid: u16) -> Option<(u64, usize)> {
        let i = gid as usize;
        if self.index_to_loc == 0 {
            let off = i * 2;
            if off + 4 > self.loca.len() {
                return None;
            }
            let a = u16::from_be_bytes([self.loca[off], self.loca[off + 1]]) as u32 * 2;
            let b = u16::from_be_bytes([self.loca[off + 2], self.loca[off + 3]]) as u32 * 2;
            Some((self.glyf_off + a as u64, (b - a) as usize))
        } else {
            let off = i * 4;
            if off + 8 > self.loca.len() {
                return None;
            }
            let a = u32::from_be_bytes([
                self.loca[off],
                self.loca[off + 1],
                self.loca[off + 2],
                self.loca[off + 3],
            ]);
            let b = u32::from_be_bytes([
                self.loca[off + 4],
                self.loca[off + 5],
                self.loca[off + 6],
                self.loca[off + 7],
            ]);
            Some((self.glyf_off + a as u64, (b - a) as usize))
        }
    }

    /// Legge i dati del glifo e ne costruisce il contorno (semplici e
    /// composti, con ricorsione bounded).
    fn build_outline(&mut self, gid: u16, depth: u8) -> Option<Outline> {
        if depth > 4 {
            return None;
        }
        let (off, len) = self.glyph_range(gid)?;
        if len == 0 || len > 64 * 1024 {
            return None; // glifo vuoto o assurdo
        }
        let mut data = alloc::vec![0u8; len];
        if self.src.read_at(off, &mut data)? < len {
            return None;
        }
        if let Some(outline) = ttf::parse_simple_glyph(&data) {
            return Some(outline);
        }
        // Glifo composto: applichiamo le componenti.
        let (comps, _) = ttf::parse_composite(&data)?;
        let mut out = Outline::default();
        for comp in comps.iter() {
            let sub = self.build_outline(comp.glyph, depth + 1)?;
            for contour in sub.contours.iter() {
                let mut moved = alloc::vec::Vec::with_capacity(contour.len());
                for p in contour.iter() {
                    let (x, y) = transform(p.x, p.y, comp);
                    moved.push(ttf::Point {
                        x: x + comp.dx,
                        y: y + comp.dy,
                        on: p.on,
                    });
                }
                out.contours.push(moved);
            }
        }
        Some(out)
    }
}

/// Applica la trasformazione della componente (scala 2.14 o matrice 2x2).
fn transform(x: i32, y: i32, comp: &ttf::Component) -> (i32, i32) {
    if let Some(m) = comp.matrix {
        // 2x2 in 2.14
        let a = m[0] as i64;
        let b = m[1] as i64;
        let c = m[2] as i64;
        let d = m[3] as i64;
        let nx = (a * x as i64 + c * y as i64) >> 14;
        let ny = (b * x as i64 + d * y as i64) >> 14;
        return (nx as i32, ny as i32);
    }
    if let Some(s) = comp.scale_x {
        let s = s as i64;
        let sy = comp.scale_y.unwrap_or(s as i16) as i64;
        return (
            ((s * x as i64) >> 14) as i32,
            ((sy * y as i64) >> 14) as i32,
        );
    }
    (x, y)
}
