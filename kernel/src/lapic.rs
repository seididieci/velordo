//! Local APIC minimale (L0a, prerequisito virtio-gpu/MSI-X).
//!
//! Accende il LAPIC e lascia il PIC come sorgente degli IRQ legacy:
//! `LINT0` in delivery mode ExtINT inoltra il segnale del PIC alla CPU, che
//! continua a vettorare IRQ0/1/14/15 su 0x20-0x2F. Niente IOAPIC, niente
//! LAPIC timer (il PIT resta lo scheduler tick), niente SMP: si assume UP e
//! dest APIC 0.
//!
//! Il vettore 0x50 e' il config change MSI-X di `Gpu` (installato in IDT da
//! `interrupts.rs`; il device lo genera solo da L0b). 0xFF e' lo spurious.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use x86_64::registers::model_specific::{ApicBase, ApicBaseFlags};
use x86_64::structures::paging::PhysFrame;
use x86_64::PhysAddr;

const APIC_BASE_DEFAULT: u64 = 0xFEE0_0000;

const REG_ID: u32 = 0x20;
const REG_TPR: u32 = 0x80;
const REG_EOI: u32 = 0xB0;
const REG_SVR: u32 = 0xF0;
const REG_ESR: u32 = 0x280;
const REG_LVT_TIMER: u32 = 0x320;
const REG_LVT_LINT0: u32 = 0x350;
const REG_LVT_LINT1: u32 = 0x360;
const REG_LVT_ERROR: u32 = 0x370;

const SVR_ENABLE: u32 = 1 << 8;
const SPURIOUS_VECTOR: u32 = 0xFF;
const LVT_MASKED: u32 = 1 << 16;
const LINT0_EXTINT: u32 = 0x700;
const LINT1_NMI: u32 = 0x400;

/// Base fisica del LAPIC (0 finche' `init` non e' stato chiamato).
static PHYS_BASE: AtomicU64 = AtomicU64::new(0);
static ID: AtomicU32 = AtomicU32::new(0);

pub fn init() {
    // IA32_APIC_BASE: abilita il LAPIC e forza la modalita' xAPIC MMIO
    // (x2APIC avrebbe un'interfaccia MSR, incompatibile con i registri
    // mappati che usiamo qui). `ApicBase::write` preserva i bit riservati.
    let (frame, flags) = ApicBase::read();
    let read_base = frame.start_address().as_u64();
    let base = if read_base == 0 {
        APIC_BASE_DEFAULT
    } else {
        read_base
    };
    let frame = PhysFrame::containing_address(PhysAddr::new(base));
    let flags = (flags | ApicBaseFlags::LAPIC_ENABLE) & !ApicBaseFlags::X2APIC_ENABLE;
    unsafe { ApicBase::write(frame, flags) };
    PHYS_BASE.store(base, Ordering::SeqCst);

    // MMIO: la direct map e' WB per la RAM; la pagina 2M del LAPIC va in UC
    // PRIMA del primo accesso.
    crate::arc::vmm::mark_direct_uncached(base);
    let p = crate::addr::phys_to_virt(base) as *mut u32;

    let (id, svr, esr) = unsafe {
        // SVR: software-enable + spurious 0xFF (entry IDT gia' installata).
        write(p, REG_SVR, SVR_ENABLE | SPURIOUS_VECTOR);
        // TPR 0: nessuna classe di priorita' blocca ExtINT/MSI.
        write(p, REG_TPR, 0);
        // LINT0 ExtINT: il PIC resta la sorgente degli IRQ legacy.
        // LINT1 NMI: path NMI architetturale, non mascherato.
        write(p, REG_LVT_LINT0, LINT0_EXTINT);
        write(p, REG_LVT_LINT1, LINT1_NMI);
        // Timer ed error mascherati: il PIT resta il timer.
        write(p, REG_LVT_TIMER, LVT_MASKED);
        write(p, REG_LVT_ERROR, LVT_MASKED);

        // ESR e' write-1-to-clear: se c'e' un errore residuo lo si azzera.
        let esr = read(p, REG_ESR);
        if esr != 0 {
            write(p, REG_ESR, esr);
        }
        let id = (read(p, REG_ID) >> 24) & 0xFF;
        (id, read(p, REG_SVR), esr)
    };

    ID.store(id, Ordering::SeqCst);
    crate::serial_println!(
        "[lapic] base={:#x} id={} svr={:#x} LINT0=ExtINT (PIC resta attivo)",
        base,
        id,
        svr
    );
    if esr != 0 {
        crate::serial_println!("[lapic] ESR azzerato: {:#x}", esr);
    }
}

/// EOI LAPIC per i vettori MSI (in L0a solo 0x50). Per ExtINT/PIC NON si usa:
/// l'EOI del PIC resta `pic::end_of_interrupt`.
pub fn eoi() {
    let base = PHYS_BASE.load(Ordering::Relaxed);
    if base != 0 {
        unsafe {
            let p = crate::addr::phys_to_virt(base) as *mut u32;
            write(p, REG_EOI, 0);
        }
    }
}

/// APIC ID del core corrente (0 su QEMU UP).
#[allow(dead_code)] // usato da L0b / diagnostica
pub fn id() -> u8 {
    ID.load(Ordering::Relaxed) as u8
}

/// Base fisica del LAPIC (serve all'MSI address in L0b).
#[allow(dead_code)] // usato da L0b
pub fn base() -> u64 {
    PHYS_BASE.load(Ordering::Relaxed)
}

unsafe fn read(p: *mut u32, off: u32) -> u32 {
    unsafe { core::ptr::read_volatile(p.add((off / 4) as usize)) }
}

unsafe fn write(p: *mut u32, off: u32, v: u32) {
    unsafe { core::ptr::write_volatile(p.add((off / 4) as usize), v) }
}
