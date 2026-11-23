//! Client delle finestre del DE (D8b, ADR-0088).
//!
//! Protocollo verso il servizio `console` (proprietario del compositor
//! `musivum`): i messaggi viaggiano su IPC per nome su `Service::Console`.
//! Ogni finestra ha un `id` restituito da [`open`]; il cliente la disegna
//! nella sua superficie (shm, mappata in RW) e annuncia il danno con
//! [`commit`].
//!
//! Il meccanismo e' neutro: non sa cosa sia una tile, una sessione o
//! un'applicazione. La policy (chi puo' aprire cosa, dove, con quali diritti)
//! vive sopra, in `impluvium`/`littera` (D8c+).

use crate::sys::{shm_create, shm_map};
use crate::{send, Service, WIN_CLOSE, WIN_COMMIT, WIN_OPEN, WIN_RAISE, WIN_SET_RECT, WIN_SET_SURFACE, WIN_STATS};

/// Esito di un'operazione sulle finestre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinError {
    /// Il servizio `console` non e' raggiungibile.
    ServiceDead,
    /// Il servizio ha rifiutato la richiesta (handle invalido, limite
    /// raggiunto, finestra sconosciuta, ecc.).
    Denied,
    /// Errore IPC (canale rotto).
    Ipc,
}

/// Risposta di [`open`].
#[derive(Clone, Copy, Debug)]
pub struct OpenReply {
    /// Id della finestra (>= 1).
    pub win: u32,
    /// Id della superficie condivisa da mappare in RW (>= 1).
    pub shm_id: u32,
}

/// Apri una finestra nella tile `tile` (handle; `0` = floating, non ancora
/// supportato in D8b). `size` e' la dimensione della superficie in pixel
/// (`w<<16 | h`): il server la crea e la mappa in sola lettura per il blit.
///
/// La finestra **non e' visibile** finche' il cliente non fa il primo
/// [`commit`]: aprire non dipinge un rect nero.
pub fn open(tile: u32, size: u32) -> Result<OpenReply, WinError> {
    let r = send(service_chan()?, WIN_OPEN, tile as u64, size as u64).map_err(|_| WinError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(WinError::Denied);
    }
    Ok(OpenReply { win: r.w0 as u32, shm_id: r.w1 as u32 })
}

/// Annuncia che il cliente ha ridisegnato: il compositor trasferisce il
/// rect danneggiato. `rect` = `0` per tutta la finestra; altrimenti
/// `x<<48 | y<<32 | w<<16 | h` (coordinate locali alla finestra).
pub fn commit(win: u32, rect: u64) -> Result<(), WinError> {
    let r = send(service_chan()?, WIN_COMMIT, win as u64, rect).map_err(|_| WinError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(WinError::Denied);
    }
    Ok(())
}

/// Sposta/ridimensiona la finestra (solo floating; D8f).
pub fn set_rect(win: u32, rect: u64) -> Result<(), WinError> {
    let r = send(service_chan()?, WIN_SET_RECT, win as u64, rect).map_err(|_| WinError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(WinError::Denied);
    }
    Ok(())
}

/// Cambia la superficie della finestra (dopo un proprio resize; D8f).
pub fn set_surface(win: u32, shm_id: u32) -> Result<(), WinError> {
    let r = send(service_chan()?, WIN_SET_SURFACE, win as u64, shm_id as u64)
        .map_err(|_| WinError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(WinError::Denied);
    }
    Ok(())
}

/// Porta la finestra in cima al proprio livello (mai fuori; D8f).
pub fn raise(win: u32) -> Result<(), WinError> {
    let r = send(service_chan()?, WIN_RAISE, win as u64, 0).map_err(|_| WinError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(WinError::Denied);
    }
    Ok(())
}

/// Chiudi la finestra: il server libera la superficie e ridisegna il
/// terminale sotto il suo vecchio rect.
pub fn close(win: u32) -> Result<(), WinError> {
    let r = send(service_chan()?, WIN_CLOSE, win as u64, 0).map_err(|_| WinError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(WinError::Denied);
    }
    Ok(())
}

/// Solo test: `(finestre visibili, finestre totali)` del compositor.
pub fn stats() -> Result<(usize, usize), WinError> {
    let r = send(service_chan()?, WIN_STATS, 0, 0).map_err(|_| WinError::Ipc)?;
    Ok((r.w0 as usize, r.w1 as usize))
}

/// Crea e mappa una superficie per una finestra: `(shm_id, va)`.
///
/// Serve al cliente che vuole disegnare **prima** di avere una finestra; il
/// percorso normale e' invece: [`open`] crea la superficie sul server, il
/// cliente la mappa in RW con `shm_map(reply.shm_id, 0, PROT_READ|PROT_WRITE)`.
pub fn surface_create(bytes: usize) -> Result<(u32, usize), WinError> {
    let id = shm_create(bytes).map_err(|_| WinError::Ipc)?;
    let va = shm_map(id, 0, crate::PROT_READ | crate::PROT_WRITE).map_err(|_| WinError::Ipc)?;
    Ok((id, va))
}

/// Risolve il canale del servizio `console`.
fn service_chan() -> Result<u64, WinError> {
    crate::service_lookup(Service::Console)
        .map(|c| c as u64)
        .map_err(|_| WinError::ServiceDead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_error_varianti() {
        assert_ne!(WinError::ServiceDead, WinError::Denied);
        assert_ne!(WinError::Denied, WinError::Ipc);
        let _ = format!("{:?}", WinError::ServiceDead);
    }
}
