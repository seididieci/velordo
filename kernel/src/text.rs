//! Shared text (Fase 32): immagini immutabili (codice `RX` + rodata `RO`)
//! condivise tra le istanze dello stesso binario, con refcount. Il loader ELF
//! (`crate::elf`) carica `[base, rw_off)` da qui e `[rw_off, end)` privatamente.
//!
//! Identita': hash FNV-1a dell'ELF + **verifica byte-per-byte** del contenuto
//! immutabile su hit (input da disco non fidato: una collisione di hash
//! mapperebbe codice sbagliato, quindi non ci si fida del solo hash). Tabella
//! statica (mai heap), stile `SHM_TABLE`; accesso serializzato dallo SCHED lock
//! (spawn e reclaim). Slot pieni → fallback al load privato (mai spawn fallito).
//! Scope: refcount-only (condivide tra istanze concorrenti, libera a 0).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::elf::{Layout, PAGE};

/// Immagini condivise massime (slot statici): **al massimo una per processo
/// vivo** — `spawn`/`exec` ne acquisiscono una, `fork` e i thread condividono
/// l'id del gruppo (`add_ref`, nessun nuovo slot) e `exec` rilascia la vecchia
/// *prima* di caricare la nuova — quindi il bound e' il numero di entita'
/// schedulabili concorrenti (`MAX_PIDS`). A questo limite il fallback privato
/// e' irraggiungibile per costruzione; se succede comunque (es. ref perso su
/// un errore di spawn) il contatore `full` e il log una-tantum lo dicono.
const TEXT_MAX: usize = crate::ordo::sched::MAX_PIDS;

/// `(phys, pages, hash, base, rw_off, refs)`; `phys == 0` = slot libero.
static mut TEXT_TABLE: [(u64, u64, u64, u64, u64, u32); TEXT_MAX] =
    [(0, 0, 0, 0, 0, 0); TEXT_MAX];

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
/// Somma dei refcount vivi (immagini * riferimenti).
static LIVE: AtomicU64 = AtomicU64::new(0);
/// Fallback privati per **tabella piena** (mai per OOM): sensore, non una
/// decisione — il fallback e' sempre corretto, solo non condivide.
static FULL: AtomicU64 = AtomicU64::new(0);
/// Un log per *episodio* di saturazione (riarmato quando uno slot si libera):
/// la saturazione era invisibile (nessun log, `misses` non la conta) e il solo
/// sintomo era un `t47: no sharing` inspiegabile.
static WARNED: AtomicBool = AtomicBool::new(false);

/// FNV-1a 64-bit sull'ELF (identita' del binario): single source in
/// `syscall-numbers` (Fase 36, riusata per l'identita' misurata nel PCB).
fn hash(bytes: &[u8]) -> u64 {
    syscall_numbers::image_hash(bytes)
}

/// Confronta il contenuto immutabile del nuovo ELF con l'immagine in cache
/// (`phys`): copre esattamente il range condiviso `[base, rw_off)`. Sicuro
/// anche in caso di collisione di hash (se i byte differiscono, rifiuta).
unsafe fn verify(bytes: &[u8], l: &Layout, phys: u64) -> bool {
    let img = crate::addr::phys_to_virt(phys) as *const u8;
    for s in &l.segments[..l.nseg] {
        if s.vaddr >= l.rw_off {
            continue; // segmento tutto nel privato
        }
        let n = (s.filesz as u64).min(l.rw_off - s.vaddr) as usize;
        for i in 0..n {
            let cached = unsafe { *img.add((s.vaddr - l.base) as usize + i) };
            if cached != bytes[s.offset + i] {
                return false;
            }
        }
    }
    true
}

/// Copia il contenuto immutabile dei segmenti in `phys` (blocco azzerato).
unsafe fn fill_shared(bytes: &[u8], l: &Layout, phys: u64) {
    let img = crate::addr::phys_to_virt(phys) as *mut u8;
    for s in &l.segments[..l.nseg] {
        if s.vaddr >= l.rw_off {
            continue;
        }
        let n = (s.filesz as u64).min(l.rw_off - s.vaddr) as usize;
        let dst = unsafe { img.add((s.vaddr - l.base) as usize) };
        unsafe { core::ptr::copy_nonoverlapping(bytes[s.offset..s.offset + n].as_ptr(), dst, n); }
    }
}

/// Acquisisce (o crea) l'immagine condivisa per `bytes`/`layout`. Ritorna
/// l'id (>= 1) o `None` (nessuna parte condivisibile, tabella piena, OOM →
/// fallback privato a carico del chiamante). `refs` incrementato.
///
/// # Safety
/// Chiamato dal loader con lo SCHED lock trattenuto (spawn/reclaim).
pub unsafe fn acquire(bytes: &[u8], l: &Layout) -> Option<u32> {
    let pages = ((l.rw_off - l.base) / PAGE) as usize;
    if pages == 0 {
        return None;
    }
    let h = hash(bytes);
    for i in 0..TEXT_MAX {
        let e = unsafe { *core::ptr::addr_of!(TEXT_TABLE[i]) };
        if e.0 != 0
            && e.2 == h
            && e.1 == pages as u64
            && e.3 == l.base
            && e.4 == l.rw_off
            && unsafe { verify(bytes, l, e.0) }
        {
            unsafe { *core::ptr::addr_of_mut!(TEXT_TABLE[i].5) += 1; }
            HITS.fetch_add(1, Ordering::Relaxed);
            LIVE.fetch_add(1, Ordering::Relaxed);
            return Some(i as u32 + 1);
        }
    }
    // Miss: **prima** lo slot libero, poi l'allocazione. A tabella piena il
    // fallback privato non paga un `alloc_contiguous` + copia + free per ogni
    // spawn: si esce subito (era il caso di `net=1`, dove i servizi di rete
    // saturavano i 16 slot della vecchia tabella e ogni spawn buttava via
    // un'immagine appena costruita).
    let mut free = None;
    for i in 0..TEXT_MAX {
        if unsafe { (*core::ptr::addr_of!(TEXT_TABLE[i])).0 } == 0 {
            free = Some(i);
            break;
        }
    }
    let Some(i) = free else {
        FULL.fetch_add(1, Ordering::Relaxed);
        if !WARNED.swap(true, Ordering::Relaxed) {
            crate::serial_println!(
                "[text] tabella piena ({} immagini condivise): fallback privato, sharing disattivato",
                TEXT_MAX
            );
        }
        return None;
    };

    // Miss: costruisci l'immagine nello slot gia' scelto.
    let phys = crate::arc::phys_mem::alloc_contiguous(pages)?;
    unsafe {
        core::ptr::write_bytes(crate::addr::phys_to_virt(phys) as *mut u8, 0, pages * PAGE as usize);
        fill_shared(bytes, l, phys);
        *core::ptr::addr_of_mut!(TEXT_TABLE[i]) = (phys, pages as u64, h, l.base, l.rw_off, 1);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    LIVE.fetch_add(1, Ordering::Relaxed);
    Some(i as u32 + 1)
}

/// Mappa il range immutabile `[base, rw_off)` di `id` in `cr3` come pagine
/// condivise read-only (`RX` dove c'e' codice, `RO` altrove), NON owned.
///
/// # Safety
/// `cr3` valido; `id` da `acquire`; `layout` coerente con l'immagine.
pub unsafe fn map_shared(cr3: u64, l: &Layout, id: u32) {
    if id == 0 || id as usize > TEXT_MAX {
        return;
    }
    let e = unsafe { *core::ptr::addr_of!(TEXT_TABLE[id as usize - 1]) };
    if e.0 == 0 {
        return;
    }
    let pages = ((l.rw_off - l.base) / PAGE) as usize;
    for p in 0..pages {
        let va = l.base + (p as u64) * PAGE;
        let (_w, x) = crate::elf::page_flags(l, va);
        unsafe {
            crate::arc::vmm_user::map_user_leaf_shared(cr3, va, e.0 + (p as u64) * PAGE, x);
        }
    }
}

/// Rilascia un riferimento: a 0 libera i frame contigui e azzera lo slot.
pub fn release(id: u32) {
    if id == 0 || id as usize > TEXT_MAX {
        return;
    }
    let idx = id as usize - 1;
    let e = unsafe { *core::ptr::addr_of!(TEXT_TABLE[idx]) };
    if e.0 == 0 {
        return;
    }
    LIVE.fetch_sub(1, Ordering::Relaxed);
    let refs = e.5.saturating_sub(1);
    if refs == 0 {
        crate::arc::phys_mem::free_contiguous(e.0, e.1 as usize);
        unsafe { *core::ptr::addr_of_mut!(TEXT_TABLE[idx]) = (0, 0, 0, 0, 0, 0); }
        // Slot di nuovo libero: un'eventuale saturazione successiva merita
        // un altro log (un avviso per episodio, non uno per boot).
        WARNED.store(false, Ordering::Relaxed);
    } else {
        unsafe { *core::ptr::addr_of_mut!(TEXT_TABLE[idx].5) = refs; }
    }
}

/// Aggiunge un riferimento all'immagine `id` (Fase 34, fork: il figlio
/// specchia le pagine condivise del padre e ne trattiene una quota).
/// Idempotente su id invalido/libero (mai panic: il walk del fork specchia
/// solo PTE presenti, l'id viene dal PCB del padre).
pub fn add_ref(id: u32) {
    if id == 0 || id as usize > TEXT_MAX {
        return;
    }
    let idx = id as usize - 1;
    let e = unsafe { *core::ptr::addr_of!(TEXT_TABLE[idx]) };
    if e.0 == 0 {
        return;
    }
    unsafe { *core::ptr::addr_of_mut!(TEXT_TABLE[idx].5) += 1; }
    LIVE.fetch_add(1, Ordering::Relaxed);
}

/// `(hits, misses, live, full)`: contatori per la syscall `text_stats`
/// (debug/test). `full` conta i fallback privati per **tabella piena**: 0 in
/// condizioni normali (il bound `TEXT_MAX` copre tutti i processi
/// concorrenti), quindi un valore non-zero e' il segnale che qualcosa tiene
/// slot senza rilasciarli.
pub fn stats() -> (u64, u64, u64, u64) {
    (
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        LIVE.load(Ordering::Relaxed),
        FULL.load(Ordering::Relaxed),
    )
}
