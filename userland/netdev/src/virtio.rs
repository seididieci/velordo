//! Trasporto virtio-net **modern** (ADR-0069).
//!
//! Enumerazione PCI, BAR, capability virtio, MSI-X (una entry per entrambe le
//! code), 1 RX + 1 TX split-ring, buffer da 2048 B, nessun offload. Il driver
//! non parla protocolli: copia i frame tra il device e il ring condiviso con
//! il servizio `net`.

use civis::pci::{self, PciDev};
use civis::{
    DEV_MMIO_NET_BASE, MSI_ADDR_BASE, MSI_VECTOR_NET, NET_RING_HDR, NET_RING_SLOTS,
    NET_RX_RING_OFF, NET_SLOT_SIZE, NET_TX_RING_OFF, USER_MMIO_NET_VA,
};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use vela::net::{
    QUEUE_RX, QUEUE_TX, VIRTIO_NET_DEVICE, VIRTIO_NET_DEVICE_LEGACY, VIRTIO_NET_F_MAC,
    VIRTIO_NET_F_STATUS, VIRTIO_NET_HDR_LEN, VIRTIO_NET_S_LINK_UP, VIRTIO_VENDOR,
};

// ── virtio-pci modern: capability e registri ─────────────────────────
const CAP_COMMON: u8 = 1;
const CAP_NOTIFY: u8 = 2;
const CAP_ISR: u8 = 3;
const CAP_DEVICE: u8 = 4;
const CAP_MSIX: u8 = 0x11;

const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
const COMMON_DEVICE_FEATURE: usize = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: usize = 0x08;
const COMMON_DRIVER_FEATURE: usize = 0x0C;
const COMMON_MSIX_CONFIG: usize = 0x10;
const COMMON_DEVICE_STATUS: usize = 0x14;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_SIZE: usize = 0x18;
const COMMON_QUEUE_MSIX_VECTOR: usize = 0x1A;
const COMMON_QUEUE_ENABLE: usize = 0x1C;
const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1E;
const COMMON_QUEUE_DESC: usize = 0x20;
const COMMON_QUEUE_AVAIL: usize = 0x28;
const COMMON_QUEUE_USED: usize = 0x30;

const BAR1_PHYS: u64 = DEV_MMIO_NET_BASE;
const BAR4_PHYS: u64 = DEV_MMIO_NET_BASE + 0x4000;
const BAR4_SIZE: usize = 0x4000;

const QUEUE_MAX: usize = 64;
const BUF_SIZE: usize = 2048;
const DESC_WRITE: u16 = 2;

// Layout del buffer DMA delle code (slot 1).
const RX_DESC_OFF: usize = 0x0000;
const RX_AVAIL_OFF: usize = 0x0400;
const RX_USED_OFF: usize = 0x0500;
const TX_DESC_OFF: usize = 0x1000;
const TX_AVAIL_OFF: usize = 0x1400;
const TX_USED_OFF: usize = 0x1500;
const RX_BUF_OFF: usize = 0x2000;
const TX_BUF_OFF: usize = 0x22000;
/// Pagine del buffer code (0x42000 byte).
const QUEUE_PAGES: usize = 0x42;
/// Pagine del ring condiviso (64 KiB).
const RING_PAGES: usize = 16;

#[derive(Clone, Copy)]
struct Cap {
    cfg_type: u8,
    bar: u8,
    offset: u32,
    notify_mul: u32,
}

pub struct Device {
    pci: PciDev,
    common: *mut u8,
    notify: *mut u8,
    isr: *mut u8,
    device_cfg: *mut u8,
    msix_table: *mut u8,
    msix_cap: u8,
    notify_off_mul: u32,
    queue_notify_off: [u16; 2],
    rx_size: u16,
    tx_size: u16,
    dma_phys: u64,
    dma: *mut u8,
    ring_id: u32,
    ring: *mut u8,
    mac: [u8; 6],
    features: u32,
    rx_avail_idx: u16,
    rx_used_idx: u16,
    tx_avail_idx: u16,
    tx_used_idx: u16,
    tx_free: [bool; QUEUE_MAX],
}

impl Device {
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Id della regione condivisa (shm) del ring, da passare al servizio.
    pub fn ring_id(&self) -> u32 {
        self.ring_id
    }

    pub fn link_up(&self) -> bool {
        if self.features & VIRTIO_NET_F_STATUS == 0 {
            return true;
        }
        let status = unsafe { read_volatile(self.device_cfg.add(6) as *const u16) };
        status & VIRTIO_NET_S_LINK_UP != 0
    }

    /// Legge e azzera l'ISR; true se c'e' un interrupt di coda.
    pub fn handle_irq(&mut self) -> bool {
        let bits = unsafe { read_volatile(self.isr) };
        bits & 1 != 0
    }

    /// Drena il used ring RX nel ring condiviso; ritorna i pacchetti copiati.
    pub fn poll_rx(&mut self) -> usize {
        let used = self.dma as usize + RX_USED_OFF;
        let mut n = 0usize;
        loop {
            let idx = unsafe { read_volatile((used + 2) as *const u16) };
            if idx == self.rx_used_idx {
                break;
            }
            let slot = (self.rx_used_idx % self.rx_size) as usize;
            let id = unsafe { read_volatile((used + 4 + slot * 8) as *const u32) } as u16;
            let len = unsafe { read_volatile((used + 4 + slot * 8 + 4) as *const u32) } as usize;
            self.rx_used_idx = self.rx_used_idx.wrapping_add(1);
            if len > VIRTIO_NET_HDR_LEN && (id as usize) < self.rx_size as usize {
                let pkt = self.dma as usize
                    + RX_BUF_OFF
                    + id as usize * BUF_SIZE
                    + VIRTIO_NET_HDR_LEN;
                let plen = len - VIRTIO_NET_HDR_LEN;
                if self.ring_push_rx(pkt as *const u8, plen) {
                    n += 1;
                }
            }
            self.post_rx(id);
        }
        if n > 0 {
            self.notify(QUEUE_RX);
        }
        n
    }

    /// Consuma il ring TX condiviso e posta i frame al device.
    pub fn tx(&mut self) -> usize {
        let mut n = 0usize;
        loop {
            let head = self.ring_tx_head();
            let tail = self.ring_tx_tail();
            if head == tail {
                break;
            }
            let slot = (head % NET_RING_SLOTS as u32) as usize;
            let p = unsafe {
                self.ring
                    .add(NET_TX_RING_OFF + NET_RING_HDR + slot * NET_SLOT_SIZE)
            };
            let len = unsafe { read_volatile(p as *const u16) } as usize;
            if len == 0 || len > NET_SLOT_SIZE - 2 {
                self.ring_set_tx_head(head.wrapping_add(1));
                continue;
            }
            let Some(id) = self.tx_alloc() else {
                break;
            };
            let dst = self.dma as usize + TX_BUF_OFF + id * BUF_SIZE;
            unsafe {
                core::ptr::write_bytes(dst as *mut u8, 0, VIRTIO_NET_HDR_LEN);
                core::ptr::copy_nonoverlapping(
                    p.add(2),
                    (dst + VIRTIO_NET_HDR_LEN) as *mut u8,
                    len,
                );
            }
            let desc = self.dma as usize + TX_DESC_OFF + id * 16;
            unsafe {
                write_volatile(
                    desc as *mut u64,
                    self.dma_phys + TX_BUF_OFF as u64 + id as u64 * BUF_SIZE as u64,
                );
                write_volatile((desc + 8) as *mut u32, (VIRTIO_NET_HDR_LEN + len) as u32);
                write_volatile((desc + 12) as *mut u16, 0);
                write_volatile((desc + 14) as *mut u16, 0);
            }
            let avail = self.dma as usize + TX_AVAIL_OFF;
            let aslot = (self.tx_avail_idx % self.tx_size) as usize;
            unsafe {
                write_volatile((avail + 4 + aslot * 2) as *mut u16, id as u16);
            }
            fence(Ordering::Release);
            self.tx_avail_idx = self.tx_avail_idx.wrapping_add(1);
            unsafe {
                write_volatile((avail + 2) as *mut u16, self.tx_avail_idx);
            }
            self.ring_set_tx_head(head.wrapping_add(1));
            n += 1;
        }
        if n > 0 {
            self.notify(QUEUE_TX);
        }
        // Reap dei TX completati.
        let used = self.dma as usize + TX_USED_OFF;
        loop {
            let idx = unsafe { read_volatile((used + 2) as *const u16) };
            if idx == self.tx_used_idx {
                break;
            }
            let slot = (self.tx_used_idx % self.tx_size) as usize;
            let id = unsafe { read_volatile((used + 4 + slot * 8) as *const u32) } as usize;
            if id < self.tx_size as usize {
                self.tx_free[id] = true;
            }
            self.tx_used_idx = self.tx_used_idx.wrapping_add(1);
        }
        n
    }

    fn tx_alloc(&mut self) -> Option<usize> {
        for i in 0..self.tx_size as usize {
            if self.tx_free[i] {
                self.tx_free[i] = false;
                return Some(i);
            }
        }
        None
    }

    fn post_rx(&mut self, id: u16) {
        let desc = self.dma as usize + RX_DESC_OFF + id as usize * 16;
        unsafe {
            write_volatile(
                desc as *mut u64,
                self.dma_phys + RX_BUF_OFF as u64 + id as u64 * BUF_SIZE as u64,
            );
            write_volatile((desc + 8) as *mut u32, BUF_SIZE as u32);
            write_volatile((desc + 12) as *mut u16, DESC_WRITE);
            write_volatile((desc + 14) as *mut u16, 0);
        }
        let avail = self.dma as usize + RX_AVAIL_OFF;
        let slot = (self.rx_avail_idx % self.rx_size) as usize;
        unsafe {
            write_volatile((avail + 4 + slot * 2) as *mut u16, id);
        }
        fence(Ordering::Release);
        self.rx_avail_idx = self.rx_avail_idx.wrapping_add(1);
        unsafe {
            write_volatile((avail + 2) as *mut u16, self.rx_avail_idx);
        }
    }

    fn notify(&self, q: u16) {
        let n = unsafe {
            self.notify
                .add(self.queue_notify_off[q as usize] as usize * self.notify_off_mul as usize)
        };
        unsafe { write_volatile(n as *mut u16, q) };
    }

    // ── Ring condiviso (header: head, tail, slots, slot_size) ────────
    fn ring_rx_head(&self) -> u32 {
        unsafe { read_volatile(self.ring.add(NET_RX_RING_OFF) as *const u32) }
    }
    fn ring_rx_tail(&self) -> u32 {
        unsafe { read_volatile(self.ring.add(NET_RX_RING_OFF + 4) as *const u32) }
    }
    fn ring_set_rx_tail(&self, v: u32) {
        unsafe { write_volatile(self.ring.add(NET_RX_RING_OFF + 4) as *mut u32, v) }
    }
    fn ring_tx_head(&self) -> u32 {
        unsafe { read_volatile(self.ring.add(NET_TX_RING_OFF) as *const u32) }
    }
    fn ring_tx_tail(&self) -> u32 {
        unsafe { read_volatile(self.ring.add(NET_TX_RING_OFF + 4) as *const u32) }
    }
    fn ring_set_tx_head(&self, v: u32) {
        unsafe { write_volatile(self.ring.add(NET_TX_RING_OFF) as *mut u32, v) }
    }

    fn ring_push_rx(&mut self, pkt: *const u8, len: usize) -> bool {
        let head = self.ring_rx_head();
        let tail = self.ring_rx_tail();
        if tail.wrapping_sub(head) >= NET_RING_SLOTS as u32 {
            return false;
        }
        let slot = (tail % NET_RING_SLOTS as u32) as usize;
        let p = unsafe {
            self.ring
                .add(NET_RX_RING_OFF + NET_RING_HDR + slot * NET_SLOT_SIZE)
        };
        unsafe {
            write_volatile(p as *mut u16, len as u16);
            core::ptr::copy_nonoverlapping(pkt, p.add(2), len);
        }
        fence(Ordering::Release);
        self.ring_set_rx_tail(tail.wrapping_add(1));
        true
    }

    fn status(&self) -> u8 {
        unsafe { read_volatile(self.common.add(COMMON_DEVICE_STATUS)) }
    }

    fn set_status(&self, status: u8) {
        unsafe { write_volatile(self.common.add(COMMON_DEVICE_STATUS), status) };
    }

    fn negotiate(&mut self) -> Result<(), &'static str> {
        self.set_status(0);
        let mut spins = 0u32;
        while self.status() != 0 {
            spins += 1;
            if spins > 1_000_000 {
                return Err("reset");
            }
            core::hint::spin_loop();
        }
        self.set_status(STATUS_ACK | STATUS_DRIVER);

        unsafe {
            write_volatile(self.common.add(COMMON_DEVICE_FEATURE_SELECT) as *mut u32, 0);
        }
        let feat_lo = unsafe { read_volatile(self.common.add(COMMON_DEVICE_FEATURE) as *const u32) };
        unsafe {
            write_volatile(self.common.add(COMMON_DEVICE_FEATURE_SELECT) as *mut u32, 1);
        }
        let feat_hi = unsafe { read_volatile(self.common.add(COMMON_DEVICE_FEATURE) as *const u32) };
        if feat_hi & 1 == 0 {
            return Err("VIRTIO_F_VERSION_1 assente");
        }
        // Solo MAC (e STATUS se offerto): niente offload, niente MRG_RXBUF.
        let mut want_lo = 0u32;
        if feat_lo & VIRTIO_NET_F_MAC != 0 {
            want_lo |= VIRTIO_NET_F_MAC;
        }
        if feat_lo & VIRTIO_NET_F_STATUS != 0 {
            want_lo |= VIRTIO_NET_F_STATUS;
        }
        self.features = want_lo;
        unsafe {
            write_volatile(self.common.add(COMMON_DRIVER_FEATURE_SELECT) as *mut u32, 0);
            write_volatile(self.common.add(COMMON_DRIVER_FEATURE) as *mut u32, want_lo);
            write_volatile(self.common.add(COMMON_DRIVER_FEATURE_SELECT) as *mut u32, 1);
            write_volatile(self.common.add(COMMON_DRIVER_FEATURE) as *mut u32, 1);
        }
        let status = self.status() | STATUS_FEATURES_OK;
        self.set_status(status);
        if self.status() & STATUS_FEATURES_OK == 0 {
            return Err("FEATURES_OK perso");
        }
        Ok(())
    }

    fn setup_queue(
        &mut self,
        q: u16,
        desc_off: usize,
        avail_off: usize,
        used_off: usize,
    ) -> Result<u16, &'static str> {
        unsafe {
            write_volatile(self.common.add(COMMON_QUEUE_SELECT) as *mut u16, q);
        }
        let size = unsafe { read_volatile(self.common.add(COMMON_QUEUE_SIZE) as *const u16) };
        if size == 0 {
            return Err("queue assente");
        }
        let size = size.min(QUEUE_MAX as u16);
        unsafe {
            write_volatile(self.common.add(COMMON_QUEUE_SIZE) as *mut u16, size);
            write_volatile(self.common.add(COMMON_QUEUE_MSIX_VECTOR) as *mut u16, 0);
            write_volatile(
                self.common.add(COMMON_QUEUE_DESC) as *mut u64,
                self.dma_phys + desc_off as u64,
            );
            write_volatile(
                self.common.add(COMMON_QUEUE_AVAIL) as *mut u64,
                self.dma_phys + avail_off as u64,
            );
            write_volatile(
                self.common.add(COMMON_QUEUE_USED) as *mut u64,
                self.dma_phys + used_off as u64,
            );
            write_volatile(self.common.add(COMMON_QUEUE_ENABLE) as *mut u16, 1);
        }
        let notify_off =
            unsafe { read_volatile(self.common.add(COMMON_QUEUE_NOTIFY_OFF) as *const u16) };
        self.queue_notify_off[q as usize] = notify_off;
        Ok(size)
    }
}

/// Inizializza il device. `Err("assente")` = nessun virtio-net sul bus.
pub fn init() -> Result<Device, &'static str> {
    let pci = pci::find_device(VIRTIO_VENDOR, VIRTIO_NET_DEVICE)
        .or_else(|| pci::find_device(VIRTIO_VENDOR, VIRTIO_NET_DEVICE_LEGACY))
        .ok_or("assente")?;
    civis::println!(
        "[netdev] virtio-net {:04x}:{:04x} @{:02x}:{:02x}.{}",
        VIRTIO_VENDOR,
        pci::config_read16(pci, 0x02),
        pci.bus,
        pci.dev,
        pci.func
    );

    if !program_bar(pci, 1, BAR1_PHYS) {
        return Err("BAR1 readback");
    }
    if !program_bar(pci, 4, BAR4_PHYS) {
        return Err("BAR4 readback");
    }
    let cmd = pci::config_read16(pci, 0x04);
    pci::config_write16(pci, 0x04, cmd | 0x0006);
    civis::println!("[netdev] BAR1={:#x} BAR4={:#x}", BAR1_PHYS, BAR4_PHYS);

    civis::map_mmio(BAR1_PHYS, USER_MMIO_NET_VA, 1).map_err(|_| "map BAR1")?;
    civis::map_mmio(BAR4_PHYS, USER_MMIO_NET_VA + 0x4000, BAR4_SIZE / 0x1000)
        .map_err(|_| "map BAR4")?;

    let caps = collect_caps(pci)?;
    let common = bar_ptr(caps.common.bar)?.wrapping_add(caps.common.offset as usize);
    let notify_base = bar_ptr(caps.notify.bar)?.wrapping_add(caps.notify.offset as usize);
    let isr = bar_ptr(caps.isr.bar)?.wrapping_add(caps.isr.offset as usize);
    let device_cfg = bar_ptr(caps.device.bar)?.wrapping_add(caps.device.offset as usize);

    let (dma_phys, dma_va) = civis::dma_alloc_slot(QUEUE_PAGES, 1).map_err(|_| "DMA code")?;
    // Ring condiviso driver↔servizio: shm (non DMA: il device non lo tocca).
    let ring_id = civis::shm_create(RING_PAGES * 4096).map_err(|_| "shm ring")?;
    let ring_va = civis::shm_map(
        ring_id,
        0,
        civis::PROT_READ | civis::PROT_WRITE,
    )
    .map_err(|_| "shm map")?;

    let mut dev = Device {
        pci,
        common,
        notify: notify_base,
        isr,
        device_cfg,
        msix_table: core::ptr::null_mut(),
        msix_cap: 0,
        notify_off_mul: caps.notify.notify_mul,
        queue_notify_off: [0; 2],
        rx_size: 0,
        tx_size: 0,
        dma_phys,
        dma: dma_va as *mut u8,
        ring_id,
        ring: ring_va as *mut u8,
        mac: [0x02, 0, 0, 0, 0, 0x01],
        features: 0,
        rx_avail_idx: 0,
        rx_used_idx: 0,
        tx_avail_idx: 0,
        tx_used_idx: 0,
        tx_free: [true; QUEUE_MAX],
    };

    dev.negotiate()?;
    dev.rx_size = dev.setup_queue(QUEUE_RX, RX_DESC_OFF, RX_AVAIL_OFF, RX_USED_OFF)?;
    dev.tx_size = dev.setup_queue(QUEUE_TX, TX_DESC_OFF, TX_AVAIL_OFF, TX_USED_OFF)?;

    let (table, cap) = setup_msix(dev.pci)?;
    dev.msix_table = table;
    dev.msix_cap = cap;
    unsafe {
        write_volatile(dev.common.add(COMMON_MSIX_CONFIG) as *mut u16, 0xFFFF);
    }

    // MAC dal device config (offset 0) se negoziato.
    if dev.features & VIRTIO_NET_F_MAC != 0 {
        for i in 0..6 {
            dev.mac[i] = unsafe { read_volatile(dev.device_cfg.add(i)) };
        }
    }

    // Ring condiviso: header + slot azzerati.
    unsafe {
        core::ptr::write_bytes(dev.ring, 0, RING_PAGES * 4096);
        write_volatile(dev.ring.add(NET_RX_RING_OFF) as *mut u32, 0);
        write_volatile(dev.ring.add(NET_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(dev.ring.add(NET_RX_RING_OFF + 8) as *mut u32, NET_RING_SLOTS as u32);
        write_volatile(dev.ring.add(NET_RX_RING_OFF + 12) as *mut u32, NET_SLOT_SIZE as u32);
        write_volatile(dev.ring.add(NET_TX_RING_OFF) as *mut u32, 0);
        write_volatile(dev.ring.add(NET_TX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(dev.ring.add(NET_TX_RING_OFF + 8) as *mut u32, NET_RING_SLOTS as u32);
        write_volatile(dev.ring.add(NET_TX_RING_OFF + 12) as *mut u32, NET_SLOT_SIZE as u32);
    }

    dev.set_status(dev.status() | STATUS_DRIVER_OK);

    // Posta tutti i buffer RX e notifica.
    for i in 0..dev.rx_size {
        dev.post_rx(i);
    }
    dev.notify(QUEUE_RX);

    civis::println!(
        "[netdev] mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} rx={} tx={} link={}",
        dev.mac[0],
        dev.mac[1],
        dev.mac[2],
        dev.mac[3],
        dev.mac[4],
        dev.mac[5],
        dev.rx_size,
        dev.tx_size,
        dev.link_up()
    );
    Ok(dev)
}

fn program_bar(dev: PciDev, idx: u8, base: u64) -> bool {
    let off = 0x10 + idx * 4;
    let is_64 = pci::config_read(dev, off) & 0x6 == 0x4;
    if is_64 {
        pci::config_write(dev, off + 4, (base >> 32) as u32);
    }
    pci::config_write(dev, off, base as u32);
    let lo = pci::config_read(dev, off);
    let hi = if is_64 {
        pci::config_read(dev, off + 4) as u64
    } else {
        0
    };
    let got = (hi << 32) | (lo & !0xF) as u64;
    got == base
}

fn bar_ptr(bar: u8) -> Result<*mut u8, &'static str> {
    match bar {
        1 => Ok(USER_MMIO_NET_VA as *mut u8),
        4 => Ok((USER_MMIO_NET_VA + 0x4000) as *mut u8),
        _ => Err("cap su BAR inattesa"),
    }
}

fn bar_size(bar: u8) -> Result<usize, &'static str> {
    match bar {
        1 => Ok(0x1000),
        4 => Ok(BAR4_SIZE),
        _ => Err("cap su BAR inattesa"),
    }
}

struct Caps {
    common: Cap,
    notify: Cap,
    isr: Cap,
    device: Cap,
}

fn collect_caps(dev: PciDev) -> Result<Caps, &'static str> {
    let mut common = None;
    let mut notify = None;
    let mut isr = None;
    let mut device = None;
    let mut off = pci::cap_first(dev).ok_or("nessuna capability")?;
    for _ in 0..48 {
        if pci::cap_id(dev, off) == 0x09 {
            let cap = parse_virtio_cap(dev, off);
            match cap.cfg_type {
                CAP_COMMON => common = Some(cap),
                CAP_NOTIFY => notify = Some(cap),
                CAP_ISR => isr = Some(cap),
                CAP_DEVICE => device = Some(cap),
                _ => {}
            }
        }
        match pci::cap_next(dev, off) {
            Some(next) => off = next,
            None => break,
        }
    }
    Ok(Caps {
        common: common.ok_or("cap common")?,
        notify: notify.ok_or("cap notify")?,
        isr: isr.ok_or("cap isr")?,
        device: device.ok_or("cap device")?,
    })
}

fn parse_virtio_cap(dev: PciDev, off: u8) -> Cap {
    let at4 = pci::config_read(dev, off + 4);
    let len = pci::config_read8(dev, off + 2);
    let notify_mul = if len >= 20 {
        pci::config_read(dev, off + 16)
    } else {
        0
    };
    Cap {
        cfg_type: pci::config_read8(dev, off + 3),
        bar: (at4 & 0xFF) as u8,
        offset: pci::config_read(dev, off + 8),
        notify_mul,
    }
}

fn setup_msix(dev: PciDev) -> Result<(*mut u8, u8), &'static str> {
    let cap = pci::find_cap(dev, CAP_MSIX).ok_or("cap MSI-X assente")?;
    let mc = pci::config_read16(dev, cap + 2);
    let vectors = (mc & 0x07FF) + 1;
    let table = pci::config_read(dev, cap + 4);
    let bir = (table & 0x7) as u8;
    let offset = (table & !0x7) as usize;
    if offset + vectors as usize * 16 > bar_size(bir)? {
        return Err("tabella MSI-X fuori BAR");
    }
    let base = bar_ptr(bir)? as usize + offset;
    for i in 0..vectors as usize {
        unsafe { write_volatile((base + i * 16 + 12) as *mut u32, 1) };
    }
    unsafe {
        write_volatile(base as *mut u32, MSI_ADDR_BASE as u32);
        write_volatile((base + 4) as *mut u32, (MSI_ADDR_BASE >> 32) as u32);
        write_volatile((base + 8) as *mut u32, MSI_VECTOR_NET as u32);
        write_volatile((base + 12) as *mut u32, 0);
    }
    let mc = (mc | (1 << 15)) & !(1 << 14);
    pci::config_write16(dev, cap + 2, mc);
    Ok((base as *mut u8, cap))
}
