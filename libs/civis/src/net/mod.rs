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

/// Esito di [`wait`] (E2, ADR-0084).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// C'e' qualcosa da fare: un hint di rete, oppure un messaggio di altro
    /// tipo (messo da parte, `recv`/`recv_poll` lo restituiscono). Il
    /// chiamante **ricontrolla** i ring e serve la coda.
    Event,
    /// Scadenza raggiunta senza nulla di nuovo (`0` = mai: qui non accade).
    Timeout,
    /// Il servizio `net` e' morto (o il canale non risponde piu').
    Dead,
}

/// Dorme finche' non arriva **qualcosa**: l'hint del servizio `net`
/// (`NET_EP_EVENT`), un altro messaggio, la morte del servizio o la
/// `deadline` (tick assoluto, `0` = mai).
///
/// E' l'attesa **unica** dei client di rete (E2, ADR-0084): lo stesso
/// "messaggio oppure il tempo" di `input::wait_until` (E1, ADR-0083), sopra
/// `recv_timeout`. Il notify e' un **hint coalescente** — il dato sta nel
/// ring — quindi un hint perso non perde dati: chi si sveglia drena.
///
/// I messaggi che non sono hint (richieste di altri client arrivate mentre
/// si aspettava) finiscono nello stash di `ipc` e `recv`/`recv_poll` li
/// restituiscono: **mai persi**, e il ritorno e' **immediato** (`Event`) —
/// un servizio che aspetta un hint di rete mentre ha una richiesta in coda
/// deve poterla servire, altrimenti il client che aspetta la reply resta
/// appeso. Una `EXIT_NOTIFY` sul canale `chan` = `Dead`; su altri canali e'
/// la morte di un altro peer e viene messa da parte.
pub fn wait(chan: u64, deadline: u64) -> Wait {
    match crate::recv_timeout(deadline) {
        Ok(m) => {
            if m.tag == crate::NET_EP_EVENT && m.channel == chan {
                return Wait::Event;
            }
            if crate::is_exit_notify(&m) && m.channel == chan {
                return Wait::Dead;
            }
            // Non e' roba di `net`: non si butta (e non si ignora: il
            // chiamante deve poterla servire subito).
            crate::ipc::stash_push(m);
            Wait::Event
        }
        Err(crate::RecvError::Timeout) => Wait::Timeout,
        Err(crate::RecvError::Dead) => Wait::Dead,
    }
}

/// `true` se almeno uno degli endpoint ha slot nel ring RX (drain = verita').
///
/// Va chiamata **prima** di [`wait`]: un hint arrivato mentre il processo era
/// occupato (o perso perche' la coda era piena) non deve far dormire chi ha
/// gia' dati da leggere.
pub fn pending(eps: &[&Endpoint]) -> bool {
    eps.iter().any(|e| e.pending())
}
