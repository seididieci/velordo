//! Terminale video (server `gpu`): VGA + echo + cursore hardware.
//!
//! Primo accumulo da R4 (mosse da `gpu` tali e quali, zero comportamento):
//! geometria testo e porte CRTC condivisibili coi futuri driver video.
//! La line discipline (`porta`) resta fuori.

/// Indirizzo fisico del frame buffer VGA testo.
pub const VGA_PHYS: u64 = 0xB8000;
/// Righe del buffer testo VGA.
pub const VGA_ROWS: usize = 25;
/// Colonne del buffer testo VGA.
pub const VGA_COLS: usize = 80;
/// Porta indice CRTC (cursore hardware).
pub const CRTC_INDEX: u16 = 0x3D4;
/// Porta dati CRTC (cursore hardware).
pub const CRTC_DATA: u16 = 0x3D5;

// ── virtio-gpu modern (L0b, ADR-0057) ─────────────────────────────────
// ID PCI del device QEMU `virtio-gpu-pci,disable-legacy=on` e costanti del
// protocollo (single source coi driver futuri; il trasporto resta nel
// binario finche' non arriva il secondo consumer virtio).

/// Vendor PCI virtio.
pub const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Device PCI virtio-gpu modern (transitional escluso: `disable-legacy=on`).
pub const VIRTIO_GPU_DEVICE: u16 = 0x1050;
/// Feature bit 32 (`VIRTIO_F_VERSION_1`): trasporto modern, split ring.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 0;
/// Comando `GET_DISPLAY_INFO`.
pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
/// Risposta `RESP_OK_DISPLAY_INFO`.
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
/// Bit `events_read`/`events_clear`: la UI host e' cambiata (config change).
pub const VIRTIO_GPU_EVENT_DISPLAY: u32 = 0x01;
/// Numero massimo di scanout nella risposta `GET_DISPLAY_INFO`.
pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;
