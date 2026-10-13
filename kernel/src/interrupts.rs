//! IDT del kernel: eccezioni CPU sincrone + interrupt hardware (Fase 3).
//!
//! Le entry 0-31 sono eccezioni CPU; le entry 32-47 (0x20-0x2F) sono gli
//! interrupt hardware rimappati dal PIC 8259. Tutti i restanti IRQ non
//! gestiti vengono intercettati da un handler generico.

use spin::Lazy;
use x86_64::structures::idt::{
    InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode,
};

/// 27.3 (solo build `selftest`): il test NULL-#PF arma questo flag prima di
/// leggere l'indirizzo 0. L'handler, invece di stampare il fault e fermarsi,
/// certifica il PASS e congela qui (il selftest finisce in questo handler per
/// disegno: niente chirurgia sul RIP di ritorno, solo log-check).
#[cfg(feature = "selftest")]
pub static EXPECT_NULL_PF: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();

    // ── Eccezioni CPU (0-31) ────────────────────────────────────────
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.general_protection_fault.set_handler_fn(gpf_handler);
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(crate::gdt::DOUBLE_FAULT_IST_INDEX);
    }
    idt.page_fault.set_handler_fn(page_fault_handler);

    // ── Interrupt hardware (32-47) ──────────────────────────────────
    idt[0x20].set_handler_fn(timer_handler); // IRQ 0 → PIT
    idt[0x21].set_handler_fn(keyboard_handler); // IRQ 1 → tastiera PS/2
    idt[0x2E].set_handler_fn(disk_primary_handler); // IRQ 14 → ATA primario
    idt[0x2F].set_handler_fn(disk_secondary_handler); // IRQ 15 → ATA secondario

    // IRQ 2-7 del master, 8-13 dello slave: handler generico.
    for i in 0x22..=0x2D {
        idt[i].set_handler_fn(unhandled_irq_handler);
    }

    idt
});

pub fn init() {
    IDT.load();
    crate::serial_println!("[idt ] installata (16 IRQ hardware abilitate)");
}

// ── Eccezioni CPU ───────────────────────────────────────────────────

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::serial_println!(
        "[int ] #BREAKPOINT @ {:#x} (ritorno all'istruzione successiva)",
        stack_frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let addr = x86_64::registers::control::Cr2::read();
    let fault_addr = addr.map(|a| a.as_u64()).unwrap_or(0);
    let pid = crate::syscall::current_id() as usize;
    let user = error_code.contains(PageFaultErrorCode::USER_MODE);
    let prot = error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION);
    let write = error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE);

    // 27.3 selftest NULL-#PF: fault basso atteso (PML4[0] = 0) con flag armato
    // = prova che il basso e' libero. PASS loggato qui, run congelata qui.
    #[cfg(feature = "selftest")]
    if EXPECT_NULL_PF.load(core::sync::atomic::Ordering::SeqCst)
        && fault_addr < 0x1000
        && !prot
    {
        crate::serial_println!("[test] NULL-#PF ok: il basso e' libero (PML4[0] = 0)");
        halt();
    }

    // 29: un fault di protezione da USER MODE e' un abuso del processo (write
    // su RO, exec su NX, accesso a PROT_NONE): il processo viene terminato
    // (kill), MAI il kernel. Un fault di protezione da supervisor resta un bug
    // del kernel → log + halt in fondo. Stessa politica per un fault user non
    // recuperabile (fuori da ogni regione gestita: es. guard page dello stack).
    // 33: prima di uccidere per protection-violation, tenta il COW fault
    // (vale per fault user E supervisor: il kernel puo' scrivere buffer user
    // COW). Le protection-violation su codice/rodata (senza COW) continuano a
    // uccidere il processo (user) o ad haltare (supervisor, bug del kernel).
    if prot {
        let cr3 = crate::arc::vmm_user::active_cr3();
        if crate::arc::vmm_user::cow_fault(cr3, fault_addr) {
            return;
        }
        if user {
            fault_kill(pid, fault_addr, error_code, &stack_frame);
        }
    }

    // Demand-zero dell'heap on-demand (test lazy): una pagina sotto il
    // `heap_brk` del processo non ancora materializzata viene mappata lazy con
    // un frame zero (vale anche per fault supervisor).
    if !prot && fault_addr >= crate::arc::vmm_user::USER_HEAP_BASE {
        // S-T: l'heap e' del gruppo (tabelle indicizzate al leader).
        let brk = crate::arc::vmm_user::heap_brk(crate::syscall::current_mm());
        if fault_addr < brk {
            let page = fault_addr & !0xfff;
            if let Some(frame) = crate::arc::phys_mem::alloc() {
                unsafe { core::ptr::write_bytes(crate::addr::phys_to_virt(frame) as *mut u8, 0, 4096); }
                let cr3 = crate::arc::vmm_user::active_cr3();
                unsafe { crate::arc::vmm_user::map_user_region_owned(cr3, page, frame, 1); }
                unsafe { flush_page(page) };
                return;
            }
            crate::serial_println!("[int ] heap demand-zero: OOM @ {:#x}", fault_addr);
            // OOM del processo: muore il processo, mai il kernel (come 29).
            fault_kill(pid, fault_addr, error_code, &stack_frame);
        }
    }

    // mmap anonimo (Fase 28/29): fault dentro una VMA viva del basso canonico
    // → materializza con i flag del prot. PROT_NONE o write su RO senza PTE =
    // abuso → kill. Altrimenti demand-zero owned (RW o RO) come l'heap.
    if !prot && fault_addr >= crate::arc::vmm_user::MMAP_BASE
        && fault_addr < crate::arc::vmm_user::MMAP_END
    {
        // S-T: le VMA sono del gruppo (registrate al leader).
        if let Some((vb, vl, vprot, vshm)) = crate::arc::vmm_user::vma_lookup(crate::syscall::current_mm(), fault_addr) {
            use syscall_numbers::{PROT_NONE, PROT_WRITE};
            // VMA condivisa (30): le pagine sono pre-materializzate a
            // `shm_map`; un fault qui e' un edge (PTE staccata) → hole-fill
            // idempotente (33.4: MAI re-map cieco dell'intera regione, che
            // clobbererebbe le copie private delle VMA COW con il contenuto
            // condiviso — le presenti si preservano, si riempiono solo i buchi).
            if vshm != 0 {
                if let Some((phys, frames)) = crate::arc::vmm_user::shm_region(vshm as u32) {
                    let writable = vprot & PROT_WRITE as u8 != 0;
                    let cr3 = crate::arc::vmm_user::active_cr3();
                    let cow = crate::arc::vmm_user::range_has_cow(cr3, vb, frames as usize);
                    crate::arc::vmm_user::remap_shared_holes(cr3, vb, phys, frames as usize, writable, cow);
                    let _ = vl;
                    return;
                }
                fault_kill(pid, fault_addr, error_code, &stack_frame);
            }
            if vprot == PROT_NONE as u8 || (write && vprot & PROT_WRITE as u8 == 0) {
                fault_kill(pid, fault_addr, error_code, &stack_frame);
            }
            let page = fault_addr & !0xfff;
            if let Some(frame) = crate::arc::phys_mem::alloc() {
                unsafe { core::ptr::write_bytes(crate::addr::phys_to_virt(frame) as *mut u8, 0, 4096); }
                let cr3 = crate::arc::vmm_user::active_cr3();
                if vprot & PROT_WRITE as u8 != 0 {
                    unsafe { crate::arc::vmm_user::map_user_region_owned(cr3, page, frame, 1); }
                } else {
                    unsafe { crate::arc::vmm_user::map_user_region_owned_ro(cr3, page, frame, 1); }
                }
                unsafe { flush_page(page) };
                return;
            }
            crate::serial_println!("[int ] mmap demand-zero: OOM @ {:#x}", fault_addr);
            // OOM del processo: muore il processo, mai il kernel (come 29).
            fault_kill(pid, fault_addr, error_code, &stack_frame);
        }
    }

    // Fault user non gestito (es. guard page dello stack, indirizzo fuori
    // regione): il processo muore, il kernel resta vivo.
    if user {
        fault_kill(pid, fault_addr, error_code, &stack_frame);
    }

    crate::serial_println!(
        "[int ] #PAGE FAULT @ {:#x}, err={:?}",
        fault_addr,
        error_code
    );
    let (pnb, pnl) = crate::ordo::sched::process_name(pid);
    let pname = core::str::from_utf8(&pnb[..pnl as usize]).unwrap_or("???");
    crate::serial_println!(
        "[int ] rip={:#x} rsp={:#x} pid={} '{}'",
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.stack_pointer.as_u64(),
        pid,
        pname,
    );
    halt();
}

/// Termina il processo `pid` che ha provocato un fault di memoria non
/// recuperabile (29), loggando indirizzo/errore/rip. Usa `exit_current`
/// (morte logica + switch via, teardown differito): il fault handler gira sul
/// kernel stack del processo e non ci ritorna mai — come il timer handler che
/// fa `switch_to` da IRQ. Non ritorna.
fn fault_kill(
    pid: usize,
    fault_addr: u64,
    error_code: PageFaultErrorCode,
    stack_frame: &InterruptStackFrame,
) -> ! {
    // S2.5b: report PRIMA di qualunque lock (come gpf_handler: un fault
    // dentro una sezione critica SCHED non deve mai morire muto).
    crate::serial_println!(
        "[int ] #PF (kill) @ {:#x}, err={:?} rip={:#x} pid={} (pre-lock)",
        fault_addr,
        error_code,
        stack_frame.instruction_pointer.as_u64(),
        pid,
    );
    let (pnb, pnl) = crate::ordo::sched::process_name(pid);
    let pname = core::str::from_utf8(&pnb[..pnl as usize]).unwrap_or("???");
    crate::serial_println!(
        "[int ] #PF (kill) @ {:#x}, err={:?} rip={:#x} pid={} '{}'",
        fault_addr,
        error_code,
        stack_frame.instruction_pointer.as_u64(),
        pid,
        pname,
    );
    crate::ordo::sched::exit_current(syscall_numbers::FAULT_EXIT_CODE)
}

/// Invalida la TLB per una singola pagina (dopo un demand-map).
unsafe fn flush_page(addr: u64) {
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags));
    }
}

extern "x86-interrupt" fn gpf_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    // S2.5b: report PRIMA di qualunque lock (un fault dentro una sezione
    // critica SCHED dead-lockherebbe il lock non-rientrante prima di parlare:
    // osservato con fxsave disallineato in `new_context`, silenzio totale).
    crate::serial_println!(
        "[int ] #GP err={} @ {:#x} (pre-lock)",
        error_code,
        stack_frame.instruction_pointer.as_u64(),
    );
    let pid = crate::syscall::current_id() as usize;
    let user = stack_frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3;
    let (pnb, pnl) = crate::ordo::sched::process_name(pid);
    let pname = core::str::from_utf8(&pnb[..pnl as usize]).unwrap_or("???");
    crate::serial_println!(
        "[int ] #GP err={} @ {:#x} pid={} '{}'{}",
        error_code,
        stack_frame.instruction_pointer.as_u64(),
        pid,
        pname,
        if user { " (kill)" } else { "" }
    );
    if user {
        // Errore del processo (es. `in`/`out` su una porta non concessa dalla
        // sua I/O bitmap TSS): muore il processo, mai il kernel (come 29).
        crate::ordo::sched::exit_current(syscall_numbers::FAULT_EXIT_CODE)
    }
    halt();
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    crate::serial_println!(
        "[int ] #DOUBLE FAULT @ {:#x} (stack IST attivo)",
        stack_frame.instruction_pointer.as_u64()
    );
    panic!("double fault");
}

// ── Interrupt hardware (32-47) ──────────────────────────────────────

extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    // EOI PRIMA dello scheduling: se on_tick fa uno switch e la CPU si sposta
    // in un altro processo, il PIC non deve restare in attesa di EOI con i
    // successivi timer bloccati.
    unsafe { crate::pic::end_of_interrupt(0x20) };
    crate::ordo::sched::on_tick();
}

extern "x86-interrupt" fn keyboard_handler(_stack_frame: InterruptStackFrame) {
    // Fase 15: routing puro + notify. Il driver PS/2 vive in userspace
    // (`kbd`, servizio `Kbd`): legge lui la porta 0x60 al risveglio (ha
    // `io_ranges` dedicati). Il kernel non tocca piu' porte ne' code: risolve
    // l'owner per nome (restart-safe) e gli accoda una notify
    // (bridge interrupt→IPC: un wake senza messaggio non farebbe mai ritorno
    // da `recv()` — vedi `notify_irq`); EOI in ogni caso (mai wedge). Senza
    // driver registrato i tasti vanno persi finche' kbd non parte.
    // 38.2d — EOI PRIMA della notify: `notify_irq` puo' cambiare contesto
    // (wakeup-preemption) e il PIC va riarmato prima (come il timer sopra).
    unsafe { crate::pic::end_of_interrupt(0x21) };
    if let Some(owner) = crate::relay::channels::lookup(syscall_numbers::Service::Kbd) {
        #[cfg(feature = "sched_debug")]
        crate::serial_println!("[irq1] wake kbd pid={}", owner);
        crate::ordo::sched::notify_irq(owner, syscall_numbers::IRQ_NOTIFY_KBD);
    } else {
        #[cfg(feature = "sched_debug")]
        crate::serial_println!("[irq1] Kbd non registrato");
    }
}

extern "x86-interrupt" fn disk_primary_handler(_stack_frame: InterruptStackFrame) {
    disk_irq(0x2E);
}

extern "x86-interrupt" fn disk_secondary_handler(_stack_frame: InterruptStackFrame) {
    disk_irq(0x2F);
}

/// Corpo comune IRQ14/15 (Fase 38, ATA DMA): routing puro + notify, come
/// IRQ1→kbd. Il driver vive in userspace (`block`, servizio `Disk`): al
/// risveglio chiude il DMA event-driven (38.2). `vector` e' il numero INT
/// (0x2E/0x2F): `notify_end_of_interrupt` fa EOI slave+master per gli IRQ slave.
/// 38.2d — EOI PRIMA della notify: `notify_irq` puo' cambiare contesto
/// (wakeup-preemption) e il PIC va riarmato prima (come il timer). EOI in ogni
/// caso (mai wedge). Senza driver registrato l'IRQ va perso finche' block
/// non parte (come i tasti senza kbd).
fn disk_irq(vector: u8) {
    unsafe { crate::pic::end_of_interrupt(vector) };
    if let Some(owner) = crate::relay::channels::lookup(syscall_numbers::Service::Block) {
        crate::ordo::sched::notify_irq(owner, syscall_numbers::IRQ_NOTIFY_DISK);
    }
}

extern "x86-interrupt" fn unhandled_irq_handler(_stack_frame: InterruptStackFrame) {
    crate::serial_println!("[int ] IRQ non gestito");
    unsafe { crate::pic::end_of_interrupt(0x20) };
}

fn halt() -> ! {
    use x86_64::instructions::hlt;
    loop {
        hlt();
    }
}
