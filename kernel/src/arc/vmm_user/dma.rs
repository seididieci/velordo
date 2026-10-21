// Split from vmm_user.rs (new module; see facade).
/// Memoria DMA per processo: N slot di frame fisici CONTIGUI (il device li
/// attraversa in hardware), via `SYS_DMA_ALLOC`.
///
/// Slot 0 = **staging** (Fase 38.1, ATA DMA: PRD + dati): single-slot per
/// processo, <= `DMA_PAGES_MAX` pagine, VA `USER_DMA_VA` (invariato).
/// Slot >= 1 = **device buffer** (D3: framebuffer della console): bound
/// `DMA_DEVICE_PAGES_MAX`, **grant riservato all'owner di
/// `Service::Console`** (come il grant MMIO di ADR-0057: il buffer e' del
/// driver video, non un'API generale — ADR-0026).
///
/// Disegno come i ring (`rings.rs`): mapping NON-owned (free esattamente una
/// volta via record, mai double-free col walk owned), record per-PID, free a
/// teardown/exec, mai ereditato dal fork (il figlio faulta come i ring).
const DMA_MAX_PROCS: usize = 128;
use syscall_numbers::DMA_SLOTS;
/// (phys_base, count) per (pid, slot): (0, 0) = slot libero.
static mut DMA_PHYS: [(u64, u64); DMA_MAX_PROCS * DMA_SLOTS] =
    [(0, 0); DMA_MAX_PROCS * DMA_SLOTS];

/// Alloca `pages` frame contigui azzerati all'address space `mm` nello slot
/// `slot` (`DMA_SLOT_ANY` = primo libero). `pid` serve solo per il grant
/// (l'owner di `Service::Console`): il record e' per address space, come i
/// ring. Ritorna `(phys, pages, indice_slot)`. `None` se lo slot e' occupato,
/// OOM, bound non rispettato o grant negato. Il mapping alla VA
/// (`syscall_numbers::dma_slot_va`) e' a carico del chiamante (syscall).
pub fn alloc_dma_pages(mm: usize, pid: usize, pages: usize, slot: u64) -> Option<(u64, usize, usize)> {
    if mm >= DMA_MAX_PROCS {
        return None;
    }
    let slots = syscall_numbers::DMA_SLOTS;

    // Risolvi l'indice dello slot (esplicito o primo libero).
    let idx = if slot == syscall_numbers::DMA_SLOT_ANY {
        let mut found = None;
        for i in 0..slots {
            if unsafe { *core::ptr::addr_of!(DMA_PHYS[mm * slots + i]) }.0 == 0 {
                found = Some(i);
                break;
            }
        }
        found?
    } else {
        let i = slot as usize;
        if i >= slots {
            return None;
        }
        i
    };

    // Bound: lo staging resta piccolo, il device buffer puo' essere grande.
    let max = if idx == 0 {
        syscall_numbers::DMA_PAGES_MAX
    } else {
        syscall_numbers::DMA_DEVICE_PAGES_MAX
    };
    if pages == 0 || pages > max {
        crate::serial_println!(
            "[dma] alloc: {} pagine fuori bound (max {})",
            pages,
            max
        );
        return None;
    }

    // Grant: gli slot device sono riservati all'owner di `Service::Console`
    // (D3). Lo staging (slot 0) resta aperto a tutti (path storico, 4 pagine).
    if idx >= 1 {
        let owner = crate::relay::channels::lookup(syscall_numbers::Service::Console);
        if owner != Some(pid) {
            crate::serial_println!(
                "[dma] alloc: pid={} non owner di Console (owner={:?}), slot {} rifiutato",
                pid,
                owner,
                idx
            );
            return None;
        }
    }

    if unsafe { *core::ptr::addr_of!(DMA_PHYS[mm * slots + idx]) }.0 != 0 {
        return None; // slot occupato
    }

    let phys = match crate::arc::phys_mem::alloc_contiguous(pages) {
        Some(p) => p,
        None => {
            crate::serial_println!(
                "[dma] alloc: nessun run contiguo di {} pagine ({})",
                pages,
                pages * 4096
            );
            return None;
        }
    };
    unsafe {
        core::ptr::write_bytes(crate::addr::phys_to_virt(phys) as *mut u8, 0, pages * 4096);
        *core::ptr::addr_of_mut!(DMA_PHYS[mm * slots + idx]) = (phys, pages as u64);
    }
    Some((phys, pages, idx))
}

/// Libera tutti gli slot DMA dell'address space `mm` e azzera i record
/// (teardown e exec: single path col walk owned, che salta il mapping
/// NON-owned). `pub(crate)` per exec (la nuova immagine rialloca se serve).
pub(crate) fn free_dma_pages(mm: usize) {
    if mm >= DMA_MAX_PROCS {
        return;
    }
    let slots = syscall_numbers::DMA_SLOTS;
    for i in 0..slots {
        let (base, count) = unsafe { *core::ptr::addr_of!(DMA_PHYS[mm * slots + i]) };
        if base != 0 {
            crate::arc::phys_mem::free_contiguous(base, count as usize);
            unsafe {
                *core::ptr::addr_of_mut!(DMA_PHYS[mm * slots + i]) = (0, 0);
            }
        }
    }
}
