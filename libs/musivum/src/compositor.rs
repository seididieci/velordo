//! Il compositor: finestre, fondo, danno, composizione.

use alloc::vec::Vec;

use crate::geom::{Damage, Rect};
use crate::surface::Surface;
use crate::wallpaper::Wallpaper;
use crate::window::{Backing, Layer, Window};

/// Limite di finestre contemporanee.
pub const MAX_WINDOWS: usize = 16;

/// Limite dei rettangoli "scoperti" durante la composizione. Oltre, il fondo
/// non viene ridipinto (degradazione sicura: meglio un pixel vecchio che
/// coprire una finestra gia' composta).
const MAX_REMAINING: usize = 64;

/// Il compositor: le finestre, il fondo e il danno.
///
/// Non conosce tile, sessioni ne' applicazioni: riceve rettangoli con una
/// superficie e li compone nell'ordine dei livelli.
pub struct Compositor {
    screen: Rect,
    wallpaper: Option<Wallpaper>,
    damage: Damage,
    windows: Vec<Window>,
    next_id: u32,
    next_z: i32,
}

impl Compositor {
    pub fn new(screen: Rect) -> Self {
        Self {
            screen,
            wallpaper: None,
            damage: Damage::none(),
            windows: Vec::new(),
            next_id: 0,
            next_z: 0,
        }
    }

    pub fn screen(&self) -> Rect {
        self.screen
    }

    /// Il modeset (D4) cambia lo schermo: il fondo e le finestre vengono
    /// ricostruiti da chi chiama, che poi fa un flush pieno.
    pub fn set_screen(&mut self, screen: Rect) {
        self.screen = screen;
    }

    pub fn set_wallpaper(&mut self, wallpaper: Wallpaper) {
        self.wallpaper = Some(wallpaper);
    }

    pub fn wallpaper(&self) -> Option<&Wallpaper> {
        self.wallpaper.as_ref()
    }

    /// Apre una finestra. Assegna `id` (da 1) e `z` (in cima al suo livello) e
    /// danneggia la sua area. Ritorna l'id, o `0` se il limite e' raggiunto.
    pub fn open(&mut self, mut w: Window) -> u32 {
        if self.windows.len() >= MAX_WINDOWS {
            return 0;
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_z = self.next_z.saturating_add(1);
        w.id = self.next_id;
        w.z = self.next_z;
        let vis = w.visible_rect();
        self.damage.add(vis);
        self.windows.push(w);
        w.id
    }

    pub fn close(&mut self, id: u32) -> bool {
        match self.windows.iter().position(|w| w.id == id) {
            Some(i) => {
                let w = self.windows.remove(i);
                self.damage.add(w.visible_rect());
                true
            }
            None => false,
        }
    }

    pub fn get(&self, id: u32) -> Option<Window> {
        self.windows.iter().find(|w| w.id == id).copied()
    }

    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter()
    }

    /// Sposta/ridimensiona: danneggia vecchia e nuova area.
    pub fn set_rect(&mut self, id: u32, rect: Rect) -> bool {
        match self.windows.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                self.damage.add(w.visible_rect());
                w.rect = rect;
                self.damage.add(w.visible_rect());
                true
            }
            None => false,
        }
    }

    /// Cambia il limite di disegno (es. la tile di un ospite cambia rect).
    pub fn set_clip(&mut self, id: u32, clip: Rect) -> bool {
        match self.windows.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                self.damage.add(w.visible_rect());
                w.clip = clip;
                self.damage.add(w.visible_rect());
                true
            }
            None => false,
        }
    }

    pub fn set_visible(&mut self, id: u32, visible: bool) -> bool {
        match self.windows.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                if w.visible != visible {
                    w.visible = visible;
                    self.damage.add(w.visible_rect());
                }
                true
            }
            None => false,
        }
    }

    /// Porta la finestra in cima al proprio livello (mai fuori).
    pub fn raise(&mut self, id: u32) -> bool {
        self.next_z = self.next_z.saturating_add(1);
        match self.windows.iter_mut().find(|w| w.id == id) {
            Some(w) => {
                w.z = self.next_z;
                true
            }
            None => false,
        }
    }

    pub fn add_damage(&mut self, r: Rect) {
        self.damage.add(r);
    }

    pub fn damage_rect(&self) -> Option<Rect> {
        self.damage.rect()
    }

    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }

    /// `(visibili, totali)`.
    pub fn stats(&self) -> (usize, usize) {
        (self.windows.iter().filter(|w| w.visible).count(), self.windows.len())
    }

    /// Compone il danno in `dst` e ritorna il rettangolo da trasferire.
    ///
    /// Ordine: livelli dal basso in alto, poi `z` e `id`. Le finestre con
    /// `Backing::Owned` vengono copiate (clip = rect ∩ clip); quelle `Direct`
    /// hanno gia' i pixel in `dst`, quindi la loro area viene solo **sottratta**
    /// dal fondo. Il wallpaper viene dipinto dove nessuna finestra copre.
    ///
    /// `None` = niente da comporre (nessun danno).
    pub fn compose(&mut self, dst: &Surface) -> Option<Rect> {
        let damage = self.damage.take()?;

        let mut order: Vec<(Layer, i32, u32, usize)> = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.visible)
            .map(|(i, w)| (w.layer, w.z, w.id, i))
            .collect();
        order.sort_unstable();

        let mut remaining: Vec<Rect> = alloc::vec![damage];
        let mut overflow = false;

        for (_, _, _, i) in order {
            let w = self.windows[i];
            let vis = w.visible_rect().intersect(damage);
            if vis.is_empty() {
                continue;
            }
            // Le finestre si disegnano **sempre**, anche sopra un'altra
            // finestra: la sottrazione qui sotto serve solo a sapere dove
            // resta scoperto il fondo.
            if let Backing::Owned(src) = &w.backing {
                // `vis` e' in coordinate schermo: si riporta sulla surface
                // della finestra. Una surface piu' piccola del rect viene
                // clippata da `blit_from`.
                let src_rect = Rect::new(vis.x - w.rect.x, vis.y - w.rect.y, vis.w, vis.h);
                unsafe {
                    dst.blit_from(vis.x, vis.y, src, src_rect);
                }
            }
            let mut next: Vec<Rect> = Vec::with_capacity(remaining.len() + 3);
            let mut this_overflow = false;
            for r in remaining.iter().copied() {
                let hit = r.intersect(vis);
                if hit.is_empty() {
                    push_capped(&mut next, r, &mut this_overflow);
                } else {
                    for part in r.sub(hit) {
                        push_capped(&mut next, part, &mut this_overflow);
                    }
                }
            }
            if this_overflow {
                // Degradazione: si tiene l'insieme precedente (sovrainsieme)
                // e a fine composizione non si ridipinge il fondo.
                overflow = true;
            } else {
                remaining = next;
            }
        }

        if let Some(wp) = &self.wallpaper {
            if !overflow {
                let src = wp.surface();
                for r in &remaining {
                    unsafe {
                        dst.blit_from(r.x, r.y, &src, *r);
                    }
                }
            }
        }

        Some(damage)
    }
}

fn push_capped(v: &mut Vec<Rect>, r: Rect, overflow: &mut bool) {
    if r.is_empty() {
        return;
    }
    if v.len() < MAX_REMAINING {
        v.push(r);
    } else {
        *overflow = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const C_BG: u32 = 0x0011_1111;
    const C_A: u32 = 0x0022_2222;
    const C_B: u32 = 0x0033_3333;
    const C_OLD: u32 = 0x0055_5555;

    fn solid(w: u32, h: u32, color: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&color.to_le_bytes());
        }
        v
    }

    fn px(buf: &[u8], stride: u32, x: u32, y: u32) -> u32 {
        let o = (y * stride + x * 4) as usize;
        u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
    }

    fn screen(w: u32, h: u32) -> Rect {
        Rect::new(0, 0, w, h)
    }

    /// Compositor 4x4 con fondo uniforme, e un buffer di destinazione.
    fn setup() -> (Compositor, Vec<u8>) {
        let mut c = Compositor::new(screen(4, 4));
        c.set_wallpaper(Wallpaper::new(solid(4, 4, C_BG), 4, 4));
        (c, vec![0u8; 4 * 4 * 4])
    }

    fn win(rect: Rect, clip: Rect, backing: Backing, layer: Layer) -> Window {
        Window { id: 0, owner: 1, rect, clip, backing, layer, z: 0, visible: true }
    }

    #[test]
    fn open_assegna_id_e_rispetta_il_limite() {
        let (mut c, _) = setup();
        for _ in 0..MAX_WINDOWS {
            let id = c.open(win(screen(4, 4), screen(4, 4), Backing::Direct, Layer::Tiles));
            assert!(id > 0);
        }
        assert_eq!(c.open(win(screen(4, 4), screen(4, 4), Backing::Direct, Layer::Tiles)), 0);
        assert_eq!(c.stats(), (MAX_WINDOWS, MAX_WINDOWS));
    }

    #[test]
    fn compose_senza_danno_e_none() {
        let (mut c, mut dst) = setup();
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        assert!(c.compose(&s).is_none());
    }

    #[test]
    fn compose_dipinge_il_fondo_dove_scoperto() {
        let (mut c, mut dst) = setup();
        c.add_damage(screen(4, 4));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        assert_eq!(c.compose(&s), Some(screen(4, 4)));
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(px(&dst, 16, x, y), C_BG, "il fondo copre tutto");
            }
        }
    }

    #[test]
    fn compose_copia_la_finestra_sopra_il_fondo() {
        let (mut c, mut dst) = setup();
        let mut src = solid(2, 2, C_A);
        let src_s = Surface::new(src.as_mut_ptr(), 2, 2, 8);
        c.open(win(
            Rect::new(1, 1, 2, 2),
            screen(4, 4),
            Backing::Owned(src_s),
            Layer::Tiles,
        ));
        // il danno parte dalla finestra: qui interessa tutto lo schermo
        c.add_damage(screen(4, 4));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        c.compose(&s);
        assert_eq!(px(&dst, 16, 1, 1), C_A);
        assert_eq!(px(&dst, 16, 2, 2), C_A);
        assert_eq!(px(&dst, 16, 0, 0), C_BG, "fuori dalla finestra c'e' il fondo");
        assert_eq!(px(&dst, 16, 3, 3), C_BG);
    }

    #[test]
    fn compose_ordine_di_livello_e_z() {
        let (mut c, mut dst) = setup();
        let mut a = solid(2, 2, C_A);
        let mut b = solid(2, 2, C_B);
        let a_s = Surface::new(a.as_mut_ptr(), 2, 2, 8);
        let b_s = Surface::new(b.as_mut_ptr(), 2, 2, 8);
        // TileGuests sta sopra Tiles, anche se Tiles e' aperto dopo.
        c.open(win(
            Rect::new(1, 1, 2, 2),
            screen(4, 4),
            Backing::Owned(a_s),
            Layer::TileGuests,
        ));
        c.open(win(
            Rect::new(1, 1, 2, 2),
            screen(4, 4),
            Backing::Owned(b_s),
            Layer::Tiles,
        ));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        c.compose(&s);
        assert_eq!(px(&dst, 16, 1, 1), C_A, "il livello piu' alto vince");
    }

    #[test]
    fn compose_il_clip_limita_la_finestra() {
        let (mut c, mut dst) = setup();
        let mut src = solid(4, 4, C_A);
        let src_s = Surface::new(src.as_mut_ptr(), 4, 4, 16);
        // finestra 4x4 ma clip solo alla colonna 0 e 1
        c.open(win(
            screen(4, 4),
            Rect::new(0, 0, 2, 4),
            Backing::Owned(src_s),
            Layer::Tiles,
        ));
        c.add_damage(screen(4, 4));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        c.compose(&s);
        assert_eq!(px(&dst, 16, 0, 0), C_A);
        assert_eq!(px(&dst, 16, 1, 3), C_A);
        assert_eq!(px(&dst, 16, 2, 0), C_BG, "fuori dal clip c'e' il fondo");
    }

    #[test]
    fn compose_direct_preserva_i_pixel_gia_disegnati() {
        let (mut c, mut dst) = setup();
        for p in dst.chunks_exact_mut(4) {
            p.copy_from_slice(&C_OLD.to_le_bytes());
        }
        // la tile che scrive direttamente nel buffer: copre tutto
        c.open(win(screen(4, 4), screen(4, 4), Backing::Direct, Layer::Tiles));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        assert!(c.compose(&s).is_some());
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(px(&dst, 16, x, y), C_OLD, "il fondo non passa sopra al Direct");
            }
        }
    }

    #[test]
    fn compose_tocca_solo_il_danno() {
        let (mut c, mut dst) = setup();
        c.add_damage(Rect::new(0, 0, 1, 1));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        c.compose(&s);
        assert_eq!(px(&dst, 16, 0, 0), C_BG);
        assert_eq!(px(&dst, 16, 1, 0), 0, "il resto non e' stato toccato");
    }

    #[test]
    fn visibile_false_non_compone() {
        let (mut c, mut dst) = setup();
        let mut src = solid(4, 4, C_A);
        let src_s = Surface::new(src.as_mut_ptr(), 4, 4, 16);
        let id = c.open(win(screen(4, 4), screen(4, 4), Backing::Owned(src_s), Layer::Tiles));
        assert!(c.set_visible(id, false));
        c.add_damage(screen(4, 4));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        c.compose(&s);
        assert_eq!(px(&dst, 16, 0, 0), C_BG, "la finestra nascosta non si vede");
        assert_eq!(c.stats(), (0, 1));
    }

    #[test]
    fn set_rect_danneggia_vecchio_e_nuovo() {
        let (mut c, _) = setup();
        let id = c.open(win(Rect::new(0, 0, 1, 1), screen(4, 4), Backing::Direct, Layer::Tiles));
        c.clear_damage();
        assert!(c.set_rect(id, Rect::new(2, 2, 1, 1)));
        let d = c.damage_rect().expect("il movimento danneggia");
        assert_eq!(d, Rect::new(0, 0, 3, 3), "unione di vecchio e nuovo");
        assert!(c.close(id));
        assert!(c.get(id).is_none());
    }

    #[test]
    fn raise_porta_sopra_nello_stesso_livello() {
        let (mut c, mut dst) = setup();
        let mut a = solid(4, 4, C_A);
        let mut b = solid(4, 4, C_B);
        let a_s = Surface::new(a.as_mut_ptr(), 4, 4, 16);
        let b_s = Surface::new(b.as_mut_ptr(), 4, 4, 16);
        let id_a = c.open(win(screen(4, 4), screen(4, 4), Backing::Owned(a_s), Layer::Tiles));
        c.open(win(screen(4, 4), screen(4, 4), Backing::Owned(b_s), Layer::Tiles));
        assert!(c.raise(id_a));
        let s = Surface::new(dst.as_mut_ptr(), 4, 4, 16);
        c.compose(&s);
        assert_eq!(px(&dst, 16, 0, 0), C_A, "raise vince la z");
    }
}
