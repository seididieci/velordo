//! Rasterizzazione di un contorno TrueType in **punto fisso** (D3).
//!
//! Nessun float: le coordinate sono in 16.16 (1/65536 px) — la scala
//! (px per unita' del font) e' un intero in 16.16, quindi il troncamento
//! non si vede. Le quadratiche di
//! Bezier vengono appiattite in segmenti (subdivisione a profondita'
//! costante) e il riempimento usa la regola del winding non-zero.
//!
//! La copertura e' **esatta in orizzontale** (la parte di span che cade in un
//! pixel si somma in 16.16, senza quantizzazione) e **campionata in
//! verticale** (8 sub-scanline per riga). Il risultato (0..255) e'
//! deterministico: stesso byte a ogni boot (il CRC del rendering e' un marker
//! stabile).
//!
//! Due invarianti che il codice deve rispettare (un errore qui si vede come
//! testo "assottigliato" e irregolare, non come un crash):
//! - uno span puo' stare **interamente dentro un pixel** (asta sottile, punta
//!   di una diagonale): l'ultimo pixel parziale va contato, non solo quelli
//!   attraversati per intero;
//! - il bounding box usa il **ceil** (un contorno che finisce sul confine non
//!   aggiunge una riga/colonna trasparente).

extern crate alloc;

use alloc::vec::Vec;
use crate::ttf::Outline;

/// Sub-pixel in 16.16: 65536 = 1 px.
pub const SUB: i32 = 65536;

/// Sotto-scanline per riga di pixel (copertura 0..SUBSAMPLES per
/// sub-scanline). A 4 il bordo orizzontale e' quantizzato a 1/4 di pixel e si
/// vede sulle aste orizzontali (errore max 32/255 contro un riferimento
/// 8x8); a 8 l'errore massimo scende a 15/255. Oltre non serve: il
/// rasterizzato e' gia' indistinguibile dal riferimento.
const SUBSAMPLES: i32 = 8;

/// Segmenti per quadratiche (appiattimento).
const CURVE_SEGMENTS: usize = 8;

/// Bitmap di copertura 8 bit per pixel.
#[derive(Clone, Debug, Default)]
pub struct Bitmap {
    /// Larghezza in pixel.
    pub w: usize,
    /// Altezza in pixel.
    pub h: usize,
    /// Copertura 0..255, riga per riga (`w` byte per riga).
    pub bits: Vec<u8>,
    /// Offset del bitmap rispetto all'origine del glifo (in pixel): il
    /// glifo va disegnato a `(x + xoff, y + yoff)`.
    pub xoff: i32,
    /// Offset verticale (in pixel, y verso il basso).
    pub yoff: i32,
}

/// Rasterizza il contorno (in unita' del font) con la scala `scale`
/// (`scale` = px per unita' del font, in 16.16).
pub fn rasterize(outline: &Outline, scale: i32) -> Bitmap {
    // 1. Appiattisci in polilinee 16.16.
    let mut polys: Vec<Vec<(i32, i32)>> = Vec::with_capacity(outline.contours.len());
    for contour in outline.contours.iter() {
        if let Some(poly) = flatten(contour, scale) {
            if poly.len() >= 2 {
                polys.push(poly);
            }
        }
    }
    if polys.is_empty() {
        return Bitmap::default();
    }

    // 2. Bounding box (in pixel interi).
    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    for poly in polys.iter() {
        for &(x, y) in poly.iter() {
            if x < min_x {
                min_x = x;
            }
            if x > max_x {
                max_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if y > max_y {
                max_y = y;
            }
        }
    }
    let x0 = min_x.div_euclid(SUB);
    let y0 = min_y.div_euclid(SUB);
    // `ceil`: un contorno che finisce esattamente sul confine di un pixel non
    // deve aggiungere una riga/colonna tutta trasparente.
    let x1 = (max_x + SUB - 1).div_euclid(SUB);
    let y1 = (max_y + SUB - 1).div_euclid(SUB);
    let w = (x1 - x0).max(0) as usize;
    let h = (y1 - y0).max(0) as usize;
    if w == 0 || h == 0 {
        return Bitmap::default();
    }

    // 3. Copertura: 4 sub-scanline per riga, winding non-zero.
    let mut cov = alloc::vec![0u32; w * h];
    for row in 0..h {
        let py = (y0 + row as i32) * SUB;
        for k in 0..SUBSAMPLES {
            let sy = py + k * (SUB / SUBSAMPLES) + (SUB / SUBSAMPLES) / 2;
            let mut xs: Vec<(i32, i32)> = Vec::new();
            for poly in polys.iter() {
                for i in 0..poly.len() {
                    let a = poly[i];
                    let b = poly[(i + 1) % poly.len()];
                    if a.1 == b.1 {
                        continue;
                    }
                    let (dir, x) = if a.1 <= sy && sy < b.1 {
                        (1i32, edge_x(a, b, sy))
                    } else if b.1 <= sy && sy < a.1 {
                        (-1i32, edge_x(b, a, sy))
                    } else {
                        continue;
                    };
                    xs.push((x, dir));
                }
            }
            if xs.is_empty() {
                continue;
            }
            xs.sort_unstable_by_key(|&(x, _)| x);
            let mut winding = 0i32;
            let mut span_start = 0i32;
            for &(x, dir) in xs.iter() {
                if winding == 0 {
                    span_start = x;
                }
                winding += dir;
                if winding == 0 {
                    add_span(&mut cov, w, row, y0, x0, span_start, x);
                }
            }
        }
    }

    // 4. Da 0..(SUB*4) a 0..255.
    let mut bits = alloc::vec![0u8; w * h];
    for i in 0..bits.len() {
        let v = (cov[i] as u64 * 255) / (SUB as u64 * SUBSAMPLES as u64);
        bits[i] = if v > 255 { 255 } else { v as u8 };
    }

    Bitmap {
        w,
        h,
        bits,
        xoff: x0,
        yoff: y0,
    }
}

/// Intersezione del segmento con la scanline `y` (16.16).
fn edge_x(a: (i32, i32), b: (i32, i32), y: i32) -> i32 {
    let dy = b.1 - a.1;
    if dy == 0 {
        return a.0;
    }
    let num = (y - a.1) as i64 * (b.0 - a.0) as i64;
    (a.0 as i64 + num / dy as i64) as i32
}

/// Accumula la copertura dello span `[xa, xb)` (16.16) sulla riga `row`.
fn add_span(
    cov: &mut [u32],
    w: usize,
    row: usize,
    y0: i32,
    x0: i32,
    xa: i32,
    xb: i32,
) {
    if xb <= xa {
        return;
    }
    let _ = y0;
    let pa = xa.div_euclid(SUB);
    let pb = xb.div_euclid(SUB);
    // `..=pb`: lo span puo' **finire dentro** il pixel `pb` senza raggiungerne
    // il bordo destro (asta sottile, punta di una diagonale) — con `..pb` la
    // sua copertura veniva persa. Il guard `b <= a` sotto scarta il caso in cui
    // `xa`/`xb` cadono sul confine (nessun doppio conteggio).
    for px in pa..=pb {
        let left = px * SUB;
        let right = left + SUB;
        let a = if xa > left { xa } else { left };
        let b = if xb < right { xb } else { right };
        if b <= a {
            continue;
        }
        let idx = px - x0;
        if idx < 0 || idx as usize >= w {
            continue;
        }
        cov[row * w + idx as usize] += (b - a) as u32;
    }
}

/// Appiattisce un contorno TrueType in una polilinea 16.16.
fn flatten(contour: &[crate::ttf::Point], scale: i32) -> Option<Vec<(i32, i32)>> {
    if contour.is_empty() {
        return None;
    }
    // Punto di partenza.
    let last = contour.len() - 1;
    let start = if contour[0].on {
        (contour[0].x, contour[0].y)
    } else if contour[last].on {
        (contour[last].x, contour[last].y)
    } else {
        (
            (contour[0].x + contour[last].x) / 2,
            (contour[0].y + contour[last].y) / 2,
        )
    };

    let mut out: Vec<(i32, i32)> = Vec::with_capacity(contour.len() * CURVE_SEGMENTS);
    out.push(scale_pt(start, scale));

    let mut cur = start;
    let mut ctrl: Option<(i32, i32)> = None;
    for p in contour.iter() {
        let pt = (p.x, p.y);
        if p.on {
            match ctrl.take() {
                Some(c) => {
                    quad(&mut out, cur, c, pt, scale);
                }
                None => {
                    out.push(scale_pt(pt, scale));
                }
            }
            cur = pt;
        } else {
            match ctrl.take() {
                Some(c) => {
                    // due punti di controllo di fila: il punto medio e' sulla curva
                    let mid = ((c.0 + pt.0) / 2, (c.1 + pt.1) / 2);
                    quad(&mut out, cur, c, mid, scale);
                    cur = mid;
                    ctrl = Some(pt);
                }
                None => ctrl = Some(pt),
            }
        }
    }
    // Chiude il contorno sull'ultimo punto di controllo.
    if let Some(c) = ctrl {
        quad(&mut out, cur, c, start, scale);
    }
    Some(out)
}

/// Scala un punto dal font (unita') ai pixel 16.16. `scale` = px per unita'
/// in 16.16.
///
/// Il TrueType ha **y verso l'alto**: qui si ribalta (y pixel verso il
/// basso), cosi' la riga 0 della bitmap e' la piu' in alto.
fn scale_pt(p: (i32, i32), scale: i32) -> (i32, i32) {
    (
        (p.0 as i64 * scale as i64) as i32,
        (-(p.1 as i64 * scale as i64)) as i32,
    )
}

/// Spezza una quadratica di Bezier in segmenti (aritmetica intera: nessun
/// float, il denominatore `N*N` e' potenza di due → divisione esatta).
fn quad(out: &mut Vec<(i32, i32)>, p0: (i32, i32), c: (i32, i32), p1: (i32, i32), scale: i32) {
    let s0 = scale_pt(p0, scale);
    let s1 = scale_pt(c, scale);
    let s2 = scale_pt(p1, scale);
    const N: i64 = CURVE_SEGMENTS as i64;
    for i in 1..=N {
        let a = N - i;
        let x = (a * a * s0.0 as i64 + 2 * a * i * s1.0 as i64 + i * i * s2.0 as i64) / (N * N);
        let y = (a * a * s0.1 as i64 + 2 * a * i * s1.1 as i64 + i * i * s2.1 as i64) / (N * N);
        out.push((x as i32, y as i32));
    }
}

#[cfg(test)]
mod tests {
    //! Regressione della **copertura** (host, `cargo test`): il bug storico
    //! era invisibile — nessun test guardava i pixel — e si vedeva solo come
    //! testo assottigliato e irregolare sullo schermo.

    use super::*;
    use crate::ttf::{Outline, Point};

    /// Rettangolo (in unita' del font) con la scala data: 1 unita' = `scale`
    /// pixel. Con `scale = 16384` un'unita' e' un quarto di pixel.
    fn rect(x0: i32, x1: i32, y0: i32, y1: i32) -> Outline {
        let p = |x, y| Point { x, y, on: true };
        Outline {
            contours: alloc::vec![alloc::vec![p(x0, y0), p(x1, y0), p(x1, y1), p(x0, y1)]],
        }
    }

    /// Un'asta di 1 px a cavallo del confine (0.25..1.25) deve coprire
    /// **entrambi** i pixel: 0.75 e 0.25. Con `pa..pb` il secondo restava 0
    /// (bordo destro senza antialiasing, ~26% d'inchiostro perso).
    #[test]
    fn asta_a_cavallo_del_confine() {
        let b = rasterize(&rect(1, 5, 0, 8), 16384);
        assert_eq!((b.w, b.h), (2, 2), "bbox: niente riga/colonna vuota");
        assert_eq!(b.bits, [191u8, 63, 191, 63]);
    }

    /// Un rettangolo allineato ai pixel e' pieno: nessuna perdita e nessuna
    /// sbavatura.
    #[test]
    fn rettangolo_allineato() {
        let b = rasterize(&rect(0, 2, 0, 2), SUB);
        assert_eq!((b.w, b.h), (2, 2));
        assert_eq!(b.bits, [255u8; 4]);
    }

    /// La copertura totale e' l'**area** della forma (1.75 x 2 px = 3.5 px):
    /// e' l'invariante che il bug violava (perdeva il resto frazionario
    /// dell'ultimo pixel, qui 0.25 x 2 px = 14%).
    #[test]
    fn inchiostro_uguale_all_area() {
        let b = rasterize(&rect(2, 9, 0, 8), 16384); // 0.5..2.25 px, 2 px alto
        let ink: u32 = b.bits.iter().map(|&v| v as u32).sum();
        let atteso = 7 * 255 / 2;
        let scarto = ink.max(atteso) - ink.min(atteso);
        assert!(scarto <= atteso / 100, "inchiostro {ink} vs area {atteso}");
    }

    /// Determinismo: stesso contorno, stesso byte a ogni rasterizzazione (il
    /// CRC del rendering e' un marker stabile fra i boot).
    #[test]
    fn deterministico() {
        let a = rasterize(&rect(1, 7, 0, 9), 12345);
        let b = rasterize(&rect(1, 7, 0, 9), 12345);
        assert_eq!((a.w, a.h, a.xoff, a.yoff), (b.w, b.h, b.xoff, b.yoff));
        assert_eq!(a.bits, b.bits);
    }
}
