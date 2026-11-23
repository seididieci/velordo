//! Superfici BGRA e copia di rettangoli con clip.

use crate::geom::Rect;

/// Un framebuffer lineare BGRA: base, dimensioni e stride in byte.
///
/// `ptr` e' grezzo: la superficie puo' essere il buffer device (DMA) o una
/// shm mappata. Chi la usa e' `unsafe` e deve garantire che il buffer resti
/// vivo per almeno `stride * height` byte.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub ptr: *mut u8,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Surface {
    pub const fn new(ptr: *mut u8, width: u32, height: u32, stride: u32) -> Self {
        Self { ptr, width, height, stride }
    }

    pub const fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Copia dentro `self` il rettangolo `src_rect` di `src`, con l'angolo in
    /// `(dst_x, dst_y)`. Clip su entrambe le superfici: niente panico, niente
    /// scrittura fuori.
    ///
    /// # Safety
    /// `self.ptr` e `src.ptr` devono puntare a buffer vivi di almeno
    /// `stride * height` byte.
    pub unsafe fn blit_from(&self, dst_x: u32, dst_y: u32, src: &Surface, src_rect: Rect) {
        // 1. il sorgente clipato a se' stesso
        let s = src_rect.intersect(src.rect());
        if s.is_empty() {
            return;
        }
        // 2. le coordinate di destinazione dell'angolo del sorgente clipato
        let dx0 = dst_x.saturating_add(s.x - src_rect.x);
        let dy0 = dst_y.saturating_add(s.y - src_rect.y);
        // 3. clip alla destinazione
        let d = Rect::new(dx0, dy0, s.w, s.h).intersect(self.rect());
        if d.is_empty() {
            return;
        }
        let sx = s.x + (d.x - dx0);
        let sy = s.y + (d.y - dy0);
        let bytes = d.w as usize * 4;
        for rr in 0..d.h {
            let doff = (d.y + rr) as usize * self.stride as usize + d.x as usize * 4;
            let soff = (sy + rr) as usize * src.stride as usize + sx as usize * 4;
            unsafe {
                core::ptr::copy_nonoverlapping(src.ptr.add(soff), self.ptr.add(doff), bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const STRIDE: u32 = 4 * 4; // 4 pixel, BGRA

    fn px(buf: &[u8], stride: u32, x: u32, y: u32) -> u32 {
        let o = (y * stride + x * 4) as usize;
        u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
    }

    /// Buffer 4x4 con pixel `0x1000 * (y*4+x)`: ogni pixel e' distinguibile.
    fn grid4() -> vec::Vec<u8> {
        let mut v = vec![0u8; (STRIDE * 4) as usize];
        for y in 0..4u32 {
            for x in 0..4u32 {
                let p = (y * 4 + x + 1).to_le_bytes();
                let o = (y * STRIDE + x * 4) as usize;
                v[o..o + 4].copy_from_slice(&p);
            }
        }
        v
    }

    #[test]
    fn blit_con_offset() {
        let mut src = grid4();
        let mut dst = vec![0u8; (STRIDE * 4) as usize];
        let s = Surface::new(src.as_mut_ptr(), 4, 4, STRIDE);
        let d = Surface::new(dst.as_mut_ptr(), 4, 4, STRIDE);
        unsafe { d.blit_from(1, 1, &s, Rect::new(2, 1, 2, 2)) };
        assert_eq!(px(&dst, STRIDE, 1, 1), px(&src, STRIDE, 2, 1));
        assert_eq!(px(&dst, STRIDE, 2, 2), px(&src, STRIDE, 3, 2));
        assert_eq!(px(&dst, STRIDE, 0, 1), 0, "fuori dal blit resta intatto");
        assert_eq!(px(&dst, STRIDE, 3, 3), 0, "fuori dal blit resta intatto");
    }

    #[test]
    fn blit_clippa_alla_destinazione() {
        let mut src = grid4();
        let mut dst = vec![0u8; (STRIDE * 4) as usize];
        let s = Surface::new(src.as_mut_ptr(), 4, 4, STRIDE);
        let d = Surface::new(dst.as_mut_ptr(), 4, 4, STRIDE);
        // 2x2 a partire da (3,3): solo 1x1 entra
        unsafe { d.blit_from(3, 3, &s, Rect::new(0, 0, 2, 2)) };
        assert_eq!(px(&dst, STRIDE, 3, 3), px(&src, STRIDE, 0, 0));
    }

    #[test]
    fn blit_fuori_e_noop() {
        let mut src = grid4();
        let mut dst = vec![0u8; (STRIDE * 4) as usize];
        let s = Surface::new(src.as_mut_ptr(), 4, 4, STRIDE);
        let d = Surface::new(dst.as_mut_ptr(), 4, 4, STRIDE);
        unsafe { d.blit_from(10, 10, &s, Rect::new(0, 0, 2, 2)) };
        unsafe { d.blit_from(0, 0, &s, Rect::EMPTY) };
        assert!(dst.iter().all(|b| *b == 0));
    }

    #[test]
    fn blit_clippa_il_sorgente() {
        let mut src = grid4();
        let mut dst = vec![0u8; (STRIDE * 4) as usize];
        let s = Surface::new(src.as_mut_ptr(), 4, 4, STRIDE);
        let d = Surface::new(dst.as_mut_ptr(), 4, 4, STRIDE);
        // sorgente 3x3 da (2,2): solo 2x2 esiste
        unsafe { d.blit_from(0, 0, &s, Rect::new(2, 2, 3, 3)) };
        assert_eq!(px(&dst, STRIDE, 0, 0), px(&src, STRIDE, 2, 2));
        assert_eq!(px(&dst, STRIDE, 1, 1), px(&src, STRIDE, 3, 3));
        assert_eq!(px(&dst, STRIDE, 2, 0), 0, "la parte fuori dal sorgente non c'e'");
    }
}
