//! impluvium-tree — il modello del DE: albero di split, layout, focus (D8e).
//!
//! Puro e `no_std`: nessuna IPC, nessun pixel, nessuna sessione. Il DE
//! (`userland/impluvium`) gli chiede **dove** vanno le tile, il server le
//! disegna. Cosi' la geometria si prova sull'host, in millisecondi, invece che
//! guardando uno schermo.
//!
//! - `Node`: `Leaf(sid)` oppure `Split { dir, ratio, a, b }`.
//! - `Tree`: radice + foglia in focus; `layout()` da' `(sid, Rect)` per ogni
//!   foglia visibile, `split()` divide la foglia in focus, `focus_dir()` si
//!   sposta geometricamente (`hjkl`), `zoom()` guarda una foglia sola.
//!
//! **Sezione aurea** (`GOLDEN = 618` per-mille): la foglia che si splitta tiene
//! il 61,8%, la nuova il 38,2% — lo schermo si riempie a spirale (il `dwindle`
//! di Hyprland). I rapporti si applicano **al momento del layout**, quindi
//! sopravvivono a resize e zoom: nessuna geometria memorizzata in pixel.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use musivum::Rect;

/// Rapporto della sezione aurea in per-mille: la parte grande e' 61,8%.
pub const GOLDEN: u32 = 618;

/// Lato minimo di una tile in pixel: il 38,2% di un asse piccolo non deve
/// poter dare zero (una tile invisibile e non ripristinabile).
pub const MIN_TILE: u32 = 8;

/// Direzione del **movimento** del focus (`hjkl`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
}

/// Direzione di uno **split**.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Due tile **affiancate**: l'asse diviso e' quello orizzontale.
    Col,
    /// Due tile **impilate**: l'asse diviso e' quello verticale.
    Row,
}

/// Un nodo dell'albero: una foglia (una sessione) o uno split.
#[derive(Clone, Debug)]
pub enum Node {
    Leaf(usize),
    Split {
        dir: Dir,
        /// Quota del figlio `a` in per-mille (618 = sezione aurea).
        ratio: u32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// L'albero delle tile di un DE.
#[derive(Clone, Debug)]
pub struct Tree {
    root: Node,
    /// La sessione in focus (riceve i tasti).
    focus: usize,
    /// `Some(sid)` = zoom: si disegna solo quella foglia, a schermo pieno.
    zoom: Option<usize>,
}

impl Tree {
    /// Un albero con una foglia sola.
    pub fn new(sid: usize) -> Tree {
        Tree {
            root: Node::Leaf(sid),
            focus: sid,
            zoom: None,
        }
    }

    /// La sessione in focus.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Porta il focus su `sid` (se esiste). Ritorna `true` se e' cambiato.
    pub fn set_focus(&mut self, sid: usize) -> bool {
        if sid == self.focus || !self.contains(sid) {
            return false;
        }
        self.focus = sid;
        true
    }

    /// `true` se `sid` e' una foglia dell'albero.
    pub fn contains(&self, sid: usize) -> bool {
        fn walk(n: &Node, sid: usize) -> bool {
            match n {
                Node::Leaf(s) => *s == sid,
                Node::Split { a, b, .. } => walk(a, sid) || walk(b, sid),
            }
        }
        walk(&self.root, sid)
    }

    /// Le sessioni dell'albero, in ordine di visita (sinistra→destra,
    /// sopra→sotto).
    pub fn leaves(&self) -> Vec<usize> {
        fn walk(n: &Node, out: &mut Vec<usize>) {
            match n {
                Node::Leaf(s) => out.push(*s),
                Node::Split { a, b, .. } => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// Quante foglie.
    pub fn len(&self) -> usize {
        self.leaves().len()
    }

    /// `true` se l'albero non ha foglie (non accade: nasce con una).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Il **layout**: `(sid, rect)` per ogni foglia, dentro `area`.
    ///
    /// Con lo zoom attivo si ritorna la sola foglia zoomata, a schermo pieno.
    /// I rect sono contigui e sommano esattamente `area` (nessuna striscia di
    /// fondo fra le tile): la divisione resta al resto di `a`, `b` prende il
    /// resto.
    pub fn layout(&self, area: Rect) -> Vec<(usize, Rect)> {
        if let Some(sid) = self.zoom {
            if self.contains(sid) {
                return alloc::vec![(sid, area)];
            }
        }
        let mut out = Vec::new();
        layout_node(&self.root, area, &mut out);
        out
    }

    /// Divide la foglia in focus: `new` e' la sessione della tile nuova.
    ///
    /// `dir` = `Col` (affiancate) o `Row` (impilate). La foglia che si splitta
    /// **tiene la parte grande** (61,8%), la nuova prende il resto. Ritorna
    /// `false` se il focus non e' una foglia (impossibile) o se la tile
    /// risultante sarebbe sotto [`MIN_TILE`].
    pub fn split(&mut self, new: usize, dir: Dir, area: Rect) -> bool {
        if !self.contains(self.focus) {
            return false;
        }
        // Lo spazio disponibile per la foglia divisa: chiediamolo al layout
        // attuale (cosi' il minimo si misura sul rect vero, non sullo schermo).
        let before = self.layout(area);
        let focused = self.focus;
        let Some(rect) = before.iter().find(|(s, _)| *s == focused).map(|(_, r)| *r) else {
            return false;
        };
        let (a, b) = split_rect(rect, dir, GOLDEN);
        if a.w < MIN_TILE || a.h < MIN_TILE || b.w < MIN_TILE || b.h < MIN_TILE {
            return false;
        }
        if !split_node(&mut self.root, focused, new, dir, GOLDEN) {
            return false;
        }
        self.focus = new;
        self.zoom = None;
        true
    }

    /// Toglie una foglia (la tile si chiude). Se era l'ultima, resta una
    /// foglia sola (mai un DE senza tile). Il focus passa al fratello.
    pub fn remove(&mut self, sid: usize) -> bool {
        if !self.contains(sid) {
            return false;
        }
        if self.len() == 1 {
            return false; // l'ultima tile non si toglie: il DE resta vivo
        }
        let mut focus = self.focus;
        remove_node(&mut self.root, sid, &mut focus);
        if focus == sid || !self.contains(focus) {
            // Il focus va alla prima foglia rimasta.
            focus = self.leaves().first().copied().unwrap_or(0);
        }
        self.focus = focus;
        if self.zoom == Some(sid) {
            self.zoom = None;
        }
        true
    }

    /// Zoom sulla foglia in focus (`Some` = solo quella, a schermo pieno).
    /// Passare alla foglia gia' zoomata toglie lo zoom.
    pub fn toggle_zoom(&mut self) -> bool {
        if self.zoom.is_some() {
            self.zoom = None;
        } else {
            self.zoom = Some(self.focus);
        }
        true
    }

    /// `true` se lo zoom e' attivo.
    pub fn zoomed(&self) -> bool {
        self.zoom.is_some()
    }

    /// Sposta il focus **geometricamente** (`h` sinistra, `j` sotto, `k` sopra,
    /// `l` destra): fra le foglie visibili si prende quella con il centro
    /// "migliore" in quella direzione (differenza minima sull'asse di
    /// movimento, poi distanza minima sull'altro asse). Funziona su qualunque
    /// forma dell'albero, anche annidata male.
    ///
    /// Ritorna la nuova sessione in focus, o `None` se non c'e' nulla da quel
    /// lato.
    pub fn focus_dir(&mut self, mv: Move, area: Rect) -> Option<usize> {
        let visible: Vec<(usize, Rect)> = self.layout(area);
        if visible.len() < 2 {
            return None;
        }
        let (cur_sid, cur) = visible.iter().find(|(s, _)| *s == self.focus).copied()?;
        let (ccx, ccy) = (cur.x as i64 * 2 + cur.w as i64, cur.y as i64 * 2 + cur.h as i64);
        let mut best: Option<(usize, i64, i64)> = None;
        for (sid, r) in visible.iter().copied() {
            if sid == cur_sid {
                continue;
            }
            let (cx, cy) = (r.x as i64 * 2 + r.w as i64, r.y as i64 * 2 + r.h as i64);
            let (along, across) = match mv {
                Move::Right => {
                    if cx <= ccx {
                        continue;
                    }
                    (cx - ccx, (cy - ccy).abs())
                }
                Move::Left => {
                    if cx >= ccx {
                        continue;
                    }
                    (ccx - cx, (cy - ccy).abs())
                }
                Move::Down => {
                    if cy <= ccy {
                        continue;
                    }
                    (cy - ccy, (cx - ccx).abs())
                }
                Move::Up => {
                    if cy >= ccy {
                        continue;
                    }
                    (ccy - cy, (cx - ccx).abs())
                }
            };
            let better = match best {
                None => true,
                Some((_, ba, bb)) => along < ba || (along == ba && across < bb),
            };
            if better {
                best = Some((sid, along, across));
            }
        }
        let sid = best?.0;
        self.focus = sid;
        Some(sid)
    }
}

/// Divide un rect in due lungo `dir`, con la parte `a` a `ratio` per-mille.
/// `b` prende il resto: la somma e' esattamente l'asse (mai una striscia di
/// fondo fra le tile).
pub fn split_rect(r: Rect, dir: Dir, ratio: u32) -> (Rect, Rect) {
    match dir {
        Dir::Col => {
            let a = (r.w as u64 * ratio as u64 / 1000) as u32;
            let a = a.max(MIN_TILE).min(r.w.saturating_sub(MIN_TILE).max(MIN_TILE));
            (
                Rect::new(r.x, r.y, a, r.h),
                Rect::new(r.x + a, r.y, r.w.saturating_sub(a), r.h),
            )
        }
        Dir::Row => {
            let a = (r.h as u64 * ratio as u64 / 1000) as u32;
            let a = a.max(MIN_TILE).min(r.h.saturating_sub(MIN_TILE).max(MIN_TILE));
            (
                Rect::new(r.x, r.y, r.w, a),
                Rect::new(r.x, r.y + a, r.w, r.h.saturating_sub(a)),
            )
        }
    }
}

fn layout_node(n: &Node, r: Rect, out: &mut Vec<(usize, Rect)>) {
    match n {
        Node::Leaf(sid) => out.push((*sid, r)),
        Node::Split { dir, ratio, a, b } => {
            let (ra, rb) = split_rect(r, *dir, *ratio);
            layout_node(a, ra, out);
            layout_node(b, rb, out);
        }
    }
}

fn split_node(n: &mut Node, sid: usize, new: usize, dir: Dir, ratio: u32) -> bool {
    match n {
        Node::Leaf(s) if *s == sid => {
            let old = Node::Leaf(*s);
            *n = Node::Split {
                dir,
                ratio,
                a: Box::new(old),
                b: Box::new(Node::Leaf(new)),
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { a, b, .. } => split_node(a, sid, new, dir, ratio) || split_node(b, sid, new, dir, ratio),
    }
}

/// Toglie il nodo foglia `sid`, sostituendo il suo genitore con il fratello.
fn remove_node(n: &mut Node, sid: usize, focus: &mut usize) -> bool {
    let Node::Split { a, b, .. } = n else {
        return false;
    };
    // Il figlio diretto e' la foglia cercata?
    let a_hit = matches!(**a, Node::Leaf(s) if s == sid);
    let b_hit = matches!(**b, Node::Leaf(s) if s == sid);
    if a_hit || b_hit {
        let keep = if a_hit {
            core::mem::replace(b.as_mut(), Node::Leaf(0))
        } else {
            core::mem::replace(a.as_mut(), Node::Leaf(0))
        };
        // Il focus va al fratello se puntava alla foglia tolta.
        if let Node::Leaf(s) = &keep {
            if *focus == sid {
                *focus = *s;
            }
        }
        *n = keep;
        return true;
    }
    if remove_node(a, sid, focus) || remove_node(b, sid, focus) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 1000, 600)
    }

    #[test]
    fn una_foglia_e_lo_schermo() {
        let t = Tree::new(0);
        let l = t.layout(area());
        assert_eq!(l.len(), 1);
        assert_eq!(l[0], (0, area()));
        assert_eq!(t.focus(), 0);
    }

    #[test]
    fn split_a_sezione_aurea_e_contiguo() {
        let mut t = Tree::new(0);
        assert!(t.split(1, Dir::Col, area()));
        let l = t.layout(area());
        assert_eq!(l.len(), 2);
        let (s0, r0) = l[0];
        let (s1, r1) = l[1];
        assert!(r0.w > r1.w, "la foglia che si splitta tiene la parte grande");
        // 61,8% di 1000 = 618.
        assert_eq!(r0.w, 618);
        // Contigui e somma esatta: nessuna striscia di fondo.
        assert_eq!(r0.x + r0.w, r1.x);
        assert_eq!(r0.w + r1.w, area().w);
        assert_eq!((s0, s1), (0, 1));
        // Il focus va sulla tile nuova.
        assert_eq!(t.focus(), 1);
    }

    #[test]
    fn split_annidato_conserva_i_rapporti() {
        let mut t = Tree::new(0);
        t.split(1, Dir::Col, area());
        // Splitta la foglia nuova (focus, 382 px di larghezza) in verticale:
        // 61,8% di 600 = 370, il resto (230) alla nuova.
        t.set_focus(1);
        assert!(t.split(2, Dir::Row, area()));
        let l = t.layout(area());
        assert_eq!(l.len(), 3);
        let r1 = l.iter().find(|(s, _)| *s == 1).unwrap().1;
        let r2 = l.iter().find(|(s, _)| *s == 2).unwrap().1;
        assert_eq!(r1.h, 370, "61,8% di 600, troncato");
        assert_eq!(r1.y + r1.h, r2.y);
        assert_eq!(r1.h + r2.h, area().h);
    }

    #[test]
    fn focus_geometrico_hjkl() {
        let mut t = Tree::new(0);
        t.split(1, Dir::Col, area());
        // 0 a sinistra, 1 a destra (in focus).
        assert_eq!(t.focus(), 1);
        assert_eq!(t.focus_dir(Move::Left, area()), Some(0), "h da 1 va a 0");
        assert_eq!(t.focus_dir(Move::Right, area()), Some(1), "l da 0 va a 1");
        assert_eq!(t.focus_dir(Move::Right, area()), None, "a destra di 1 non c'e' nulla");
        assert_eq!(t.focus_dir(Move::Up, area()), None, "sopra non c'e' nulla");
    }

    #[test]
    fn zoom_e_ritorno() {
        let mut t = Tree::new(0);
        t.split(1, Dir::Col, area());
        t.set_focus(0);
        assert!(t.toggle_zoom());
        let l = t.layout(area());
        assert_eq!(l, alloc::vec![(0, area())], "lo zoom mostra solo la foglia");
        assert!(t.toggle_zoom());
        assert_eq!(t.layout(area()).len(), 2, "e al ritorno ci sono tutte");
    }

    #[test]
    fn rimozione_ricompatta_e_mai_vuoto() {
        let mut t = Tree::new(0);
        t.split(1, Dir::Col, area());
        assert!(t.remove(1));
        assert_eq!(t.len(), 1);
        assert_eq!(t.layout(area())[0], (0, area()), "il fratello prende tutto");
        assert!(!t.remove(0), "l'ultima tile non si toglie");
    }

    #[test]
    fn minimo_di_tile() {
        let mut t = Tree::new(0);
        // Un'area minuscola: il 38,2% sarebbe sotto il minimo -> split negato.
        let tiny = Rect::new(0, 0, 30, 12);
        t.split(1, Dir::Row, tiny);
        // (30x12: 618 per-mille di 12 = 7 < MIN_TILE -> rifiutato)
        assert_eq!(t.len(), 1, "senza spazio lo split non si fa");
    }

}
