//! Protocollo display (D3, ADR-0060): come la console parla col driver
//! `gpu`.
//!
//! Il canale e' un **IPC diretto** col servizio `Gpu` (per nome), non un
//! device FS: il cardo e' single-threaded, e un driver che lo blocca (per
//! fare una scrittura) mentre serve un cliente si deadlocka — la console
//! serve `/dev/console` *e* parla col driver.
//!
//! I tag vivono in `syscall-numbers` (single source); qui solo l'impacchettamento
//! della geometria (che deve stare in un registro).
//!
//! Byte order: little-endian (x86-64, come tutto il resto).

/// Backend: nessun device pixel, la console usa la VGA testuale.
pub const BACKEND_VGA_TEXT: u8 = 0;
/// Backend: device virtio-gpu con scanout pixel.
pub const BACKEND_VIRTIO_GPU: u8 = 1;

/// Formato pixel B8G8R8A8: in memoria i byte sono **B, G, R, A** (una
/// `u32` little-endian = `A<<24 | R<<16 | G<<8 | B`).
pub const FORMAT_B8G8R8A8: u32 = 1;

/// Impacchetta la geometria in un registro 64 bit:
/// `[format:8][stride:16][height:16][width:16]`.
pub fn pack_geometry(width: u32, height: u32, stride: u32, format: u32) -> u64 {
    ((width as u64) & 0xFFFF)
        | (((height as u64) & 0xFFFF) << 16)
        | (((stride as u64) & 0xFFFF) << 32)
        | (((format as u64) & 0xFF) << 48)
}

/// L'inverso di [`pack_geometry`]: `(width, height, stride, format)`.
pub fn unpack_geometry(v: u64) -> (u32, u32, u32, u32) {
    (
        (v & 0xFFFF) as u32,
        ((v >> 16) & 0xFFFF) as u32,
        ((v >> 32) & 0xFFFF) as u32,
        ((v >> 48) & 0xFF) as u32,
    )
}

/// La geometria che la console ha ricevuto dal driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct Geometry {
    /// Larghezza in pixel.
    pub width: u32,
    /// Altezza in pixel.
    pub height: u32,
    /// Byte per riga.
    pub stride: u32,
    /// Formato pixel (`FORMAT_*`).
    pub format: u32,
}

impl Geometry {
    /// Impacchetta per il wire.
    pub fn pack(&self) -> u64 {
        pack_geometry(self.width, self.height, self.stride, self.format)
    }

    /// Dall'impacchettatura.
    pub fn unpack(v: u64) -> Geometry {
        let (width, height, stride, format) = unpack_geometry(v);
        Geometry {
            width,
            height,
            stride,
            format,
        }
    }

    /// La geometria e' usabile (dimensioni e formato sensati)?
    pub fn valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.format == FORMAT_B8G8R8A8
            && self.stride >= self.width * 4
    }
}
