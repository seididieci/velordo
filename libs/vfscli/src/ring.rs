// ── FS wrappers (Fase 10.2): ring buffer SPSC + IPC diretta a cardo ──
//
// Ogni processo ha DUE pagine ring (request + response) allocate dalla
// syscall 26 (`SYS_RING_ALLOC`) e mappate a `REQ_RING_VA` e `RESP_RING_VA`.
// Le pagine vengono registrate presso cardo con una IPC `FS_BUF_REG`.
// Le operazioni FS scrivono un request frame nel request ring, notificano
// cardo con `FS_NOTIFY`, e leggono il response frame dalla response ring
// dopo la reply IPC.

// ── Ring buffer constants ─────────────────────────────────────────

/// Request/response ring virtuali: via `super::abi` (single source S2.2 in
/// `syscall-numbers`, mai i valori — vale anche per la copia in `std`).
pub use super::abi::{USER_FS_BUFFER as REQ_RING_VA, USER_RESP_RING as RESP_RING_VA};
/// Finestre DEDICATE per i relay cardo→driver (zero-copy senza clobber):
/// cardo inietta qui (via `map_in`) i ring del client quando inoltra una DEV_*.
/// Separate dalle finestre proprie (REQ/RESP): i ring propri di un driver-server
/// non vengono mai rimappati da nessuno, quindi niente `remap` dance, niente
/// race di preemption tra remap e uso (osservato: letture congelate/wedge).
/// Libere nella mappa user (heap da +0x400000, stack sotto, VGA +0x100000).
pub use super::abi::{CLI_REQ_VA, CLI_RESP_VA};
/// Capacita' dati per ring (4088 byte; gli ultimi 8 byte della pagina
/// 4KiB = head + tail a 0xFF8/0xFFC, fuori dall'area dati).
/// Single source (A1): prima duplicata in cardo/block/vela/gpu/tty/kbd.
pub const RING_DATA_CAP: usize = 4088;
/// Dimensione massima di un payload dati in un singolo frame del ring.
/// Il response frame occupa 16 B di header: il payload utile massimo e'
/// RING_DATA_CAP - 16. Un valore sotto il massimo lascia margine per il
/// wrap e per eventuali frame non ancora letti.
pub const RING_MAX_PAYLOAD: usize = 4000;
/// Offset head nel ring page.
pub const RING_HEAD: usize = 0xFF8;
/// Offset tail nel ring page.
pub const RING_TAIL: usize = 0xFFC;

/// Valore di errore IPC + no-handshake: via `super::abi` (S2.2: li invia
/// cardo, sono ABI in `syscall-numbers`).
pub use super::abi::{ERR, ERR_NOHANDSHAKE};

// ── Ring I/O helpers ──────────────────────────────────────────────

/// Legge head e tail dal ring a `ring_va`.
pub unsafe fn ring_positions(ring_va: u64) -> (u32, u32) {
    let head = unsafe { core::ptr::read_volatile::<u32>(core::ptr::with_exposed_provenance((ring_va + RING_HEAD as u64) as usize)) };
    let tail = unsafe { core::ptr::read_volatile::<u32>(core::ptr::with_exposed_provenance((ring_va + RING_TAIL as u64) as usize)) };
    (head, tail)
}

/// Quanti byte di dati sono disponibili nel ring (producer=head, consumer=tail).
/// Head e tail sono wrapped in [0, RING_DATA_CAP).
pub fn ring_available(head: u32, tail: u32) -> usize {
    ((head + RING_DATA_CAP as u32 - tail) % RING_DATA_CAP as u32) as usize
}

/// Quanti byte di spazio libero ci sono nel ring (max usabile = CAP - 1).
pub fn ring_free_space(head: u32, tail: u32) -> usize {
    RING_DATA_CAP - 1 - ring_available(head, tail)
}

/// Scrive `data` nel ring a `ring_va` partendo dalla posizione `head`.
/// Avanza head di `data.len()`. Non verifica lo spazio (chiamante deve farlo).
///
/// I dati sono copiati in due span contigui (senza wrap, poi wrap) invece
/// che byte-per-byte con `% RING_DATA_CAP`: elimina la divisione per ogni
/// byte sulla hot path di tutti i frame FS/DISK.
pub unsafe fn ring_write_at(ring_va: u64, head: u32, data: &[u8]) {
    let dst: *mut u8 = core::ptr::with_exposed_provenance_mut(ring_va as usize);
    // Un singolo modulo per chiamata (non per byte): rende la funzione
    // robusta anche se `head` arriva fuori range (il vecchio `%` per byte
    // avvolgeva silenziosamente). Dopo questo, `head` e' sempre < RING_DATA_CAP.
    let head = head as usize % RING_DATA_CAP;
    let n = data.len();
    if n == 0 {
        return;
    }
    // Span 1: da `head` fino al limite del ring (niente wrap).
    let first = n.min(RING_DATA_CAP - head);
    for i in 0..first {
        unsafe { core::ptr::write_volatile(dst.add(head + i), data[i]); }
    }
    // Span 2: avvolgimento dall'inizio dell'area dati del ring.
    for i in first..n {
        unsafe { core::ptr::write_volatile(dst.add(i - first), data[i]); }
    }
}

/// Legge `count` byte dal ring a `ring_va` partendo dalla posizione `tail`.
///
/// Come `ring_write_at`: un modulo per chiamata + due span contigui,
/// niente modulo per byte.
pub unsafe fn ring_read_at(ring_va: u64, tail: u32, dst: &mut [u8], count: usize) {
    let src: *const u8 = core::ptr::with_exposed_provenance(ring_va as usize);
    let tail = tail as usize % RING_DATA_CAP;
    let n = count.min(dst.len());
    if n == 0 {
        return;
    }
    // Span 1: da `tail` fino al limite del ring (niente wrap).
    let first = n.min(RING_DATA_CAP - tail);
    for i in 0..first {
        dst[i] = unsafe { core::ptr::read_volatile(src.add(tail + i)) };
    }
    // Span 2: avvolgimento dall'inizio dell'area dati del ring.
    for i in first..n {
        dst[i] = unsafe { core::ptr::read_volatile(src.add(i - first)) };
    }
}

// ── Frame helpers lato server (A2) ─────────────────────────────────
// Prima identici in vela/gpu/tty/kbd/block: operano sulla response/
// request ring DEL CLIENT (VA parametrica, mappata da cardo via `map_in`).
// Formato response `[len:8][0:8][payload]`, request `[tag:4][w0:8][w1:8]
// [payload]` (header 20 B). Diversi dai frame FS di cardo (`[result:8]
// [w1:8]`), che restano locali al server.

/// Scrive un response frame `[len:8][0:8][payload]` nel ring a `resp_va`.
pub unsafe fn resp_frame_write(resp_va: u64, data: &[u8]) {
    let frame_len = 16 + data.len();
    unsafe {
        let (head, _tail) = ring_positions(resp_va);
        let mut hdr = [0u8; 16];
        hdr[0..8].copy_from_slice(&(data.len() as u64).to_le_bytes());
        hdr[8..16].copy_from_slice(&0u64.to_le_bytes());
        let dst: *mut u8 = core::ptr::with_exposed_provenance_mut(resp_va as usize);
        for (i, byte) in hdr.iter().enumerate() {
            let p = ((head as usize) + i) % RING_DATA_CAP;
            core::ptr::write_volatile(dst.add(p), *byte);
        }
        for (i, byte) in data.iter().enumerate() {
            let p = ((head as usize) + 16 + i) % RING_DATA_CAP;
            core::ptr::write_volatile(dst.add(p), *byte);
        }
        let new_head = ((head as usize) + frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile(core::ptr::with_exposed_provenance_mut((resp_va + RING_HEAD as u64) as usize), new_head as u32);
    }
}

/// Consuma `count` byte di payload dalla request ring a `req_va`, avanzando
/// la tail di (20 + count). I dati vengono scartati, ma la tail va comunque
/// avanzata o il prossimo request dello stesso client verrebbe letto male.
pub unsafe fn req_frame_consume(req_va: u64, count: usize) {
    unsafe {
        let tail = core::ptr::read_volatile::<u32>(core::ptr::with_exposed_provenance((req_va + RING_TAIL as u64) as usize));
        let new_tail = ((tail as usize) + 20 + count) % RING_DATA_CAP;
        core::ptr::write_volatile(core::ptr::with_exposed_provenance_mut((req_va + RING_TAIL as u64) as usize), new_tail as u32);
    }
}

/// Legge `count` byte di payload dalla request ring a `req_va` (dopo l'header
/// frame da 20 B) e avanza la tail di (20 + letti). Ritorna i byte letti.
pub unsafe fn req_frame_read(req_va: u64, dst: &mut [u8], count: usize) -> usize {
    unsafe {
        let (_head, tail) = ring_positions(req_va);
        let src: *const u8 = core::ptr::with_exposed_provenance(req_va as usize);
        let n = count.min(dst.len());
        for i in 0..n {
            let p = ((tail as usize) + 20 + i) % RING_DATA_CAP;
            dst[i] = core::ptr::read_volatile(src.add(p));
        }
        let new_tail = ((tail as usize) + 20 + n) % RING_DATA_CAP;
        core::ptr::write_volatile(core::ptr::with_exposed_provenance_mut((req_va + RING_TAIL as u64) as usize), new_tail as u32);
        n
    }
}

/// Scrive un frame nel request ring. Formato: [tag:4][w0:8][w1:8][payload].
/// Ritorna true se il frame e' stato scritto, false se non c'e' spazio.
pub fn req_ring_write(tag: u32, w0: u64, w1: u64, payload: &[u8]) -> bool {
    let frame_len = 20 + payload.len();
    unsafe {
        let (head, tail) = ring_positions(REQ_RING_VA);
        if ring_free_space(head, tail) < frame_len {
            return false;
        }
        // Scrivi header (tag + w0 + w1)
        let mut hdr = [0u8; 20];
        hdr[0..4].copy_from_slice(&tag.to_le_bytes());
        hdr[4..12].copy_from_slice(&w0.to_le_bytes());
        hdr[12..20].copy_from_slice(&w1.to_le_bytes());
        ring_write_at(REQ_RING_VA, head, &hdr);
        // Scrivi payload (ring_write_at gestisce il wrap con % RING_DATA_CAP)
        if !payload.is_empty() {
            ring_write_at(REQ_RING_VA, (head + 20) % RING_DATA_CAP as u32, payload);
        }
        // Aggiorna head (wrappa entro [0, RING_DATA_CAP))
        let new_head = ((head as usize) + frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile(core::ptr::with_exposed_provenance_mut((REQ_RING_VA + RING_HEAD as u64) as usize), new_head as u32);
    }
    true
}

/// Legge un frame dal response ring. Formato: [result:8][w1:8][payload].
/// Ritorna (result, w1, payload_len) o None se il ring e' vuoto.
pub fn resp_ring_read() -> Option<(u64, u64, usize)> {
    unsafe {
        let (head, tail) = ring_positions(RESP_RING_VA);
        if ring_available(head, tail) < 8 {
            return None;
        }
        // Leggi result (8 byte)
        let mut result_bytes = [0u8; 8];
        ring_read_at(RESP_RING_VA, tail, &mut result_bytes, 8);
        let result = u64::from_le_bytes(result_bytes);
        // Leggi w1 (8 byte)
        let mut w1_bytes = [0u8; 8];
        ring_read_at(RESP_RING_VA, tail + 8, &mut w1_bytes, 8);
        let w1 = u64::from_le_bytes(w1_bytes);
        // Calcola lunghezza payload
        let total = ring_available(head, tail);
        let payload_len = if total > 16 { total - 16 } else { 0 };
        Some((result, w1, payload_len))
    }
}

/// Legge i payload bytes dal response ring (dopo result+w1).
pub fn resp_ring_read_payload(dst: &mut [u8], payload_len: usize) {
    unsafe {
        let (_, tail) = ring_positions(RESP_RING_VA);
        ring_read_at(RESP_RING_VA, (tail + 16) % RING_DATA_CAP as u32, dst, payload_len);
        // Avanza tail (wrappa entro [0, RING_DATA_CAP))
        let total = 16 + payload_len;
        let new_tail = ((tail as usize) + total) % RING_DATA_CAP;
        core::ptr::write_volatile(core::ptr::with_exposed_provenance_mut((RESP_RING_VA + RING_TAIL as u64) as usize), new_tail as u32);
    }
}

/// Avanza la tail del response ring (per frame letti senza payload).
pub fn resp_ring_consume(frame_len: usize) {
    unsafe {
        let (_, tail) = ring_positions(RESP_RING_VA);
        let new_tail = ((tail as usize) + frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile(core::ptr::with_exposed_provenance_mut((RESP_RING_VA + RING_TAIL as u64) as usize), new_tail as u32);
    }
}

/// Riporta indietro la head del request ring di `frame_len` byte: usato per
/// "disfare" un frame appena scritto quando la successiva `send_async` fallisce
/// (coda piena / canale morto). Sicuro perche' il frame non e' mai stato
/// notificato: il server non puo' averlo consumato (tail ferma).
pub fn req_ring_rollback(frame_len: usize) {
    unsafe {
        let (head, _tail) = ring_positions(REQ_RING_VA);
        let new_head = (head as usize + RING_DATA_CAP - frame_len) % RING_DATA_CAP;
        core::ptr::write_volatile(core::ptr::with_exposed_provenance_mut((REQ_RING_VA + RING_HEAD as u64) as usize), new_head as u32);
    }
}
