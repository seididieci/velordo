//! Driver ATA PIO in userspace (Fase 16, da `cardo/block.rs`; scrittura in
//! Fase 20: `WRITE SECTORS EXT` + LBA28, stesso polling con timeout).
//!
//! Generalizzato a qualunque canale/drive (primario + secondario, master +
//! slave) e a LBA48 (`READ SECTORS EXT`): il rilevamento (`detect.rs`)
//! decide capacita' e geometria, qui solo il trasferimento. Polling con
//! timeout a contatore, come prima: senza disco la porta status resta 0xFF
//! (BSY set) e le operazioni falliscono in tempo finito.

use civis::pio as io;

/// Timeout di polling in iterazioni (nessuna unita' di tempo: solo un freno).
/// Su QEMU senza disco il controller non clear mai BSY: 2M iterazioni di `inb`
/// emulato costano decine di secondi; con disco la risposta arriva in pochi
/// microsecondi, quindi un timeout contenuto basta ed evita il blocco.
const TIMEOUT: u32 = 2_000_000;

pub struct AtaDisk {
    /// Porta base comandi (0x1F0 primario, 0x170 secondario).
    cmd: u16,
    /// 0 = master, 1 = slave (bit 4 del registro drive/head).
    drive: u8,
    /// Vero se IDENTIFY riporta LBA48 (word 83 bit 10): usa `READ SECTORS EXT`.
    pub lba48: bool,
}

impl AtaDisk {
    pub fn open(cmd: u16, drive: u8, lba48: bool) -> AtaDisk {
        AtaDisk { cmd, drive, lba48 }
    }

    /// Negozia il modo UDMA del drive (Fase 38.1b, `SET FEATURES 0xEF/0x03`):
    /// modo = min(max del drive da IDENTIFY word 88, UDMA2 del PIIX3).
    /// Ritorna il modo negoziato (0-2) o `None` (niente UDMA / comando
    /// rifiutato → il chiamante resta in PIO). Solo log a questo passo (38.1c
    /// usera' il modo per i trasferimenti DMA); il PIO non e' toccato dal
    /// modo (i comandi PIO restano PIO).
    pub fn set_dma_mode(&self, udma_word: u16) -> Option<u8> {
        let mut max: Option<u8> = None;
        for m in 0..=6u8 {
            if udma_word & (1 << m) != 0 {
                max = Some(m);
            }
        }
        let mode = max?.min(2);
        if !self.wait_not_busy() {
            return None;
        }
        unsafe {
            io::outb(self.cmd + 6, 0xA0 | (self.drive << 4));
            io::outb(self.cmd + 1, 0x03); // SET TRANSFER MODE
            io::outb(self.cmd + 2, 0x40 | mode); // UDMA n
            io::outb(self.cmd + 3, 0x00);
            io::outb(self.cmd + 4, 0x00);
            io::outb(self.cmd + 5, 0x00);
            io::outb(self.cmd + 7, 0xEF); // SET FEATURES
        }
        if !self.wait_not_busy() {
            return None;
        }
        let st = unsafe { io::inb(self.cmd + 7) };
        if st & 0x01 != 0 {
            return None; // ERR: modo rifiutato
        }
        Some(mode)
    }

    /// Attende che il controller non sia piu' busy. Ritorna `false` su timeout.
    fn wait_not_busy(&self) -> bool {
        for _ in 0..TIMEOUT {
            let st = unsafe { io::inb(self.cmd + 7) };
            if st & 0x80 == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Attende DRQ (data request) o errore. Ritorna `false` su timeout/errore.
    fn wait_drq(&self) -> bool {
        for _ in 0..TIMEOUT {
            let st = unsafe { io::inb(self.cmd + 7) };
            if st & 0x08 != 0 {
                return true;
            }
            if st & 0x01 != 0 {
                return false; // ERR durante l'attesa dati
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Legge le 256 word di dati dopo DRQ nel buffer (primi 512 byte).
    fn read_data(&self, buf: &mut [u8]) {
        for i in 0..256 {
            let w = unsafe { io::inw(self.cmd) };
            buf[i * 2] = (w & 0xFF) as u8;
            buf[i * 2 + 1] = (w >> 8) as u8;
        }
    }

    /// Legge un settore (512 byte) via PIO LBA28. Ritorna `false` su
    /// errore/timeout o se `lba` non sta in 28 bit.
    fn read_lba28(&self, lba: u64, buf: &mut [u8; 512]) -> bool {
        if lba > 0x0FFF_FFFF {
            return false;
        }
        if !self.wait_not_busy() {
            return false;
        }

        // 0xE0 = LBA mode + drive; i 4 bit alti di LBA28 in drive/head.
        unsafe {
            io::outb(self.cmd + 6, 0xE0 | (self.drive << 4) | ((lba >> 24) & 0x0F) as u8);
            io::outb(self.cmd + 1, 0x00); // feature
            io::outb(self.cmd + 2, 0x01); // sector count = 1
            io::outb(self.cmd + 3, (lba & 0xFF) as u8);
            io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8);
            io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8);
            io::outb(self.cmd + 7, 0x20); // READ SECTORS with retry
        }

        if !self.wait_not_busy() {
            return false;
        }

        // ERR bit (0x01) setto subito dopo BSY clear = errore.
        let st = unsafe { io::inb(self.cmd + 7) };
        if st & 0x01 != 0 {
            return false;
        }
        if !self.wait_drq() {
            return false;
        }
        self.read_data(buf);
        true
    }

    /// Legge un settore (512 byte) via PIO LBA48 (`READ SECTORS EXT 0x24`).
    /// Ritorna `false` su errore/timeout o se `lba` non sta in 48 bit.
    fn read_lba48(&self, lba: u64, buf: &mut [u8; 512]) -> bool {
        if lba > 0xFFFF_FFFF_FFFF {
            return false;
        }
        if !self.wait_not_busy() {
            return false;
        }

        // Ordine 48-bit: prima i byte alti (HOB), poi i bassi. Count = 1.
        // 0x40 = LBA mode + drive (i bit alti di LBA viaggiano nei registri).
        unsafe {
            io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
            io::outb(self.cmd + 1, 0x00); // features high
            io::outb(self.cmd + 2, 0x00); // count high
            io::outb(self.cmd + 3, ((lba >> 24) & 0xFF) as u8); // LBA 3
            io::outb(self.cmd + 4, ((lba >> 32) & 0xFF) as u8); // LBA 4
            io::outb(self.cmd + 5, ((lba >> 40) & 0xFF) as u8); // LBA 5
            io::outb(self.cmd + 1, 0x00); // features low
            io::outb(self.cmd + 2, 0x01); // count low = 1
            io::outb(self.cmd + 3, (lba & 0xFF) as u8); // LBA 0
            io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8); // LBA 1
            io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8); // LBA 2
            io::outb(self.cmd + 7, 0x24); // READ SECTORS EXT
        }

        if !self.wait_not_busy() {
            return false;
        }
        let st = unsafe { io::inb(self.cmd + 7) };
        if st & 0x01 != 0 {
            return false;
        }
        if !self.wait_drq() {
            return false;
        }
        self.read_data(buf);
        true
    }

    /// Legge un settore (512 byte) via PIO. Sceglie LBA28/LBA48 dalla
    /// capacita' rilevata. Ritorna `false` su errore/timeout/fuori range.
    pub fn read_sector(&self, lba: u64, buf: &mut [u8; 512]) -> bool {
        if self.lba48 {
            self.read_lba48(lba, buf)
        } else {
            self.read_lba28(lba, buf)
        }
    }

    // ── Taskfile per DMA Bus-Master (Fase 38.1c) ─────────────────────
    // Il motore (`dma.rs`) orchestra PRD + registri BM; qui solo taskfile e
    // status. Solo LBA48 (`READ/WRITE DMA EXT`): i dischi !lba48 restano PIO
    // (guardia nel motore, mai comando emesso).

    /// Avvia un comando DMA LBA48 (`0x25` read / `0x35` write, count = n):
    /// taskfile come `read_lba48` ma count piena. Ritorna false su timeout
    /// pre-comando (il BM non va startato). Il completamento si osserva sui
    /// registri BM + status (motore).
    pub(crate) fn start_dma_ext(&self, lba: u64, n: u8, write: bool) -> bool {
        if n == 0 || !self.wait_not_busy() {
            return false;
        }
        unsafe {
            io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
            io::outb(self.cmd + 1, 0x00); // features high
            io::outb(self.cmd + 2, 0x00); // count high
            io::outb(self.cmd + 3, ((lba >> 24) & 0xFF) as u8); // LBA 3
            io::outb(self.cmd + 4, ((lba >> 32) & 0xFF) as u8); // LBA 4
            io::outb(self.cmd + 5, ((lba >> 40) & 0xFF) as u8); // LBA 5
            io::outb(self.cmd + 1, 0x00); // features low
            io::outb(self.cmd + 2, n); // count low
            io::outb(self.cmd + 3, (lba & 0xFF) as u8); // LBA 0
            io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8); // LBA 1
            io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8); // LBA 2
            io::outb(self.cmd + 7, if write { 0x35 } else { 0x25 });
        }
        true
    }

    /// Legge il registro status taskfile. A fine DMA la lettura SPEGNE l'IRQ
    /// ATA (va fatta prima del clear INTR del BM, vedi motore).
    pub(crate) fn task_status(&self) -> u8 {
        unsafe { io::inb(self.cmd + 7) }
    }

    /// Offset base dei registri Bus-Master del canale (primario +0,
    /// secondario +8): il BMIBA e' uno per controller, i registri per canale.
    pub(crate) fn bm_chan_off(&self) -> u16 {
        if self.cmd == super::detect::SECONDARY.cmd {
            8
        } else {
            0
        }
    }

    /// Flush cache in scrittura (come `write_lba48`, senza i dati): dopo un
    /// WRITE DMA i dati sono nel buffer del disco, la durabilita' per-
    /// richiesta vuole il flush dedicato (stessa semantica del PIO).
    pub(crate) fn flush_write_cache(&self) -> bool {
        if !self.wait_not_busy() {
            return false;
        }
        unsafe {
            if self.lba48 {
                io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
                io::outb(self.cmd + 7, 0xEA); // FLUSH CACHE EXT
            } else {
                io::outb(self.cmd + 6, 0xE0 | (self.drive << 4));
                io::outb(self.cmd + 7, 0xE7); // FLUSH CACHE
            }
        }
        if !self.wait_not_busy() {
            return false;
        }
        let st = unsafe { io::inb(self.cmd + 7) };
        st & 0x01 == 0
    }

    /// 24.2 — legge `n` (1..=255) settori contigui con UN solo comando PIO
    /// (count=n): una fase di setup invece di n. `out` deve contenere almeno
    /// `n*512` byte. Ritorna `false` (e dati parziali in `out`) su
    /// errore/timeout/fuori range.
    pub fn read_sectors(&self, lba: u64, n: u8, out: &mut [u8]) -> bool {
        if n == 0 || out.len() < n as usize * 512 {
            return false;
        }
        if self.lba48 {
            if lba > 0xFFFF_FFFF_FFFF {
                return false;
            }
            if !self.wait_not_busy() {
                return false;
            }
            unsafe {
                io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
                io::outb(self.cmd + 1, 0x00);
                io::outb(self.cmd + 2, 0x00); // count high
                io::outb(self.cmd + 3, ((lba >> 24) & 0xFF) as u8);
                io::outb(self.cmd + 4, ((lba >> 32) & 0xFF) as u8);
                io::outb(self.cmd + 5, ((lba >> 40) & 0xFF) as u8);
                io::outb(self.cmd + 1, 0x00);
                io::outb(self.cmd + 2, n); // count low
                io::outb(self.cmd + 3, (lba & 0xFF) as u8);
                io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8);
                io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8);
                io::outb(self.cmd + 7, 0x24); // READ SECTORS EXT
            }
        } else {
            if lba > 0x0FFF_FFFF {
                return false;
            }
            if !self.wait_not_busy() {
                return false;
            }
            unsafe {
                io::outb(self.cmd + 6, 0xE0 | (self.drive << 4) | ((lba >> 24) & 0x0F) as u8);
                io::outb(self.cmd + 1, 0x00);
                io::outb(self.cmd + 2, n); // sector count
                io::outb(self.cmd + 3, (lba & 0xFF) as u8);
                io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8);
                io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8);
                io::outb(self.cmd + 7, 0x20); // READ SECTORS with retry
            }
        }
        for k in 0..n as usize {
            if !self.wait_not_busy() {
                return false;
            }
            let st = unsafe { io::inb(self.cmd + 7) };
            if st & 0x01 != 0 {
                return false;
            }
            if !self.wait_drq() {
                return false;
            }
            self.read_data(&mut out[k * 512..(k + 1) * 512]);
        }
        true
    }

    /// Scrive le 256 word di dati dopo DRQ dal buffer (primi 512 byte, speculare
    /// a read_data).
    fn write_data(&self, buf: &[u8]) {
        for i in 0..256 {
            let w = (buf[i * 2] as u16) | ((buf[i * 2 + 1] as u16) << 8);
            unsafe { io::outw(self.cmd, w) };
        }
    }

    /// Scrive un settore (512 byte) via PIO LBA28. Ritorna `false` su
    /// errore/timeout o se `lba` non sta in 28 bit.
    fn write_lba28(&self, lba: u64, buf: &[u8; 512]) -> bool {
        if lba > 0x0FFF_FFFF {
            return false;
        }
        if !self.wait_not_busy() {
            return false;
        }
        unsafe {
            io::outb(self.cmd + 6, 0xE0 | (self.drive << 4) | ((lba >> 24) & 0x0F) as u8);
            io::outb(self.cmd + 1, 0x00); // feature
            io::outb(self.cmd + 2, 0x01); // sector count = 1
            io::outb(self.cmd + 3, (lba & 0xFF) as u8);
            io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8);
            io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8);
            io::outb(self.cmd + 7, 0x30); // WRITE SECTORS with retry
        }
        if !self.wait_drq() {
            return false;
        }
        self.write_data(buf);
        // Flush cache del drive (0xE7): senza, i dati restano nel buffer del
        // disco e un controllo offline (fsck) subito dopo li perderebbe.
        if !self.wait_not_busy() {
            return false;
        }
        unsafe {
            io::outb(self.cmd + 6, 0xE0 | (self.drive << 4));
            io::outb(self.cmd + 7, 0xE7); // FLUSH CACHE
        }
        if !self.wait_not_busy() {
            return false;
        }
        let st = unsafe { io::inb(self.cmd + 7) };
        st & 0x01 == 0
    }

    /// Scrive un settore (512 byte) via PIO LBA48 (`WRITE SECTORS EXT 0x34`).
    /// Ritorna `false` su errore/timeout o se `lba` non sta in 48 bit.
    fn write_lba48(&self, lba: u64, buf: &[u8; 512]) -> bool {
        if lba > 0xFFFF_FFFF_FFFF {
            return false;
        }
        if !self.wait_not_busy() {
            return false;
        }
        unsafe {
            io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
            io::outb(self.cmd + 1, 0x00); // features high
            io::outb(self.cmd + 2, 0x00); // count high
            io::outb(self.cmd + 3, ((lba >> 24) & 0xFF) as u8); // LBA 3
            io::outb(self.cmd + 4, ((lba >> 32) & 0xFF) as u8); // LBA 4
            io::outb(self.cmd + 5, ((lba >> 40) & 0xFF) as u8); // LBA 5
            io::outb(self.cmd + 1, 0x00); // features low
            io::outb(self.cmd + 2, 0x01); // count low = 1
            io::outb(self.cmd + 3, (lba & 0xFF) as u8); // LBA 0
            io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8); // LBA 1
            io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8); // LBA 2
            io::outb(self.cmd + 7, 0x34); // WRITE SECTORS EXT
        }
        if !self.wait_drq() {
            return false;
        }
        self.write_data(buf);
        if !self.wait_not_busy() {
            return false;
        }
        unsafe {
            io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
            io::outb(self.cmd + 7, 0xEA); // FLUSH CACHE EXT
        }
        if !self.wait_not_busy() {
            return false;
        }
        let st = unsafe { io::inb(self.cmd + 7) };
        st & 0x01 == 0
    }

    /// Scrive un settore (512 byte) via PIO. Sceglie LBA28/LBA48 dalla
    /// capacita' rilevata. Ritorna `false` su errore/timeout/fuori range.
    pub fn write_sector(&self, lba: u64, buf: &[u8; 512]) -> bool {
        if self.lba48 {
            self.write_lba48(lba, buf)
        } else {
            self.write_lba28(lba, buf)
        }
    }

    /// 24.2 — scrive `n` (1..=255) settori contigui con UN solo comando PIO e
    /// UN solo FLUSH CACHE alla fine (prima: un comando + un flush a settore).
    /// Durabilita' per-richiesta invariata (il flush chiude l'intero run);
    /// `data` deve contenere almeno `n*512` byte. `false` su errore/timeout.
    pub fn write_sectors(&self, lba: u64, n: u8, data: &[u8]) -> bool {
        if n == 0 || data.len() < n as usize * 512 {
            return false;
        }
        // (comando, flush): (0x30, 0xE7) in LBA28, (0x34, 0xEA) in LBA48.
        if self.lba48 {
            if lba > 0xFFFF_FFFF_FFFF {
                return false;
            }
            if !self.wait_not_busy() {
                return false;
            }
            unsafe {
                io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
                io::outb(self.cmd + 1, 0x00);
                io::outb(self.cmd + 2, 0x00);
                io::outb(self.cmd + 3, ((lba >> 24) & 0xFF) as u8);
                io::outb(self.cmd + 4, ((lba >> 32) & 0xFF) as u8);
                io::outb(self.cmd + 5, ((lba >> 40) & 0xFF) as u8);
                io::outb(self.cmd + 1, 0x00);
                io::outb(self.cmd + 2, n);
                io::outb(self.cmd + 3, (lba & 0xFF) as u8);
                io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8);
                io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8);
                io::outb(self.cmd + 7, 0x34); // WRITE SECTORS EXT
            }
            for k in 0..n as usize {
                if !self.wait_drq() {
                    return false;
                }
                self.write_data(&data[k * 512..(k + 1) * 512]);
            }
            if !self.wait_not_busy() {
                return false;
            }
            unsafe {
                io::outb(self.cmd + 6, 0x40 | (self.drive << 4));
                io::outb(self.cmd + 7, 0xEA); // FLUSH CACHE EXT
            }
        } else {
            if lba > 0x0FFF_FFFF {
                return false;
            }
            if !self.wait_not_busy() {
                return false;
            }
            unsafe {
                io::outb(self.cmd + 6, 0xE0 | (self.drive << 4) | ((lba >> 24) & 0x0F) as u8);
                io::outb(self.cmd + 1, 0x00);
                io::outb(self.cmd + 2, n);
                io::outb(self.cmd + 3, (lba & 0xFF) as u8);
                io::outb(self.cmd + 4, ((lba >> 8) & 0xFF) as u8);
                io::outb(self.cmd + 5, ((lba >> 16) & 0xFF) as u8);
                io::outb(self.cmd + 7, 0x30); // WRITE SECTORS with retry
            }
            for k in 0..n as usize {
                if !self.wait_drq() {
                    return false;
                }
                self.write_data(&data[k * 512..(k + 1) * 512]);
            }
            if !self.wait_not_busy() {
                return false;
            }
            unsafe {
                io::outb(self.cmd + 6, 0xE0 | (self.drive << 4));
                io::outb(self.cmd + 7, 0xE7); // FLUSH CACHE
            }
        }
        if !self.wait_not_busy() {
            return false;
        }
        let st = unsafe { io::inb(self.cmd + 7) };
        st & 0x01 == 0
    }
}
