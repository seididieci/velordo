//! tls-net — collante fra `tls` e il servizio `net` (N6.3, ADR-0072).
//!
//! `libs/tls` e' **puro**: conosce solo `Transport` e `TlsClock`. Questo
//! crate porta quelle due cose nel sistema reale:
//!
//! - `TcpStream`: lo stream TCP di `civis::net` visto come `tls::Transport`
//!   (dal N7.0 il trasporto — ring, cursori, deadline — vive nel meccanismo,
//!   vedi ADR-0074: qui resta solo la delegazione del trait);
//! - `SystemClock`: secondi epoch UTC dal servizio `Time`.
//!
//! Perche' un crate a parte e non una feature di `tls`: l'adattatore deve
//! restare testabile sull'host senza `civis` (allocatore + syscall) e senza
//! il servizio `Time`.

#![no_std]

extern crate alloc;

mod clock;
mod stream;

pub use clock::SystemClock;
pub use stream::TcpStream;
