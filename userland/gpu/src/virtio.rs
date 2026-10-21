//! Trasporto virtio-pci **modern** per `virtio-gpu` (L0b, ADR-0057).
//!
//! Enumerazione PCI, programmazione BAR, capability virtio, MSI-X (una entry
//! per il *config change*), controlq split-ring in polling e un comando reale
//! (`GET_DISPLAY_INFO`). Il device non sostituisce la VGA: se qualcosa manca
//! o non torna, il driver resta sul path VGA (fallback dichiarato).
//!
//! Layout: BAR1 (4 KiB) ospita la tabella MSI-X, BAR4 (16 KiB) il modern
//! transport (common @0x0000, ISR @0x1000, device @0x2000, notify @0x3000).
//! Le due BAR sono programmate nella finestra `DEV_MMIO_BASE` e mappate UC
//! dal kernel con `SYS_MAP_MMIO` (grant dell'owner di `Service::Gpu`).

use civis::pci::{self, PciDev};
use civis::{DEV_MMIO_BASE, MSI_ADDR_BASE, USER_MMIO_VA};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use vela::gpu::{
    VIRTIO_F_VERSION_1, VIRTIO_GPU_DEVICE, VIRTIO_GPU_EVENT_DISPLAY, VIRTIO_VENDOR,
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

/// Offset del common config (relativi alla capacita').
const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
const COMMON_DEVICE_FEATURE: usize = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: usize = 0x08;
const COMMON_DRIVER_FEATURE: usize = 0x0C;
const COMMON_MSIX_CONFIG: usize = 0x10;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_SIZE: usize = 0x18;
const COMMON_QUEUE_MSIX_VECTOR: usize = 0x1A;
const COMMON_QUEUE_ENABLE: usize = 0x1C;
const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1E;
const COMMON_QUEUE_DESC: usize = 0x20;
const COMMON_QUEUE_AVAIL: usize = 0x28;
const COMMON_QUEUE_USED: usize = 0x30;
const COMMON_DEVICE_STATUS: usize = 0x14;

/// Offsets relativi alla BAR della capability ISR.
const ISR_CONFIG_CHANGE: u8 = 0x02;

/// BAR1 = tabella MSI-X (4 KiB), BAR4 = modern transport (16 KiB).
const BAR1_PHYS: u64 = DEV_MMIO_BASE;
const BAR4_PHYS: u64 = DEV_MMIO_BASE + 0x4000;
const BAR4_SIZE: usize = 0x4000;

/// Pagina DMA (split ring + request + response).
const DMA_DESC_OFF: usize = 0x0000;
const DMA_AVAIL_OFF: usize = 0x0400;
const DMA_USED_OFF: usize = 0x0500;
const DMA_REQUEST_OFF: usize = 0x0800;
const DMA_RESPONSE_OFF: usize = 0x0900;
/// Byte utilizzabili nel buffer di richiesta (0x800..0x900).
const DMA_REQUEST_MAX: usize = 0x100;
/// Byte utilizzabili nel buffer di risposta (0x900..fine pagina).
const DMA_RESPONSE_MAX: usize = 0x700;

const QUEUE_MAX: usize = 64;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;

/// Capacita' virtio trovata nella config PCI.
#[derive(Clone, Copy)]
struct Cap {
    cfg_type: u8,
    bar: u8,
    offset: u32,
    notify_mul: u32,
}

/// Device virtio-gpu inizializzato (transport + controlq + MSI-X).
pub struct Device {
    pci: PciDev,
    common: *mut u8,
    notify: *mut u8,
    isr: *mut u8,
    device_cfg: *mut u8,
    msix_table: *mut u8,
    msix_cap: u8,
    notify_off_mul: u32,
    queue_notify_off: u16,
    queue_size: u16,
    dma_phys: u64,
    dma: *mut u8,
    avail_idx: u16,
    used_idx: u16,
}

impl Device {
    /// Legge e azzera l'ISR. Ritorna true se c'e' stato un config change.
    pub fn handle_irq(&mut self) -> bool {
        let bits = unsafe { read_volatile(self.isr) };
        if bits & ISR_CONFIG_CHANGE == 0 {
            return false;
        }
        let events = unsafe { read_volatile(self.device_cfg as *const u32) };
        if events != 0 {
            unsafe { write_volatile(self.device_cfg.add(4) as *mut u32, events) };
        }
        if events & VIRTIO_GPU_EVENT_DISPLAY == 0 {
            return false;
        }
        civis::println!("[gpu] config change: events={:#x}", events);
        if let Some(info) = self.display_info() {
            log_display_info(&info);
        }
        true
    }
}

/// Risoluzione riportata da `GET_DISPLAY_INFO`.
#[derive(Clone, Copy)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub enabled: u32,
}

fn log_display_info(info: &DisplayInfo) {
    civis::println!(
        "[gpu] display info: {}x{} enabled={}",
        info.width,
        info.height,
        info.enabled
    );
}

/// Inizializza il device. `Err("assente")` = nessun virtio-gpu sul bus;
/// ogni altro errore e' un init fallito (il chiamante resta su VGA).
pub fn init() -> Result<Device, &'static str> {
    let pci = match pci::find_device(VIRTIO_VENDOR, VIRTIO_GPU_DEVICE) {
        Some(d) => d,
        None => return Err("assente"),
    };
    civis::println!(
        "[gpu] virtio-gpu {:04x}:{:04x} @{:02x}:{:02x}.{}",
        VIRTIO_VENDOR,
        VIRTIO_GPU_DEVICE,
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
    // Memory Space + Bus Master (il resto del command resta com'e').
    let cmd = pci::config_read16(pci, 0x04);
    pci::config_write16(pci, 0x04, cmd | 0x0006);
    civis::println!("[gpu] BAR1={:#x} BAR4={:#x}", BAR1_PHYS, BAR4_PHYS);

    civis::map_mmio(BAR1_PHYS, USER_MMIO_VA, 1).map_err(|_| "map BAR1")?;
    civis::map_mmio(BAR4_PHYS, USER_MMIO_VA + 0x4000, BAR4_SIZE / 0x1000)
        .map_err(|_| "map BAR4")?;

    let caps = collect_caps(pci)?;
    let common = bar_ptr(caps.common.bar)?.wrapping_add(caps.common.offset as usize);
    let notify_base = bar_ptr(caps.notify.bar)?.wrapping_add(caps.notify.offset as usize);
    let isr = bar_ptr(caps.isr.bar)?.wrapping_add(caps.isr.offset as usize);
    let device_cfg = bar_ptr(caps.device.bar)?.wrapping_add(caps.device.offset as usize);

    let mut dev = Device {
        pci,
        common,
        notify: notify_base,
        isr,
        device_cfg,
        msix_table: core::ptr::null_mut(),
        msix_cap: 0,
        notify_off_mul: caps.notify.notify_mul,
        queue_notify_off: 0,
        queue_size: 0,
        dma_phys: 0,
        dma: core::ptr::null_mut(),
        avail_idx: 0,
        used_idx: 0,
    };

    dev.negotiate()?;
    dev.setup_controlq()?;
    let (table, cap) = setup_msix(dev.pci)?;
    dev.msix_table = table;
    dev.msix_cap = cap;
    // Config change -> entry 0 (te lo dice il device via msix_config).
    unsafe {
        write_volatile(
            dev.common.add(COMMON_MSIX_CONFIG) as *mut u16,
            0,
        );
    }
    civis::println!(
        "[gpu] MSI-X: {} vect, config=0 -> {:#x}, queue=0xffff",
        dev.msix_vectors(),
        civis::MSI_VECTOR_GPU
    );

    dev.set_status(dev.status() | STATUS_DRIVER_OK);

    match dev.display_info() {
        Some(info) => {
            log_display_info(&info);
            Ok(dev)
        }
        None => {
            dev.disable_msix();
            Err("timeout GET_DISPLAY_INFO")
        }
    }
}

impl Device {
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

        // Feature alte: serve solo VERSION_1 (bit 32).
        unsafe {
            write_volatile(
                self.common.add(COMMON_DEVICE_FEATURE_SELECT) as *mut u32,
                1,
            );
        }
        let feat_hi = unsafe {
            read_volatile(self.common.add(COMMON_DEVICE_FEATURE) as *const u32)
        };
        if feat_hi & VIRTIO_F_VERSION_1 as u32 == 0 {
            return Err("VIRTIO_F_VERSION_1 assente");
        }
        unsafe {
            write_volatile(
                self.common.add(COMMON_DRIVER_FEATURE_SELECT) as *mut u32,
                0,
            );
            write_volatile(self.common.add(COMMON_DRIVER_FEATURE) as *mut u32, 0);
            write_volatile(
                self.common.add(COMMON_DRIVER_FEATURE_SELECT) as *mut u32,
                1,
            );
            write_volatile(
                self.common.add(COMMON_DRIVER_FEATURE) as *mut u32,
                VIRTIO_F_VERSION_1 as u32,
            );
        }

        let status = self.status() | STATUS_FEATURES_OK;
        self.set_status(status);
        if self.status() & STATUS_FEATURES_OK == 0 {
            return Err("FEATURES_OK perso");
        }
        Ok(())
    }

    fn setup_controlq(&mut self) -> Result<(), &'static str> {
        let dma_phys = civis::dma_alloc(1).map_err(|_| "DMA alloc")?;
        self.dma_phys = dma_phys;
        self.dma = civis::USER_DMA_VA as *mut u8;

        unsafe {
            write_volatile(self.common.add(COMMON_QUEUE_SELECT) as *mut u16, 0);
        }
        let size = unsafe {
            read_volatile(self.common.add(COMMON_QUEUE_SIZE) as *const u16)
        };
        if size == 0 {
            return Err("controlq assente");
        }
        let size = size.min(QUEUE_MAX as u16);
        self.queue_size = size;

        unsafe {
            write_volatile(self.common.add(COMMON_QUEUE_SIZE) as *mut u16, size);
            // Controlq in polling: nessun vettore MSI-X per la queue.
            write_volatile(
                self.common.add(COMMON_QUEUE_MSIX_VECTOR) as *mut u16,
                0xFFFF,
            );
            write_volatile(
                self.common.add(COMMON_QUEUE_DESC) as *mut u64,
                dma_phys + DMA_DESC_OFF as u64,
            );
            write_volatile(
                self.common.add(COMMON_QUEUE_AVAIL) as *mut u64,
                dma_phys + DMA_AVAIL_OFF as u64,
            );
            write_volatile(
                self.common.add(COMMON_QUEUE_USED) as *mut u64,
                dma_phys + DMA_USED_OFF as u64,
            );
            write_volatile(self.common.add(COMMON_QUEUE_ENABLE) as *mut u16, 1);
        }
        self.queue_notify_off = unsafe {
            read_volatile(self.common.add(COMMON_QUEUE_NOTIFY_OFF) as *const u16)
        };
        Ok(())
    }

    /// Esegue `GET_DISPLAY_INFO` (request a 24 byte, response 408 byte).
    pub fn display_info(&mut self) -> Option<DisplayInfo> {
        let mut req = [0u8; 24];
        req[0..4].copy_from_slice(&vela::gpu::VIRTIO_GPU_CMD_GET_DISPLAY_INFO.to_le_bytes());
        let mut resp = [0u8; 408];
        let kind = self.submit(&req, &mut resp)?;
        if kind != vela::gpu::VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            return None;
        }
        crate::virtio_gpu::parse_display_info(&resp)
    }

    /// `RESOURCE_CREATE_2D`: crea la risorsa pixel (formato BGRA).
    pub fn create_2d(&mut self, resource_id: u32, format: u32, width: u32, height: u32) -> bool {
        let mut req = [0u8; 40];
        req[0..4].copy_from_slice(&vela::gpu::VIRTIO_GPU_CMD_RESOURCE_CREATE_2D.to_le_bytes());
        req[24..28].copy_from_slice(&resource_id.to_le_bytes());
        req[28..32].copy_from_slice(&format.to_le_bytes());
        req[32..36].copy_from_slice(&width.to_le_bytes());
        req[36..40].copy_from_slice(&height.to_le_bytes());
        let mut resp = [0u8; 24];
        self.submit(&req, &mut resp) == Some(vela::gpu::VIRTIO_GPU_RESP_OK_NODATA)
    }

    /// `RESOURCE_ATTACH_BACKING`: aggancia il buffer fisico del client.
    pub fn attach_backing(&mut self, resource_id: u32, phys: u64, length: u32) -> bool {
        let mut req = [0u8; 48];
        req[0..4].copy_from_slice(
            &vela::gpu::VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING.to_le_bytes(),
        );
        req[24..28].copy_from_slice(&resource_id.to_le_bytes());
        req[28..32].copy_from_slice(&1u32.to_le_bytes());
        req[32..40].copy_from_slice(&phys.to_le_bytes());
        req[40..44].copy_from_slice(&length.to_le_bytes());
        let mut resp = [0u8; 24];
        self.submit(&req, &mut resp) == Some(vela::gpu::VIRTIO_GPU_RESP_OK_NODATA)
    }

    /// `SET_SCANOUT`: mostra la risorsa sullo scanout (il guest sceglie la
    /// risoluzione: in QEMU la finestra host segue).
    pub fn set_scanout(
        &mut self,
        scanout_id: u32,
        resource_id: u32,
        width: u32,
        height: u32,
    ) -> bool {
        let mut req = [0u8; 48];
        req[0..4].copy_from_slice(&vela::gpu::VIRTIO_GPU_CMD_SET_SCANOUT.to_le_bytes());
        // rect x, y, w, h
        req[24..28].copy_from_slice(&0u32.to_le_bytes());
        req[28..32].copy_from_slice(&0u32.to_le_bytes());
        req[32..36].copy_from_slice(&width.to_le_bytes());
        req[36..40].copy_from_slice(&height.to_le_bytes());
        req[40..44].copy_from_slice(&scanout_id.to_le_bytes());
        req[44..48].copy_from_slice(&resource_id.to_le_bytes());
        let mut resp = [0u8; 24];
        self.submit(&req, &mut resp) == Some(vela::gpu::VIRTIO_GPU_RESP_OK_NODATA)
    }

    /// `RESOURCE_FLUSH`: il path non-virgl lo ignora (il device mappa la RAM
    /// del guest): si manda lo stesso, e un errore non e' fatale.
    pub fn flush(&mut self, resource_id: u32, width: u32, height: u32) -> bool {
        let mut req = [0u8; 48];
        req[0..4].copy_from_slice(&vela::gpu::VIRTIO_GPU_CMD_RESOURCE_FLUSH.to_le_bytes());
        req[24..28].copy_from_slice(&0u32.to_le_bytes());
        req[28..32].copy_from_slice(&0u32.to_le_bytes());
        req[32..36].copy_from_slice(&width.to_le_bytes());
        req[36..40].copy_from_slice(&height.to_le_bytes());
        req[40..44].copy_from_slice(&resource_id.to_le_bytes());
        let mut resp = [0u8; 24];
        self.submit(&req, &mut resp) == Some(vela::gpu::VIRTIO_GPU_RESP_OK_NODATA)
    }

    /// `RESOURCE_UNREF`: rilascia la risorsa (il device smette di puntarci).
    pub fn unref(&mut self, resource_id: u32) -> bool {
        let mut req = [0u8; 28];
        req[0..4].copy_from_slice(&vela::gpu::VIRTIO_GPU_CMD_RESOURCE_UNREF.to_le_bytes());
        req[24..28].copy_from_slice(&resource_id.to_le_bytes());
        let mut resp = [0u8; 24];
        self.submit(&req, &mut resp) == Some(vela::gpu::VIRTIO_GPU_RESP_OK_NODATA)
    }

    /// Invia una richiesta e attendi la risposta. Ritorna il **tipo** della
    /// risposta (`VIRTIO_GPU_RESP_*`) o `None` (timeout/errore).
    ///
    /// La richiesta e la risposta vivono nei buffer DMA della pagina unica
    /// (request a 0x800, response a 0x900): la copia e' fatta qui, cosi' il
    /// chiamante lavora con slice normali.
    pub fn submit(&mut self, req: &[u8], resp: &mut [u8]) -> Option<u32> {
        if req.len() > DMA_REQUEST_MAX || resp.len() > DMA_RESPONSE_MAX {
            return None;
        }
        let desc = self.dma as usize + DMA_DESC_OFF;
        let avail = self.dma as usize + DMA_AVAIL_OFF;
        let used = self.dma as usize + DMA_USED_OFF;
        let req_len = req.len();
        let resp_len = resp.len();

        unsafe {
            // Copia la richiesta nel buffer DMA e azzera la risposta.
            let dst = (self.dma as usize + DMA_REQUEST_OFF) as *mut u8;
            core::ptr::copy_nonoverlapping(req.as_ptr(), dst, req_len);
            let rdst = (self.dma as usize + DMA_RESPONSE_OFF) as *mut u8;
            core::ptr::write_bytes(rdst, 0, resp_len);

            // desc[0] = request (device-read), desc[1] = response (device-write).
            write_volatile(desc as *mut u64, self.dma_phys + DMA_REQUEST_OFF as u64);
            write_volatile((desc + 8) as *mut u32, req_len as u32);
            write_volatile((desc + 12) as *mut u16, DESC_NEXT);
            write_volatile((desc + 14) as *mut u16, 1);

            write_volatile((desc + 16) as *mut u64, self.dma_phys + DMA_RESPONSE_OFF as u64);
            write_volatile((desc + 24) as *mut u32, resp_len as u32);
            write_volatile((desc + 28) as *mut u16, DESC_WRITE);
            write_volatile((desc + 30) as *mut u16, 0);

            fence(Ordering::Release);
            let slot = (self.avail_idx % self.queue_size) as usize;
            write_volatile((avail + 4 + slot * 2) as *mut u16, 0);
            fence(Ordering::Release);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            write_volatile((avail + 2) as *mut u16, self.avail_idx);

            // Notify: BAR4 + offset + queue_notify_off * multiplier.
            let n = self
                .notify
                .add(self.queue_notify_off as usize * self.notify_off_mul as usize);
            write_volatile(n as *mut u16, 0);
        }

        let start = civis::get_ticks();
        loop {
            let idx = unsafe { read_volatile((used + 2) as *const u16) };
            if idx != self.used_idx {
                self.used_idx = idx;
                // Copia la risposta e restituiscne il tipo.
                unsafe {
                    let src = (self.dma as usize + DMA_RESPONSE_OFF) as *const u8;
                    core::ptr::copy_nonoverlapping(src, resp.as_mut_ptr(), resp_len);
                }
                if resp_len < 4 {
                    return None;
                }
                return Some(u32::from_le_bytes([
                    resp[0], resp[1], resp[2], resp[3],
                ]));
            }
            for _ in 0..256 {
                core::hint::spin_loop();
            }
            if civis::get_ticks() - start > 100 {
                return None;
            }
        }
    }

    fn msix_vectors(&self) -> u16 {
        let mc = pci::config_read16(self.pci, self.msix_cap + 2);
        (mc & 0x07FF) + 1
    }

    fn disable_msix(&mut self) {
        if self.msix_table.is_null() || self.msix_cap == 0 {
            return;
        }
        for i in 0..self.msix_vectors() as usize {
            unsafe {
                write_volatile((self.msix_table as usize + i * 16 + 12) as *mut u32, 1);
            }
        }
        let mc = pci::config_read16(self.pci, self.msix_cap + 2);
        pci::config_write16(self.pci, self.msix_cap + 2, mc & !(1 << 15));
    }
}

/// Programma una BAR a `base`; readback (con i flag di tipo mascherati).
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

/// Offset del trasporto nel VA user a partire dalla BAR (1 o 4).
fn bar_ptr(bar: u8) -> Result<*mut u8, &'static str> {
    match bar {
        1 => Ok(USER_MMIO_VA as *mut u8),
        4 => Ok((USER_MMIO_VA + 0x4000) as *mut u8),
        _ => Err("cap su BAR inattesa"),
    }
}

/// Dimensione mappata delle due BAR del device.
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
        let cap_id = pci::cap_id(dev, off);
        if cap_id == 0x09 {
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

/// Arma MSI-X: maschera tutte le entry, sblocca solo la 0 (config change).
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
        write_volatile((base + 8) as *mut u32, civis::MSI_VECTOR_GPU as u32);
        write_volatile((base + 12) as *mut u32, 0);
    }

    let mc = (mc | (1 << 15)) & !(1 << 14);
    pci::config_write16(dev, cap + 2, mc);
    Ok((base as *mut u8, cap))
}
