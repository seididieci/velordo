// Split from vmm_user.rs (byte-identical move; see facade).
use super::layout::{USER_OWNED, MAX_PROCS, PTE_ADDR_MASK};
use super::paging::{kernel_cr3, PTE_PRESENT};
use super::heap_brk::HEAP_BRK;
use super::vma::vma_clear;
use super::rings::free_ring_pages;
use super::dma::free_dma_pages;

// ── Teardown dell'address space (Fase 14, ADR-0010) ────────────────
//
// Quando un processo muore (exit/kill) e viene reclamato, l'address space
// user va distrutto per riusare i frame. Il PML4 di un processo condivide la
// mappa kernel (entry U=0 copiate dal PML4 di boot) e possiede una regione
// user privata (sotto l'indice PML4 di USER_BASE). Il walker:
//   - per ogni entry PML4 PRESENT e DIVERSA da quella del kernel → sottoalbero
//     privato: libera tutte le page-table frames (PDPT/PD/PT) e le PTE foglia
//     marcate `USER_OWNED`;
//   - le PTE foglia NON owned (pagine iniettate: VGA, ring di altri processi,
//     scratch `MAP_TEST_PHYS`) NON vengono liberate: le libera il loro owner.
// Non deve mai girare mentre si usa ancora il `cr3` del processo (solo su
// processi Terminated, mai su `current`).


/// Legge una entry di page table grezza (con i flag).
pub(super) unsafe fn raw_entry(table_phys: u64, idx: usize) -> u64 {
    unsafe { core::ptr::read_volatile((crate::addr::phys_to_virt(table_phys) + (idx as u64) * 8) as *const u64) }
}

/// Libera le PTE foglia sotto una tabella di livello 3 (PT): solo quelle
/// `owned` (le altre restano al proprietario). Dalla Fase 33 via `deref`:
/// un frame condiviso in COW (ref>1) sopravvive al teardown del primo sharer.
unsafe fn free_pt_leaves(pt_phys: u64) {
    for i in 0..512 {
        let e = unsafe { raw_entry(pt_phys, i) };
        if e & PTE_PRESENT != 0 {
            if e & USER_OWNED != 0 {
                crate::arc::phys_mem::deref(PTE_ADDR_MASK & e);
            }
        }
    }
}

/// Libera le tabelle sotto un PD (PD → PT → foglie owned).
unsafe fn free_pd_tree(pd_phys: u64) {
    for i in 0..512 {
        let e = unsafe { raw_entry(pd_phys, i) };
        if e & PTE_PRESENT != 0 {
            let pt = PTE_ADDR_MASK & e;
            unsafe { free_pt_leaves(pt) };
            crate::arc::phys_mem::free(pt);
        }
    }
}

/// Libera le tabelle sotto un PDPT (PDPT → PD → PT → foglie owned).
unsafe fn free_pdp_tree(pdp_phys: u64) {
    for i in 0..512 {
        let e = unsafe { raw_entry(pdp_phys, i) };
        if e & PTE_PRESENT != 0 {
            let pd = PTE_ADDR_MASK & e;
            unsafe { free_pd_tree(pd) };
            crate::arc::phys_mem::free(pd);
        }
    }
}

/// Distrugge l'address space user del processo `pid`: libera i frame delle
/// page table private e le pagine `owned` (codice/stack/heap/ring), e azzera
/// le strutture bookkeeping per-processo (`HEAP_BRK`, `RING_PHYS`) cosi' un
/// eventuale riuso del pid parte pulito.
///
/// # Safety
/// `cr3` deve essere l'address space di un processo Terminated che non verra'
/// piu' schedulato.
pub unsafe fn teardown_user_space(cr3: u64, pid: usize) {    let kernel_pml4 = kernel_cr3();
    unsafe {
        for i in 0..512 {
            let e = raw_entry(cr3, i);
            if e & PTE_PRESENT == 0 {
                continue;
            }
            let tbl = PTE_ADDR_MASK & e;
            // Entry condivise col kernel (mappa U=0): NON sono del processo.
            let k = raw_entry(kernel_pml4, i) & PTE_ADDR_MASK;
            if tbl == k {
                continue;
            }
            free_pdp_tree(tbl);
            crate::arc::phys_mem::free(tbl);
        }
        crate::arc::phys_mem::free(cr3);
    }
    if pid < MAX_PROCS {
        unsafe { *core::ptr::addr_of_mut!(HEAP_BRK[pid]) = 0; }
    }
    // VMA: dimentica i record (i frame owned cadono col walk sopra, che
    // libera tutte le foglie owned private — le pagine mappate lo sono).
    vma_clear(pid);
    // Ring: free via record (le PTE ring sono NON-owned, il walk le salta).
    free_ring_pages(pid);
    // Staging DMA (38.1): stesso pattern dei ring (NON-owned + record).
    free_dma_pages(pid);
}

