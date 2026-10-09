use super::*;

/// Request/response ring virtuali: via `civis` (single source S2.2 in
/// `syscall-numbers`, mai i valori).
pub use civis::{USER_FS_BUFFER as REQ_RING_VA, USER_RESP_RING as RESP_RING_VA};

// ── Ring I/O (Fase 10.2) ─────────────────────────────────────────
// `ring_positions`/`ring_available` da `civis` (import sopra, A1).

/// Legge `count` byte dal ring a `ring_va` dalla posizione `pos`.
unsafe fn ring_read_at(ring_va: u64, pos: u32, dst: &mut [u8], count: usize) {
    let src = ring_va as *const u8;
    for i in 0..count.min(dst.len()) {
        let p = ((pos as usize) + i) % RING_DATA_CAP;
        dst[i] = unsafe { core::ptr::read_volatile(src.add(p)) };
    }
}

/// Scrive `data` nel ring a `ring_va` dalla posizione `pos`.
unsafe fn ring_write_at(ring_va: u64, pos: u32, data: &[u8]) {
    let dst = ring_va as *mut u8;
    for (i, byte) in data.iter().enumerate() {
        let p = ((pos as usize) + i) % RING_DATA_CAP;
        unsafe { core::ptr::write_volatile(dst.add(p), *byte); }
    }
}

/// Legge un request frame dal request ring. Formato: [tag:4][w0:8][w1:8][payload].
/// Ritorna (tag, w0, w1, payload_len) o None se il ring e' vuoto.
pub fn req_ring_read() -> Option<(u32, u64, u64, usize)> {
    unsafe {
        let (head, tail) = ring_positions(REQ_RING_VA);
        if ring_available(head, tail) < 20 {
            return None;
        }
        let mut tag_bytes = [0u8; 4];
        ring_read_at(REQ_RING_VA, tail, &mut tag_bytes, 4);
        let tag = u32::from_le_bytes(tag_bytes);
        let mut w0_bytes = [0u8; 8];
        ring_read_at(REQ_RING_VA, tail + 4, &mut w0_bytes, 8);
        let w0 = u64::from_le_bytes(w0_bytes);
        let mut w1_bytes = [0u8; 8];
        ring_read_at(REQ_RING_VA, tail + 12, &mut w1_bytes, 8);
        let w1 = u64::from_le_bytes(w1_bytes);
        let total = ring_available(head, tail);
        let payload_len = if total > 20 { total - 20 } else { 0 };
        Some((tag, w0, w1, payload_len))
    }
}

/// Legge i payload bytes dal request ring (dopo tag+w0+w1).
pub fn req_ring_read_payload(dst: &mut [u8], payload_len: usize) {
    unsafe {
        let (_, tail) = ring_positions(REQ_RING_VA);
        ring_read_at(REQ_RING_VA, (tail + 20) % RING_DATA_CAP as u32, dst, payload_len);
        let total = 20 + payload_len;
        let new_tail = ((tail as usize) + total) % RING_DATA_CAP;
        core::ptr::write_volatile((REQ_RING_VA + RING_TAIL as u64) as *mut u32, new_tail as u32);
    }
}

/// Avanza la tail del request ring (per frame letti senza payload).
pub fn req_ring_consume(frame_len: usize) {
    unsafe {
        let (_, tail) = ring_positions(REQ_RING_VA);
        let new_tail = ((tail as usize) + frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile((REQ_RING_VA + RING_TAIL as u64) as *mut u32, new_tail as u32);
    }
}

/// Resync del request ring del client (tail=head, scarta tutto): il frame in
/// testa e' impossibile (tag sconosciuto o incompleto) e qualunque consumo lo
/// disallineerebbe per sempre (osservato: tag=0x0, RINGFULL lato client e
/// stallo senza recovery). Il mittente riceve ERR; i client ritentano
/// (tty: flush riprova, pump alla prossima notify) o vedono -1 (sync).
/// Sicuro: il mittente scrive un frame intero prima di notificare, quindi
/// scartare qui non taglia mai un frame valido a meta'.
pub fn req_resync() {
    unsafe {
        let (head, _) = ring_positions(REQ_RING_VA);
        core::ptr::write_volatile((REQ_RING_VA + RING_TAIL as u64) as *mut u32, head);
    }
    println!("[cardo] resync request ring (frame impossibile, tail=head)");
}

/// Scrive un response frame nel response ring. Formato: [result:8][w1:8][payload].
pub fn resp_ring_write(result: u64, w1: u64, payload: &[u8]) {
    let frame_len = 16 + payload.len();
    unsafe {
        let (head, _tail) = ring_positions(RESP_RING_VA);
        let mut hdr = [0u8; 16];
        hdr[0..8].copy_from_slice(&result.to_le_bytes());
        hdr[8..16].copy_from_slice(&w1.to_le_bytes());
        ring_write_at(RESP_RING_VA, head, &hdr);
        if !payload.is_empty() {
            ring_write_at(RESP_RING_VA, (head + 16) % RING_DATA_CAP as u32, payload);
        }
        let new_head = ((head as usize) + frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile((RESP_RING_VA + RING_HEAD as u64) as *mut u32, new_head as u32);
    }
}

/// Mappa il request ring del client a REQ_RING_VA nello spazio di cardo.
pub fn map_client_req_ring(rings: &BTreeMap<u64, (u64, u64)>, chan: u64) -> bool {
    if let Some(&(req_phys, _)) = rings.get(&chan) {
        if civis::map_physical(req_phys, REQ_RING_VA, 1).is_ok() {
            return true;
        }
    }
    false
}

/// Mappa il response ring del client a RESP_RING_VA nello spazio di cardo.
pub fn map_client_resp_ring(rings: &BTreeMap<u64, (u64, u64)>, chan: u64) -> bool {
    if let Some(&(_, resp_phys)) = rings.get(&chan) {
        if civis::map_physical(resp_phys, RESP_RING_VA, 1).is_ok() {
            return true;
        }
    }
    false
}
