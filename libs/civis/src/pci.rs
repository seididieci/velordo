//! Spazio di configurazione PCI in ring 3 (Fase 38.0d, ATA DMA).
//!
//! Modulo condiviso e traslocabile (il servizio `userland/pci` nascerà solo al
//! SECONDO consumer — audio — per YAGNI): oggi lo usa solo block per trovare
//! il PIIX3-IDE di QEMU, programmarne la BAR4 Bus-Master e abilitarne il DMA.
//!
//! Contesto: col boot diretto PVH nessun BIOS programma le BAR — BAR4 all'avvio
//! vale 0 e il Bus-Master e' disabilitato. Siamo noi a scegliere la finestra
//! (`BM_BASE`, QEMU-scoped: sopra il legacy non c'e' nulla di programmato) e a
//! verificarla in lettura. Fuori finestra o senza PIIX3 → `None` e il chiamante
//! resta in PIO (fallback dichiarato, codice PIO intatto).
//!
//! Serve `io_ranges` con `0xCF8-0xCFC` (conf) + la finestra BM (TSS
//! per-processo, ADR-0006): senza, ogni accesso e' #GP. Nota onesta (ADR-0026):
//! chi ha il conf PCI puo' riprogrammare qualunque device — il contenimento
//! vale poco finche' DMA compromesso = game over (no IOMMU).

use crate::pio;

/// Porte dello spazio di configurazione PCI e finestra BM: valori in
/// `syscall-numbers` (single source con kernel e rector).
use syscall_numbers::{PCI_CONF_ADDR, PCI_CONF_DATA, BM_BASE};
pub use syscall_numbers::PIIX_BAR4_SIZE;

/// PIIX3 IDE di QEMU (`-drive if=ide`).
pub const PIIX3_VENDOR: u16 = 0x8086;
pub const PIIX3_IDE_DEVICE: u16 = 0x7010;

/// Funzione PCI (bus/device/function).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciDev {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

/// Indirizzo CONFIG_ADDRESS per `(dev, offset)` (meccanismo 1).
#[inline]
fn conf_addr(dev: PciDev, offset: u8) -> u32 {
    0x8000_0000
        | ((dev.bus as u32) << 16)
        | ((dev.dev as u32) << 11)
        | ((dev.func as u32) << 8)
        | ((offset & 0xFC) as u32)
}

/// Legge 32 bit del conf di `dev` a `offset` (deve essere allineato a 4).
pub fn config_read(dev: PciDev, offset: u8) -> u32 {
    unsafe {
        pio::outl(PCI_CONF_ADDR, conf_addr(dev, offset));
        pio::inl(PCI_CONF_DATA)
    }
}

/// Scrive 32 bit del conf di `dev` a `offset` (deve essere allineato a 4).
pub fn config_write(dev: PciDev, offset: u8, val: u32) {
    unsafe {
        pio::outl(PCI_CONF_ADDR, conf_addr(dev, offset));
        pio::outl(PCI_CONF_DATA, val);
    }
}

/// Legge 8 bit del conf di `dev` a `offset`.
pub fn config_read8(dev: PciDev, offset: u8) -> u8 {
    unsafe {
        pio::outl(PCI_CONF_ADDR, conf_addr(dev, offset));
        pio::inb(PCI_CONF_DATA + (offset & 3) as u16)
    }
}

/// Legge 16 bit del conf di `dev` a `offset` (allineato a 2).
pub fn config_read16(dev: PciDev, offset: u8) -> u16 {
    unsafe {
        pio::outl(PCI_CONF_ADDR, conf_addr(dev, offset));
        pio::inw(PCI_CONF_DATA + (offset & 2) as u16)
    }
}

/// Scrive 8 bit del conf di `dev` a `offset`.
pub fn config_write8(dev: PciDev, offset: u8, val: u8) {
    unsafe {
        pio::outl(PCI_CONF_ADDR, conf_addr(dev, offset));
        pio::outb(PCI_CONF_DATA + (offset & 3) as u16, val);
    }
}

/// Scrive 16 bit del conf di `dev` a `offset` (allineato a 2).
pub fn config_write16(dev: PciDev, offset: u8, val: u16) {
    unsafe {
        pio::outl(PCI_CONF_ADDR, conf_addr(dev, offset));
        pio::outw(PCI_CONF_DATA + (offset & 2) as u16, val);
    }
}

/// Scan del bus 0 (tutte le function) per `vendor:device`.
pub fn find_device(vendor: u16, device: u16) -> Option<PciDev> {
    let want = ((device as u32) << 16) | vendor as u32;
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let d = PciDev { bus: 0, dev, func };
            if config_read(d, 0x00) == want {
                return Some(d);
            }
        }
    }
    None
}

/// Primo offset di capability (status bit 4), se presente.
pub fn cap_first(dev: PciDev) -> Option<u8> {
    let status = config_read16(dev, 0x06);
    if status & 0x0010 == 0 {
        return None;
    }
    let off = config_read8(dev, 0x34) & 0xFC;
    if off == 0 || off < 0x40 {
        None
    } else {
        Some(off)
    }
}

/// Offset della capability successiva nella catena (0 = fine).
pub fn cap_next(dev: PciDev, off: u8) -> Option<u8> {
    let next = config_read8(dev, off + 1) & 0xFC;
    if next == 0 || next < 0x40 {
        None
    } else {
        Some(next)
    }
}

/// Id della capability all'offset `off`.
pub fn cap_id(dev: PciDev, off: u8) -> u8 {
    config_read8(dev, off)
}

/// Cerca la prima capability con id `id` (es. 0x11 = MSI-X).
pub fn find_cap(dev: PciDev, id: u8) -> Option<u8> {
    let mut off = cap_first(dev)?;
    for _ in 0..32 {
        if cap_id(dev, off) == id {
            return Some(off);
        }
        off = cap_next(dev, off)?;
    }
    None
}

/// Indirizzo della BAR `idx` (0..5): maschera i bit di flag e, per le BAR
/// 64-bit, unisce la dword alta. `None` se la BAR e' I/O (non MMIO).
pub fn bar_addr(dev: PciDev, idx: u8) -> Option<u64> {
    let off = 0x10 + idx * 4;
    let lo = config_read(dev, off);
    if lo & 0x1 != 0 {
        return None; // BAR di I/O
    }
    let base = (lo & !0xF) as u64;
    if (lo & 0x6) == 0x4 {
        let hi = config_read(dev, off + 4) as u64;
        Some((hi << 32) | base)
    } else {
        Some(base)
    }
}

/// Cerca il PIIX3-IDE sul bus 0 (tutte le function: su QEMU e' 00:01.1).
/// `None` = assente o diverso (fallback PIO, mai hang: solo letture bound).
pub fn find_piix3_ide() -> Option<PciDev> {
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let d = PciDev { bus: 0, dev, func };
            let id = config_read(d, 0x00);
            if id == ((PIIX3_IDE_DEVICE as u32) << 16) | (PIIX3_VENDOR as u32) {
                return Some(d);
            }
        }
    }
    None
}

/// Programma la BAR4 (offset 0x20) a `BM_BASE` e abilita I/O Space + Bus Master
/// nel command (offset 0x04, bit 0 e 2, resto preservato). Ritorna la BMIBA se
/// la readback conferma, altrimenti `None` (PIO).
///
/// Se la BAR vale gia' `BM_BASE` (restart block senza reboot: la BAR
/// sopravvive) la si tiene; altrimenti la si riprogramma — QEMU di default la
/// pre-programma a `0xC040`, fuori dal nostro grant statico. Col boot diretto
/// nessun driver e' live sul Bus-Master (il PIO usa le porte legacy, non la
/// BAR4): la readback e' la verifica, se non tiene si resta in PIO.
pub fn enable_bus_master(dev: PciDev) -> Option<u16> {
    let bar = config_read(dev, 0x20);
    if bar != ((BM_BASE as u32) | 0x1) {
        config_write(dev, 0x20, (BM_BASE as u32) | 0x1);
        let back = config_read(dev, 0x20);
        if back != ((BM_BASE as u32) | 0x1) {
            return None;
        }
    }
    // Command: preserva, accendi I/O Space (bit 0) + Bus Master (bit 2).
    let cmd = (config_read(dev, 0x04) & 0xFFFF) as u16;
    let cmd_addr_val = (config_read(dev, 0x04) & 0xFFFF_0000) | ((cmd | 0x0005) as u32);
    config_write(dev, 0x04, cmd_addr_val);
    Some(BM_BASE)
}

/// Linea IRQ PCI del device (offset 0x3C, byte basso): solo diagnostica — il
/// routing resta via PIC fisso (IRQ14/15, Fase 38.0c), mai via questa.
pub fn irq_line(dev: PciDev) -> u8 {
    (config_read(dev, 0x3C) & 0xFF) as u8
}
