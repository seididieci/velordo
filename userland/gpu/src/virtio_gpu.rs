//! Protocollo `virtio-gpu` (L0b): solo `GET_DISPLAY_INFO`.
//!
//! Strutture wire del device (little-endian, layout del Linux uapi) e parsing
//! difensivo delle risposte: nessun accesso oltre il buffer, nessuna fiducia
//! nei valori del device (fallback VGA se la risposta non e' valida).

use crate::virtio::DisplayInfo;
use vela::gpu::VIRTIO_GPU_RESP_OK_DISPLAY_INFO;

/// Dimensione di `virtio_gpu_ctrl_hdr`.
pub const CTRL_HDR_LEN: usize = 24;
/// Dimensione di una `virtio_gpu_display_one`.
const DISPLAY_ONE_LEN: usize = 24;
/// Offset di `pmodes` nella risposta `GET_DISPLAY_INFO`.
const PMODES_OFF: usize = CTRL_HDR_LEN;
/// Numero massimo di scanout.
const MAX_SCANOUTS: usize = vela::gpu::VIRTIO_GPU_MAX_SCANOUTS;

#[inline]
unsafe fn rd32(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_unaligned((base + off) as *const u32) }
}

/// `hdr.type == RESP_OK_DISPLAY_INFO`.
pub fn response_ok(resp: usize, len: usize) -> bool {
    if len < CTRL_HDR_LEN {
        return false;
    }
    unsafe { rd32(resp, 0) == VIRTIO_GPU_RESP_OK_DISPLAY_INFO }
}

/// Primo scanout abilitato con geometria non nulla.
pub fn parse_display_info(resp: usize, len: usize) -> Option<DisplayInfo> {
    if len < PMODES_OFF + MAX_SCANOUTS * DISPLAY_ONE_LEN {
        return None;
    }
    for i in 0..MAX_SCANOUTS {
        let base = PMODES_OFF + i * DISPLAY_ONE_LEN;
        let enabled = unsafe { rd32(resp, base + 16) };
        if enabled == 0 {
            continue;
        }
        let width = unsafe { rd32(resp, base + 8) };
        let height = unsafe { rd32(resp, base + 12) };
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
