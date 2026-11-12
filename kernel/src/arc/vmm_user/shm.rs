//! Shared memory (Fase 30): regioni fisiche condivise tra processi.
//!
//! `shm_create` alloca frame contigui azzerati e ritorna un id (>= 1);
//! `shm_map` mappa la regione in una VMA del processo (PTE non-owned,
//! pre-materializzate: niente demand-zero, le pagine esistono da subito e
//! sono le STESSE per tutti i mappatori) e incrementa un refcount; `munmap`
//! e il teardown del processo rilasciano il riferimento, e a 0 i frame
//! contigui vengono liberati. Tabella statica (mai heap), stesso stile di
//! `VMA_TABLE`/`RING_PHYS`.
//!
//! Fase 30.1 (ownership): `shm_create` registra l'**owner** (pid) e prende
//! un riferimento (refs = 1). `shm_destroy` e' owner-only: marca la regione
//! `destroyed` (i `shm_map` successivi falliscono) e rilascia il riferimento
//! dell'owner; se non ci sono mappature i frame vanno via subito, altrimenti
//! restano vivi finche' l'ultimo `munmap`/teardown non scende a 0. Il
//! teardown di un processo rilascia i riferimenti owner delle regioni che
//! possiede: chi crea e muore non lascia frame orfani.

use super::layout::PAGE_SIZE;

/// Regioni condivise massime (slot statici).
const SHM_MAX: usize = 16;
/// Pagine massime per regione (256 KiB): bound strutturale sui frame contigui.
const SHM_MAX_FRAMES: usize = 64;

/// Una regione condivisa: `phys == 0` = slot libero. `refs` = 1 (owner) +
/// una per mappatura. `destroyed` = l'owner ha chiuso: `shm_map` fallisce,
/// le mappature esistenti restano valide fino al loro `munmap`.
#[derive(Clone, Copy)]
struct ShmEntry {
    phys: u64,
    frames: u64,
    refs: u32,
    owner: u64,
    destroyed: bool,
}

impl ShmEntry {
    const fn free() -> Self {
        Self {
            phys: 0,
            frames: 0,
            refs: 0,
            owner: 0,
            destroyed: false,
        }
    }
}

static mut SHM_TABLE: [ShmEntry; SHM_MAX] = [ShmEntry::free(); SHM_MAX];

/// Crea una regione condivisa di `len` byte (arrotondata a pagina, max
/// `SHM_MAX_FRAMES` pagine) con frame contigui azzerati. Ritorna l'id (>= 1)
/// o `None`. `owner` = pid del chiamante: il riferimento iniziale (refs = 1)
/// e' dell'owner, che lo rilascia con `shm_destroy` (o col teardown).
pub fn shm_create(len: u64, owner: u64) -> Option<u32> {
    if len == 0 {
        return None;
    }
    let frames = (len.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE) as usize;
    if frames == 0 || frames > SHM_MAX_FRAMES {
        return None;
    }
    let phys = crate::arc::phys_mem::alloc_contiguous(frames)?;
    unsafe {
        core::ptr::write_bytes(
            crate::addr::phys_to_virt(phys) as *mut u8,
            0,
            frames * PAGE_SIZE as usize,
        );
    }
    for i in 0..SHM_MAX {
        let e = unsafe { *core::ptr::addr_of!(SHM_TABLE[i]) };
        if e.phys == 0 {
            unsafe {
                *core::ptr::addr_of_mut!(SHM_TABLE[i]) = ShmEntry {
                    phys,
                    frames: frames as u64,
                    refs: 1,
                    owner,
                    destroyed: false,
                };
            }
            return Some(i as u32 + 1);
        }
    }
    crate::arc::phys_mem::free_contiguous(phys, frames);
    None
}

/// `(phys_base, frames)` della regione `id`, se viva (anche se `destroyed`:
/// le mappature esistenti restano valide e il fault handler deve poter
/// riempire i buchi).
pub fn shm_region(id: u32) -> Option<(u64, u64)> {
    if id == 0 || id as usize > SHM_MAX {
        return None;
    }
    let e = unsafe { *core::ptr::addr_of!(SHM_TABLE[id as usize - 1]) };
    if e.phys == 0 {
        None
    } else {
        Some((e.phys, e.frames))
    }
}

/// True se la regione `id` e' stata chiusa dall'owner: `shm_map` rifiuta.
pub fn shm_destroyed(id: u32) -> bool {
    if id == 0 || id as usize > SHM_MAX {
        return false;
    }
    unsafe { (*core::ptr::addr_of!(SHM_TABLE[id as usize - 1])).destroyed }
}

/// Incrementa il refcount della regione `id` (una mappatura in piu').
pub fn shm_ref(id: u32) {
    if id == 0 || id as usize > SHM_MAX {
        return;
    }
    let idx = id as usize - 1;
    unsafe { *core::ptr::addr_of_mut!(SHM_TABLE[idx].refs) += 1; }
}

/// Rilascia un riferimento alla regione `id`: a 0 rilascia i frame contigui e
/// azzera lo slot. Idempotente su id invalido/gia' libero. Dalla Fase 33 via
/// `deref_contiguous` (le mappature COW tengono ref per-frame: il frame
/// condiviso sopravvive finche' almeno un sharer o la regione lo referenzia).
pub fn shm_release(id: u32) {
    if id == 0 || id as usize > SHM_MAX {
        return;
    }
    let idx = id as usize - 1;
    let e = unsafe { *core::ptr::addr_of!(SHM_TABLE[idx]) };
    if e.phys == 0 {
        return;
    }
    let refs = e.refs.saturating_sub(1);
    if refs == 0 {
        crate::arc::phys_mem::deref_contiguous(e.phys, e.frames as usize);
        unsafe { *core::ptr::addr_of_mut!(SHM_TABLE[idx]) = ShmEntry::free(); }
    } else {
        unsafe { *core::ptr::addr_of_mut!(SHM_TABLE[idx].refs) = refs; }
    }
}

/// Fase 30.1 — `shm_destroy(id, owner)`: solo l'owner puo' chiudere la
/// regione. Marca `destroyed` (i `shm_map` successivi falliscono) e rilascia
/// il riferimento dell'owner; se non ci sono mappature i frame vanno via
/// subito, altrimenti restano finche' l'ultimo `munmap`/teardown non scende a
/// 0. Ritorna `true` se la regione e' stata chiusa, `false` se id inesistente,
/// gia' chiusa o non di questo owner.
pub fn shm_destroy(id: u32, owner: u64) -> bool {
    if id == 0 || id as usize > SHM_MAX {
        return false;
    }
    let idx = id as usize - 1;
    let e = unsafe { *core::ptr::addr_of!(SHM_TABLE[idx]) };
    if e.phys == 0 || e.destroyed || e.owner != owner {
        return false;
    }
    unsafe { *core::ptr::addr_of_mut!(SHM_TABLE[idx].destroyed) = true; }
    shm_release(id);
    true
}

/// Teardown: rilascia i riferimenti owner delle regioni del processo `pid`
/// (chi crea e muore non lascia frame orfani). Le mappature di altri processi
/// restano valide finche' non le smappano.
pub fn release_owner(pid: u64) {
    for i in 0..SHM_MAX {
        let e = unsafe { *core::ptr::addr_of!(SHM_TABLE[i]) };
        if e.phys != 0 && e.owner == pid && !e.destroyed {
            unsafe { *core::ptr::addr_of_mut!(SHM_TABLE[i].destroyed) = true; }
            shm_release(i as u32 + 1);
        }
    }
}
