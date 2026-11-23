//! Geometria: rettangoli half-open e accumulatore del danno.

/// Un rettangolo in pixel, half-open: `x..x+w` × `y..y+h`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x: 0, y: 0, w: 0, h: 0 };

    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub const fn right(&self) -> u32 {
        self.x.saturating_add(self.w)
    }

    pub const fn bottom(&self) -> u32 {
        self.y.saturating_add(self.h)
    }

    /// L'intersezione; `EMPTY` se non si toccano.
    pub fn intersect(self, other: Rect) -> Rect {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x0 >= x1 || y0 >= y1 {
            Rect::EMPTY
        } else {
            Rect::new(x0, y0, x1 - x0, y1 - y0)
        }
    }

    /// Il minimo rettangolo che contiene entrambi (bounding box).
    pub fn union(self, other: Rect) -> Rect {
        if other.is_empty() {
            return self;
        }
        if self.is_empty() {
            return other;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// `self` meno `cut`: fino a 4 rettangoli (sopra, sotto, sinistra,
    /// destra). Gli slot non usati sono vuoti; se `cut` non tocca `self`
    /// il risultato e' `self` stesso.
    pub fn sub(self, cut: Rect) -> [Rect; 4] {
        let i = self.intersect(cut);
        if i.is_empty() {
            return [self, Rect::EMPTY, Rect::EMPTY, Rect::EMPTY];
        }
        [
            Rect::new(self.x, self.y, self.w, i.y - self.y),
            Rect::new(self.x, i.bottom(), self.w, self.bottom() - i.bottom()),
            Rect::new(self.x, i.y, i.x - self.x, i.h),
            Rect::new(i.right(), i.y, self.right() - i.right(), i.h),
        ]
    }
}

/// L'unione (bounding box) dei rettangoli toccati: un solo rettangolo da
/// trasferire al driver (D7, ADR-0066).
#[derive(Clone, Copy, Debug, Default)]
pub struct Damage {
    rect: Rect,
}

impl Damage {
    pub const fn none() -> Self {
        Self { rect: Rect::EMPTY }
    }

    pub fn add(&mut self, r: Rect) {
        if !r.is_empty() {
            self.rect = self.rect.union(r);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rect.is_empty()
    }

    pub fn rect(&self) -> Option<Rect> {
        if self.rect.is_empty() {
            None
        } else {
            Some(self.rect)
        }
    }

    pub fn clear(&mut self) {
        self.rect = Rect::EMPTY;
    }

    /// Prende il danno accumulato e azzera.
    pub fn take(&mut self) -> Option<Rect> {
        let r = self.rect();
        self.rect = Rect::EMPTY;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_e_union() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(b), Rect::new(5, 5, 5, 5));
        assert_eq!(a.union(b), Rect::new(0, 0, 15, 15));
        assert!(a.intersect(Rect::new(20, 20, 1, 1)).is_empty());
        assert_eq!(a.union(Rect::EMPTY), a);
        assert_eq!(Rect::EMPTY.union(a), a);
    }

    #[test]
    fn sub_lascia_quattro_pezzi() {
        let a = Rect::new(0, 0, 10, 10);
        let cut = Rect::new(3, 4, 2, 2);
        let parts = a.sub(cut);
        let mut area = cut.w * cut.h;
        for p in parts {
            area += p.w * p.h;
        }
        assert_eq!(area, a.w * a.h, "i pezzi devono coprire l'area senza buchi ne' sovrapposizioni");
        // nessun pezzo tocca il taglio
        for p in parts {
            assert!(p.intersect(cut).is_empty());
        }
        // e ognuno sta dentro l'originale
        for p in parts {
            assert_eq!(p.intersect(a), p);
        }
    }

    #[test]
    fn sub_intero_e_fuori() {
        let a = Rect::new(0, 0, 4, 4);
        // copertura totale: niente resta
        assert!(a.sub(a).iter().all(|p| p.is_empty()));
        // nessuna intersezione: resta tutto
        let parts = a.sub(Rect::new(10, 10, 2, 2));
        assert_eq!(parts[0], a);
        assert!(parts[1..].iter().all(|p| p.is_empty()));
    }

    #[test]
    fn damage_unisce_e_svuota() {
        let mut d = Damage::none();
        assert!(d.take().is_none());
        d.add(Rect::new(10, 10, 4, 4));
        d.add(Rect::new(0, 2, 2, 2));
        assert_eq!(d.take(), Some(Rect::new(0, 2, 14, 12)));
        assert!(d.is_empty(), "take azzera");
        d.add(Rect::EMPTY);
        assert!(d.is_empty(), "un rect vuoto non danneggia");
    }
}
