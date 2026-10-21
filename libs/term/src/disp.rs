//! Protocollo del device `/dev/display` (D3, ADR-0060).
//!
//! Il driver `gpu` pubblica `/dev/display` e parla questo protocollo; la
//! console lo usa per sapere **cosa** puo' mostrare e per **presentare** un
//! buffer. La proprieta' dei pixel e' della **console**: il driver non
//! alloca e non scrive nel buffer, programma solo il device (virtio-gpu:
//! risorsa 2D + scanout).
//!
//! I viaggi sono i frame del device (FS): la console scrive un
//! [`Present`] con `write_fs`, il driver risponde con il [`Descriptor`] in
//! `read_fs`. Nessun tag IPC nuovo: il device riusa il protocollo `DEV_*`.
//!
//! Byte order: little-endian (x86-64, come tutto il resto).

/// Backend: nessun device pixel, la console usa la VGA testuale.
pub const BACKEND_VGA_TEXT: u8 = 0;
/// Backend: device virtio-gpu con scanout pixel.
pub const BACKEND_VIRTIO_GPU: u8 = 1;

/// Formato pixel B8G8R8A8: in memoria i byte sono **B, G, R, A** (una
/// `u32` little-endian = `A<<24 | R<<16 | G<<8 | B`).
pub const FORMAT_B8G8R8A8: u32 = 1;

/// Operazione "presenta questo buffer" (unica per ora).
pub const OP_PRESENT: u8 = 1;

/// Cosa il driver puo' mostrare (frame da 24 byte, restituito in `read_fs`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Descriptor {
    /// `BACKEND_*`.
    pub backend: u8,
    pub _pad0: [u8; 3],
    /// Larghezza in pixel (0 se il backend e' VGA testuale).
    pub width: u32,
    /// Altezza in pixel (0 se il backend e' VGA testuale).
    pub height: u32,
    /// Byte per riga del framebuffer (0 se VGA testuale).
    pub stride: u32,
    /// `FORMAT_*` (0 se VGA testuale).
    pub format: u32,
    /// Flag riservati (0 per ora).
    pub flags: u32,
}

/// Richiesta del client al driver (frame da 32 byte, scritto con `write_fs`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Present {
    /// `OP_PRESENT`.
    pub op: u8,
    pub _pad0: [u8; 3],
    /// Indirizzo **fisico** del buffer (il device lo scrive: serve il phys,
    /// non la VA).
    pub phys: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub _reserved: u32,
}

/// Byte del frame del descrittore.
pub const DESCRIPTOR_LEN: usize = 24;
/// Byte del frame della richiesta.
pub const PRESENT_LEN: usize = 32;

/// Allinea/valida un frame `Present` letto dal client: ritorna `None` se la
/// richiesta non e' sensata (op sconosciuta, dimensioni zero, formato
/// sconosciuto, phys non allineato a pagina).
pub fn parse_present(buf: &[u8]) -> Option<Present> {
    if buf.len() < PRESENT_LEN {
        return None;
    }
    let op = buf[0];
    if op != OP_PRESENT {
        return None;
    }
    let mut p = Present {
        op,
        _pad0: [0; 3],
        phys: u64::from_le_bytes(buf[4..12].try_into().ok()?),
        width: u32::from_le_bytes(buf[12..16].try_into().ok()?),
        height: u32::from_le_bytes(buf[16..20].try_into().ok()?),
        stride: u32::from_le_bytes(buf[20..24].try_into().ok()?),
        format: u32::from_le_bytes(buf[24..28].try_into().ok()?),
        _reserved: 0,
    };
    if p.width == 0 || p.height == 0 || p.format != FORMAT_B8G8R8A8 {
        return None;
    }
    if p.stride < p.width * 4 {
        return None;
    }
    if p.phys & 0xFFF != 0 {
        return None;
    }
    p._reserved = 0;
    Some(p)
}

/// Serializza un descrittore nel frame da `read_fs`.
pub fn descriptor_bytes(d: &Descriptor) -> [u8; DESCRIPTOR_LEN] {
    let mut out = [0u8; DESCRIPTOR_LEN];
    out[0] = d.backend;
    out[4..8].copy_from_slice(&d.width.to_le_bytes());
    out[8..12].copy_from_slice(&d.height.to_le_bytes());
    out[12..16].copy_from_slice(&d.stride.to_le_bytes());
    out[16..20].copy_from_slice(&d.format.to_le_bytes());
    out[20..24].copy_from_slice(&d.flags.to_le_bytes());
    out
}

/// Serializza una richiesta `Present` nel frame da `write_fs`.
pub fn present_bytes(p: &Present) -> [u8; PRESENT_LEN] {
    let mut out = [0u8; PRESENT_LEN];
    out[0] = p.op;
    out[4..12].copy_from_slice(&p.phys.to_le_bytes());
    out[12..16].copy_from_slice(&p.width.to_le_bytes());
    out[16..20].copy_from_slice(&p.height.to_le_bytes());
    out[20..24].copy_from_slice(&p.stride.to_le_bytes());
    out[24..28].copy_from_slice(&p.format.to_le_bytes());
    out
}

/// Legge un descrittore dal frame di `read_fs`.
pub fn parse_descriptor(buf: &[u8]) -> Option<Descriptor> {
    if buf.len() < DESCRIPTOR_LEN {
        return None;
    }
    Some(Descriptor {
        backend: buf[0],
        _pad0: [0; 3],
        width: u32::from_le_bytes(buf[4..8].try_into().ok()?),
        height: u32::from_le_bytes(buf[8..12].try_into().ok()?),
        stride: u32::from_le_bytes(buf[12..16].try_into().ok()?),
        format: u32::from_le_bytes(buf[16..20].try_into().ok()?),
        flags: u32::from_le_bytes(buf[20..24].try_into().ok()?),
    })
}
