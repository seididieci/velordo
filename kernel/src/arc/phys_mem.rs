//! Physical frame allocator — bitmap dinamica, 1 frame = 4 KiB.
//!
//! La bitmap è piazzata subito dopo `_kernel_end` e dimensionata a runtime
//! in base all'indirizzo fisico più alto trovato nella memory map.

use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

pub const FRAME_SIZE: u64 = 4096;

static TOTAL: AtomicU64 = AtomicU64::new(0);
static FREE: AtomicU64 = AtomicU64::new(0);

/// Puntatore alla bitmap e sua dimensione (inizializzati da `init`).
static mut BITMAP_PTR: *mut u8 = core::ptr::null_mut();
static mut BITMAP_BYTES: usize = 0;

/// Refcount per-frame (Fase 33, COW): 1 byte per frame, allocato a boot
/// subito dopo la bitmap (dinamico, stesso schema di `BITMAP_PTR`: evita un
/// `.bss` enorme alle config grandi). `alloc`/`alloc_contiguous` impostano
/// `ref = 1`; `deref` decrementa e libera a 0. `free` resta per i frame a
/// ref 1 (page table, stack kernel, ring, text image, shm non-COW).
static mut REFCOUNT_PTR: *mut u8 = core::ptr::null_mut();

/// Fault COW gestiti (Fase 33): incrementato da `cow_fault`, letto dal test
/// via `SYS_TEXT_STATS` (rdx). Contatore debug, mai su path critici oltre
/// l'incremento atomico.
static COW_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cursore di allocazione (hint): `alloc()` inizia la scansione da qui invece
/// che da frame 1, evitando la riscansione ripetuta dei frame usati in basso.
/// Wrap-around a frame 1 quando raggiunge `total`. Invariante: sempre in
/// [1, total). Aggiornato solo da `alloc()` (non da `free()` per semplicità
/// e sicurezza).
static NEXT_FREE: AtomicU64 = AtomicU64::new(1);

static LOCK: Mutex<()> = Mutex::new(());

// ── Funzioni bitmap ─────────────────────────────────────────────────

fn is_used(frame: usize) -> bool {
    unsafe {
        let byte = *BITMAP_PTR.add(frame / 8);
        byte & (1 << (frame % 8)) != 0
    }
}

fn mark_used(frame: usize) {
    unsafe {
        let p = BITMAP_PTR.add(frame / 8);
        *p |= 1 << (frame % 8);
    }
}

fn mark_free(frame: usize) {
    unsafe {
        let p = BITMAP_PTR.add(frame / 8);
        *p &= !(1 << (frame % 8));
    }
}

fn ref_of(frame: usize) -> u8 {
    unsafe { *REFCOUNT_PTR.add(frame) }
}

fn set_ref(frame: usize, v: u8) {
    unsafe {
        *REFCOUNT_PTR.add(frame) = v;
    }
}

// ── Inizializzazione ───────────────────────────────────────────────

pub fn init(
    memmap: &[crate::boot_info::HvmMemmapEntry],
    kernel_start_virt: u64,
    kernel_end_virt: u64,
) {
    use crate::addr::{kern_virt_to_phys, phys_to_virt};
    // I bound dell'immagine arrivano come VIRT (linker); la contabilita'
    // frame e' in PHYS (27.1: identici; 27.2: scarto KERNEL_OFFSET).
    let kernel_start = kern_virt_to_phys(kernel_start_virt);
    let kernel_end = kern_virt_to_phys(kernel_end_virt);
    // 1. Trova l'indirizzo fisico più alto dalla memory map.
    let max_addr = memmap
        .iter()
        .filter(|e| e.kind == crate::boot_info::MEM_RAM)
        .map(|e| e.addr + e.size)
        .max()
        .unwrap_or(256 * 1024 * 1024);

    let total = (max_addr + FRAME_SIZE - 1) / FRAME_SIZE;
    TOTAL.store(total, Ordering::Relaxed);

    let bitmap_bytes = ((total as usize) + 7) / 8;

    // 2. Piazza la bitmap subito dopo _kernel_end (page-aligned). La bitmap
    // vive in RAM generica: indirizzo PHYS per la contabilita', VIRT
    // (direct map) per accedervi. `bitmap_end` resta VIRT (base heap).
    let bitmap_phys = align_up(kernel_end, FRAME_SIZE);

    // Refcount per-frame (33.1): 1 byte/frame subito dopo la bitmap
    // (page-aligned). Zero = mai allocato / riservato (i riservati non passano
    // mai da `deref`: il ref e' significativo solo per i frame allocati).
    let refcount_phys = align_up(bitmap_phys + bitmap_bytes as u64, FRAME_SIZE);
    let refcount_bytes = total as usize;

    unsafe {
        BITMAP_PTR = phys_to_virt(bitmap_phys) as *mut u8;
        BITMAP_BYTES = bitmap_bytes;

        // 3. Fill 0xFF = tutti usati.
        let slice = core::slice::from_raw_parts_mut(BITMAP_PTR, BITMAP_BYTES);
        slice.fill(0xFF);

        REFCOUNT_PTR = phys_to_virt(refcount_phys) as *mut u8;
        let refs = core::slice::from_raw_parts_mut(REFCOUNT_PTR, refcount_bytes);
        refs.fill(0);
    }

    // 4. Libera le regioni MEM_RAM.
    for entry in memmap {
        if entry.kind == crate::boot_info::MEM_RAM {
            let start_frame = (entry.addr / FRAME_SIZE) as usize;
            let end_frame = ((entry.addr + entry.size) / FRAME_SIZE) as usize;
            let end = (end_frame as u64).min(total) as usize;
            for f in start_frame..end {
                mark_free(f);
                FREE.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // 5. Ri-marca usati: kernel, page tables, PD, bitmap.
    let ks = (kernel_start / FRAME_SIZE) as usize;
    let ke = (align_up(kernel_end, FRAME_SIZE) / FRAME_SIZE) as usize;
    for f in ks..ke {
        if !is_used(f) {
            mark_used(f);
            FREE.fetch_sub(1, Ordering::Relaxed);
        }
    }

    // Tabelle base di boot (PML4 + PDPT/PD/PT low + PDPT_K/PD_K +
    // PDPT_DIRECT = 7 pagine) in 0x90000–0x97000; la riserva copre fino a
    // 0x100000 (buco PCI/VGA oltre 0x9FC00: mai RAM, mai allocato comunque).
    // Le PD direct + PT_VGA sono a LMA 16M (`.tables_high`, oltre heap+bitmap
    // in ogni config): verificate RAM qui sotto, poi riservate.
    // LMA fisse per CR3; contabilita' sempre in PHYS.
    let pt_start = 0x90000 / FRAME_SIZE as usize;  // frame 36
    let pt_end = 0x100000 / FRAME_SIZE as usize;    // frame 256
    for f in pt_start..pt_end {
        if !is_used(f) {
            mark_used(f);
            FREE.fetch_sub(1, Ordering::Relaxed);
        }
    }

    // PD direct + PT VGA a LMA 16M (`.tables_high`, 33 pagine = 132 KiB):
    // devono cadere in RAM vera (il buco PCI/VGA sotto 1M ha insegnato).
    // Fail-loud se la memmap non le contiene.
    use crate::boot_tables::{TABLES_HIGH_END, TABLES_HIGH_PAGES, TABLES_HIGH_START};
    // Invariante anti-collisione (27.3): la scratch dei test non deve MAI
    // sovrapporsi alle tabelle (t12 ci scriveva pattern sopra le PD direct).
    // Verificata dal compilatore: chi sposta una delle due e rompe l'altra
    // non compila.
    const _: () = assert!(
        syscall_numbers::MAP_TEST_PHYS + syscall_numbers::MAP_TEST_FRAMES * 4096 <= TABLES_HIGH_START
            || syscall_numbers::MAP_TEST_PHYS >= TABLES_HIGH_END,
        "MAP_TEST_PHYS si sovrappone alle tabelle .tables_high"
    );
    {
        let mut ok = false;
        for entry in memmap {
            if entry.kind == crate::boot_info::MEM_RAM
                && entry.addr <= TABLES_HIGH_START
                && entry.addr + entry.size >= TABLES_HIGH_END
            {
                ok = true;
                break;
            }
        }
        if !ok {
            crate::serial_println!("[pmm] TABELLE 16M fuori RAM: direct map inutilizzabile");
            loop {
                unsafe { core::arch::asm!("hlt") };
            }
        }
        reserve(TABLES_HIGH_START, TABLES_HIGH_PAGES * 4096);
    }

    // Bitmap stessa (contabilita' in PHYS)
    let bfn = (bitmap_phys / FRAME_SIZE) as usize;
    let bfe = (align_up(bitmap_phys + bitmap_bytes as u64, FRAME_SIZE) / FRAME_SIZE) as usize;
    for f in bfn..bfe {
        if !is_used(f) {
            mark_used(f);
            FREE.fetch_sub(1, Ordering::Relaxed);
        }
    }

    // Array refcount (33.1): riservato come la bitmap, mai allocato ai processi.
    let rfn = (refcount_phys / FRAME_SIZE) as usize;
    let rfe = (align_up(refcount_phys + refcount_bytes as u64, FRAME_SIZE) / FRAME_SIZE) as usize;
    for f in rfn..rfe {
        if !is_used(f) {
            mark_used(f);
            FREE.fetch_sub(1, Ordering::Relaxed);
        }
    }

    // VGA buffer (0xB8000): frame 186
    if !is_used(186) {
        mark_used(186);
        FREE.fetch_sub(1, Ordering::Relaxed);
    }

    crate::serial_println!(
        "[pmm] {} frame totali, {} liberi ({} MiB)",
        total,
        FREE.load(Ordering::Relaxed),
        FREE.load(Ordering::Relaxed) * 4 / 1024
    );
}

/// Ritorna l'indirizzo di fine bitmap (page-aligned), utile per posizionare l'heap.
/// Dalla Fase 33 include l'array refcount (sta subito dopo la bitmap).
pub fn bitmap_end() -> u64 {
    let start = unsafe { REFCOUNT_PTR as u64 };
    let total = TOTAL.load(Ordering::Relaxed);
    align_up(start + total, FRAME_SIZE)
}

/// Marca come USATI i frame nell'intervallo fisico [start, start+len): il
/// frame allocator non li restituira' mai piu'. Necessario per regioni carvate
/// fuori dal bitmap (es. il kernel heap), altrimenti finirebbero in mano ai
/// processi e verrebbero sovrascritti.
pub fn reserve(start: u64, len: u64) {
    let mut f0 = (start / FRAME_SIZE) as usize;
    let f1 = ((align_up(start + len, FRAME_SIZE)) / FRAME_SIZE) as usize;
    while f0 < f1 {
        if !is_used(f0) {
            mark_used(f0);
            FREE.fetch_sub(1, Ordering::Relaxed);
        }
        f0 += 1;
    }
}

// ── Allocazione ────────────────────────────────────────────────────

pub fn alloc() -> Option<u64> {
    let _lock = LOCK.lock();
    let total = TOTAL.load(Ordering::Relaxed) as usize;

    if total <= 1 {
        return None;
    }

    // Inizia dal cursore con wrap-around: evita di riscansionare da frame 1
    // i frame usati in basso a ogni allocazione (lezione: O(n²) su RAM grande).
    let mut start = NEXT_FREE.load(Ordering::Relaxed) as usize;
    if start < 1 || start >= total {
        start = 1;
    }

    // Scansione completa del bitmap: da `start` a `total`, poi da 1 a `start`.
    let mut i = start;
    let mut scanned = 0usize;
    while scanned < total - 1 {
        if !is_used(i) {
            mark_used(i);
            set_ref(i, 1);
            FREE.fetch_sub(1, Ordering::Relaxed);
            // Avanza il cursore oltre il frame appena allocato.
            let mut next = i + 1;
            if next >= total {
                next = 1;
            }
            NEXT_FREE.store(next as u64, Ordering::Relaxed);
            return Some((i as u64) * FRAME_SIZE);
        }
        i += 1;
        if i >= total {
            i = 1;
        }
        scanned += 1;
    }
    None
}

/// Alloca `n` frame fisici CONTIGUI. Ritorna l'indirizzo del primo frame,
/// oppure `None` se non c'e' un blocco contiguo di `n` frame liberi.
pub fn alloc_contiguous(n: usize) -> Option<u64> {
    if n == 0 {
        return None;
    }
    let _lock = LOCK.lock();
    let total = TOTAL.load(Ordering::Relaxed) as usize;

    let mut i = 0usize;
    while i <= total - n {
        if !is_used(i) {
            let mut ok = true;
            for j in 1..n {
                if is_used(i + j) {
                    ok = false;
                    i += j; // salta il blocco occupato trovato
                    break;
                }
            }
            if ok {
                for j in 0..n {
                    mark_used(i + j);
                    set_ref(i + j, 1);
                }
                FREE.fetch_sub(n as u64, Ordering::Relaxed);
                return Some((i as u64) * FRAME_SIZE);
            }
        } else {
            i += 1;
        }
    }
    None
}

pub fn free(frame: u64) {
    let _lock = LOCK.lock();
    let i = (frame / FRAME_SIZE) as usize;
    assert!((i as u64) < TOTAL.load(Ordering::Relaxed), "frame fuori range: {:#x}", frame);
    assert!(is_used(i), "frame già libero: {:#x}", frame);
    assert!(ref_of(i) == 1, "free su frame condiviso (ref {}): {:#x}, usare deref", ref_of(i), frame);
    set_ref(i, 0);
    mark_free(i);
    FREE.fetch_add(1, Ordering::Relaxed);
}

/// Libera `count` frame CONTIGUI a partire da `start` (Fase 14, teardown dei
/// processi: stack kernel, binario copiato, etc.). Il blocco deve essere stato
/// allocato contiguo (`alloc_contiguous`).
pub fn free_contiguous(start: u64, count: usize) {
    let _lock = LOCK.lock();
    let f0 = (start / FRAME_SIZE) as usize;
    for i in 0..count {
        let idx = f0 + i;
        assert!((idx as u64) < TOTAL.load(Ordering::Relaxed), "frame fuori range: {:#x}", start);
        assert!(is_used(idx), "frame già libero: {:#x}", (idx as u64) * FRAME_SIZE);
        assert!(ref_of(idx) == 1, "free su frame condiviso (ref {})", ref_of(idx));
        set_ref(idx, 0);
        mark_free(idx);
    }
    FREE.fetch_add(count as u64, Ordering::Relaxed);
}

/// Decrementa il refcount del frame; a 0 lo libera (Fase 33, COW). Per i
/// frame a ref 1 equivale a `free`. Panic su frame libero o fuori range
/// (stessa severita' di `free`: un refcount sbilanciato e' un bug del kernel).
pub fn deref(frame: u64) {
    let _lock = LOCK.lock();
    let i = (frame / FRAME_SIZE) as usize;
    assert!((i as u64) < TOTAL.load(Ordering::Relaxed), "frame fuori range: {:#x}", frame);
    assert!(is_used(i), "deref su frame libero: {:#x}", frame);
    let r = ref_of(i);
    assert!(r >= 1, "deref su frame con ref 0: {:#x}", frame);
    if r == 1 {
        set_ref(i, 0);
        mark_free(i);
        FREE.fetch_add(1, Ordering::Relaxed);
    } else {
        set_ref(i, r - 1);
    }
}

/// Come `deref` su `count` frame contigui (regioni shm COW, text image COW).
pub fn deref_contiguous(start: u64, count: usize) {
    let _lock = LOCK.lock();
    let f0 = (start / FRAME_SIZE) as usize;
    for i in 0..count {
        let idx = f0 + i;
        assert!((idx as u64) < TOTAL.load(Ordering::Relaxed), "frame fuori range: {:#x}", start);
        assert!(is_used(idx), "deref su frame libero: {:#x}", (idx as u64) * FRAME_SIZE);
        let r = ref_of(idx);
        assert!(r >= 1, "deref su frame con ref 0");
        if r == 1 {
            set_ref(idx, 0);
            mark_free(idx);
            FREE.fetch_add(1, Ordering::Relaxed);
        } else {
            set_ref(idx, r - 1);
        }
    }
}

/// Incrementa il refcount del frame (condivisione COW): false se saturo
/// (255, irraggiungibile con 32 processi ma mai wrappare in silenzio).
pub fn ref_inc(frame: u64) -> bool {
    let _lock = LOCK.lock();
    let i = (frame / FRAME_SIZE) as usize;
    assert!((i as u64) < TOTAL.load(Ordering::Relaxed), "frame fuori range: {:#x}", frame);
    assert!(is_used(i), "ref_inc su frame libero: {:#x}", frame);
    let r = ref_of(i);
    if r == 255 {
        return false;
    }
    set_ref(i, r + 1);
    true
}

/// True se il frame e' condivisibile ancora una volta (usato e ref < 255).
/// Pre-check read-only per il two-phase di `sys_shm_map` COW: fallire prima
/// di registrare VMA/mappare evita qualunque rollback.
pub fn ref_available(frame: u64) -> bool {
    let _lock = LOCK.lock();
    let i = (frame / FRAME_SIZE) as usize;
    if (i as u64) >= TOTAL.load(Ordering::Relaxed) || !is_used(i) {
        return false;
    }
    ref_of(i) < 255
}

/// Fault COW gestiti (Fase 33): incrementato da `cow_fault`, esposto via
/// `SYS_TEXT_STATS` (rdx) per il test t48.
pub fn cow_note() {
    COW_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Fault COW gestiti finora.
pub fn cow_count() -> u64 {
    COW_COUNT.load(Ordering::Relaxed)
}

pub fn free_frames() -> u64 {
    FREE.load(Ordering::Relaxed)
}

/// Frame totali censiti (Fase 52, sensore `SYS_MEMINFO`).
pub fn total_frames() -> u64 {
    TOTAL.load(Ordering::Relaxed)
}

/// Usato solo dai selftest (`#[cfg(feature = "selftest")]` in main.rs).
#[allow(dead_code)]
pub fn used_frames() -> u64 {
    TOTAL.load(Ordering::Relaxed) - FREE.load(Ordering::Relaxed)
}

fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}
