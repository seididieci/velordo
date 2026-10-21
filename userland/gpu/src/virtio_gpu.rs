//! Protocollo `virtio-gpu` (L0b/D3): `GET_DISPLAY_INFO` e parsing
//! difensivo delle risposte.
//!
//! Strutture wire del device (little-endian) e parsing difensivo: nessun
//! accesso oltre il buffer, nessuna fiducia nei valori del device (il
//! fallback VGA parte se la risposta non e' valida).

use crate::virtio::DisplayInfo;
use vela::gpu::{VIRTIO_GPU_MAX_SCANOUTS, VIRTIO_GPU_RESP_OK_DISPLAY_INFO};

/// Dimensione di `virtio_gpu_ctrl_hdr`.
pub const CTRL_HDR_LEN: usize = 24;
/// Dimensione di una `virtio_gpu_display_one`.
const DISPLAY_ONE_LEN: usize = 24;
/// Offset di `pmodes` nella risposta `GET_DISPLAY_INFO`.
const PMODES_OFF: usize = CTRL_HDR_LEN;

/// Byte del frame di risposta di `GET_DISPLAY_INFO`.
pub const DISPLAY_INFO_LEN: usize = CTRL_HDR_LEN + VIRTIO_GPU_MAX_SCANOUTS * DISPLAY_ONE_LEN;

#[inline]
fn rd32(base: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([base[off], base[off + 1], base[off + 2], base[off + 3]])
}

/// `hdr.type == RESP_OK_DISPLAY_INFO`.
pub fn response_ok(resp: &[u8]) -> bool {
    if resp.len() < CTRL_HDR_LEN {
        return false;
    }
    rd32(resp, 0) == VIRTIO_GPU_RESP_OK_DISPLAY_INFO
}

/// Primo scanout abilitato con geometria non nulla.
pub fn parse_display_info(resp: &[u8]) -> Option<DisplayInfo> {
    if resp.len() < DISPLAY_INFO_LEN {
        return None;
    }
    for i in 0..VIRTIO_GPU_MAX_SCANOUTS {
        let base = PMODES_OFF + i * DISPLAY_ONE_LEN;
        let enabled = rd32(resp, base + 16);
        if enabled == 0 {
            continue;
        }
        let width = rd32(resp, base + 8);
        let height = rd32(resp, base + 12);
        if width == 0 || height == 0 {
            continue;
        }
        return Some(DisplayInfo {
            width,
            height,
            enabled,
        });
    }
    None
}
