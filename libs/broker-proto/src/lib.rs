//! Protocollo IPC del servizio `NetBroker` (ADR-0080, fase 6).
//! Condiviso da: il servizio `netbroker`, il client `civis::broker` e
//! eventuali test. Nessuna syscall nuova: solo tag IPC e costanti.

#![no_std]

use syscall_numbers as sys;

/// Versione del protocollo.
pub const PROTO_VERSION: u32 = 1;

/// Marcatore di fine trasmissione (w0 del messaggio EOF).
pub const MSG_EOF: u64 = 0xFFFF_FFFF;

/// Chunk massimo per messaggio (byte utili in w1).
pub const CHUNK: usize = 4;

// ── Tag IPC (single source: syscall-numbers, riesportati qui) ─────

pub const BROKER_OPEN: u64 = sys::BROKER_OPEN;
pub const BROKER_TX: u64 = sys::BROKER_TX;
pub const BROKER_CLOSE: u64 = sys::BROKER_CLOSE;
pub const BROKER_STATS: u64 = sys::BROKER_STATS;

// ── Porte allowlist (predefinite, il broker le applica) ───────────

/// Porta echo UDP.
pub const PORT_ECHO: u16 = 7;
/// Porta echo TCP.
pub const PORT_ECHO_TCP: u16 = 8;
/// Porta SIIT/NAT64.
pub const PORT_SIIT: u16 = 30000;

/// `true` se `port` e' nella allowlist predefinita del broker.
pub fn is_allowed_port(port: u16) -> bool {
    matches!(port, PORT_ECHO | PORT_ECHO_TCP | PORT_SIIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_ecce_un_insieme_chiuso() {
        assert!(is_allowed_port(PORT_ECHO));
        assert!(is_allowed_port(PORT_ECHO_TCP));
        assert!(is_allowed_port(PORT_SIIT));
        // Nessuna porta privilegiata o di sistema: la lista e' esplicita.
        for p in [0u16, 9, 22, 53, 80, 443, 853, 8080, 49152] {
            assert!(!is_allowed_port(p), "porta {p} non deve essere in allowlist");
        }
    }

    #[test]
    fn tag_condivisi_con_syscall_numbers() {
        assert_eq!(BROKER_OPEN, sys::BROKER_OPEN);
        assert_eq!(BROKER_TX, sys::BROKER_TX);
        assert_eq!(BROKER_CLOSE, sys::BROKER_CLOSE);
        assert_eq!(BROKER_STATS, sys::BROKER_STATS);
        // Nessun tag di richiesta puo' coincidere con una notifica senza
        // reply del kernel (`EXIT_NOTIFY`): la reply implicita non si arma.
        for t in [BROKER_OPEN, BROKER_TX, BROKER_CLOSE, BROKER_STATS] {
            assert_ne!(t, sys::EXIT_NOTIFY);
        }
    }
}
