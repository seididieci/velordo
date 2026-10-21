//! Direct-map manager (27.2): la direct map statica [0, 64 GiB) a pagine 2M
//! vive nelle tabelle di boot (`boot_tables.rs`: PDPT_DIRECT + 32 PD).
//! Le pagine 2M sono baseline long-mode su OGNI x86-64: nessun check CPUID,
//! nessun prerequisito oltre il long mode (vale per TCG qemu64 come per
//! l'hardware reale piu' vecchio — le pagine 1G avrebbero richiesto PDPE1GB).
//!
//! Tetto statico 64G: oltre, fail-loud (le configurazioni di test usano
//! <= 32G; alzare il tetto = piu' PD statiche in `boot_tables.rs`, meccanico).
//!
//! `mapped_max` = tetto (byte) della direct map attuale.

use crate::addr::DIRECT_MAP_BASE;

const STATIC_DIRECT_MAX: u64 = 64 * 1024 * 1024 * 1024;

static MAPPED_MAX: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Tetto (byte) della direct map attuale. Indirizzi >= questo non sono mappati.
/// Usato solo dai selftest (`#[cfg(feature = "selftest")]` in main.rs).
#[allow(dead_code)]
pub fn mapped_max() -> u64 {
    MAPPED_MAX.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn init(max_addr: u64) {    if max_addr > STATIC_DIRECT_MAX {
        crate::serial_println!(
            "[vmm] RAM oltre 64G ({:#x}): direct map statica insufficiente",
            max_addr
        );
        loop {
            unsafe { core::arch::asm!("hlt") };
        }
    }

    let mapped = max_addr.min(STATIC_DIRECT_MAX);
    MAPPED_MAX.store(mapped, core::sync::atomic::Ordering::Relaxed);
    let total_mib = mapped / (1024 * 1024);
    crate::serial_println!(
        "[vmm] direct map {:#x} - {:#x}: {} MiB (statica, pagine 2M)",
        DIRECT_MAP_BASE,
        DIRECT_MAP_BASE + mapped,
        total_mib
    );
}

/// 27.3: rimuove l'identity di transizione (`PML4[0] = 0`) + flush TLB globale.
///
/// Da chiamare a inizio `rust_main` (dopo i guard, che girano gia' su stack
/// alto): da qui il basso canonico non e' piu' mappato — un NULL-deref
/// faulta invece di leggere spazzatura. I PML4 user creati dopo ereditano il
/// PML4 kernel gia' pulito (copia integrale in `new_address_space`), quindi
/// nessun walk sui processi vivi: a questo punto non ne esiste ancora alcuno.
/// Le tabelle LOW restano nell'ELF (servono a OGNI boot per la transizione):
/// si pulisce solo la mappa runtime, mai i dati.
pub fn unmap_low() {
    use crate::addr::phys_to_virt;
    use crate::boot_tables::PML4_ADDR;
    unsafe {
        core::ptr::write_volatile(phys_to_virt(PML4_ADDR) as *mut u64, 0);
        // Flush globale: ricarica CR3 (stesso valore, azzera TLB incluse large).
        let cr3: u64;
        core::arch::asm!("mov {cr3}, cr3", cr3 = out(reg) cr3);
        core::arch::asm!("mov cr3, {cr3}", cr3 = in(reg) cr3);
    }
}

/// L0a/L0b: marca UC (PCD|PWT) la pagina 2M della direct map che contiene
/// `phys` (LAPIC, BAR MMIO). Solo PDE large: lo split VGA 4K non si tocca.
///
/// Va chiamata PRIMA del primo accesso MMIO e puo' girare prima di
/// `vmm::init` (le PD statiche sono gia' raggiungibili via direct map).
pub fn mark_direct_uncached(phys: u64) {
    use crate::addr::phys_to_virt;
    use crate::boot_tables::{DIRECT_STATIC_PDS, LARGE_PAGE, PAGE_UC, PD_DIRECT_ADDR};

    let idx = (phys >> 21) as usize;
    if idx >= DIRECT_STATIC_PDS * 512 {
        return;
    }

    let pd = phys_to_virt(PD_DIRECT_ADDR) as *mut u64;
    unsafe {
        let e = pd.add(idx);
        let v = core::ptr::read_volatile(e);
        if v & LARGE_PAGE != 0 {
            core::ptr::write_volatile(e, v | PAGE_UC);
        }
        core::arch::asm!(
            "invlpg [{}]",
            in(reg) DIRECT_MAP_BASE + ((idx as u64) << 21),
            options(nostack, preserves_flags)
        );
    }
}
