//! Client delle **tile** del DE (D8e, ADR-0092).
//!
//! Una tile e' un rettangolo della console che mostra una **sessione**: il
//! layout lo decide il DE (albero di split), il server applica. La tile non ha
//! stato proprio nel server — e' `rect + sessione` — e l'handle e' `sid + 1`
//! (1:1: una tile e' una sessione, la torsione del piano DE).
//!
//! Protocollo: [`set`] per ogni tile (staging), [`commit`] applica tutto in un
//! render+flush solo. Solo le immagini >= `ThirdParty` possono disporre il
//! layout (il DE); `SESS_FOCUS` resta il modo per dire chi riceve i tasti.

use crate::{Service, TILE_COMMIT, TILE_CRC, TILE_DROP, TILE_INFO, TILE_SET, TILE_SIZE};

/// Esito di un'operazione sulle tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileError {
    /// Il servizio `console` non e' raggiungibile.
    ServiceDead,
    /// Il server ha rifiutato (policy, sessione inesistente, staging vuoto).
    Denied,
    /// Errore IPC.
    Ipc,
}

/// Risolve il canale della console.
fn chan() -> Result<u64, TileError> {
    crate::service_lookup(Service::Console)
        .map(|c| c as u64)
        .map_err(|_| TileError::ServiceDead)
}

/// Dispone la tile della sessione `sid`: rettangolo `(x, y, w, h)` in pixel.
///
/// `w = 0` = tile **nascosta** (la sessione resta viva, non viene disegnata:
/// e' cosi' che si fa lo zoom — il DE manda il layout con una sola tile
/// visibile). E' uno **staging**: si applica con [`commit`].
pub fn set(sid: u32, x: u32, y: u32, w: u32, h: u32) -> Result<(), TileError> {
    let r = crate::send(
        chan()?,
        TILE_SET,
        sid as u64,
        syscall_numbers::pack_tile_rect(x, y, w, h),
    )
    .map_err(|_| TileError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(TileError::Denied);
    }
    Ok(())
}

/// Applica lo staging: ridimensiona le sessioni, marca il danno e fa **un**
/// render + flush.
pub fn commit() -> Result<(), TileError> {
    let r = crate::send(chan()?, TILE_COMMIT, 0, 0).map_err(|_| TileError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(TileError::Denied);
    }
    Ok(())
}

/// `(tile visibili, handle della tile in focus)`.
pub fn info() -> Result<(usize, u32), TileError> {
    let r = crate::send(chan()?, TILE_INFO, 0, 0).map_err(|_| TileError::Ipc)?;
    Ok((r.w0 as usize, r.w1 as u32))
}

/// `(larghezza, altezza)` del display **in pixel**: l'unita' del layout.
pub fn size() -> Result<(u32, u32), TileError> {
    let r = crate::send(chan()?, TILE_SIZE, 0, 0).map_err(|_| TileError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(TileError::Denied);
    }
    Ok(((r.w0 >> 16) as u32, (r.w0 & 0xFFFF) as u32))
}

/// Toglie la tile della sessione `sid` (il rect torna al wallpaper). La
/// sessione **non** si chiude: lo fa chi la possiede, con `sess::Session::close`.
pub fn drop_tile(sid: u32) -> Result<(), TileError> {
    let r = crate::send(chan()?, TILE_DROP, sid as u64, 0).map_err(|_| TileError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(TileError::Denied);
    }
    Ok(())
}

/// CRC32 del **rect** della tile: `(crc, byte)`. Prova che un'operazione ha
/// toccato una tile e non l'altra (test del DE).
pub fn crc(sid: u32) -> Result<(u32, usize), TileError> {
    let r = crate::send(chan()?, TILE_CRC, sid as u64, 0).map_err(|_| TileError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(TileError::Denied);
    }
    Ok((r.w1 as u32, r.w0 as usize))
}
