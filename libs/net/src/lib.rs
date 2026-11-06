//! net — core dello stack di rete nativo (ADR-0068/0069).
//!
//! **Puro**: nessun I/O, nessuna syscall, nessuna dipendenza da `civis`.
//! Il servizio `userland/net` gli passa i frame ricevuti e riceve i frame da
//! trasmettere; i test host lo esercitano in modo deterministico.
//!
//! IPv6-first: il core parla solo IPv6/ICMPv6/NDP. IPv4 arriverà con un
//! traduttore separato (SIIT/NAT64), mai qui dentro.
//!
//! Invarianti (ADR-0068): `#![forbid(unsafe_code)]`, nessun panic su input
//! arbitrario, risorse bounded, default-deny.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

// Allocator per i test host (non usato nel build freestanding).
#[cfg(test)]
#[global_allocator]
static A: std::alloc::System = std::alloc::System;

pub mod addr;
pub mod checksum;
pub mod config;
pub mod eth;
pub mod icmpv6;
pub mod ipv6;
pub mod ndp;
pub mod net;

pub use addr::{Ipv6Addr, MacAddr};
pub use config::Config;
pub use net::{Event, FrameSink, Net};
