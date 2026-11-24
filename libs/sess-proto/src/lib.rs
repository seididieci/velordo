#![cfg_attr(not(test), no_std)]

//! Layout della **mailbox di sessione** (D8c1, ADR-0089).
//!
//! Una sessione di `littera` ha una pagina condivisa (shm creata dal server,
//! mappata in RW da server e client) divisa in due meta':
//!
//! ```text
//! 0x000..0x800  OUT  client -> server  (SESS_WRITE, SESS_TEST_INPUT)
//! 0x800..0xFFF  IN   server -> client  (SESS_READ, SESS_DUMP)
//! ```
//!
//! L'accesso e' **sincrono**: il client riempie la sua meta' e resta bloccato
//! nella `send` mentre il server consuma; non serve un cursore head/tail.
//! Il formato e' condiviso da `civis::sess` (client) e `littera` (server):
//! una sola fonte, come `net-ep` per i ring di rete.

/// Dimensione della mailbox (una pagina).
pub const MAILBOX_BYTES: usize = 4096;

/// Inizio della meta' di **uscita** (client → sessione).
pub const OUT_OFF: usize = 0;

/// Inizio della meta' di **ingresso** (sessione → client).
pub const IN_OFF: usize = 2048;

/// Dimensione di ciascuna meta': il massimo trasferibile per comando.
pub const HALF: usize = 2048;

/// Il massimo numero di byte di un singolo comando (`SESS_WRITE`,
/// `SESS_READ`, `SESS_DUMP`): la capienza di una meta'.
pub const MAX_CHUNK: usize = HALF;

/// La mailbox e' grande esattamente una pagina (il `munmap` vuole geometria
/// di pagina: si mappa/smappa `MAILBOX_BYTES`).
pub const MAILBOX_PAGES: usize = 1;

/// `true` se `len` sta in una meta'. I server lo usano per validare il
/// comando invece di fidarsi del chiamante.
pub const fn fits(len: usize) -> bool {
    len <= MAX_CHUNK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_meta_non_si_sovrappongono() {
        assert_eq!(OUT_OFF, 0);
        assert_eq!(OUT_OFF + HALF, IN_OFF);
        assert_eq!(IN_OFF + HALF, MAILBOX_BYTES);
        assert_eq!(HALF * 2, MAILBOX_BYTES);
    }

    #[test]
    fn chunk_entro_le_meta() {
        assert!(fits(0));
        assert!(fits(MAX_CHUNK));
        assert!(!fits(MAX_CHUNK + 1));
        assert!(!fits(MAILBOX_BYTES));
    }

    #[test]
    fn pagina_intera() {
        assert_eq!(MAILBOX_PAGES * 4096, MAILBOX_BYTES);
        assert_eq!(IN_OFF % 4096, IN_OFF, "IN e' dentro la pagina");
    }
}
