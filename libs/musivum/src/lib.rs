//! musivum — il compositor del DE: meccanismi puri, nessuna policy.
//!
//! Un compositor e' "rettangoli + copie + clip": non sa cosa sia una tile, una
//! sessione o un'applicazione. Il modello del DE (albero di split, sessioni,
//! privilegi) vive sopra, in `impluvium`/`littera`.
//!
//! Vocabolario:
//! - [`Rect`]: rettangolo half-open in pixel.
//! - [`Damage`]: l'unione dei rettangoli toccati (un solo flush, D7).
//! - [`Surface`]: un buffer BGRA lineare (la surface di una finestra o il
//!   destino della composizione).
//! - [`Wallpaper`]: il fondo del DE, grande quanto lo schermo; chi disegna ne
//!   legge la propria fetta ([`Wallpaper::view`]).
//! - [`Window`]: una superficie con un rect, un clip e un livello.
//! - [`Compositor`]: le finestre, il fondo, il danno e la composizione.
//!
//! Niente alpha in questa fase: le finestre sono opache e il compositor fa
//! solo copie di rettangoli.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

// Allocator per i test host (non usato nel build freestanding).
#[cfg(test)]
#[global_allocator]
static A: std::alloc::System = std::alloc::System;

pub mod compositor;
pub mod geom;
pub mod surface;
pub mod wallpaper;
pub mod window;

pub use compositor::{Compositor, MAX_WINDOWS};
pub use geom::{Damage, Rect};
pub use surface::Surface;
pub use wallpaper::{View, Wallpaper};
pub use window::{Backing, Layer, Window};
