//! `formae` — componenti di **disegno** condivisi fra la TUI e la futura GUI
//! (ADR-0097, ADR-0099). Il primo consumatore e' il monitor di sistema.
//!
//! Non e' un toolkit: niente albero di widget, niente event loop, niente
//! invalidazione. Un componente e' una funzione `draw(&mut Target, &stato)`; lo
//! stato (storia, selezione, ordinamento) lo possiede l'app, e il frame lo
//! compone l'app.
//!
//! ## Il confine
//!
//! - **Layout in celle, disegno in pixel.** Un'app di terminale ragiona in
//!   celle e converte (`CellRect::px`); un componente riceve un [`PxRect`].
//! - **Il testo lo misura il target.** Un widget chiede [`Target::measure`] e
//!   scrive a una x in pixel: nella TUI `measure` e' `caratteri × cella` e
//!   `snap_x` arrotonda alla cella; in una GUI con font proporzionale e'
//!   la misura vera, e il widget non cambia.
//! - **Il frame e' dell'app.** [`Frame`] produce le toppe (pixel) e il
//!   differenziale delle celle; l'IPC resta dell'app.
//!
//! Niente `civis`: si prova sull'host (unit test), come `pictura`/`tabula`.

#![no_std]

extern crate alloc;

pub mod bar;
pub mod chart;
pub mod fmt;
pub mod frame;
pub mod geom;
pub mod history;
pub mod target;
pub mod theme;

pub use bar::Bar;
pub use chart::Chart;
pub use frame::{Frame, Patch, PatchTarget};
pub use geom::{CellRect, Metrics, PxRect};
pub use history::History;
pub use target::Target;
pub use theme::{cell_color, Theme};

/// Lo stile del testo di una cella (rimando: `tabula`), cosi' un componente
/// non inventa un secondo tipo di colore.
pub use tabula::Style;
/// I colori delle primitive sono quelli di `pictura` (`0xAARRGGBB`).
pub use pictura::Color;

/// Tutto cio' che serve a un'app per disegnare una dashboard.
pub mod prelude {
    pub use crate::bar::Bar;
    pub use crate::chart::Chart;
    pub use crate::fmt;
    pub use crate::frame::{Frame, Patch, PatchTarget};
    pub use crate::geom::{CellRect, Metrics, PxRect};
    pub use crate::history::History;
    pub use crate::target::Target;
    pub use crate::theme::{cell_color, Theme};
    pub use crate::{Color, Style};
}
