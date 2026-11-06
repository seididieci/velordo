//! Rete (ADR-0069): costanti virtio-net condivise tra driver e servizio.

/// Vendor PCI virtio.
pub const VIRTIO_VENDOR: u16 = 0x1af4;
/// Device virtio-net modern (`disable-legacy=on`).
pub const VIRTIO_NET_DEVICE: u16 = 0x1041;
/// Device virtio-net legacy (fallback di enumerazione).
pub const VIRTIO_NET_DEVICE_LEGACY: u16 = 0x1000;

/// Feature: MAC address fornito dal device.
pub const VIRTIO_NET_F_MAC: u32 = 1 << 5;
/// Feature: link status nel device config.
pub const VIRTIO_NET_F_STATUS: u32 = 1 << 16;

/// Lunghezza dell'header virtio-net (modern, senza MRG_RXBUF).
pub const VIRTIO_NET_HDR_LEN: usize = 12;
/// `status` del device: link up.
pub const VIRTIO_NET_S_LINK_UP: u16 = 1;

/// Coda RX.
pub const QUEUE_RX: u16 = 0;
/// Coda TX.
pub const QUEUE_TX: u16 = 1;
