//! Feature CPU opzionali oltre il baseline long-mode (S-T, T2).
//!
//! Il boot richiede solo long-mode + 2M paging (niente CPUID). Qui si
//! abilitano le estensioni che rendono la TLS veloce ma non sono
//! indispensabili: FSGSBASE (rdfsbase/wrfsbase da ring 3 per TLS senza
//! syscall — la via MSR resta il fallback e la usa il kernel allo switch).
//! Senza FSGSBASE il sistema funziona (TLS via MSR programmato dal kernel),
//! solo std userà la via lenta.

use core::arch::x86_64::{__cpuid, __cpuid_count};

/// Presenza FSGSBASE (CPUID leaf 7 EBX bit 0). Cache a init: la CPU non
/// cambia a runtime (single core, niente hotplug).
static mut HAS_FSGSBASE: bool = false;

/// Abilita FSGSBASE (CR4 bit 16) se la CPU lo supporta. Da chiamare a init,
/// prima che qualunque thread user esista (l'MSR FS_BASE programmato dal
/// kernel allo switch non dipende da questo bit; abilitarlo dopo non sposta
/// le basi correnti, solo sblocca le istruzioni).
/// S2.5b (FPU eager): accende anche OSFXSR+OSXMMEXCPT (CR4 bit 9+10, presenti
/// su ogni x86_64: senza, le SSE faultano #UD) e inizializza l'area fxsave
/// del contesto di boot (altrimenti il primo switch la salverebbe da zeri
/// grezzi con FCW smascherato).
pub fn init() {
    // Leaf 0: max basic leaf (niente structured-extended sotto il 7).
    let max_basic = unsafe { __cpuid(0) }.eax;
    let supported = if max_basic >= 7 {
        unsafe { __cpuid_count(7, 0) }.ebx & (1 << 0) != 0
    } else {
        false
    };
    if supported {
        let mut cr4: u64;
        unsafe {
            core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nostack, preserves_flags));
            cr4 |= 1 << 16; // CR4.FSGSBASE
            core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
        }
    }
    unsafe {
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nostack, preserves_flags));
        cr4 |= (1 << 9) | (1 << 10); // CR4.OSFXSR + CR4.OSXMMEXCPT
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
        crate::ordo::sched::ctx::init_boot_fpu();
    }
    unsafe {
        HAS_FSGSBASE = supported;
    }
    crate::serial_println!(
        "[cpu  ] FSGSBASE {} (TLS user {})",
        if supported { "on" } else { "off (MSR-only)" },
        if supported { "rdfsbase/wrfsbase" } else { "syscall" }
    );
}

/// FSGSBASE disponibile per ring 3? (La suite threadtest adatta il test TLS:
/// isolamento via rdfsbase se presente, smoke set_fs altrimenti.)
pub fn has_fsgsbase() -> bool {
    unsafe { HAS_FSGSBASE }
}
