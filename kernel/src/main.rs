#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]

extern crate alloc;

mod addr;
mod arc;
mod boot_info;
mod boot_tables;
mod cpu;
mod elf;
mod gdt;
mod idle;
mod interrupts;
mod lapic;
mod ordo;
mod pic;
mod pit;
mod relay;
mod serial;
mod syscall;
mod text;
mod user_binary;
mod vga;

use boot_info::HVM_START_MAGIC;
use core::panic::PanicInfo;
use x86_64::instructions::hlt;

#[unsafe(no_mangle)]
pub extern "C" fn rust_main(boot_info_phys: u64) -> ! {
    // Guard 27.2 a stadi (PRIMA di qualunque print): ogni indirizzo sbagliato
    // nel flip e' triple-fault muto — questi sono l'unica diagnostica. Raw
    // serial (porta diretta, niente TICKS, niente heap, niente format): solo
    // immediati e indirizzi linker.
    //
    // Stadio 1: l'immagine (LMA 1M..end) sta nella finestra statica PD_K
    // ([0, 16M) phys)? Altrimenti il codice oltre manca di mappa alta.
    {
        unsafe extern "C" {
            static _kernel_end: u8;
        }
        let kend_phys = crate::addr::kern_virt_to_phys(unsafe { &_kernel_end as *const u8 as u64 });
        if kend_phys >= boot_tables::KERN_IMAGE_PHYS_LIMIT {
            const MSG: &[u8] = b"BOOT IMAGE TOO BIG: kernel exceeds static high window\r\n";
            let mut i = 0usize;
            while i < MSG.len() {
                unsafe {
                    core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") MSG[i]);
                }
                i += 1;
            }
            loop {
                unsafe { core::arch::asm!("hlt") };
            }
        }
    }
    // Stadio 2 (rimosso): le pagine 2M sono baseline long-mode su ogni
    // x86-64 (niente feature, niente CPUID) — la direct map statica non ha
    // prerequisiti oltre il long mode stesso.
    // Stadio 3: CR3 == LMA del PML4 di boot? (stub ha caricato il CR3 giusto?)
    {
        let cr3: u64;
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
        if cr3 & !0xFFF != boot_tables::PML4_ADDR {
            const MSG: &[u8] = b"BOOT BAD CR3: CR3 != BOOT_PML4 LMA\r\n";
            let mut i = 0usize;
            while i < MSG.len() {
                unsafe {
                    core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") MSG[i]);
                }
                i += 1;
            }
            loop {
                unsafe { core::arch::asm!("hlt") };
            }
        }
    }

    // 27.3: il basso canonico finisce qui. Da ora solo alto + direct map:
    // un NULL-deref faulta invece di leggere spazzatura (lo stack e' gia'
    // alto dallo stub; nessun processo user esiste ancora, quindi nessun
    // walk sui PML4 vivi — i futuri ereditano il PML4 pulito).
    arc::vmm::unmap_low();
    serial_println!("[boot] low unmapped: solo alto + direct map");

    gdt::init();
    interrupts::init();
    lapic::init();
    pic::init();
    pit::init();
    syscall::init();
    cpu::init();

    // 27.3 (solo build `selftest`): prova del basso libero PRIMA di qualunque
    // preemption. Il thread di boot non riprende piu' dopo il primo tick
    // (magra pre-esistente scoperta in 27.3: tutto il codice post-BOOT_OK in
    // rust_main — Welcome, selftests(), halt loop — non esegue mai; vedi
    // ADR-0020), quindi la prova regina vive qui, single-thread garantito:
    // IDT installata (riga sopra) + unmap gia' fatto = fault pulito.
    #[cfg(feature = "selftest")]
    selftest_lapic_vector();
    #[cfg(feature = "selftest")]
    selftest_low_unmap();

    let info = unsafe { boot_info::at(boot_info_phys) };
    assert_eq!(
        info.magic, HVM_START_MAGIC,
        "hvm_start_info magic errato: EBX non punta alla struttura PVH"
    );
    boot_info::dump(info);
    // Fase 2 (root su volume): la cmdline resta disponibile a userland via
    // SYS_BOOT_CMDLINE (copia al boot, niente borrow sulla zona loader).
    boot_info::save_cmdline(boot_info::cmdline(info).unwrap_or(""));

    // Calcola max_addr dalla memory map PRIMA di tutto.
    let memmap = boot_info::memmap(info);
    let max_addr = memmap
        .iter()
        .filter(|e| e.kind == boot_info::MEM_RAM)
        .map(|e| e.addr + e.size)
        .max()
        .unwrap_or(256 * 1024 * 1024);

    serial_println!("[boot] max_addr RAM: {:#x} ({} MiB)", max_addr, max_addr / (1024 * 1024));

    // Fase 4 + 27: direct map 64G (verificata) → frame allocator → heap
    arc::vmm::init(max_addr);
    arc::vmm_user::init();

    unsafe extern "C" {
        static _kernel_start: u8;
        static _kernel_end: u8;
    }
    let kernel_start = unsafe { &_kernel_start as *const u8 as u64 };
    let kernel_end = unsafe { &_kernel_end as *const u8 as u64 };

    arc::phys_mem::init(memmap, kernel_start, kernel_end);

    // Riserva la regione del kernel heap NEL frame allocator: se non la si
    // marca "used", i frame che la compongono verrebbero dati ai processi e
    // sovrascriverebbero la free-list dell'heap (corruzione).
    {
        let hs = arc::phys_mem::bitmap_end();
        arc::phys_mem::reserve(crate::addr::virt_to_phys(hs), crate::arc::heap::HEAP_SIZE as u64);
    }

    // Pagina fisica scratch per i test userspace di `map_physical`
    // (usertests, testland): riservata qui cosi' il frame allocator non la
    // assegna a nessun processo.
    {
        arc::phys_mem::reserve(syscall_numbers::MAP_TEST_PHYS, syscall_numbers::MAP_TEST_FRAMES * 4096);
    }

    // Heap: subito dopo bitmap + kernel
    let heap_start = arc::phys_mem::bitmap_end();
    arc::heap::init(heap_start);

    // Fase 5 step 2: scheduler preemptive con context switch reale.
    ordo::sched::init();
    // Ordine spawn = ordine PID: idle=0, init=1 (Linux convention). Gli altri
    // processi user sono spaw da init. I processi kernel non hanno canale di
    // nascita (parent_chan=None, ADR-0008). (Fase 15: il processo `keyboard`
    // e' stato eliminato — il driver PS/2 vive in userspace come `kbd`.)
    ordo::sched::spawn("idle", ordo::sched::Priority::Idle, idle::idle, None, None);

    // Fase 8.1: init, primo processo user (PID 1), antenato dei servizi
    // che poi creera' via syscall `spawn`.
    user_binary::spawn_init();

    serial_println!("BOOT_OK");

    x86_64::instructions::interrupts::enable();

    println!("Welcome to Velordo v0.5");
    println!("Scheduler preemptive timer-driven attivo");
    println!();

    #[cfg(feature = "selftest")]
    selftests();

    loop {
        hlt();
    }
}

#[cfg(feature = "selftest")]
fn selftests() {
    // NOTA (27.3): questo corpo non esegue mai — il thread di boot viene
    // deschedulato per sempre al primo tick (pre-esistente, vedi ADR-0020:
    // feature `selftest` marcita in silenzio, Welcome mai mostrata). La prova
    // 27.3 vive in `selftest_low_unmap()` (pre-preemption). Il resto sotto resta
    // come documentazione del vecchio harness finche' il ciclo vita del thread
    // di boot non viene ridisegnato (follow-up scheduler, fuori 27.3).
    use x86_64::instructions::interrupts::int3;

    serial_println!("[test] int3 -> atteso #BREAKPOINT e continuazione");
    int3();
    serial_println!("[test] int3 ok (siamo tornati)");

    let t0 = pit::ticks();
    for _ in 0..50_000 {
        core::hint::spin_loop();
    }
    let t1 = pit::ticks();
    serial_println!("[test] timer ticks: {} -> {} (differenza {})", t0, t1, t1 - t0);
    if t1 > t0 {
        serial_println!("[test] timer ok");
    } else {
        serial_println!("[test] ERRORE: ticks non avanzati");
    }

    serial_println!("[test] alloc -> Box + Vec");
    let b = alloc::boxed::Box::new(42u64);
    serial_println!("[test] Box value = {}", *b);
    let v = alloc::vec![1u32, 2, 3, 4, 5];
    serial_println!("[test] Vec = {:?}", v.as_slice());
    drop(b);
    drop(v);
    serial_println!("[test] alloc ok");

    serial_println!("[test] frame alloc: liberi prima = {}", arc::phys_mem::free_frames());
    let f1 = arc::phys_mem::alloc().expect("frame alloc fallito");
    let f2 = arc::phys_mem::alloc().expect("frame alloc fallito");
    assert_ne!(f1, f2, "allocatore ha restituito due volte la stessa frame");
    assert!(f1 < arc::vmm::mapped_max() && f2 < arc::vmm::mapped_max());
    serial_println!(
        "[test] frame {} e {} allocati, liberi dopo = {}, usati = {}",
        f1, f2,
        arc::phys_mem::free_frames(),
        arc::phys_mem::used_frames()
    );
    arc::phys_mem::free(f1);
    arc::phys_mem::free(f2);
    serial_println!("[test] frame liberati, liberi di nuovo = {}", arc::phys_mem::free_frames());
    serial_println!("[test] frame alloc ok");
}

/// L0a, solo build `selftest`: la entry IDT 0x50 e l'EOI LAPIC sono
/// raggiungibili anche senza un device MSI-X. `int 0x50` passa dall'handler
/// (`lapic::eoi()` + lookup `Gpu`); qui non c'e' ancora un owner, quindi
/// `notify_irq` non viene chiamato. Va PRIMA di `selftest_low_unmap`, che
/// termina in `halt()`. Non prova la consegna MSI-X reale (L0b).
#[cfg(feature = "selftest")]
fn selftest_lapic_vector() {
    serial_println!("[test] int 0x50 -> handler MSI (LAPIC EOI)");
    unsafe {
        core::arch::asm!("int 0x50", clobber_abi("C"), options(nostack));
    }
    serial_println!("[test] int 0x50 ok");
}

/// 27.3, solo build `selftest`: prova regina del basso libero, eseguita qui
/// (pre-preemption, vedi nota sopra) invece che in `selftests()` (mai
/// raggiunto). Prima l'invariante strutturale (`PML4[0] == 0` dopo l'unmap),
/// poi la prova comportamentale: leggere NULL deve faultare. L'handler
/// certifica il PASS e congela qui per disegno (niente chirurgia sul RIP di
/// ritorno): la build selftest e' una build di prova, verificata via log.
#[cfg(feature = "selftest")]
fn selftest_low_unmap() {
    let pml4_0 = unsafe {
        core::ptr::read_volatile(crate::addr::phys_to_virt(crate::boot_tables::PML4_ADDR) as *const u64)
    };
    assert_eq!(pml4_0, 0, "PML4[0] != 0: identity ancora presente");
    serial_println!("[test] PML4[0] == 0 ok");
    serial_println!("[test] lettura NULL -> atteso #PAGE FAULT");
    crate::interrupts::EXPECT_NULL_PF.store(true, core::sync::atomic::Ordering::SeqCst);
    let _ = unsafe { (0 as *const u64).read_volatile() };
    serial_println!("[test] ERRORE: la lettura NULL non doveva riuscire");
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    crate::serial_println!("[PANIC] {}", info);
    println!("[PANIC] {}", info);
    loop {
        hlt();
    }
}
