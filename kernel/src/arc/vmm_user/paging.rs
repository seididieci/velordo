pub(super) const PTE_PRESENT: u64 = 0x1;

// Split from vmm_user.rs (byte-identical move; see facade).
use core::sync::atomic::{AtomicU64, Ordering};
use super::layout::{USER_BASE, USER_PRESENT_WRITABLE, PAGE_SIZE, USER_STACK_TOP, USER_STACK_FRAMES};

/// Ritorna il CR3 attivo (del processo correntemente in esecuzione).
pub fn active_cr3() -> u64 {
    read_cr3()
}

/// Indirizzo fisico del PML4 corrente (lettura CR3).
static ACTIVE_PML4: AtomicU64 = AtomicU64::new(0);

/// Indici dei 4 livelli per un indirizzo virtuale.
pub(super) fn pml4_index(vaddr: u64) -> usize { ((vaddr >> 39) & 0x1FF) as usize }
pub(super) fn pdpt_index(vaddr: u64) -> usize { ((vaddr >> 30) & 0x1FF) as usize }
pub(super) fn pd_index(vaddr: u64) -> usize { ((vaddr >> 21) & 0x1FF) as usize }
pub(super) fn pt_index(vaddr: u64) -> usize { ((vaddr >> 12) & 0x1FF) as usize }

/// Legge l'attuale CR3 (PML4 del kernel / active).
fn read_cr3() -> u64 {
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
    cr3 & !0xFFF // clear low flags
}

/// Invalida la TLB per la pagina virtuale `vaddr` (necessario dopo aver
/// sovrascritto una PTE gia' presente: es. la finestra FS che cardo rimappa
/// a ogni client). Single-core: nessun shootdown, basta `invlpg` locale.
pub fn flush_page(vaddr: u64) {
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags));
    }
}

/// Durante `vmm::init` il CR3 di boot resta quello di boot (0x90000).
/// Inizializza la CR3 base ("kernel_cr3") usata dai processi kernel.
pub fn init() {
    ACTIVE_PML4.store(read_cr3(), Ordering::Relaxed);
    crate::serial_println!("[vm_user] base cr3 = {:#x}", read_cr3());
}

/// CR3 condivisa del kernel (usata dai processi kernel e come base).
pub fn kernel_cr3() -> u64 {
    ACTIVE_PML4.load(Ordering::Relaxed)
}

/// Legge una entry della page table a un dato indirizzo fisico di livello.
/// `table_phys` e' fisico: l'accesso passa dalla direct map (`addr.rs`, 27.1).
pub(super) unsafe fn entry_at(table_phys: u64, idx: usize) -> u64 {
    let ptr = (crate::addr::phys_to_virt(table_phys) + (idx as u64) * 8) as *const u64;
    unsafe { (*ptr) & !0xFFF }
}

/// Imposta una entry e ritorna l'indirizzo fisico del livello puntato.
pub(super) unsafe fn set_entry(table_phys: u64, idx: usize, value: u64) {
    let ptr = (crate::addr::phys_to_virt(table_phys) + (idx as u64) * 8) as *mut u64;
    // Conserva i flag esistenti se la voce e' gia' presente? No: sovrascrive.
    unsafe { core::ptr::write_volatile(ptr, value); }
}

/// Zera un frame (512 entry) appena allocato.
pub(super) unsafe fn zero_frame(phys: u64) {
    unsafe { core::ptr::write_bytes(crate::addr::phys_to_virt(phys) as *mut u8, 0, 4096); }
}

/// Crea un nuovo address space per un processo user.
///
/// La pagina di `vaddr` e' una pagina USER presente nel `cr3` dato? Walk
/// read-only via direct map (mai fault: legge solo page table). Gestisce
/// pagine grandi (bit PS a PDPT/PD). Richiede il bit U/S sulla foglia: le
/// entry kernel condivise (U=0: direct map, tabelle) non passano mai.
/// S-T: il futex la usa per validare l'indirizzo prima di leggerlo (mai
/// fault supervisor su memoria non-user; il basso canonico e' assente per
/// costruzione, PML4[0] = 0).
pub fn is_mapped_page(cr3: u64, vaddr: u64) -> bool {
    const PRESENT: u64 = 0x1;
    const USER: u64 = 0x4;
    const PS: u64 = 0x80;
    const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
    // Lettura RAW (con flag): `entry_at` maschera i bassi 12 bit e non va
    // bene per i check PRESENT/USER.
    let table = |phys: u64, idx: usize| unsafe {
        *((crate::addr::phys_to_virt(phys) + (idx as u64) * 8) as *const u64)
    };
    let e1 = table(cr3 & ADDR_MASK, pml4_index(vaddr));
    if e1 & PRESENT == 0 {
        return false;
    }
    let e2 = table(e1 & ADDR_MASK, pdpt_index(vaddr));
    if e2 & PRESENT == 0 {
        return false;
    }
    if e2 & PS != 0 {
        return e2 & USER != 0; // pagina 1G
    }
    let e3 = table(e2 & ADDR_MASK, pd_index(vaddr));
    if e3 & PRESENT == 0 {
        return false;
    }
    if e3 & PS != 0 {
        return e3 & USER != 0; // pagina 2M
    }
    let e4 = table(e3 & ADDR_MASK, pt_index(vaddr));
    e4 & PRESENT != 0 && e4 & USER != 0
}

/// Ritorna l'indirizzo fisico del PML4 (da caricare in CR3), oppure `None`
/// se mancano frame. Il PML4 condivide la mappa kernel (U=0) e riserva la
/// regione user in alta con i propri PDPT/PD/PT (U=1).
pub fn new_address_space() -> Option<u64> {
    let kernel_pml4 = kernel_cr3();

    // PML4 del processo: copia dell'active (condivide la mappa kernel).
    let pm = crate::arc::phys_mem::alloc()?;
    unsafe {
        zero_frame(pm);
        core::ptr::copy_nonoverlapping(
            crate::addr::phys_to_virt(kernel_pml4) as *const u64,
            crate::addr::phys_to_virt(pm) as *mut u64,
            512,
        );
    }

    // Regione user: alloco i 3 livelli sotto USER_BASE.
    let pdp = crate::arc::phys_mem::alloc()?;
    let pd = crate::arc::phys_mem::alloc()?;
    let pt = crate::arc::phys_mem::alloc()?;
    unsafe {
        zero_frame(pdp);
        zero_frame(pd);
        zero_frame(pt);
    }

    // PML4[USER_BASE pml4 idx] → pdp (supervisor, ma serve U per l'accesso
    // user alle pagine sotto: impostiamo U sul PDPT e la catena discendente
    // per far passare i permessi user. Qui è una page table: U qui è irrilevante
    // per l'accesso, conta solo su PD/PT. Mettiamo P|W).
    let pm_idx = pml4_index(USER_BASE);
    let pdp_idx = pdpt_index(USER_BASE);
    let pd_idx = pd_index(USER_BASE);
    let pt_idx = pt_index(USER_BASE);

    unsafe {
        set_entry(pm, pm_idx, pdp | USER_PRESENT_WRITABLE);
        set_entry(pdp, pdp_idx, pd | USER_PRESENT_WRITABLE);
        set_entry(pd, pd_idx, pt | USER_PRESENT_WRITABLE);
    }

    // Nota: USER_BASE cade al confine di PDPT/PD/PT con indici 0, quindi i
    // tre frame bastano; se si espandesse oltre 1 GiB servirebbero altri PT.
    let _ = (pdp_idx, pd_idx, pt_idx);

    Some(pm)
}

/// Mappa `count` frame fisici contigui a partire da `phys` all'indirizzo
/// virtuale `vaddr` nello spazio user del processo `cr3`. Le pagine sono
/// user-accessible (U=1), writable, NON eseguibili (NX, 29) e present.
/// **NON** marca il bit `owned`:
/// questo e' il percorso delle pagine "estranee" iniettate nel processo
/// (syscall `map_physical`/`map_in`), che restano di proprieta' di chi le ha
/// allocate.
///
/// # Safety
/// Richiede `cr3` valido e `vaddr` dentro la regione user del processo.
pub unsafe fn map_user_region(cr3: u64, vaddr: u64, phys: u64, count: usize) {
    unsafe { map_user_region_flags(cr3, vaddr, phys, count, super::layout::USER_LEAF_RW) }
}

/// Come `map_user_region`, ma marca le PTE con il bit `owned`: usato per le
/// pagine di proprieta' del processo (codice copiato, stack user, ring della
/// syscall `ring_alloc`, heap demand-zero). Saranno liberate dal teardown
/// dell'address space (Fase 14). RW + NX (29; il codice usa `..._exec`).
///
/// # Safety
/// Richiede `cr3` valido e `vaddr` dentro la regione user del processo.
pub unsafe fn map_user_region_owned(cr3: u64, vaddr: u64, phys: u64, count: usize) {
    unsafe { map_user_region_flags(cr3, vaddr, phys, count, super::layout::USER_LEAF_RW | super::layout::USER_OWNED) }
}

/// Come `map_user_region_owned`, ma read-only (29): per le pagine di VMA
/// PROT_READ materializzate lazy dal fault handler. Scrittura → #PF con
/// protection-violation → kill (mai corruzione silenziosa).
///
/// # Safety
/// Come `map_user_region_owned`.
pub unsafe fn map_user_region_owned_ro(cr3: u64, vaddr: u64, phys: u64, count: usize) {
    unsafe { map_user_region_flags(cr3, vaddr, phys, count, super::layout::USER_LEAF_RO | super::layout::USER_OWNED) }
}

unsafe fn map_user_region_flags(cr3: u64, vaddr: u64, phys: u64, count: usize, flags: u64) {
    let cur = cr3;
    let mut addr = vaddr;

    for _ in 0..count {
        // Livello 1: PML4
        let l1 = unsafe { entry_at(cur, pml4_index(addr)) };
        let pdp = if l1 == 0 {
            let f = crate::arc::phys_mem::alloc().expect("oom page table");
            unsafe { zero_frame(f); }
            unsafe { set_entry(cur, pml4_index(addr), f | USER_PRESENT_WRITABLE); }
            f
        } else { l1 };

        // Livello 2: PDPT
        let l2 = unsafe { entry_at(pdp, pdpt_index(addr)) };
        let pd = if l2 == 0 {
            let f = crate::arc::phys_mem::alloc().expect("oom page table");
            unsafe { zero_frame(f); }
            unsafe { set_entry(pdp, pdpt_index(addr), f | USER_PRESENT_WRITABLE); }
            f
        } else { l2 };

        // Livello 3: PD (pagine 4 KiB, niente large page nello user)
        let l3 = unsafe { entry_at(pd, pd_index(addr)) };
        let pt = if l3 == 0 {
            let f = crate::arc::phys_mem::alloc().expect("oom page table");
            unsafe { zero_frame(f); }
            unsafe { set_entry(pd, pd_index(addr), f | USER_PRESENT_WRITABLE); }
            f
        } else { l3 };

        // Livello 4: PTE → pagina fisica (flag dal chiamante: RW/RO/RX + NX
        // tranne il codice; owned per le pagine di proprieta').
        let pp = phys + ((addr - vaddr) / PAGE_SIZE) * PAGE_SIZE;
        unsafe { set_entry(pt, pt_index(addr), pp | flags); }

        addr += PAGE_SIZE;
    }
}

/// COW fault (Fase 33): se la PTE di `vaddr` in `cr3` e' `present && COW &&
/// !W`, materializza la copia privata (alloca un frame, copia 4 KiB dal
/// vecchio via direct map, rimappa `owned|RW|NX` senza COW, `deref` il vecchio,
/// `invlpg`) e ritorna true. Altrimenti false (OOM, foglia assente, pagina
/// non-COW: il chiamante uccide o halta come prima).
pub fn cow_fault(cr3: u64, vaddr: u64) -> bool {
    use super::layout::{PTE_ADDR_MASK, USER_COW, USER_LEAF_RW, USER_OWNED};
    use super::teardown::raw_entry;
    let page = vaddr & !(PAGE_SIZE - 1);
    // Walk senza allocare livelli (come `flip_write_bit`: i buchi = false).
    let l1 = unsafe { entry_at(cr3, pml4_index(page)) };
    if l1 == 0 {
        return false;
    }
    let l2 = unsafe { entry_at(l1, pdpt_index(page)) };
    if l2 == 0 {
        return false;
    }
    let l3 = unsafe { entry_at(l2, pd_index(page)) };
    if l3 == 0 {
        return false;
    }
    let e = unsafe { raw_entry(l3, pt_index(page)) };
    if e & PTE_PRESENT == 0 || e & USER_COW == 0 || e & 0x2 != 0 {
        return false; // assente, non-COW (codice/rodata → kill) o gia' W
    }
    let old = PTE_ADDR_MASK & e;
    let new = match crate::arc::phys_mem::alloc() {
        Some(f) => f,
        None => return false, // OOM: il chiamante uccide (mai halt per user)
    };
    unsafe {
        core::ptr::copy_nonoverlapping(
            crate::addr::phys_to_virt(old) as *const u8,
            crate::addr::phys_to_virt(new) as *mut u8,
            PAGE_SIZE as usize,
        );
        set_entry(l3, pt_index(page), new | USER_LEAF_RW | USER_OWNED);
    }
    flush_page(page);
    crate::arc::phys_mem::deref(old);
    crate::arc::phys_mem::cow_note();
    true
}

/// Mapping di pagine condivise (30): U=1, NX, NON owned (i frame sono della
/// regione condivisa, liberati a refcount da `shm.rs`), RW o RO.
///
/// # Safety
/// Come `map_user_region`.
pub unsafe fn map_user_region_shared(cr3: u64, vaddr: u64, phys: u64, count: usize, writable: bool) {
    let flags = if writable { super::layout::USER_LEAF_RW } else { super::layout::USER_LEAF_RO };
    unsafe { map_user_region_flags(cr3, vaddr, phys, count, flags) }
}

/// Mapping di pagine condivise in COW (33.4): U=1, NX, owned+COW, read-only.
/// Il primo write fa protection-violation → `cow_fault` materializza la copia
/// privata. I frame restano della regione (ref++ a carico del chiamante).
///
/// # Safety
/// Come `map_user_region`.
pub unsafe fn map_user_region_cow(cr3: u64, vaddr: u64, phys: u64, count: usize) {
    use super::layout::{USER_COW, USER_LEAF_RO, USER_OWNED};
    unsafe { map_user_region_flags(cr3, vaddr, phys, count, USER_LEAF_RO | USER_OWNED | USER_COW) }
}

/// Mappa UNA pagina user con flag espliciti W/X (Fase 31, loader ELF):
/// U=1, owned, NX se non eseguibile. E' il solo percorso che puo' mappare una
/// pagina eseguibile (il codice); tutto il resto e' NX.
///
/// # Safety
/// Come `map_user_region`.
pub unsafe fn map_user_leaf(cr3: u64, vaddr: u64, phys: u64, writable: bool, executable: bool) {
    let mut flags = 0x4 | 0x1 | super::layout::USER_OWNED; // U + P + owned
    if writable {
        flags |= 0x2;
    }
    if !executable {
        flags |= super::layout::PTE_NX;
    }
    unsafe { map_user_region_flags(cr3, vaddr, phys, 1, flags) }
}

/// Mappa UNA pagina condivisa read-only (Fase 32, shared text): U=1, non
/// owned, `RX` se `executable` altrimenti `RO` (mai scrivibile). I frame sono
/// della text image, liberati a refcount da `crate::text`.
///
/// # Safety
/// Come `map_user_region`.
pub unsafe fn map_user_leaf_shared(cr3: u64, vaddr: u64, phys: u64, executable: bool) {
    let mut flags = 0x4 | 0x1; // U + P (non-owned, read-only)
    if !executable {
        flags |= super::layout::PTE_NX;
    }
    unsafe { map_user_region_flags(cr3, vaddr, phys, 1, flags) }
}

/// Re-map dei buchi di una VMA condivisa (33.4): rimappa come condivise solo
/// le pagine la cui PTE e' assente, preservando quelle presenti. Per le VMA
/// COW e' l'unico re-map corretto: un re-map cieco dell'intera regione
/// clobbererebbe le copie private gia' materializzate (owned senza COW) con
/// il contenuto condiviso. Per le shm normali equivale al re-map totale (le
/// PTE presenti puntano gia' ai frame della regione). `writable`/`cow`
/// decidono i flag delle pagine riempite (mai entrambe: COW = RO+COW).
pub fn remap_shared_holes(cr3: u64, vbase: u64, phys: u64, count: usize, writable: bool, cow: bool) {
    use super::layout::{PTE_ADDR_MASK, USER_COW, USER_LEAF_RO, USER_LEAF_RW, USER_OWNED};
    use super::teardown::raw_entry;
    let mut addr = vbase;
    for i in 0..count {
        // Walk con allocazione livelli (come `map_user_region_flags`).
        let l1 = unsafe { entry_at(cr3, pml4_index(addr)) };
        let pdp = if l1 == 0 {
            let f = crate::arc::phys_mem::alloc().expect("oom page table");
            unsafe { zero_frame(f); }
            unsafe { set_entry(cr3, pml4_index(addr), f | USER_PRESENT_WRITABLE); }
            f
        } else { l1 };
        let l2 = unsafe { entry_at(pdp, pdpt_index(addr)) };
        let pd = if l2 == 0 {
            let f = crate::arc::phys_mem::alloc().expect("oom page table");
            unsafe { zero_frame(f); }
            unsafe { set_entry(pdp, pdpt_index(addr), f | USER_PRESENT_WRITABLE); }
            f
        } else { l2 };
        let l3 = unsafe { entry_at(pd, pd_index(addr)) };
        let pt = if l3 == 0 {
            let f = crate::arc::phys_mem::alloc().expect("oom page table");
            unsafe { zero_frame(f); }
            unsafe { set_entry(pd, pd_index(addr), f | USER_PRESENT_WRITABLE); }
            f
        } else { l3 };
        let e = unsafe { raw_entry(pt, pt_index(addr)) };
        if e & PTE_PRESENT == 0 {
            let flags = if cow {
                USER_LEAF_RO | USER_OWNED | USER_COW
            } else if writable {
                USER_LEAF_RW
            } else {
                USER_LEAF_RO
            };
            let _ = PTE_ADDR_MASK;
            unsafe { set_entry(pt, pt_index(addr), phys + (i as u64) * PAGE_SIZE | flags); }
            flush_page(addr);
        }
        addr += PAGE_SIZE;
    }
}

/// True se almeno una PTE presente in `[vaddr, vaddr+count*4K)` ha il bit COW
/// (33.4): usato da `mprotect` per rifiutare il passaggio a RW su VMA con
/// pagine ancora condivise (flippare W renderebbe le scritture condivise,
/// rompendo l'isolamento COW). Walk senza allocare.
pub fn range_has_cow(cr3: u64, vaddr: u64, count: usize) -> bool {
    use super::layout::USER_COW;
    use super::teardown::raw_entry;
    let mut addr = vaddr;
    for _ in 0..count {
        let l1 = unsafe { entry_at(cr3, pml4_index(addr)) };
        if l1 != 0 {
            let l2 = unsafe { entry_at(l1, pdpt_index(addr)) };
            if l2 != 0 {
                let l3 = unsafe { entry_at(l2, pd_index(addr)) };
                if l3 != 0 {
                    let e = unsafe { raw_entry(l3, pt_index(addr)) };
                    if e & PTE_PRESENT != 0 && e & USER_COW != 0 {
                        return true;
                    }
                }
            }
        }
        addr += PAGE_SIZE;
    }
    false
}
/// Mappa UNA pagina con flag foglia espliciti (Fase 34, fork): alloca i
/// livelli mancanti (fallibile: `None` su OOM, mai panic — a differenza dei
/// mapper di spawn che usano `expect`), poi imposta la foglia. `flags` deve
/// includere U+P ed eventuali W/NX/OWNED/COW. Ritorna false su OOM.
pub fn map_leaf_raw(cr3: u64, vaddr: u64, phys: u64, flags: u64) -> bool {
    let page = vaddr & !(PAGE_SIZE - 1);
    let l1 = unsafe { entry_at(cr3, pml4_index(page)) };
    let pdp = if l1 == 0 {
        match crate::arc::phys_mem::alloc() {
            Some(f) => {
                unsafe { zero_frame(f); }
                unsafe { set_entry(cr3, pml4_index(page), f | USER_PRESENT_WRITABLE); }
                f
            }
            None => return false,
        }
    } else { l1 };
    let l2 = unsafe { entry_at(pdp, pdpt_index(page)) };
    let pd = if l2 == 0 {
        match crate::arc::phys_mem::alloc() {
            Some(f) => {
                unsafe { zero_frame(f); }
                unsafe { set_entry(pdp, pdpt_index(page), f | USER_PRESENT_WRITABLE); }
                f
            }
            None => return false,
        }
    } else { l2 };
    let l3 = unsafe { entry_at(pd, pd_index(page)) };
    let pt = if l3 == 0 {
        match crate::arc::phys_mem::alloc() {
            Some(f) => {
                unsafe { zero_frame(f); }
                unsafe { set_entry(pd, pd_index(page), f | USER_PRESENT_WRITABLE); }
                f
            }
            None => return false,
        }
    } else { l3 };
    unsafe { set_entry(pt, pt_index(page), phys | flags); }
    true
}

/// Converte una foglia owned del padre in condivisa COW (Fase 34, fork):
/// azzera W, imposta `USER_COW` (resta owned+NX+U+P), `invlpg`. Ritorna
/// `(phys, child_flags)` — i flag da mappare nel figlio (gia' con COW, senza
/// W) — se la foglia e' `present && owned`, altrimenti `None` (assente o
/// non-owned: il chiamante specchia o salta). Walk senza allocare.
pub fn share_parent_leaf(cr3: u64, vaddr: u64) -> Option<(u64, u64)> {
    use super::layout::{PTE_ADDR_MASK, USER_COW, USER_OWNED};
    use super::teardown::raw_entry;
    let page = vaddr & !(PAGE_SIZE - 1);
    let l1 = unsafe { entry_at(cr3, pml4_index(page)) };
    if l1 == 0 {
        return None;
    }
    let l2 = unsafe { entry_at(l1, pdpt_index(page)) };
    if l2 == 0 {
        return None;
    }
    let l3 = unsafe { entry_at(l2, pd_index(page)) };
    if l3 == 0 {
        return None;
    }
    let e = unsafe { raw_entry(l3, pt_index(page)) };
    if e & PTE_PRESENT == 0 || e & USER_OWNED == 0 {
        return None;
    }
    // Gia' COW (es. shm COW della Fase 33): niente da cambiare.
    let shared = if e & USER_COW != 0 { e } else { (e | USER_COW) & !0x2 };
    if shared != e {
        unsafe { set_entry(l3, pt_index(page), shared); }
        flush_page(page);
    }
    Some((PTE_ADDR_MASK & e, shared & !PTE_ADDR_MASK))
}

/// Legge una foglia (Fase 34, fork): `(phys, flags)` se presente a qualunque
/// livello (owned o no), `None` se un livello manca o la foglia e' assente.
/// Walk senza allocare; i flag includono tutto tranne l'indirizzo.
pub fn read_leaf(cr3: u64, vaddr: u64) -> Option<(u64, u64)> {
    use super::layout::PTE_ADDR_MASK;
    use super::teardown::raw_entry;
    let page = vaddr & !(PAGE_SIZE - 1);
    let l1 = unsafe { entry_at(cr3, pml4_index(page)) };
    if l1 == 0 {
        return None;
    }
    let l2 = unsafe { entry_at(l1, pdpt_index(page)) };
    if l2 == 0 {
        return None;
    }
    let l3 = unsafe { entry_at(l2, pd_index(page)) };
    if l3 == 0 {
        return None;
    }
    let e = unsafe { raw_entry(l3, pt_index(page)) };
    if e & PTE_PRESENT == 0 {
        return None;
    }
    Some((PTE_ADDR_MASK & e, e & !PTE_ADDR_MASK))
}

/// Alloca e mappa lo stack user a `USER_STACK_TOP` (Fase 31: separato dal
/// caricamento del codice, che ora e' `elf::load`). `frames` = profondita'
/// (`USER_STACK_FRAMES` di default, `BIGSTACK_FRAMES` col flag S2.5d).
/// Ritorna il RSP iniziale.
///
/// Convenzione argv (37.1): OGNI processo nasce con argc valido in cima —
/// spawn scrive argc=0 + argv NULL + envp NULL (exec stende il layout completo
/// in `exec_current`). RSP iniziale punta ad argc (`rsp % 16 == 8`, ABI da
/// CALL). Retrocompatibile: i vecchi `_start` ignorano lo stack; i nuovi
/// (`entry!`) vedono argc=0. La pagina guard sotto lo stack
/// (`USER_STACK_GUARD`) resta mai mappata.
///
/// Scrittura via direct map (non via VA user: il CR3 attivo qui e' del
/// creatore, non del nuovo spazio — vedi `create_user`).
///
/// # Safety
/// `cr3` e' un address space creato da `new_address_space`.
pub unsafe fn setup_user_stack(
    cr3: u64,
    frames: usize,
    args: Option<&super::args::ParsedArgs<'_>>,
) -> u64 {
    let stack_base = USER_STACK_TOP - (frames as u64 * PAGE_SIZE);
    let stack_phys = crate::arc::phys_mem::alloc_contiguous(frames)
        .expect("oom per lo stack user");
    unsafe { map_user_region_owned(cr3, stack_base, stack_phys, frames); }
    let top = crate::addr::phys_to_virt(stack_phys) + (frames as u64 * PAGE_SIZE);
    unsafe {
        core::ptr::write((top - 8) as *mut u64, 0); // argc = 0
        core::ptr::write((top - 16) as *mut u64, 0); // argv NULL
        core::ptr::write((top - 24) as *mut u64, 0); // envp NULL
    }
    match args {
        // S2.5e: layout argv via direct map (CR3 del chiamante attivo:
        // scrivere VA user del figlio toccherebbe il chiamante). Le 3 parole
        // qui sopra vengono sovrascritte (come in exec).
        Some(p) => unsafe { super::args::layout_argv(top, USER_STACK_TOP, p) },
        None => USER_STACK_TOP - 24,
    }
}
