//! `spawn_copy` — duplicazione COW dell'address space (Fase 34, ADR-0024).
//!
//! `spawn_copy_current` crea un figlio-copia del processo corrente che
//! condivide le pagine owned in COW (`vmm_user::spawn_copy_share`) e riprende
//! come ritorno dalla syscall con `rax = 0` (fake kernel stack +
//! `spawn_copy_child_exit`, mai copia dello stack
//! del padre). Il figlio NON eredita: canali (tranne nascita), fd lato server,
//! registrazioni, ring FS (finestre non mappate: uso = kill rumoroso), porte
//! I/O (bitmap vuota, least privilege), CBS, messaggi. Ritorna
//! `(child_pid, birth_chan)` o `None` (nessun PID / canale / OOM walk / OOM
//! stack / TSS esaurito). Fallimento = `-1` al chiamante, padre intatto (le
//! pagine gia' COW-izzate si privatizzano al write).

use crate::ordo::process::{Process, STACK_FRAMES};
use super::ctx::SCHED;
use super::queue::Scheduler;
use crate::syscall::{
    spawn_copy_child_exit, SAVED_R8, SAVED_R9, SAVED_R10, SAVED_R11, SAVED_R13, SAVED_R14,
    SAVED_R15, SAVED_RBX, SAVED_RBP, SAVED_RCX, SAVED_RDI, SAVED_RDX, SAVED_RSI,
    SAVED_USER_R12, SAVED_USER_RSP,
};

/// Duplica il processo corrente in COW. Chiamato da `sys_spawn_copy` (il chiamante
/// e' in esecuzione in syscall: il suo stato user e' sullo stack kernel a
/// offset noti da `rsp0`). SCHED lock trattenuto per tutta l'operazione.
pub fn spawn_copy_current() -> Option<(usize, usize)> {
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");
    let parent_pid = sched.current.expect("spawn_copy senza processo corrente");

    let child_pid = sched.alloc_pid()?;
    // S-T: il canale di nascita e' leader↔figlio (il thread chiama ma opera
    // come leader sul wire: `sender_of` risolve, gli endpoint combaciano).
    let group = sched.processes[parent_pid].thread_group.unwrap_or(parent_pid);
    // Canale di nascita PER PRIMO: fallisce → solo release pid, nessuno stato.
    let chan = match crate::relay::channels::alloc(group, child_pid) {
        Some(c) => c,
        None => {
            sched.release_pid(child_pid);
            return None;
        }
    };
    // Snapshot scalari del padre (il borrow finisce qui). S-T: heap e VMA
    // sono indicizzati al leader del gruppo (il chiamante puo' essere un
    // thread: il figlio eredita lo spazio CONDIVISO del gruppo, non tabelle
    // vuote del tid).
    // S-T: gruppo e mm del chiamante (il figlio e' single-threaded del
    // GRUPPO: parent = leader, mai un tid riusabile come orfano).
    let p_mm = sched.processes[parent_pid].thread_group.unwrap_or(parent_pid);
    let p_parent = p_mm;
    let (p_cr3, p_top, p_prio, p_req, p_text, p_hash, p_name, p_owned, p_nlen, p_brk) = {
        let p = &sched.processes[parent_pid];
        (
            p.cr3,
            p.kernel_stack_top,
            p.priority,
            p.req_next,
            p.text_id,
            p.image_hash,
            p.name,
            p.name_owned,
            p.name_len,
            crate::arc::vmm_user::heap_brk(p_mm),
        )
    };
    // Solo processi user duplicabili (cr3 propria, mai quella kernel).
    if p_cr3 == crate::arc::vmm_user::kernel_cr3() {
        crate::relay::channels::release_pid(child_pid);
        sched.release_pid(child_pid);
        return None;
    }

    // Address space figlio + walk COW (puo' fallire OOM → unwind).
    let child_cr3 = match crate::arc::vmm_user::new_address_space() {
        Some(c) => c,
        None => {
            unwind(sched, child_pid, None, None, None);
            return None;
        }
    };
    if !crate::arc::vmm_user::spawn_copy_share(p_cr3, child_cr3) {
        unwind(sched, child_pid, Some(child_cr3), None, None);
        return None;
    }

    // Kernel stack figlio (PHYS base per teardown, VIRT top per RSP0/frame).
    let stack_base = match crate::arc::phys_mem::alloc_contiguous(STACK_FRAMES) {
        Some(b) => b,
        None => {
            unwind(sched, child_pid, Some(child_cr3), None, None);
            return None;
        }
    };
    let stack_top = crate::addr::phys_to_virt(stack_base + (STACK_FRAMES as u64 * crate::arc::phys_mem::FRAME_SIZE));

    // TSS figlio, bitmap I/O VUOTA (34: nessuna porta ereditata).
    let tss_slot = match Process::alloc_tss(stack_top, &[]) {
        Some(s) => s,
        None => {
            unwind(sched, child_pid, Some(child_cr3), Some(stack_base), None);
            return None;
        }
    };
    let tss_sel = crate::gdt::selectors().tss_selector(tss_slot);

    // Fake kernel stack (11 word) + contesto: copia i registri user salvati
    // dall'entry sullo stack del padre (offset da rsp0 = p_top).
    let rsp_child = stack_top - 88;
    let saved = unsafe {
        let src = p_top;
        let dst = rsp_child;
        let cp = |doff: u64, soff: u64| {
            core::ptr::write(
                (dst + doff) as *mut u64,
                core::ptr::read((src - soff) as *const u64),
            );
        };
        core::ptr::write(dst as *mut u64, spawn_copy_child_exit as *const () as u64);
        cp(8, SAVED_R11);
        cp(16, SAVED_RCX);
        cp(24, SAVED_RDX);
        cp(32, SAVED_RSI);
        cp(40, SAVED_RDI);
        cp(48, SAVED_R10);
        cp(56, SAVED_R9);
        cp(64, SAVED_R8);
        cp(72, SAVED_USER_R12);
        cp(80, SAVED_USER_RSP);
        let rd = |soff: u64| core::ptr::read((src - soff) as *const u64);
        crate::ordo::context::CpuContext {
            rbx: rd(SAVED_RBX),
            rbp: rd(SAVED_RBP),
            r12: 0, // scartato: spawn_copy_child_exit fa pop r12 dallo stack finto
            r13: rd(SAVED_R13),
            r14: rd(SAVED_R14),
            r15: rd(SAVED_R15),
            rsp: rsp_child,
            fpu: 0,
        }
    };
    // Record per-processo (infallibili da qui in poi: nessun unwind).
    // S-T: clona le VMA del gruppo (vedi sopra).
    crate::arc::vmm_user::vma_clone(p_mm, child_pid);
    crate::arc::vmm_user::set_heap_brk(child_pid, p_brk);
    if p_text != 0 {
        crate::text::add_ref(p_text);
    }
    let child = unsafe {
        Process::create_spawn_copy(
            p_name, p_owned, p_nlen, p_prio, p_req, p_parent, child_cr3,
            stack_base, stack_top, saved, tss_slot, tss_sel, p_text, p_hash,
        )
    };
    sched.place_process(child_pid, child);
    super::spawn::inherit_cwd(sched, child_pid, Some(parent_pid));
    sched.set_ready(child_pid);
    // Niente `set_parent_chan` (riprenderebe SCHED, gia' trattenuto qui):
    // assegnazione diretta sotto lock.
    sched.processes[child_pid].parent_chan = Some(chan);
    crate::serial_println!("[spawn_copy] pid={} → figlio pid={} canale={}", parent_pid, child_pid, chan);
    Some((child_pid, chan))
}

/// Unwind di uno spawn_copy fallito: distrugge lo spazio parziale del figlio (i
/// `deref` bilanciano da soli le condivisioni), libera stack/TSS, canale e
/// PID. Il padre resta valido (pagine COW-izzate si privatizzano al write).
fn unwind(
    sched: &mut Scheduler,
    child_pid: usize,
    child_cr3: Option<u64>,
    stack_base: Option<u64>,
    tss_slot: Option<usize>,
) {
    if let Some(cr3) = child_cr3 {
        unsafe { crate::arc::vmm_user::teardown_user_space(cr3, child_pid) };
    }
    if let Some(b) = stack_base {
        crate::arc::phys_mem::free_contiguous(b, STACK_FRAMES);
    }
    if let Some(s) = tss_slot {
        crate::gdt::free_tss_slot(s);
    }
    crate::relay::channels::release_pid(child_pid);
    sched.release_pid(child_pid);
}
