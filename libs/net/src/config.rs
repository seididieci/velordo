//! Configurazione del core di rete (ADR-0069 §6).
//!
//! Per N1 la config è compilata nel servizio (o passata da init); il bucket
//! `sys:net` arriverà con la policy di subtree (ADR-0070).

use crate::addr::{Ipv6Addr, MacAddr};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// MAC dell'interfaccia (dal device virtio-net).
    pub mac: MacAddr,
    /// Accetta Router Advertisement (RA guard: solo dal router autorizzato).
    pub ra_accept: bool,
    /// Router autorizzato; `None` = accetta il primo RA (TOFU, solo test).
    pub ra_router: Option<Ipv6Addr>,
    /// Risponde agli echo request (default: no; test: sì).
    pub ping: bool,
    /// Target dell'echo di prova; `None` = il router (link-local).
    pub ping_target: Option<Ipv6Addr>,
    /// Seed del PRNG per l'IID randomizzato (RFC 4941).
    pub seed: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mac: MacAddr::default(),
            ra_accept: true,
            ra_router: None,
            ping: false,
            ping_target: None,
            seed: 0x9e37_79b9_7f4a_7c15,
        }
    }
}
