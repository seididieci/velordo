//! tls-net — collante fra `tls` e il servizio `net` (N6.3, ADR-0072).
//!
//! `libs/tls` e' **puro**: conosce solo `Transport` e `TlsClock`. Questo
//! crate porta quelle due cose nel sistema reale:
//!
//! - `TcpStream`: un endpoint TCP del servizio `net` (`NET_EP_OPEN` con
//!   `NET_EP_FLAG_CONNECT`, ring SPSC in shm) visto come stream blocking;
//! - `SystemClock`: secondi epoch UTC dal servizio `Time`.
//!
//! Perche' un crate a parte e non una feature di `tls`: l'adattatore deve
//! restare testabile sull'host senza `civis` (allocatore + syscall), e il
//! trasporto TCP e' codice `unsafe` sui ring che va scritto **una volta sola**
//! (lo usano il resolver `dns` e il probe `tlsprobe`).

#![no_std]

extern crate alloc;

mod clock;
mod stream;

pub use clock::SystemClock;
pub use stream::TcpStream;