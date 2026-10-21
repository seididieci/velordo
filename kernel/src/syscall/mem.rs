// Split from syscall.rs (byte-identical move; see facade).
use core::ptr::addr_of;
use super::entry::{current_id, current_mm, PERCPU};
use super::dispatch::apply_ipc;

/// mmap(hint, len, prot, flags): mappa anonima privata nel basso canonico
/// (Fase 28, zero-fill lazy come `sbrk`: VA subito, frame al primo fault).
/// Ritorna la base o -1. 29: `prot` = NONE/R/RW (W solo ed EXEC rifiutati);
/// `flags` 0 (hint consigliato, 0 = scelta kernel) o `MMAP_FIXED`.
pub(super) fn sys_mmap(hint: u64, len: usize, prot: u64, flags: u64) -> i64 {
    use syscall_numbers::{MMAP_FIXED, PROT_NONE, PROT_READ, PROT_WRITE};
    let prot_ok = prot == PROT_NONE || prot == PROT_READ || prot == PROT_READ | PROT_WRITE;
    if !prot_ok {
        return -1; // 29: NONE/R/RW (W solo, EXEC e altri bit rifiutati)
    }
    if flags & !MMAP_FIXED != 0 {
        return -1; // flag sconosciuti (file-backed in M2a, shared in 30)
    }
    let fixed = flags & MMAP_FIXED != 0;
    if fixed && hint == 0 {
        return -1; // FIXED senza hint non ha senso
    }
    let cur = current_mm();
    match crate::arc::vmm_user::vma_map(cur, hint, len as u64, fixed, prot as u8, 0) {
        Some(base) => base as i64,
        None => -1,
    }
}

/// shm_create(len): crea una regione di memoria condivisa (Fase 30) di `len`
/// byte (frame contigui azzerati, max 256 KiB), ritorna l'id (>= 1) o -1.
pub(super) fn sys_shm_create(len: u64) -> i64 {
    match crate::arc::vmm_user::shm_create(len) {
        Some(id) => id as i64,
        None => -1,
    }
}

/// shm_map(id, hint, prot, flags): mappa la regione condivisa `id` nello
/// spazio del processo corrente come VMA (prot RW/RO), PTE non-owned
/// pre-materializzate (le stesse pagine per tutti), refcount++. Ritorna la
/// base o -1. `flags` 0 o `MMAP_FIXED`. Con `MAP_COW` (33.4, solo
/// `prot == PROT_READ`): le pagine sono mappate `RO`+`COW` (stessi frame per
/// tutti finche' nessuno scrive; al primo write `cow_fault` materializza la
/// copia privata) e ogni mappatura incrementa il refcount per-frame.
pub(super) fn sys_shm_map(id: u64, hint: u64, prot: u64, flags: u64) -> i64 {
    use syscall_numbers::{MAP_COW, MMAP_FIXED, PROT_READ, PROT_WRITE};
    let id = id as u32;
    if id == 0 {
        return -1;
    }
    if flags & !(MMAP_FIXED | MAP_COW) != 0 {
        return -1;
    }
    let cow = flags & MAP_COW != 0;
    // COW = read-only condiviso fino al primo write: solo PROT_READ (con RW
    // la scrittura sarebbe condivisa, contraddicendo il COW).
    let prot_ok = if cow {
        prot == PROT_READ
    } else {
        prot == PROT_READ || prot == PROT_READ | PROT_WRITE
    };
    if !prot_ok {
        return -1;
    }
    if flags & MMAP_FIXED != 0 && hint == 0 {
        return -1;
    }
    let fixed = flags & MMAP_FIXED != 0;
    let (phys, frames) = match crate::arc::vmm_user::shm_region(id) {
        Some(r) => r,
        None => return -1,
    };
    let len = frames * 0x1000;
    let cr3 = unsafe { (*(addr_of!(PERCPU))).current_cr3 };
    if cr3 == 0 {
        return -1;
    }
    let cur = current_mm();
    // Two-phase: se un frame e' saturo (ref 255, irraggiungibile con 32
    // processi ma mai wrappare in silenzio) si fallisce PRIMA di registrare
    // la VMA o mappare: nessun cambio di stato, nessun rollback.
    if cow {
        for i in 0..frames {
            if !crate::arc::phys_mem::ref_available(phys + i * 0x1000) {
                return -1;
            }
        }
    }
    // `id` (1..=16) entra nel campo shm (u8) del record VMA.
    let base = match crate::arc::vmm_user::vma_map(cur, hint, len, fixed, prot as u8, id as u8) {
        Some(b) => b,
        None => return -1,
    };
    if cow {
        for i in 0..frames {
            assert!(crate::arc::phys_mem::ref_inc(phys + i * 0x1000));
        }
        unsafe {
            crate::arc::vmm_user::map_user_region_cow(cr3, base, phys, frames as usize);
        }
    } else {
        let writable = prot & PROT_WRITE != 0;
        unsafe {
            crate::arc::vmm_user::map_user_region_shared(cr3, base, phys, frames as usize, writable);
        }
    }
    for i in 0..frames {
        crate::arc::vmm_user::flush_page(base + i * 0x1000);
    }
    crate::arc::vmm_user::shm_ref(id);
    base as i64
}

/// mprotect(addr, len, prot): cambia le protezioni di VMA intere (Fase 29).
/// Stesse regole di `munmap` (copertura esatta, parziali = -1 senza stato) +
/// `prot` validato come `mmap`. A NONE le pagine cadono (smappa+libera) e il
/// riuso rimaterializza zero; RO↔RW flippa il bit W in place. 0 o -1.
pub(super) fn sys_mprotect(addr: u64, len: usize, prot: u64) -> i64 {
    use syscall_numbers::{PROT_NONE, PROT_READ, PROT_WRITE};
    let prot_ok = prot == PROT_NONE || prot == PROT_READ || prot == PROT_READ | PROT_WRITE;
    if !prot_ok {
        return -1;
    }
    let cr3 = unsafe { (*(addr_of!(PERCPU))).current_cr3 };
    if cr3 == 0 {
        return -1; // cr3 non impostata
    }
    let cur = current_mm();
    if crate::arc::vmm_user::vma_protect(cur, cr3, addr, len as u64, prot as u8) {
        0
    } else {
        -1
    }
}

/// munmap(addr, len): smappa VMA intere (Fase 28, niente split). 0 o -1.
pub(super) fn sys_munmap(addr: u64, len: usize) -> i64 {
    let cr3 = unsafe { (*(addr_of!(PERCPU))).current_cr3 };
    if cr3 == 0 {
        return -1; // cr3 non impostata
    }
    let cur = current_mm();
    if crate::arc::vmm_user::vma_unmap(cur, cr3, addr, len as u64) {
        0
    } else {
        -1
    }
}

/// True se il frame `phys` e' mappabile via `map_physical`/`map_in` (Fase 35,
/// hardening): solo frame del sistema legittimi — ring page registrata (di
/// qualunque processo: data-plane FS), scratch dei test (`MAP_TEST_PHYS`),
/// frame VGA. Qualunque altro frame (kernel, page table, heap, bitmap,
/// pagine private di altri processi) e' rifiutato: senza questo, la syscall
/// era un sandbox escape totale (RW su qualunque RAM).
fn is_mappable_phys(phys: u64) -> bool {
    const VGA_PHYS: u64 = 0xB8000;
    if phys == VGA_PHYS {
        return true;
    }
    let test_start = syscall_numbers::MAP_TEST_PHYS;
    let test_end = test_start + syscall_numbers::MAP_TEST_FRAMES * 0x1000;
    if phys >= test_start && phys < test_end {
        return true;
    }
    crate::arc::vmm_user::is_ring_page(phys)
}

/// map_physical(phys_addr, virt_addr, count): mappa `count` pagine fisiche
/// a partire da `phys_addr` all'indirizzo virtuale `virt_addr` nello spazio
/// del chiamante. Usato dal console server (VGA), da cardo (ring req/resp
/// di un client, dai phys registrati via `FS_BUF_REG`) e dalla test suite
/// (pagina scratch MAP_TEST_PHYS). Fase 35: solo frame del sistema (vedi
/// `is_mappable_phys`), mai RAM arbitraria.
pub(super) fn sys_map_physical(phys_addr: u64, virt_addr: u64, count: usize) -> i64 {
    const PAGE_SIZE: u64 = 0x1000;
    const MAX_PAGES: usize = 256;

    if phys_addr & (PAGE_SIZE - 1) != 0 {
        return -1; // phys_addr non allineato a pagina
    }
    if virt_addr < crate::arc::vmm_user::USER_BASE {
        return -1; // virt_addr fuori spazio user
    }
    if count == 0 || count > MAX_PAGES {
        return -1;
    }
    // Ogni pagina deve essere un frame del sistema (Fase 35).
    for i in 0..count {
        if !is_mappable_phys(phys_addr + (i as u64) * PAGE_SIZE) {
            crate::serial_println!(
                "[syscall] map_physical: frame non mappabile {:#x} rifiutato",
                phys_addr + (i as u64) * PAGE_SIZE
            );
            return -1;
        }
    }

    let cr3 = unsafe { (*(addr_of!(PERCPU))).current_cr3 };
    if cr3 == 0 {
        return -1; // cr3 non impostata
    }
    unsafe {
        crate::arc::vmm_user::map_user_region(cr3, virt_addr, phys_addr, count);
    }
    // La PTE puo' gia' esistere (es. cardo rimappa la finestra FS a ogni
    // client): invalida la TLB perche' il processo continua a girare dopo la
    // syscall e non deve riusare la traduzione vecchia.
    for i in 0..count {
        crate::arc::vmm_user::flush_page(virt_addr + (i as u64) * PAGE_SIZE);
    }
    0
}

/// map_mmio(phys, virt, count): mappa `count` pagine MMIO device (L0b,
/// virtio-gpu) a `virt` nello spazio del chiamante. A differenza di
/// `map_physical` (WB, allowlist di frame di sistema) il mapping e' UC
/// (PCD|PWT), non-owned, e puo' toccare solo la finestra `DEV_MMIO_BASE` /
/// `USER_MMIO_VA`. Ristretta all'owner di `Service::Gpu`: il grant e' del
/// driver video, non un'API generale (ADR-0057). Ritorna 0 o -1.
pub(super) fn sys_map_mmio(phys_addr: u64, virt_addr: u64, count: usize) -> i64 {
    const PAGE_SIZE: u64 = 0x1000;
    const MAX_PAGES: usize = 16;

    if phys_addr & (PAGE_SIZE - 1) != 0 || virt_addr & (PAGE_SIZE - 1) != 0 {
        return -1;
    }
    if count == 0 || count > MAX_PAGES {
        return -1;
    }
    let dev_end = syscall_numbers::DEV_MMIO_BASE + syscall_numbers::DEV_MMIO_SIZE;
    let user_end = syscall_numbers::USER_MMIO_VA + syscall_numbers::USER_MMIO_SIZE;
    let dev_len = match (count as u64).checked_mul(PAGE_SIZE) {
        Some(n) => n,
        None => return -1,
    };
    if phys_addr < syscall_numbers::DEV_MMIO_BASE
        || phys_addr.checked_add(dev_len).map_or(true, |e| e > dev_end)
    {
        crate::serial_println!("[syscall] map_mmio: phys {:#x} fuori finestra", phys_addr);
        return -1;
    }
    if virt_addr < syscall_numbers::USER_MMIO_VA
        || virt_addr.checked_add(dev_len).map_or(true, |e| e > user_end)
    {
        return -1;
    }

    // L0b: solo l'owner di Gpu mappa MMIO device. Se gpu non e' registrato
    // (init fallito/prima registrazione) il grant non esiste.
    let me = current_id() as usize;
    if crate::relay::channels::lookup(syscall_numbers::Service::Gpu) != Some(me) {
        crate::serial_println!(
            "[syscall] map_mmio: pid={} non owner di Gpu, rifiutato",
            me
        );
        return -1;
    }

    let cr3 = unsafe { (*(addr_of!(PERCPU))).current_cr3 };
    if cr3 == 0 {
        return -1;
    }
    unsafe {
        crate::arc::vmm_user::map_user_region_uc(cr3, virt_addr, phys_addr, count);
    }
    for i in 0..count {
        crate::arc::vmm_user::flush_page(virt_addr + (i as u64) * PAGE_SIZE);
    }
    0
}

/// sys_sbrk(inc): estende (solo crescita) l'heap del processo corrente di `inc`
/// byte, arrotondati alla pagina. NON mappa nulla: riserva solo VA aggiornando
/// il `heap_brk`. Le pagine sotto il break vengono materializzate lazy dal
/// page-fault handler (demand-zero) al primo accesso.
/// Ritorna il vecchio `heap_brk` (inizio della nuova regione) o -1 se
/// l'estensione non e' possibile (overflow / oltre il tetto soft).
pub(super) fn sys_sbrk(inc: u64) -> i64 {
    const PAGE: u64 = 0x1000;
    let cur = current_mm();
    let old = crate::arc::vmm_user::heap_brk(cur);
    if inc == 0 {
        return old as i64;
    }
    // Arrotonda a pagina (saturando: inc enormi falliscono dopo).
    let n = inc.saturating_add(PAGE - 1) & !(PAGE - 1);
    let new = match old.checked_add(n) {
        Some(v) => v,
        None => return -1,
    };
    if new > crate::arc::vmm_user::USER_HEAP_LIMIT {
        return -1;
    }

    crate::arc::vmm_user::set_heap_brk(cur, new);
    old as i64
}

// ── Ring buffer SPSC per-processo (Fase 10.2) ─────────────────────
//
// Ogni processo alloca COPPIE fresche di pagine ring (request + response)
// via `SYS_RING_ALLOC` (Fase 16: multi-coppia, es. block FS+DISK),
// mappate a `USER_FS_BUFFER` (request) e `USER_RESP_RING` (response).
// Il chiamante registra entrambi gli indirizzi fisici presso cardo con
// una IPC `FS_BUF_REG`. Le operazioni FS sono IPC dirette client→cardo
// con trasferimento dati via ring buffer.

/// ring_alloc(): alloca due pagine ring (request + response) per il processo
/// corrente, le mappa a `USER_FS_BUFFER` e `USER_RESP_RING`, e ritorna gli
/// indirizzi fisici via IpcResult (req_phys in rax, resp_phys in rdi). -1 su OOM o errore.
/// Ogni chiamata da' pagine FRESCHE (Fase 16: un processo puo' allocare piu'
/// coppie, es. block FS+DISK). Il mapping e' NON-owned: il free avviene via
/// record a teardown (`free_ring_pages`), mai double-free col walk owned.
pub(super) fn sys_ring_alloc() -> i64 {
    let cur = current_mm();
    let (req_phys, resp_phys) = match crate::arc::vmm_user::alloc_ring_pages(cur) {
        Some(p) => p,
        None => {
            crate::serial_println!("[syscall] ring_alloc: oom");
            return -1;
        }
    };
    let cr3 = unsafe { (*(addr_of!(PERCPU))).current_cr3 };
    if cr3 == 0 {
        return -1;
    }
    unsafe {
        crate::arc::vmm_user::map_user_region(cr3, crate::arc::vmm_user::USER_FS_BUFFER, req_phys, 1);
        crate::arc::vmm_user::map_user_region(cr3, crate::arc::vmm_user::USER_RESP_RING, resp_phys, 1);
    }
    crate::arc::vmm_user::flush_page(crate::arc::vmm_user::USER_FS_BUFFER);
    crate::arc::vmm_user::flush_page(crate::arc::vmm_user::USER_RESP_RING);
    // Restituiamo entrambi gli indirizzi fisici via IpcResult.
    apply_ipc(crate::ordo::sched::IpcResult { rax: req_phys as i64, rdi: resp_phys, rsi: 0, rdx: 0, r10: 0 })
}

// ── Staging DMA per-processo (Fase 38.1) ─────────────────────────
// Il device Bus-Master attraversa la RAM in hardware: servono frame FISICI
// contigui (il PRD li elenca per phys) e il driver deve conoscerne il phys.
// Come i ring, la syscall mappa e ritorna il phys (neutralita' ADR-0025:
// allocazione frame, mai accesso al disco).

/// dma_alloc(pages): alloca `pages` (1..=DMA_PAGES_MAX) frame contigui
/// azzerati al processo corrente, li mappa RW/NX a `USER_DMA_VA` e ritorna il
/// fisico base in rax (VA fissa e nota, niente da ritornare) e `pages` in
/// rdi. Single-slot: seconda alloc = -1. -1 anche su OOM/range invalido.
pub(super) fn sys_dma_alloc(pages: usize) -> i64 {
    let cur = current_mm();
    let cr3 = unsafe { (*(addr_of!(PERCPU))).current_cr3 };
    if cr3 == 0 {
        return -1; // cr3 non impostata (prima di allocare: mai record orfani)
    }
    let (phys, n) = match crate::arc::vmm_user::alloc_dma_pages(cur, pages) {
        Some(p) => p,
        None => {
            crate::serial_println!("[syscall] dma_alloc: oom/busy/range");
            return -1;
        }
    };
    unsafe {
        crate::arc::vmm_user::map_user_region(cr3, crate::arc::vmm_user::USER_DMA_VA, phys, n);
    }
    for i in 0..n {
        crate::arc::vmm_user::flush_page(crate::arc::vmm_user::USER_DMA_VA + (i as u64) * 4096);
    }
    apply_ipc(crate::ordo::sched::IpcResult { rax: phys as i64, rdi: n as u64, rsi: 0, rdx: 0, r10: 0 })
}

/// map_in(chan, phys, virt, count): mappa `count` pagine fisiche a partire da
/// `phys` all'indirizzo virtuale `virt` nello spazio del PEER del canale
/// `chan` (ADR-0008). Mapper generico cross-process: usato da cardo per
/// iniettare la response ring del client in un driver remoto (hub `vela`, `gpu`).
///
/// Validazione: il peer deve essere un processo user, phys allineata a pagina,
/// virt nello spazio user, count <= 16. Ritorna 0 o -1.
pub(super) fn sys_map_in(chan: usize, phys: u64, virt_addr: u64, count: usize) -> i64 {
    const PAGE_SIZE: u64 = 0x1000;
    const MAX_PAGES: usize = 16;

    if phys & (PAGE_SIZE - 1) != 0 {
        return -1;
    }
    if virt_addr < crate::arc::vmm_user::USER_BASE {
        return -1;
    }
    if count == 0 || count > MAX_PAGES {
        return -1;
    }
    // Fase 35: `map_in` inietta solo ring page (il data-plane FS); mai RAM
    // arbitraria nello spazio di un altro processo.
    for i in 0..count {
        if !crate::arc::vmm_user::is_ring_page(phys + (i as u64) * PAGE_SIZE) {
            crate::serial_println!(
                "[syscall] map_in: frame non-ring {:#x} rifiutato",
                phys + (i as u64) * PAGE_SIZE
            );
            return -1;
        }
    }
    // Il target e' il peer del canale: deve essere un processo user esistente
    // (page table propria). Channel 0 = canale di nascita.
    let me = current_id() as usize;
    let real = if chan == syscall_numbers::CHANNEL_PARENT as usize {
        crate::ordo::sched::parent_channel(me)
    } else {
        Some(chan)
    };
    let target_pid = match real.and_then(|c| crate::relay::channels::peer(c, me)) {
        Some(p) => p,
        None => return -1,
    };
    let cr3 = match crate::ordo::sched::process_cr3(target_pid) {
        Some(cr3) if cr3 != crate::arc::vmm_user::kernel_cr3() => cr3,
        _ => {
            crate::serial_println!("[syscall] map_in: peer {} non e' un processo user", target_pid);
            return -1;
        }
    };
    unsafe {
        crate::arc::vmm_user::map_user_region(cr3, virt_addr, phys, count);
    }
    for i in 0..count {
        crate::arc::vmm_user::flush_page(virt_addr + (i as u64) * PAGE_SIZE);
    }
    0
}
