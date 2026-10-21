//! Parser TrueType minimale (D3): tabelle, `cmap` (formato 4 e 12) e
//! contorno dei glifi semplici.
//!
//! Nessun float e nessuna libm: solo interi. Il rasterizzatore (`raster.rs`)
//! fa il resto in punto fisso. Formati non supportati (CFF/OTTO, hinting)
//! sono semplicemente assenti: il font e' "non leggibile" e la console
//! ripiega sul fallback.

extern crate alloc;

use alloc::vec::Vec;

/// Errore di parsing (nessun dettaglio: la console registra il fatto).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError;

/// La tabella `cmap` (una delle subtable supportate).
#[derive(Clone, Debug)]
pub enum Cmap {
    /// Formato 4: BMP, segmenti.
    Format4(Cmap4),
    /// Formato 12: gruppi (copre anche i PUA estesi).
    Format12(Cmap12),
}

/// Subtable `cmap` formato 4.
#[derive(Clone, Debug)]
pub struct Cmap4 {
    /// `endCode` per segmento.
    pub end: Vec<u16>,
    /// `startCode` per segmento.
    pub start: Vec<u16>,
    /// `idDelta` per segmento.
    pub delta: Vec<i16>,
    /// `idRangeOffset` per segmento.
    pub range_offset: Vec<u16>,
    /// `glyphIdArray` (dopo gli array dei segmenti).
    pub glyph_ids: Vec<u8>,
}

/// Subtable `cmap` formato 12.
#[derive(Clone, Debug)]
pub struct Cmap12 {
    /// Gruppi `(startCharCode, endCharCode, startGlyphID)`.
    pub groups: Vec<(u32, u32, u32)>,
}

impl Cmap {
    /// Cerca la subtable Unicode nel blob `cmap`: preferisce il formato 12
    /// (copre i PUA estesi: le icone Nerd), altrimenti il formato 4.
    pub fn parse(data: &[u8]) -> Result<Cmap, ParseError> {
        if data.len() < 4 {
            return Err(ParseError);
        }
        let n = u16::from_be_bytes([data[2], data[3]]) as usize;
        let mut best12: Option<usize> = None;
        let mut best4: Option<usize> = None;
        for i in 0..n {
            let rec = 4 + i * 8;
            if rec + 8 > data.len() {
                break;
            }
            let platform = u16::from_be_bytes([data[rec], data[rec + 1]]);
            let encoding = u16::from_be_bytes([data[rec + 2], data[rec + 3]]);
            let off = u32::from_be_bytes([
                data[rec + 4],
                data[rec + 5],
                data[rec + 6],
                data[rec + 7],
            ]) as usize;
            if off + 4 > data.len() {
                continue;
            }
            let format = u16::from_be_bytes([data[off], data[off + 1]]);
            let unicode = matches!(
                (platform, encoding),
                (0, _) | (3, 1) | (3, 10)
            );
            if !unicode {
                continue;
            }
            if format == 12 && best12.is_none() {
                best12 = Some(off);
            }
            if format == 4 && best4.is_none() {
                best4 = Some(off);
            }
        }
        if let Some(off) = best12 {
            return Cmap12::parse(data, off).map(Cmap::Format12);
        }
        if let Some(off) = best4 {
            return Cmap4::parse(data, off).map(Cmap::Format4);
        }
        Err(ParseError)
    }

    /// Codepoint -> indice del glifo (0 = assente).
    pub fn glyph_index(&self, cp: u32) -> u16 {
        match self {
            Cmap::Format4(c) => c.glyph_index(cp),
            Cmap::Format12(c) => c.glyph_index(cp),
        }
    }
}

impl Cmap4 {
    /// Parse della subtable formato 4 (offset assoluto nel blob `cmap`).
    pub fn parse(data: &[u8], off: usize) -> Result<Cmap4, ParseError> {
        if off + 14 > data.len() {
            return Err(ParseError);
        }
        let seg_x2 = u16::from_be_bytes([data[off + 6], data[off + 7]]) as usize;
        if seg_x2 == 0 || seg_x2 % 2 != 0 {
            return Err(ParseError);
        }
        let seg = seg_x2 / 2;
        let base = off + 14;
        if base + seg_x2 * 4 > data.len() {
            return Err(ParseError);
        }
        let mut end = Vec::with_capacity(seg);
        let mut start = Vec::with_capacity(seg);
        let mut delta = Vec::with_capacity(seg);
        let mut range_offset = Vec::with_capacity(seg);
        for i in 0..seg {
            let e = off + 14 + i * 2;
            end.push(u16::from_be_bytes([data[e], data[e + 1]]));
            let s = off + 16 + seg_x2 + i * 2;
            start.push(u16::from_be_bytes([data[s], data[s + 1]]));
            let d = off + 16 + seg_x2 * 2 + i * 2;
            delta.push(i16::from_be_bytes([data[d], data[d + 1]]));
            let r = off + 16 + seg_x2 * 3 + i * 2;
            range_offset.push(u16::from_be_bytes([data[r], data[r + 1]]));
        }
        let ids_off = off + 16 + seg_x2 * 4;
        let glyph_ids = data[ids_off..].to_vec();
        Ok(Cmap4 {
            end,
            start,
            delta,
            range_offset,
            glyph_ids,
        })
    }

    /// Lookup del glifo (formato 4).
    pub fn glyph_index(&self, cp: u32) -> u16 {
        if cp > 0xFFFF {
            return 0;
        }
        let cp = cp as u16;
        let mut lo = 0usize;
        let mut hi = self.end.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.end[mid] < cp {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= self.end.len() || cp < self.start[lo] {
            return 0;
        }
        let delta = self.delta[lo];
        let ro = self.range_offset[lo];
        if ro == 0 {
            return ((cp as i16).wrapping_add(delta)) as u16;
        }
        // glyphIdArray: l'indice e' relativo a (indirizzo di idRangeOffset + ro)
        let addr = (lo as isize * 2 + ro as isize) as usize
            + (cp as isize - self.start[lo] as isize) as usize * 2;
        if addr + 2 > self.glyph_ids.len() {
            return 0;
        }
        let gid = u16::from_be_bytes([self.glyph_ids[addr], self.glyph_ids[addr + 1]]);
        if gid == 0 {
            return 0;
        }
        gid.wrapping_add(delta as u16)
    }
}

impl Cmap12 {
    /// Parse della subtable formato 12 (offset assoluto nel blob `cmap`).
    pub fn parse(data: &[u8], off: usize) -> Result<Cmap12, ParseError> {
        if off + 16 > data.len() {
            return Err(ParseError);
        }
        let n = u32::from_be_bytes([
            data[off + 12],
            data[off + 13],
            data[off + 14],
            data[off + 15],
        ]) as usize;
        if off + 16 + n * 12 > data.len() {
            return Err(ParseError);
        }
        let mut groups = Vec::with_capacity(n);
        for i in 0..n {
            let g = off + 16 + i * 12;
            groups.push((
                u32::from_be_bytes([data[g], data[g + 1], data[g + 2], data[g + 3]]),
                u32::from_be_bytes([data[g + 4], data[g + 5], data[g + 6], data[g + 7]]),
                u32::from_be_bytes([data[g + 8], data[g + 9], data[g + 10], data[g + 11]]),
            ));
        }
        Ok(Cmap12 { groups })
    }

    /// Lookup del glifo (formato 12).
    pub fn glyph_index(&self, cp: u32) -> u16 {
        let mut lo = 0usize;
        let mut hi = self.groups.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (s, e, g) = self.groups[mid];
            if cp < s {
                hi = mid;
            } else if cp > e {
                lo = mid + 1;
            } else {
                let gid = g + (cp - s);
                return if gid > 0xFFFF { 0 } else { gid as u16 };
            }
        }
        0
    }
}

/// Un punto di contorno (in unita' del font) con il flag "on curve".
#[derive(Clone, Copy, Debug, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
    /// true = punto sulla curva, false = punto di controllo (quadratica).
    pub on: bool,
}

/// Un contorno chiuso (sequenza di punti in unita' del font).
pub type Contour = Vec<Point>;

/// Il contorno di un glifo (in unita' del font).
#[derive(Clone, Debug, Default)]
pub struct Outline {
    /// I contorni del glifo.
    pub contours: Vec<Contour>,
}

/// Componente di un glifo composto.
#[derive(Clone, Copy, Debug)]
pub struct Component {
    /// Indice del glifo componente.
    pub glyph: u16,
    /// Offset X (in unita' del font, se `args_are_words`).
    pub dx: i32,
    /// Offset Y.
    pub dy: i32,
    /// true se gli argomenti sono `int16`, false se sono `int8` (signed).
    pub args_are_words: bool,
    /// true se gli argomenti sono `uint8` (non signed).
    pub args_are_unsigned: bool,
    /// Scala X (fissa 2.14) se presente.
    pub scale_x: Option<i16>,
    /// Scala Y.
    pub scale_y: Option<i16>,
    /// Matrice di trasformazione se presente.
    pub matrix: Option<[i16; 4]>,
}

/// Parse del contorno di un glifo **semplice** (contorni con retta e
/// quadratiche). `None` se il glifo e' composto (lo gestisce `font.rs`).
pub fn parse_simple_glyph(data: &[u8]) -> Option<Outline> {
    if data.len() < 10 {
        return None;
    }
    let num_contours = i16::from_be_bytes([data[0], data[1]]);
    if num_contours < 0 {
        return None; // composto
    }
    let num_contours = num_contours as usize;
    if num_contours == 0 {
        return Some(Outline::default());
    }
    let ends_off = 10;
    if ends_off + num_contours * 2 + 2 > data.len() {
        return None;
    }
    let mut ends = Vec::with_capacity(num_contours);
    for i in 0..num_contours {
        let e = ends_off + i * 2;
        ends.push(u16::from_be_bytes([data[e], data[e + 1]]) as usize);
    }
    let num_points = match ends.last() {
        Some(&e) => e + 1,
        None => return None,
    };
    let ins_len_off = ends_off + num_contours * 2;
    let ins_len = u16::from_be_bytes([data[ins_len_off], data[ins_len_off + 1]]) as usize;
    let mut off = ins_len_off + 2 + ins_len;

    // flags (con repeat)
    let mut flags = Vec::with_capacity(num_points);
    while flags.len() < num_points {
        if off >= data.len() {
            return None;
        }
        let f = data[off];
        off += 1;
        flags.push(f);
        if f & 0x08 != 0 {
            if off >= data.len() {
                return None;
            }
            let r = data[off];
            off += 1;
            for _ in 0..r {
                if flags.len() < num_points {
                    flags.push(f);
                }
            }
        }
    }

    // coordinate X (delta)
    let mut xs = Vec::with_capacity(num_points);
    let mut x = 0i32;
    for &f in flags.iter() {
        if f & 0x02 != 0 {
            if off >= data.len() {
                return None;
            }
            let d = data[off] as i32;
            off += 1;
            x += if f & 0x10 != 0 { d } else { -d };
        } else if f & 0x10 == 0 {
            if off + 2 > data.len() {
                return None;
            }
            let d = i16::from_be_bytes([data[off], data[off + 1]]) as i32;
            off += 2;
            x += d;
        }
        xs.push(x);
    }

    // coordinate Y (delta)
    let mut ys = Vec::with_capacity(num_points);
    let mut y = 0i32;
    for &f in flags.iter() {
        if f & 0x04 != 0 {
            if off >= data.len() {
                return None;
            }
            let d = data[off] as i32;
            off += 1;
            y += if f & 0x20 != 0 { d } else { -d };
        } else if f & 0x20 == 0 {
            if off + 2 > data.len() {
                return None;
            }
            let d = i16::from_be_bytes([data[off], data[off + 1]]) as i32;
            off += 2;
            y += d;
        }
        ys.push(y);
    }

    // spezza nei contorni
    let mut contours = Vec::with_capacity(num_contours);
    let mut first = 0usize;
    for &end in ends.iter() {
        if end >= num_points {
            return None;
        }
        let mut c = Contour::with_capacity(end - first + 1);
        for i in first..=end {
            c.push(Point {
                x: xs[i],
                y: ys[i],
                on: flags[i] & 0x01 != 0,
            });
        }
        contours.push(c);
        first = end + 1;
    }
    Some(Outline { contours })
}

/// Parse delle componenti di un glifo **composto**. Ritorna le componenti e
/// i byte consumati.
pub fn parse_composite(data: &[u8]) -> Option<(Vec<Component>, usize)> {
    if data.len() < 10 {
        return None;
    }
    let mut off = 10usize;
    let mut comps = Vec::new();
    loop {
        if off + 4 > data.len() {
            return None;
        }
        let flags = u16::from_be_bytes([data[off], data[off + 1]]);
        let glyph = u16::from_be_bytes([data[off + 2], data[off + 3]]);
        off += 4;
        let args_are_words = flags & 0x0001 != 0;
        let args_are_unsigned = flags & 0x0008 != 0;
        let (dx, dy) = if args_are_words {
            if off + 4 > data.len() {
                return None;
            }
            let a = i16::from_be_bytes([data[off], data[off + 1]]) as i32;
            let b = i16::from_be_bytes([data[off + 2], data[off + 3]]) as i32;
            off += 4;
            (a, b)
        } else {
            if off + 2 > data.len() {
                return None;
            }
            let a = data[off] as i32;
            let b = data[off + 1] as i32;
            off += 2;
            if !args_are_unsigned {
                // gli argomenti a byte sono signed
                (a as i8 as i32, b as i8 as i32)
            } else {
                (a, b)
            }
        };
        let mut scale_x = None;
        let mut scale_y = None;
        let mut matrix = None;
        if flags & 0x0008 != 0 {
            // scaled offset: non ancora supportato (raro nei font monospace)
            if off + 2 > data.len() {
                return None;
            }
            off += 2;
        }
        if flags & 0x0040 != 0 {
            // WE_HAVE_A_SCALE
            if off + 2 > data.len() {
                return None;
            }
            let s = i16::from_be_bytes([data[off], data[off + 1]]);
            off += 2;
            scale_x = Some(s);
            scale_y = Some(s);
        } else if flags & 0x0080 != 0 {
            // WE_HAVE_AN_X_AND_Y_SCALE
            if off + 4 > data.len() {
                return None;
            }
            let sx = i16::from_be_bytes([data[off], data[off + 1]]);
            let sy = i16::from_be_bytes([data[off + 2], data[off + 3]]);
            off += 4;
            scale_x = Some(sx);
            scale_y = Some(sy);
        } else if flags & 0x0200 != 0 {
            // WE_HAVE_A_2X2
            if off + 8 > data.len() {
                return None;
            }
            let mut m = [0i16; 4];
            for i in 0..4 {
                m[i] = i16::from_be_bytes([data[off + i * 2], data[off + i * 2 + 1]]);
            }
            off += 8;
            matrix = Some(m);
        }
        comps.push(Component {
            glyph,
            dx,
            dy,
            args_are_words,
            args_are_unsigned,
            scale_x,
            scale_y,
            matrix,
        });
        if flags & 0x0020 == 0 {
            // MORE_COMPONENTS assente: finisce
            break;
        }
    }
    Some((comps, off))
}
