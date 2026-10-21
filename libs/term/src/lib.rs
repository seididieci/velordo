//! term — il lato "terminale" del display: modello a celle, UTF-8, font
//! rasterizzato e le surface che mettono i glifi sullo schermo (D3,
//! ADR-0060).
//!
//! Due moduli con confini netti:
//! - `disp`: il **protocollo display** (`/dev/display`) — lo parla il driver
//!   `gpu` e lo usa la console. Nessun rendering qui dentro.
//! - `term`: griglia, UTF-8, font TrueType e surface (pixel / VGA testuale).
//!
//! Il driver linka **solo** `term::disp`: tutto il resto (modello, rasterizer)
//! resta dead code per lui — il driver non sa cosa sia una cella.

#![no_std]

extern crate alloc;

pub mod disp;
pub mod font;
pub mod grid;
pub mod raster;
pub mod surface;
pub mod ttf;
pub mod utf8;
