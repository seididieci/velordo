//! Le finestre del compositor: rettangolo, clip, livello e pixel.

use crate::geom::Rect;
use crate::surface::Surface;

/// I livelli del DE, dal fondo al cursore.
///
/// L'ordine di dichiarazione **e'** la z globale (deriva `Ord`): il
/// compositor li attraversa in quest'ordine, poi per `z` e per `id`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Layer {
    /// Il fondo (wallpaper): non e' una finestra, e' del compositor.
    Background,
    /// Le sessioni di terminale: un rect per tile.
    Tiles,
    /// Ospiti GUI dentro una tile, clippati al rect della tile.
    TileGuests,
    /// Finestre screen-global (privilegiate).
    Floating,
    /// Sopra tutto: prompt di consenso, chrome del DE.
    Overlay,
    /// Il cursore del puntatore.
    Cursor,
}

/// Dove stanno i pixel di una finestra.
#[derive(Clone, Copy, Debug)]
pub enum Backing {
    /// Gia' nel buffer di destinazione: e' la tile che scrive direttamente
    /// nel device buffer (D8a). Il compositor non copia nulla, sottrae solo
    /// la sua area dal fondo.
    Direct,
    /// Una superficie propria (shm o buffer): il compositor la copia.
    Owned(Surface),
}

/// Una finestra: una superficie posizionata, con un limite di disegno.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub id: u32,
    /// Il processo che la possiede (`0` = il compositor stesso).
    pub owner: u32,
    /// Posizione sullo schermo.
    pub rect: Rect,
    /// Limite di disegno: nessun pixel fuori. Per un ospite e' il rect della
    /// sua tile (torsione 1, §10.1 del piano).
    pub clip: Rect,
    pub backing: Backing,
    pub layer: Layer,
    pub z: i32,
    pub visible: bool,
}

impl Window {
    /// L'area effettivamente visibile (rect ∩ clip), senza il danno.
    pub fn visible_rect(&self) -> Rect {
        self.rect.intersect(self.clip)
    }
}
