//! Client degli endpoint del servizio `net` (N7.0, ADR-0074).
//!
//! Il protocollo e' quello di ADR-0069: un ring SPSC in shm per endpoint
//! (16 slot da 2048 B, RX a offset 0, TX a `0x2000`), il formato degli slot
//! lo definisce `net-ep`, e i controlli (`open`/`tx`/`close`/`stats`)
//! viaggiano su IPC per nome verso il servizio.
//!
//! Prima di N7.0 questo codice era ricopiato in otto client con invarianti
//! diverse (chi scartava in silenzio a ring pieno, chi non controllava,
//! chi ritentava). Qui le regole sono una sola volta:
//!
//! 1. **TX**: il cursore autorevole e' locale (`tx_seq`). Il servizio legge lo
//!    slot indicato in `NET_EP_TX` (`w1`), non i contatori del ring: quelli
//!    sono consultivi. `w0 == 0` = payload preso in carico, solo allora il
//!    cursore avanza; `w0 == !0` = `Full` (backpressure), lo stesso slot si
//!    riscrive; errore IPC = `Dead` (servizio morto, mai attesa infinita).
//! 2. **RX**: la testa avanza **dopo** che il payload e' stato copiato nel
//!    buffer del chiamante. Uno slot con `len` fuori range viene consumato e
//!    saltato: mai panic, mai frame parziale.
//! 3. Nessun buffer grande sullo stack (lo userland ha 16 KiB di stack): il
//!    buffer di ricezione e' del chiamante, il pending di `Stream` sta
//!    sull'heap.

mod endpoint;
mod stream;

pub use endpoint::{Endpoint, Event, Flow, Proto, Recv, SendError};
pub use stream::{Stream, StreamError};

/// Capacita' di un payload in uno slot (metadati IPv6 esclusi): il buffer di
/// ricezione di un client non serve piu' grande di cosi'.
pub const PAYLOAD_MAX: usize = crate::NET_EP_SLOT_SIZE - net_ep::Layout::IPV6.meta();
