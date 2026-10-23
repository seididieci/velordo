//! Tabelle di boot 27.2: dual-map di transizione + kernel alto + direct map.
//!
//! Sono `static` const-valutate: il compilatore le emette gia' pronte nell'ELF
//! (LMA basse, VMA alte) e il loader PVH le carica in RAM insieme al resto.
//! Lo stub (`boot.asm`, tutto RIP-relative + `SYM - low32(OFFSET)` a runtime)
//! punta CR3 alla LMA del PML4, abilita il paging e salta HIGH.
//!
//! PML4 (LMA 0x90000, via CR3 phys):
//!   [0]    → PDPT_LOW: identity [0, 8M) di TRANSIZIONE (stub LOW, stack di
//!             transizione, tabelle stesse). Rimossa in 27.3 (`PML4[0] = 0`).
//!   [511]  → PDPT_K: immagine kernel alta (VMA `KERN_VIRT_BASE`, pagine 2M,
//!             finestra VMA [base1G, +16M) -> phys [0, 16M)).
//!   [DIR]  → PDPT_DIRECT: direct map [0, 64G) a pagine 1G (top-up oltre i
//!             64G in `vmm.rs`: 1 entry per GiB extra, zero nuove tabelle).
//! Permessi: PRESENT|WRITABLE, supervisor (U=0) — come prima (niente W^X qui).
//!
//! NB: in long mode le entry delle page table sono larghe 8 byte — qui è
//! garantito dal tipo (`u64`), l'errore classico dello stride a 4 byte non
//! può nemmeno compilare.
//!
//! I simboli servono a `boot.asm` via indirizzo (VMA alta, LMA derivata a
//! runtime): il linker li tiene vivi, ma il compilatore non vede usi Rust —
//! allow mirato, non rimosso.
#![allow(dead_code)]

use core::mem::size_of;

use crate::addr::{DIRECT_MAP_BASE, KERN_VIRT_BASE};

pub const PML4_ADDR: u64 = 0x0009_0000;
pub const PDPT_ADDR: u64 = 0x0009_1000;
pub const PD_ADDR: u64 = 0x0009_2000;
pub const PT_ADDR: u64 = 0x0009_3000;
/// PDPT dell'immagine kernel alta (PML4[511]).
pub const PDPT_K_ADDR: u64 = 0x0009_4000;
/// PD dell'immagine kernel alta (finestra 16M a pagine 2M).
pub const PD_K_ADDR: u64 = 0x0009_5000;
/// PDPT della direct map (32 entry -> 32 PD da 1G = 64G statici, pagine 2M).
pub const PDPT_DIRECT_ADDR: u64 = 0x0009_6000;
/// PD della direct map: 32 tabelle contigue (128 KiB).
/// LMA FISSA a 16M (sezione `.tables_high`, vedi linker.ld): sotto 1M solo
/// 0x90000-0x9FC00 e' RAM (il resto e' buco PCI/VGA: letture 0xFF, osservato
/// con PT_VGA a 0xB7000). 16M e' oltre heap+bitmap per ogni config testata
/// (<= 32G; max 64G) ed e' verificata RAM da `phys_mem::init` (fail-loud).
/// La pagina scratch dei test (MAP_TEST_PHYS) sta ALTROVE per invariante
/// compilata (vedi sotto): scriverci sopra le PD fu il fault ritardato di 27.3.
pub const TABLES_HIGH_START: u64 = 0x100_0000;
/// Pagine `.tables_high`: 32 PD + 1 PT VGA.
pub const TABLES_HIGH_PAGES: u64 = 33;
/// Fine (esclusiva) dell'area tabelle alte.
pub const TABLES_HIGH_END: u64 = TABLES_HIGH_START + TABLES_HIGH_PAGES * 4096;
pub const PD_DIRECT_ADDR: u64 = 0x100_0000;
/// PT dello split VGA (stessa sezione, dopo le 32 PD).
pub const PT_VGA_ADDR: u64 = 0x100_0000 + 32 * 4096;
/// PD statiche della direct map (ognuna 1G a pagine 2M).
pub const DIRECT_STATIC_PDS: usize = 32;
/// Indice 4K della pagina VGA dentro PT_VGA (0xB8000 / 4K).
const VGA_PT_IDX: usize = 0xB8;
/// Bit UC per PTE 4K con PAT di reset (PCD|PWT = indice PAT 3 = UC).
pub(crate) const PAGE_UC: u64 = 0x018;

const PRESENT_WRITABLE: u64 = 0x003;
/// Bit PS: large page (2M nel PD, 1G nel PDPT).
pub(crate) const LARGE_PAGE: u64 = 0x080;

/// Indice PML4 della VMA kernel (= 511 per KERN_VIRT_BASE -2G).
const PML4_KERN: usize = ((KERN_VIRT_BASE >> 39) & 0x1FF) as usize;
/// Indice PDPT della VMA kernel dentro PDPT_K.
const PDPT_K_IDX: usize = ((KERN_VIRT_BASE >> 30) & 0x1FF) as usize;
/// Indice PML4 della direct map.
const PML4_DIRECT: usize = ((DIRECT_MAP_BASE >> 39) & 0x1FF) as usize;

/// Tetto PHYS (esclusivo) della finestra immagine: la PD_K mappa VMA
/// [KERN-1M, +16M) -> phys [0, 16M); l'immagine (phys 1M..end) cade dentro.
/// `rust_main` abortisce fail-loud se `_kernel_end` fisico lo supera
/// (stesso ruolo del vecchio BOOT_MAP_LIMIT).
pub const KERN_IMAGE_PHYS_LIMIT: u64 = 0x100_0000;

/// Tabella con allineamento garantito a pagina.
#[repr(C, align(4096))]
#[derive(Clone, Copy)]
pub struct PageTable([u64; 512]);

impl PageTable {
    /// Limite per lo pseudo-descrittore LGDT (512 entry × 8 byte − 1).
    pub const LIMIT: u16 = (size_of::<Self>() - 1) as u16;
}

#[unsafe(link_section = ".pagetables.pml4")]
#[unsafe(no_mangle)]
pub static BOOT_PML4: PageTable = {
    let mut t = [0u64; 512];
    t[0] = PDPT_ADDR | PRESENT_WRITABLE;
    t[PML4_KERN] = PDPT_K_ADDR | PRESENT_WRITABLE;
    t[PML4_DIRECT] = PDPT_DIRECT_ADDR | PRESENT_WRITABLE;
    PageTable(t)
};

#[unsafe(link_section = ".pagetables.pdpt")]
#[unsafe(no_mangle)]
pub static BOOT_PDPT: PageTable = {
    let mut t = [0u64; 512];
    t[0] = PD_ADDR | PRESENT_WRITABLE;
    PageTable(t)
};

#[unsafe(link_section = ".pagetables.pd")]
#[unsafe(no_mangle)]
pub static BOOT_PD: PageTable = {
    let mut t = [0u64; 512];
    t[0] = PT_ADDR | PRESENT_WRITABLE;
    // Large page 2M: identity di transizione [2M, 8M).
    t[1] = 0x200000 | PRESENT_WRITABLE | LARGE_PAGE;
    t[2] = 0x400000 | PRESENT_WRITABLE | LARGE_PAGE;
    t[3] = 0x600000 | PRESENT_WRITABLE | LARGE_PAGE;
    PageTable(t)
};

/// Identity di transizione [0, 2M) a 4K (stub LOW, stack, VGA, tabelle).
#[unsafe(link_section = ".pagetables.pt")]
#[unsafe(no_mangle)]
pub static BOOT_PT: PageTable = {
    let mut t = [0u64; 512];
    let mut i = 0usize;
    while i < 512 {
        t[i] = ((i as u64) << 12) | PRESENT_WRITABLE;
        i += 1;
    }
    PageTable(t)
};

/// PDPT dell'immagine kernel: una sola entry verso PD_K.
#[unsafe(link_section = ".pagetables.pdpt_k")]
#[unsafe(no_mangle)]
pub static BOOT_PDPT_K: PageTable = {
    let mut t = [0u64; 512];
    t[PDPT_K_IDX] = PD_K_ADDR | PRESENT_WRITABLE;
    PageTable(t)
};

/// PD dell'immagine kernel: 8 large page 2M — VMA [KERN-1M, +16M) -> phys
/// [0, 16M), cioe' VMA [KERN+i*2M) -> phys [1M+i*2M): lo scarto uniforme di
/// 1M (KERN_VIRT_BASE = -2G+1M, vedi `addr.rs`) rende TUTTE le basi pari e
/// 2M-allineate. Senza lo scarto (VMA tonda a -2G, LMA 1M) le basi sarebbero
/// dispari (bit 20 = riservato per pagine 2M -> #PF con RSVD, osservato).
/// L'immagine (VMA KERN..end = phys 1M..) cade dentro; oltre = guard fail-loud.
#[unsafe(link_section = ".pagetables.pd_k")]
#[unsafe(no_mangle)]
pub static BOOT_PD_K: PageTable = {
    let mut t = [0u64; 512];
    let mut i = 0u64;
    while i < 8 {
        t[i as usize] = (i << 21) | PRESENT_WRITABLE | LARGE_PAGE;
        i += 1;
    }
    PageTable(t)
};

/// PDPT della direct map: 32 entry -> 32 PD (64G statici, pagine 2M).
/// 2M e' baseline long-mode su OGNI x86-64 (niente CPUID, niente feature):
/// funziona sul TCG qemu64 come sull'hardware reale piu' vecchio.
#[unsafe(link_section = ".pagetables.pdpt_direct")]
#[unsafe(no_mangle)]
pub static BOOT_PDPT_DIRECT: PageTable = {
    let mut t = [0u64; 512];
    let mut i = 0u64;
    while i < DIRECT_STATIC_PDS as u64 {
        t[i as usize] = (PD_DIRECT_ADDR + i * 4096) | PRESENT_WRITABLE;
        i += 1;
    }
    PageTable(t)
};

/// PD della direct map: 32 tabelle (128 KiB contigui) — phys [0, 64G) a
/// pagine 2M. ECCEZIONE: entry [0][0] (primi 2M, che contengono il buffer
/// VGA) punta a PT_VGA invece che a una large page: la direct map e' WB, ma
/// l'MMIO VGA vuole UC (su QEMU invisibile, su HW reale letture stale).
/// Oltre i 64G: `vmm::init` abortisce fail-loud (le configurazioni di test
/// usano <= 32G; alzare il tetto = piu' PD statiche, meccanico).
#[unsafe(link_section = ".tables_high.pd_direct")]
#[unsafe(no_mangle)]
pub static BOOT_PD_DIRECT: [PageTable; DIRECT_STATIC_PDS] = {
    let mut arr = [PageTable([0u64; 512]); DIRECT_STATIC_PDS];
    let mut j = 0usize;
    while j < DIRECT_STATIC_PDS {
        let mut k = 0u64;
        while k < 512 {
            if j == 0 && k == 0 {
                // Split VGA: PT 4K al posto della large page.
                arr[j].0[k as usize] = PT_VGA_ADDR | PRESENT_WRITABLE;
            } else {
                arr[j].0[k as usize] = ((j as u64 * 512 + k) << 21) | PRESENT_WRITABLE | LARGE_PAGE;
            }
            k += 1;
        }
        j += 1;
    }
    arr
};

/// PT dello split VGA (27.3): 512 pagine 4K per VMA [DIRBASE, +2M) -> phys
/// [0, 2M), tutte WB tranne la pagina del buffer (UC). Una pagina da 4K,
/// statica, zero codice: il prezzo dell'MMIO corretto su HW reale.
#[unsafe(link_section = ".tables_high.pt_vga")]
#[unsafe(no_mangle)]
pub static BOOT_PT_VGA: PageTable = {
    let mut t = [0u64; 512];
    let mut i = 0u64;
    while i < 512 {
        if (i as usize) == VGA_PT_IDX {
            t[i as usize] = (i << 12) | PRESENT_WRITABLE | PAGE_UC;
        } else {
            t[i as usize] = (i << 12) | PRESENT_WRITABLE;
        }
        i += 1;
    }
    PageTable(t)
};

/// Stack alto di boot (27.3): 16 KiB in .bss (VMA alta, finestra PD_K).
/// Lo stub vi commuta RSP subito dopo il jump-high, prima di `rust_main`:
/// dopo l'unmap di PML4[0] lo stack LOW di transizione non e' piu' mappato.
#[repr(C, align(16))]
pub struct BootStack([u8; 16384]);

#[unsafe(no_mangle)]
pub static BOOT_HIGH_STACK: BootStack = BootStack([0; 16384]);

/// Selettori della GDT di boot.
pub const SEL_CODE64: u16 = 0x08;
pub const SEL_DATA: u16 = 0x10;

/// null | code64 (L=1 D=0 G=1) | data (RW)
#[repr(C, align(8))]
pub struct Gdt([u64; 3]);

#[unsafe(no_mangle)]
pub static BOOT_GDT: Gdt = Gdt([
    0x0000_0000_0000_0000,
    0x00AF_9A00_0000_FFFF,
    0x00CF_9200_0000_FFFF,
]);
