//! Il fondo del DE: un'immagine BGRA grande quanto lo schermo.

use alloc::vec::Vec;

use crate::geom::Rect;
use crate::surface::Surface;

/// Il wallpaper: fondo di pagina + immagine, gia' composto in un buffer
/// grande quanto lo schermo.
///
/// E' la **sorgente pristina** del fondo: chi disegna (una tile) ne legge la
/// fetta che cade nel proprio rect, invece di tenere una copia privata del
/// fondo. Una copia sola per tutto il DE.
pub struct Wallpaper {
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

impl Wallpaper {
    /// `data` deve essere BGRA di `width × height` (stride `width * 4`).
    pub fn new(data: Vec<u8>, width: u32, height: u32) -> Self {
        debug_assert!(data.len() >= width as usize * height as usize * 4);
        Self { data, width, height, stride: width * 4 }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// La stessa immagine come superficie. La usano solo letture: nessun
    /// chiamante deve scriverci (il fondo e' del compositor).
    pub fn surface(&self) -> Surface {
        Surface::new(self.data.as_ptr() as *mut u8, self.width, self.height, self.stride)
    }

    /// La fetta di `rect`: la slice inizia all'angolo del rettangolo, quindi
    /// le coordinate sono **locali alla tile**; lo stride resta quello pieno
    /// del fondo. `None` se il rect e' fuori dall'immagine.
    ///
    /// E' il modo in cui una tile compone il proprio fondo: la stessa logica
    /// di `Pixel::copy_from`, ma con una sorgente che parte dal suo angolo.
    pub fn view(&self, rect: Rect) -> Option<View<'_>> {
        let r = rect.intersect(self.rect());
        if r.is_empty() {
            return None;
        }
        let off = r.y as usize * self.stride as usize + r.x as usize * 4;
        Some(View {
            slice: &self.data[off..],
            stride: self.stride,
            width: r.w,
            height: r.h,
        })
    }

    /// `true` se tutte le righe sono byte-identiche alla prima: e' la
    /// condizione in cui lo scroll puo' spostare i pixel (T4, ADR-0066).
    pub fn rows_equal(&self) -> bool {
        let stride = self.stride as usize;
        if stride == 0 || self.data.len() < stride {
            return true;
        }
        let first = &self.data[..stride];
        let mut off = stride;
        while off + stride <= self.data.len() {
            if &self.data[off..off + stride] != first {
                return false;
            }
            off += stride;
        }
        true
    }
}

/// La fetta di wallpaper di un rect, in coordinate locali.
pub struct View<'a> {
    pub slice: &'a [u8],
    pub stride: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const W: u32 = 4;
    const STRIDE: u32 = W * 4;

    fn px(buf: &[u8], stride: u32, x: u32, y: u32) -> u32 {
        let o = (y * stride + x * 4) as usize;
        u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
    }

    fn grid() -> Wallpaper {
        let mut v = vec![0u8; (STRIDE * 4) as usize];
        for y in 0..4u32 {
            for x in 0..4u32 {
                let p = (y * 4 + x + 1).to_le_bytes();
                let o = (y * STRIDE + x * 4) as usize;
                v[o..o + 4].copy_from_slice(&p);
            }
        }
        Wallpaper::new(v, W, 4)
    }

    #[test]
    fn view_parte_dall_angolo() {
        let wp = grid();
        let v = wp.view(Rect::new(1, 1, 2, 2)).expect("dentro l'immagine");
        assert_eq!((v.width, v.height), (2, 2));
        assert_eq!(v.stride, STRIDE, "lo stride resta quello del fondo");
        assert_eq!(px(v.slice, v.stride, 0, 0), px(wp.data(), STRIDE, 1, 1));
        assert_eq!(px(v.slice, v.stride, 1, 1), px(wp.data(), STRIDE, 2, 2));
    }

    #[test]
    fn view_clippa_e_fuori() {
        let wp = grid();
        // 3x3 da (2,2): solo 2x2 esiste
        let v = wp.view(Rect::new(2, 2, 3, 3)).expect("clippata");
        assert_eq!((v.width, v.height), (2, 2));
        assert!(wp.view(Rect::new(10, 10, 2, 2)).is_none());
        assert!(wp.view(Rect::EMPTY).is_none());
    }

    #[test]
    fn rows_equal() {
        let uniform = Wallpaper::new(vec![7u8; (STRIDE * 4) as usize], W, 4);
        assert!(uniform.rows_equal());
        assert!(!grid().rows_equal());
        // una riga diversa in fondo basta a negarlo
        let mut v = vec![7u8; (STRIDE * 4) as usize];
        v[(STRIDE * 3) as usize] = 8;
        assert!(!Wallpaper::new(v, W, 4).rows_equal());
    }
}
