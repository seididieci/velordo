// Split from vmm_user.rs (byte-identical move; see facade).
use super::layout::{PAGE_SIZE, USER_CODE, MMAP_BASE, MMAP_END, USER_OWNED, MAX_PROCS, PTE_ADDR_MASK};
use super::paging::{entry_at, set_entry, flush_page, pml4_index, pdpt_index, pd_index, pt_index, PTE_PRESENT};
use super::teardown::raw_entry;
use super::heap_brk::heap_brk;
use super::shm;

/// VMA massime per processo (record statici, mai heap: anche il fault
/// handler fa lookup qui, stesso stile di `HEAP_BRK`/`RING_PHYS`). S-T: 64
/// (16 non bastano ai programmi multithread: N stack + TLS + heap VMA —
/// 32 thread ne vogliono gia' 32 solo di stack).
const VMA_MAX: usize = 64;
/// Record VMA per pid: (base, len, prot, shm) a pagine; len == 0 = libero.
/// `prot` = PROT_* di `syscall-numbers` (29: NONE/R/RW; W solo rifiutato a
/// `mmap`). `shm` = 0 per anonima, altrimenti id+1 di `SHM_TABLE` (30: la VMA
/// referenzia una regione condivisa, le pagine sono pre-materializzate al
/// `shm_map` e NON owned — il free e' a refcount). Le pagine anonime vengono
/// materializzate lazy al fault con i flag del prot (RW → RW, RO → RO, NONE →
/// mai: fault = kill); owned → il teardown esistente le libera. Le VMA COW
/// (33.4, `MAP_COW`: `shm != 0`, `prot` = RO) hanno le pagine pre-materializzate
/// come owned+COW read-only: il primo write fa `cow_fault` (copia privata),
/// `munmap`/teardown rilasciano via `deref`, `mprotect` a RW con pagine ancora
/// condivise e' rifiutato.
static mut VMA_TABLE: [(u64, u64, u8, u8); MAX_PROCS * VMA_MAX] =
    [(0, 0, 0, 0); MAX_PROCS * VMA_MAX];
/// VMA del processo `pid` che contiene `addr`, se esiste.
pub fn vma_lookup(pid: usize, addr: u64) -> Option<(u64, u64, u8, u8)> {
    if pid >= MAX_PROCS {
        return None;
    }
    let base = pid * VMA_MAX;
    for i in 0..VMA_MAX {
        let (b, l, p, s) = unsafe { *core::ptr::addr_of!(VMA_TABLE[base + i]) };
        if l != 0 && b <= addr && addr < b + l {
            return Some((b, l, p, s));
        }
    }
    None
}

/// True se `[addr, addr+len)` sta interamente in UNA VMA viva del processo
/// (usato da `is_user_range`: i buffer syscall non scavallano VMA).
fn vma_contains_range(pid: usize, addr: u64, len: u64) -> bool {
    if pid >= MAX_PROCS || len == 0 {
        return false;
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    let base = pid * VMA_MAX;
    for i in 0..VMA_MAX {
        let (b, l, _, _) = unsafe { *core::ptr::addr_of!(VMA_TABLE[base + i]) };
        if l != 0 && b <= addr && end <= b + l {
            return true;
        }
    }
    false
}

/// Fine (esclusiva) della prima VMA che si sovrappone a `[s, s+len)`, o
/// `None` se liberi. Le VMA registrate non si sovrappongono mai (invariante
/// di `vma_map`), quindi basta la prima trovata.
fn vma_overlap_end(pid: usize, s: u64, len: u64) -> Option<u64> {
    let e = s.checked_add(len)?;
    let base = pid * VMA_MAX;
    for i in 0..VMA_MAX {
        let (b, l, _, _) = unsafe { *core::ptr::addr_of!(VMA_TABLE[base + i]) };
        if l != 0 && b < e && s < b + l {
            return Some(b + l);
        }
    }
    None
}

/// Registra una VMA (senza materializzare: lazy come `sbrk` per le anonime;
/// le condivise sono pre-materializzate dal chiamante `shm_map`).
/// `hint == 0 && !fixed` = scelta kernel (first-fit dal basso);
/// altrimenti `hint` deve essere libero (o fallisce, mai fallback).
/// `prot` = PROT_NONE/READ/(READ|WRITE) (29; W solo rifiutato dal chiamante).
/// `shm` = 0 anonima, altrimenti id+1 della regione condivisa (30).
/// Ritorna la base o `None`.
pub fn vma_map(pid: usize, hint: u64, len: u64, fixed: bool, prot: u8, shm: u8) -> Option<u64> {
    if pid >= MAX_PROCS || len == 0 {
        return None;
    }
    let len = len.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    if len == 0 {
        return None; // overflow nell'arrotondamento
    }
    if hint & (PAGE_SIZE - 1) != 0 {
        return None; // hint non allineato
    }
    let base = if hint == 0 && !fixed {
        // First-fit: ogni giro salta una VMA → al piu' VMA_MAX+1 giri.
        let mut cand = MMAP_BASE;
        let mut found = false;
        for _ in 0..=VMA_MAX {
            if cand.checked_add(len).is_none_or(|e| e > MMAP_END) {
                return None;
            }
            match vma_overlap_end(pid, cand, len) {
                None => {
                    found = true;
                    break;
                }
                Some(end) => cand = end,
            }
        }
        if !found {
            return None;
        }
        cand
    } else {
        if hint < MMAP_BASE {
            return None;
        }
        let end = hint.checked_add(len)?;
        if end > MMAP_END {
            return None;
        }
        if vma_overlap_end(pid, hint, len).is_some() {
            return None;
        }
        hint
    };
    // Slot libero (se la tabella e' piena si fallisce: niente merge in 28).
    let tbase = pid * VMA_MAX;
    for i in 0..VMA_MAX {
        let e = unsafe { *core::ptr::addr_of!(VMA_TABLE[tbase + i]) };
        if e.1 == 0 {
            unsafe { *core::ptr::addr_of_mut!(VMA_TABLE[tbase + i]) = (base, len, prot, shm); }
            return Some(base);
        }
    }
    None
}

/// Smappa `[addr, addr+len)`: solo VMA INTERE in 28 (copertura esatta,
/// parziali = false senza cambiare stato). Two-phase: prima valida tutto,
/// poi smappa (PTE + frame owned + flush) e cancella i record.
pub fn vma_unmap(pid: usize, cr3: u64, addr: u64, len: u64) -> bool {
    if pid >= MAX_PROCS || len == 0 || addr & (PAGE_SIZE - 1) != 0 || len & (PAGE_SIZE - 1) != 0 {
        return false;
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    let (idxs, n) = match exact_cover(pid, addr, end) {
        Some(x) => x,
        None => return false,
    };
    // Fase 2: smappa + libera + cancella record (`len` e' multipla di pagina).
    // `unmap_user_range` libera solo le foglie owned: quelle condivise (non
    // owned) vengono staccate ma non liberate — il free e' a refcount.
    unsafe { unmap_user_range(cr3, addr, (len / PAGE_SIZE) as usize); }
    for k in 0..n {
        let (_, _, _, s) = unsafe { *core::ptr::addr_of!(VMA_TABLE[pid * VMA_MAX + idxs[k]]) };
        if s != 0 {
            shm::shm_release(s as u32); // ultimo riferimento → libera i frame
        }
        unsafe { *core::ptr::addr_of_mut!(VMA_TABLE[pid * VMA_MAX + idxs[k]]) = (0, 0, 0, 0); }
    }
    true
}

/// Raccoglie gli slot VMA dentro `[addr, end)` verificando copertura esatta:
/// VMA ordinate per base (insertion sort su <= 16), prima a `addr`, ultima a
/// `end`, contigue (mai overlap per invariante). `None` se buchi/disallineato.
fn exact_cover(pid: usize, addr: u64, end: u64) -> Option<([usize; VMA_MAX], usize)> {
    let tbase = pid * VMA_MAX;
    let mut idxs = [0usize; VMA_MAX];
    let mut n = 0usize;
    for i in 0..VMA_MAX {
        let (b, l, _, _) = unsafe { *core::ptr::addr_of!(VMA_TABLE[tbase + i]) };
        if l != 0 && b >= addr && b + l <= end {
            idxs[n] = i;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    for a in 1..n {
        let mut j = a;
        while j > 0 {
            let (ba, _, _, _) = unsafe { *core::ptr::addr_of!(VMA_TABLE[tbase + idxs[j]]) };
            let (bb, _, _, _) = unsafe { *core::ptr::addr_of!(VMA_TABLE[tbase + idxs[j - 1]]) };
            if bb <= ba {
                break;
            }
            let t = idxs[j];
            idxs[j] = idxs[j - 1];
            idxs[j - 1] = t;
            j -= 1;
        }
    }
    let (first_b, first_l, _, _) = unsafe { *core::ptr::addr_of!(VMA_TABLE[tbase + idxs[0]]) };
    if first_b != addr {
        return None;
    }
    let mut covered = first_b + first_l;
    for k in 1..n {
        let (b, l, _, _) = unsafe { *core::ptr::addr_of!(VMA_TABLE[tbase + idxs[k]]) };
        if b != covered {
            return None;
        }
        covered += l;
    }
    if covered != end {
        return None;
    }
    Some((idxs, n))
}

/// Cambia il prot di `[addr, addr+len)` (mprotect, 29): solo VMA INTERE con
/// copertura esatta (come `munmap`; parziali = false senza cambiare stato).
/// Per ogni VMA aggiorna il record e le PTE presenti: prot NONE smappa+libera
/// (come `munmap`: il riuso rimaterializza zero al fault), altrimenti flippa
/// il bit W in place (i frame restano). NX non cambia mai in 29 (il codice e'
/// l'unico eseguibile, niente PROT_EXEC). Flush per pagina toccata.
/// `prot` gia' validato dal chiamante (NONE/R/RW).
pub fn vma_protect(pid: usize, cr3: u64, addr: u64, len: u64, prot: u8) -> bool {
    use syscall_numbers::{PROT_NONE, PROT_WRITE};
    if pid >= MAX_PROCS || len == 0 || addr & (PAGE_SIZE - 1) != 0 || len & (PAGE_SIZE - 1) != 0 {
        return false;
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    let (idxs, n) = match exact_cover(pid, addr, end) {
        Some(x) => x,
        None => return false,
    };
    // PROT_NONE su una VMA condivisa = drop della mappatura (dovrebbe
    // decrementare il refcount): non supportato, rifiutato senza stato (30).
    if prot == PROT_NONE as u8 {
        for k in 0..n {
            let (_, _, _, s) = unsafe { *core::ptr::addr_of!(VMA_TABLE[pid * VMA_MAX + idxs[k]]) };
            if s != 0 {
                return false;
            }
        }
    }
    // 33.4: passaggio a RW su pagine ancora condivise in COW = scritture
    // condivise (rottura dell'isolamento: il W bypasserebbe `cow_fault`).
    // Rifiutato senza stato; su VMA COW interamente privatizzata (nessun bit
    // COW rimasto) il flip e' sicuro e procede.
    if prot & PROT_WRITE as u8 != 0
        && super::paging::range_has_cow(cr3, addr, (len / PAGE_SIZE) as usize)
    {
        return false;
    }
    // Fase 2: applica (record + PTE).
    for k in 0..n {
        let (b, l, _, s) = unsafe { *core::ptr::addr_of!(VMA_TABLE[pid * VMA_MAX + idxs[k]]) };
        if prot == PROT_NONE as u8 {
            unsafe { unmap_user_range(cr3, b, (l / PAGE_SIZE) as usize); }
        } else {
            let writable = prot & PROT_WRITE as u8 != 0;
            unsafe { flip_write_bit(cr3, b, (l / PAGE_SIZE) as usize, writable); }
        }
        unsafe { *core::ptr::addr_of_mut!(VMA_TABLE[pid * VMA_MAX + idxs[k]]) = (b, l, prot, s); }
    }
    true
}

/// Flippa il bit W delle PTE presenti in `[vaddr, vaddr+count*4K)` (walk senza
/// allocare livelli: le pagine non materializzate non hanno PTE e il record
/// governa la materializzazione futura). Flush per pagina cambiata.
unsafe fn flip_write_bit(cr3: u64, vaddr: u64, count: usize, writable: bool) {
    let mut addr = vaddr;
    for _ in 0..count {
        let l1 = unsafe { entry_at(cr3, pml4_index(addr)) };
        if l1 != 0 {
            let l2 = unsafe { entry_at(l1, pdpt_index(addr)) };
            if l2 != 0 {
                let l3 = unsafe { entry_at(l2, pd_index(addr)) };
                if l3 != 0 {
                    let e = unsafe { raw_entry(l3, pt_index(addr)) };
                    if e & PTE_PRESENT != 0 {
                        let e2 = if writable { e | 0x2 } else { e & !0x2 };
                        if e2 != e {
                            unsafe { set_entry(l3, pt_index(addr), e2); }
                            flush_page(addr);
                        }
                    }
                }
            }
        }
        addr += PAGE_SIZE;
    }
}

/// Smappa PTE presenti in `[vaddr, vaddr+count*4K)` senza allocare livelli
/// (tollerante ai buchi: le VMA registrate li hanno, ma mai assumere).
/// Le foglie owned (pagine materializzate, incluse le condivise COW della Fase
/// 33) vengono rilasciate via `deref` (il frame condiviso sopravvive); le
/// altre solo staccate. Flush per pagina.
pub(crate) unsafe fn unmap_user_range(cr3: u64, vaddr: u64, count: usize) {
    let mut addr = vaddr;
    for _ in 0..count {
        let l1 = unsafe { entry_at(cr3, pml4_index(addr)) };
        if l1 != 0 {
            let l2 = unsafe { entry_at(l1, pdpt_index(addr)) };
            if l2 != 0 {
                let l3 = unsafe { entry_at(l2, pd_index(addr)) };
                if l3 != 0 {
                    let e = unsafe { raw_entry(l3, pt_index(addr)) };
                    if e & PTE_PRESENT != 0 {
                        if e & USER_OWNED != 0 {
                            crate::arc::phys_mem::deref(PTE_ADDR_MASK & e);
                        }
                        unsafe { set_entry(l3, pt_index(addr), 0); }
                        flush_page(addr);
                    }
                }
            }
        }
        addr += PAGE_SIZE;
    }
}

/// Dimentica le VMA del processo (teardown: i frame owned cadono col walk
/// esistente; qui i record, come `HEAP_BRK`). Per le VMA condivise (30)
/// rilascia il riferimento: l'ultimo libera i frame della regione.
pub(crate) fn vma_clear(pid: usize) {
    if pid >= MAX_PROCS {
        return;
    }
    let base = pid * VMA_MAX;
    for i in 0..VMA_MAX {
        let (_, l, _, s) = unsafe { *core::ptr::addr_of!(VMA_TABLE[base + i]) };
        if l != 0 && s != 0 {
            shm::shm_release(s as u32);
        }
        unsafe { *core::ptr::addr_of_mut!(VMA_TABLE[base + i]) = (0, 0, 0, 0); }
    }
}

/// Clona i record VMA da `src` a `dst` (Fase 34, fork): copia i 16 record e
/// incrementa il refcount di ogni regione condivisa referenziata (`shm_ref`,
/// una volta per regione distinta — i frame restano referenziati finche' un
/// sharer vive). Chiamato DOPO il walk delle pagine (che non dipende dai
/// record) e dopo tutti i passi fallibili: infallibile, nessun unwind.
pub fn vma_clone(src: usize, dst: usize) {
    if src >= MAX_PROCS || dst >= MAX_PROCS {
        return;
    }
    let sbase = src * VMA_MAX;
    let dbase = dst * VMA_MAX;
    let mut seen = [0u32; 16];
    let mut n = 0usize;
    for i in 0..VMA_MAX {
        let e = unsafe { *core::ptr::addr_of!(VMA_TABLE[sbase + i]) };
        unsafe { *core::ptr::addr_of_mut!(VMA_TABLE[dbase + i]) = e; }
        if e.1 != 0 && e.3 != 0 {
            let id = e.3 as u32;
            let mut dup = false;
            for k in 0..n {
                if seen[k] == id {
                    dup = true;
                    break;
                }
            }
            if !dup {
                if n < seen.len() {
                    seen[n] = id;
                    n += 1;
                }
                shm::shm_ref(id);
            }
        }
    }
}

/// Verifica che l'intervallo [addr, addr+len) sia interamente nello spazio
/// user legalmente accessibile dal kernel: dal codice (USER_CODE) fino al
/// `heap_brk` corrente del processo (lo heap committato via `sbrk`; le pagine
/// sotto il break vengono materializzate lazy dal page-fault handler anche per
/// fault supervisor). Gli indirizzi oltre il break sono rifiutati.
pub fn is_user_range(addr: u64, len: usize) -> bool {
    let (start, end) = (addr, addr.checked_add(len as u64));
    match end {
        Some(end) => {
            // S-T: VMA e heap sono indicizzati al leader del gruppo (mm
            // condiviso) — col tid del chiamante i thread non troverebbero
            // mai i propri range (heap_brk=base + VMA vuote = ogni syscall
            // con buffer rifiutata; osservato: thread rayon dello staticone
            // con stack mmap, write sempre -1 e abort 134).
            let pid = crate::syscall::current_mm();
            let brk = heap_brk(pid);
            // Heap committato oppure UNA vma viva (i buffer syscall possono
            // stare in memoria mappata: la validazione resta centrale qui).
            // NB: a `vma_contains_range` va `len`, non `end` (bug 27.3: con
            // `end` la somma raddoppiava e ogni VMA veniva rifiutata).
            (start >= USER_CODE && end <= brk) || vma_contains_range(pid, start, len as u64)
        }
        None => false,
    }
}
