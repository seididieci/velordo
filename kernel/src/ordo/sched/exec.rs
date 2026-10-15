//! `exec` in-place — sostituzione dell'immagine del processo corrente
//! (Fase 37).
//!
//! Stesso PID/parent/priorita'/canali (fd server-side, code IPC e registrazioni
//! sopravvivono: sono indicizzati per canale, non per immagine); cade TUTTO
//! l'address space e ne viene caricato uno nuovo leggendo DIRETTAMENTE dai
//! byte user del chiamante (S2.5c, exec streaming: niente staging in heap,
//! come `spawn_image` — lo spazio vecchio resta mappato finche' serve).
//! Stack nuovo con argv stile Linux (37.1; argc=0 senza args); `image_hash`
//! rimisurato (Strato 2: senza, `peer_info` mentirebbe e la regola same-image
//! 36.5 sarebbe bypassabile); porte I/O azzerate (least privilege, come il
//! figlio-copia). Il nome display resta quello vecchio in 37.0 (37.1 lo deriva da
//! argv[0] in shell, mai nel kernel).
//!
//! Ritorna `Ok(())` e NON ritorna al chiamante: il frame syscall salvato viene
//! riscritto (RIP→entry nuova, RSP→stack nuovo) e `sysretq` atterra nella nuova
//! immagine con `rax = 0`. `Err(())` = validazione fallita, processo intatto
//! (mai toccato nulla prima della validazione).
//!
//! OOM a load (come `create_user`, pre-esistente): `map_private` va in panic —
//! stessa proprieta' dello spawn, documentata per ADR-0028 (mai introdotto un
//! nuovo modo di fallire a meta').

use super::ctx::SCHED;
use crate::arc::vmm_user::{layout_argv, parse_args, ParsedArgs};

/// Sostituisce l'immagine del processo corrente con `bytes` (slice user del
/// chiamante, range validato: IF=0 in syscall, niente preemption/munmap nel
/// mezzo, come `spawn_image`) e gli argomenti `args_block` (`None` = nessun
/// argv/env → argc=0; `Some` = blocco `[argc:8][envc:8][payload]` in heap
/// kernel — piccolo, sopravvive al teardown per `layout_argv`). SCHED lock
/// trattenuto per tutta l'operazione (come `spawn_copy_current`); niente
/// blocking nel mezzo (mai context switch su spazio dimezzato).
/// S2.5c: nuovo address space + load diretto (come `create_user`), poi switch
/// CR3 + teardown del vecchio — mai staging dell'immagine in heap.
pub fn exec_current(bytes: &[u8], args_block: Option<&[u8]>) -> Result<(), ()> {
    // 1. Validazione PRIMA di toccare qualunque stato (ELF malformato o args
    // malformati = -1, processo intatto, come `create_user` prima di allocare).
    let layout = crate::elf::validate(bytes).ok_or(())?;
    let hash = syscall_numbers::image_hash(bytes);
    let parsed = match args_block {
        Some(b) => Some(parse_args(b).ok_or(())?),
        None => None,
    };

    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");
    let me = sched.current.expect("exec senza processo corrente");

    // S-T: exec solo dal leader (o da single-thread: stesso caso). Da un
    // thread non-leader = rifiuto loud: il morph di leadership (canali del
    // leader + mm condiviso da svuotare sotto i piedi dei fratelli) non ha
    // semantica sicura senza trasferimento di ownership dei canali — la PAL
    // fa exec solo da leader (spawn_copy+exec intatto: il figlio e' single).
    if sched.processes[me].thread_group.is_some() {
        crate::serial_println!("[exec ] pid={} thread non-leader: exec rifiutata", me);
        return Err(());
    }

    // Snapshot scalari (il borrow finisce qui).
    let (cr3, top, slot, old_text, frames) = {
        let p = &sched.processes[me];
        // Solo processi user (CR3 propria, mai quella kernel).
        if p.cr3 == crate::arc::vmm_user::kernel_cr3() {
            return Err(());
        }
        (p.cr3, p.kernel_stack_top, p.tss_slot, p.text_id, p.stack_frames)
    };

    // S-T: il leader con thread vivi li termina PRIMA di svuotare lo spazio
    // condiviso (via breve thread: niente canali/CBS/peer — restano al
    // gruppo che l'exec preserva come persona).
    for tid in 0..sched.processes.len() {
        let t = sched.processes[tid].thread_group == Some(me)
            && sched.processes[tid].state != crate::ordo::process::State::Terminated;
        if t {
            sched.terminate(tid, 0);
        }
    }

    // 2. Nuovo address space + load DIRETTO dalla slice user (spazio vecchio
    // ancora attivo e mappato: come `create_user`, niente staging in heap).
    // Fallimento qui = processo intatto (il vecchio spazio non e' toccato).
    let new_cr3 = crate::arc::vmm_user::new_address_space().ok_or(())?;
    // 3. Reset bookkeeping PRIMA del load (i record descrivono il vecchio
    // spazio; il nuovo parte vuoto): heap, VMA (+ref shm rilasciati), ring
    // (i frame verranno riallocati al primo handshake lazy), staging DMA
    // (38.1: la nuova immagine rialloca se serve), text image vecchia.
    crate::arc::vmm_user::set_heap_brk(me, 0);
    crate::arc::vmm_user::vma_clear(me);
    crate::arc::vmm_user::free_ring_pages(me);
    crate::arc::vmm_user::free_dma_pages(me);
    if old_text != 0 {
        crate::text::release(old_text);
    }

    // 4. Carica la nuova immagine + stack nuovo nel NUOVO spazio (stesso
    // percorso dello spawn: condivisione text, W^X per-segmento, NX ovunque
    // tranne il codice). `setup_user_stack` mappa e basta (le 3 parole
    // argc=0 che scrive per lo spawn vengono sovrascritte qui sotto: il
    // layout parte da USER_STACK_TOP).
    let new_text = unsafe { crate::elf::load(new_cr3, bytes, &layout) };
    // Lo stack nuovo ha la stessa profondita' del vecchio (flag bigstack
    // preservato: un rustc che exec'un altro rustc resta largo).
    let _ = unsafe { crate::arc::vmm_user::setup_user_stack(new_cr3, frames as usize, None) };
    // 4b. Switch al nuovo spazio + teardown del vecchio (S2.5c): da qui in
    // poi gira sul nuovo CR3 (`mov cr3` flusha la TLB: niente stale, come lo
    // switch in `ctx.rs`). PERCPU allineato (syscall future devono vedere
    // il cr3 giusto).
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) new_cr3, options(nostack, preserves_flags));
    }
    crate::syscall::set_current(me, top, new_cr3, me as u64);
    unsafe { crate::arc::vmm_user::teardown_user_space(cr3, me) };
    let new_rsp = match &parsed {
        Some(p) => unsafe { layout_argv(crate::arc::vmm_user::USER_STACK_TOP, crate::arc::vmm_user::USER_STACK_TOP, p) },
        None => unsafe {
            layout_argv(
                crate::arc::vmm_user::USER_STACK_TOP,
                crate::arc::vmm_user::USER_STACK_TOP,
                &ParsedArgs { argc: 0, envc: 0, payload: &[], arg_strs: alloc::vec::Vec::new(), env_strs: alloc::vec::Vec::new() },
            )
        },
    };

    // 5. PCB: nuovo cr3 + nuova identita' (text + hash), stessa persona
    // (pid/parent/prio/canali/req_next/CBS intatti — nessuno li tocca). Il
    // cr3 VA aggiornato: il reclaim fa teardown dallo spazio registrato —
    // col vecchio (gia' liberato qui) corromperebbe l'allocator e i processi
    // che hanno riusato quei frame (deref su frame libero / double fault).
    // S2.5b: FPU resettata (nuova immagine, come Unix: niente registri ereditati).
    {
        sched.processes[me].cr3 = new_cr3;
        // Il cr3 e' condiviso col gruppo: i thread (vivi o in reclaim — il
        // reclaim salta l'mm ma `ctx.rs`/`spawn` leggono il cr3 registrato)
        // devono vedere quello nuovo, mai il vecchio liberato qui sopra.
        for i in 0..sched.processes.len() {
            if sched.processes[i].thread_group == Some(me) {
                sched.processes[i].cr3 = new_cr3;
            }
        }
        let p = &mut sched.processes[me];
        p.text_id = new_text;
        p.image_hash = hash;
        // S2.5b: contenuto resettato (l'indirizzo pool resta: stesso pid).
        unsafe { crate::ordo::context::fpu_init(p.saved.fpu as *mut [u128; 32]) };
    }

    // 6. TSS: bitmap I/O azzerata (idempotente). Nessuno puo' chiedere porte
    // via exec (solo init via spawn_image): least privilege di default.
    crate::gdt::configure_tss(slot, x86_64::VirtAddr::new(top), &[]);

    // 7. Riscrive il frame syscall salvato sullo stack kernel (`top` == rsp0,
    // vedi `set_current` in ctx.rs): RIP→entry nuova, RSP→stack nuovo (punta
    // ad argc, vedi sopra). r11 (rflags user, IF=1) e' preservato; `rax` = 0.
    let entry = crate::elf::entry(&layout);
    unsafe {
        core::ptr::write((top - crate::syscall::SAVED_RCX) as *mut u64, entry);
        core::ptr::write((top - crate::syscall::SAVED_USER_RSP) as *mut u64, new_rsp);
    }
    crate::serial_println!(
        "[exec] pid={} nuova immagine entry={:#x} hash={:#x}",
        me, entry, hash
    );
    Ok(())
}
