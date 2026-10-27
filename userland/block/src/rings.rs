use super::*;
// Copia anello (no-wrap/wrap, niente modulo per byte): single source in
// `vfscli::ring`, riesportata da `civis`.
use civis::{ring_read_at, ring_write_at};

/// Scrive un response frame `[result:8][w1:8][payload]` nel ring DISK.
pub(crate) unsafe fn disk_resp_write(result: u64, w1: u64, payload: &[u8]) {
    let frame_len = 16 + payload.len();
    unsafe {
        let head = core::ptr::read_volatile((DISK_RESP_VA + RING_HEAD as u64) as *const u32);
        let mut hdr = [0u8; 16];
        hdr[0..8].copy_from_slice(&result.to_le_bytes());
        hdr[8..16].copy_from_slice(&w1.to_le_bytes());
        ring_write_at(DISK_RESP_VA, head, &hdr);
        if !payload.is_empty() {
            ring_write_at(DISK_RESP_VA, (head + 16) % RING_DATA_CAP as u32, payload);
        }
        let new_head = ((head as usize) + frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile((DISK_RESP_VA + RING_HEAD as u64) as *mut u32, new_head as u32);
    }
}

/// Legge un frame di resolve `[namelen:8][name]` dal DISK_REQ ring e lo
/// consuma (SPSC: si legge a `tail`, il producer cardo avanza `head`).
/// Ritorna il nome o None a ring vuoto/frame malformato (resync tail=head:
/// il mittente scrive il frame intero prima di notificare, quindi un frame
/// incompleto appartiene a un'epoca morta — stessa invariante dei ring FS).
pub(crate) fn disk_req_read_name() -> Option<String> {
    unsafe {
        let head = core::ptr::read_volatile((DISK_REQ_VA + RING_HEAD as u64) as *const u32);
        let tail = core::ptr::read_volatile((DISK_REQ_VA + RING_TAIL as u64) as *const u32);
        let avail = (head.wrapping_sub(tail)) % RING_DATA_CAP as u32;
        if avail < 8 {
            return None;
        }
        let t = tail as usize;
        let mut len_b = [0u8; 8];
        ring_read_at(DISK_REQ_VA, tail, &mut len_b, 8);
        let len = u64::from_le_bytes(len_b) as usize;
        if len == 0 || len > nodes::DISK_MAX_NAME || avail < (8 + len) as u32 {
            core::ptr::write_volatile((DISK_REQ_VA + RING_TAIL as u64) as *mut u32, head);
            return None;
        }
        let mut name_b = [0u8; nodes::DISK_MAX_NAME];
        ring_read_at(DISK_REQ_VA, (tail + 8) % RING_DATA_CAP as u32, &mut name_b[..len], len);
        let new_tail = (t + 8 + len) % RING_DATA_CAP;
        core::ptr::write_volatile((DISK_REQ_VA + RING_TAIL as u64) as *mut u32, new_tail as u32);
        core::str::from_utf8(&name_b[..len]).ok().map(String::from)
    }
}

/// Legge un frame di richiesta read `[count:8]` dal DISK_REQ ring e lo
/// consuma (24.2). Ritorna count (1..=7) o None a ring vuoto/count invalido
/// (resync tail=head: frame intero prima della notify, incompleto = epoca
/// morta — stessa invariante dei ring FS).
pub(crate) fn disk_req_read_count() -> Option<usize> {
    unsafe {
        let head = core::ptr::read_volatile((DISK_REQ_VA + RING_HEAD as u64) as *const u32);
        let tail = core::ptr::read_volatile((DISK_REQ_VA + RING_TAIL as u64) as *const u32);
        let avail = (head.wrapping_sub(tail)) % RING_DATA_CAP as u32;
        if avail < 8 {
            return None;
        }
        let t = tail as usize;
        let mut count_b = [0u8; 8];
        ring_read_at(DISK_REQ_VA, tail, &mut count_b, 8);
        let count = u64::from_le_bytes(count_b) as usize;
        let new_tail = (t + 8) % RING_DATA_CAP;
        core::ptr::write_volatile((DISK_REQ_VA + RING_TAIL as u64) as *mut u32, new_tail as u32);
        if count == 0 || count > nodes::DISK_MAX_SECTORS {
            return None;
        }
        Some(count)
    }
}

/// Legge un frame di write `[count:8][count*512 byte]` dal DISK_REQ ring e lo
/// consuma (24.2, generalizza il vecchio `[len:8][512]`). `out` deve tenere
/// `DISK_MAX_SECTORS` settori. Ritorna count o None (resync come sopra).
pub(crate) fn disk_req_read_multi(out: &mut [u8]) -> Option<usize> {
    if out.len() < nodes::DISK_MAX_SECTORS * 512 {
        return None;
    }
    unsafe {
        let head = core::ptr::read_volatile((DISK_REQ_VA + RING_HEAD as u64) as *const u32);
        let tail = core::ptr::read_volatile((DISK_REQ_VA + RING_TAIL as u64) as *const u32);
        let avail = (head.wrapping_sub(tail)) % RING_DATA_CAP as u32;
        if avail < 8 {
            return None;
        }
        let t = tail as usize;
        let mut count_b = [0u8; 8];
        ring_read_at(DISK_REQ_VA, tail, &mut count_b, 8);
        let count = u64::from_le_bytes(count_b) as usize;
        if count == 0 || count > nodes::DISK_MAX_SECTORS || avail < (8 + count * 512) as u32 {
            core::ptr::write_volatile((DISK_REQ_VA + RING_TAIL as u64) as *mut u32, head);
            return None;
        }
        ring_read_at(DISK_REQ_VA, (tail + 8) % RING_DATA_CAP as u32, &mut out[..count * 512], count * 512);
        let new_tail = (t + 8 + count * 512) % RING_DATA_CAP;
        core::ptr::write_volatile((DISK_REQ_VA + RING_TAIL as u64) as *mut u32, new_tail as u32);
        Some(count)
    }
}

/// Scrive un request frame `[tag:4][w0:8][w1:8][payload]` nel ring FS proprio
/// (a FS_REQ_VA). Ritorna false se non c'e' spazio (il chiamante riprova).
pub(crate) fn fs_req_write(tag: u32, w0: u64, w1: u64, payload: &[u8]) -> bool {
    let frame_len = 20 + payload.len();
    unsafe {
        let head = core::ptr::read_volatile((FS_REQ_VA + RING_HEAD as u64) as *const u32);
        let tail = core::ptr::read_volatile((FS_REQ_VA + RING_TAIL as u64) as *const u32);
        let used = (head.wrapping_sub(tail)) % RING_DATA_CAP as u32;
        if (RING_DATA_CAP as u32) - used < frame_len as u32 + 1 {
            return false;
        }
        let mut hdr = [0u8; 20];
        hdr[0..4].copy_from_slice(&tag.to_le_bytes());
        hdr[4..12].copy_from_slice(&w0.to_le_bytes());
        hdr[12..20].copy_from_slice(&w1.to_le_bytes());
        ring_write_at(FS_REQ_VA, head, &hdr);
        if !payload.is_empty() {
            ring_write_at(FS_REQ_VA, (head + 20) % RING_DATA_CAP as u32, payload);
        }
        let new_head = ((head as usize) + frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile((FS_REQ_VA + RING_HEAD as u64) as *mut u32, new_head as u32);
        true
    }
}
/// Toglie l'ultimo frame scritto (rollback su send_async fallita, come civis).
pub(crate) fn fs_req_rollback(frame_len: usize) {
    unsafe {
        let head = core::ptr::read_volatile((FS_REQ_VA + RING_HEAD as u64) as *const u32);
        let new_head = (head as usize + RING_DATA_CAP - frame_len % RING_DATA_CAP) % RING_DATA_CAP;
        core::ptr::write_volatile((FS_REQ_VA + RING_HEAD as u64) as *mut u32, new_head as u32);
    }
}
/// Legge il result di un response frame FS proprio (a FS_RESP_VA) e lo
/// consuma. Ritorna None a ring vuoto.
pub(crate) fn fs_resp_read() -> Option<u64> {
    unsafe {
        let head = core::ptr::read_volatile((FS_RESP_VA + RING_HEAD as u64) as *const u32);
        let tail = core::ptr::read_volatile((FS_RESP_VA + RING_TAIL as u64) as *const u32);
        if head == tail {
            return None;
        }
        let mut hdr = [0u8; 16];
        ring_read_at(FS_RESP_VA, head, &mut hdr, 16);
        let result = u64::from_le_bytes(hdr[0..8].try_into().unwrap_or([0xFF; 8]));
        let new_tail = ((head as usize) + 16) % RING_DATA_CAP;
        core::ptr::write_volatile((FS_RESP_VA + RING_TAIL as u64) as *mut u32, new_tail as u32);
        Some(result)
    }
}
/// Azzera entrambi i ring FS propri (epoca morta dopo EXIT_NOTIFY di cardo).
pub(crate) fn fs_rings_reset() {
    unsafe {
        core::ptr::write_volatile((FS_REQ_VA + RING_HEAD as u64) as *mut u32, 0);
        core::ptr::write_volatile((FS_REQ_VA + RING_TAIL as u64) as *mut u32, 0);
        core::ptr::write_volatile((FS_RESP_VA + RING_HEAD as u64) as *mut u32, 0);
        core::ptr::write_volatile((FS_RESP_VA + RING_TAIL as u64) as *mut u32, 0);
    }
}
