//! Rilevamento dischi ATA PATA (Fase 16).
//!
//! Probe primario/secondario × master/slave via `IDENTIFY 0xEC`: i dischi
//! presenti rispondono con 256 word (modello, capacita' LBA28/48), gli ATAPI
//! (CD/DVD) con la firma `0x14/0xEB` e vengono skippati con log (serve il
//! protocollo PACKET, fase futura), gli assenti con status `0x00`/`0xFF` o
//! `ERR`. Ogni attesa e' bound (stesso `TIMEOUT` del trasferimento): un canale
//! vuoto non puo' appendere il boot.

use civis::pio as io;

/// Timeout di polling in iterazioni (come `block.rs`).
const TIMEOUT: u32 = 2_000_000;

/// Canale ATA: base comandi + registro di controllo.
pub struct AtaChannel {
    /// Porta base comandi (0x1F0 primario, 0x170 secondario).
    pub cmd: u16,
    /// Registro di controllo (0x3F6 primario, 0x376 secondario).
    pub ctl: u16,
}

pub const PRIMARY: AtaChannel = AtaChannel { cmd: 0x1F0, ctl: 0x3F6 };
pub const SECONDARY: AtaChannel = AtaChannel { cmd: 0x170, ctl: 0x376 };

/// Disco rilevato: tutto cio' che serve ad aprirlo (`block::AtaDisk`).
pub struct DiskInfo {
    /// Base comandi del canale.
    pub cmd: u16,
    /// 0 = master, 1 = slave.
    pub drive: u8,
    /// Vero se IDENTIFY riporta LBA48 (word 83 bit 10).
    pub lba48: bool,
    /// Settori totali (word 60-61 LBA28, word 100-103 LBA48).
    pub sectors: u64,
    /// Stringa modello IDENTIFY (word 27-46), senza padding.
    pub model: [u8; 40],
    /// Lunghezza significativa di `model`.
    pub model_len: usize,
    /// Seriale IDENTIFY (word 10-19), senza padding. Stabile per disco
    /// (Fase 16d): base per futuri by-id, oggi solo diagnostica.
    pub serial: [u8; 20],
    /// Lunghezza significativa di `serial`.
    pub serial_len: usize,
    /// Modi MDMA supportati (IDENTIFY word 63, bit 0-2, Fase 38.1b):
    /// diagnostica/futuro, oggi la negoziazione usa solo UDMA.
    pub mdma_modes: u16,
    /// Modi UDMA supportati (IDENTIFY word 88, bit 0-6, Fase 38.1b): il bit
    /// piu' alto e' il modo max del drive (PIIX3 arriva a UDMA2).
    pub udma_modes: u16,
    /// TRIM capability (IDENTIFY word 169 bit 0 = DATA SET MANAGEMENT,
    /// Fase 51 P2): solo rilevata, MAI usata (nessun comando DSM emesso).
    pub trim: bool,
    /// Rotation rate nominale (IDENTIFY word 217, Fase 51): `1` =
    /// non-rotazionale (SSD), `0` = non riportato, altrimenti RPM.
    pub rotation: u16,
    /// Settore logico in byte (IDENTIFY word 117-118, Fase 51): 512 se la
    /// word 106 e' invalida o la size e' zero (tutti i dischi reali/QEMU).
    pub sec_logical: u32,
    /// Settore fisico in byte (IDENTIFY word 106 bit 13 + exp bit 0-3,
    /// Fase 51): = logico se non multiplo (quasi sempre 512/4096).
    pub sec_physical: u32,
    /// Modo UDMA negoziato (Fase 51, assegnato in `server.rs` dopo
    /// `set_dma_mode`): `None` = PIO (nessun UDMA o BM assente).
    pub udma_neg: Option<u8>,
}

impl DiskInfo {
    /// Flags topologia (Fase 51, layout single-source in `syscall-numbers`):
    /// bit 0 = LBA48, bit 1 = TRIM capability (mai usata), bit 8-11 = modo
    /// UDMA negoziato (0-2) o 0xF = PIO, bit 16-31 = rotation word 217
    /// (1 = SSD, altrimenti RPM), bit 32-47 = settore logico (B), bit 48-63
    /// = settore fisico (B). Condiviso da DISK_LIST/INFO e R_DISK_*.
    pub fn topo_flags(&self) -> u64 {
        let mut f = 0u64;
        if self.lba48 {
            f |= 1;
        }
        if self.trim {
            f |= 1 << 1;
        }
        f |= ((self.udma_neg.unwrap_or(0xF) as u64) & 0xF) << 8;
        f |= (self.rotation as u64) << 16;
        f |= ((self.sec_logical as u64) & 0xFFFF) << 32;
        f |= ((self.sec_physical as u64) & 0xFFFF) << 48;
        f
    }
}

/// Esito del probe di un singolo drive.
pub enum Probe {
    /// Niente collegato (status 0x00/0xFF, ERR o timeout).
    Absent,
    /// ATAPI (CD/DVD): rilevato, non servito (PACKET futuro).
    Atapi,
    /// Disco ATA pronto.
    Ata(DiskInfo),
}

/// Reset software del canale (bit SRST del control register): riporta entrambi
/// i drive a uno stato noto prima del probe.
fn software_reset(ch: &AtaChannel) {
    unsafe {
        io::outb(ch.ctl, 0x04);
    }
    for _ in 0..10_000 {
        core::hint::spin_loop();
    }
    unsafe {
        io::outb(ch.ctl, 0x00);
    }
    // Attendi BSY clear su entrambi i drive (bound: canale vuoto).
    for _ in 0..TIMEOUT {
        unsafe {
            io::outb(ch.cmd + 6, 0xA0);
        }
        let st0 = unsafe { io::inb(ch.cmd + 7) };
        unsafe {
            io::outb(ch.cmd + 6, 0xB0);
        }
        let st1 = unsafe { io::inb(ch.cmd + 7) };
        if st0 & 0x80 == 0 && st1 & 0x80 == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Seleziona il drive e attende ~400ns (4 letture status scartate).
fn select(ch: &AtaChannel, drive: u8) {
    unsafe {
        io::outb(ch.cmd + 6, 0xA0 | (drive << 4));
    }
    for _ in 0..4 {
        let _ = unsafe { io::inb(ch.cmd + 7) };
    }
}

/// Legge le 256 word IDENTIFY dopo DRQ.
fn read_identify(ch: &AtaChannel, words: &mut [u16; 256]) {
    for i in 0..256 {
        words[i] = unsafe { io::inw(ch.cmd) };
    }
}

/// Decodifica la stringa modello (word 27-46, byte scambiati per word).
fn decode_model(words: &[u16; 256], out: &mut [u8; 40]) -> usize {
    let mut n = 0;
    for i in 0..20 {
        let w = words[27 + i];
        out[n] = (w >> 8) as u8;
        n += 1;
        out[n] = (w & 0xFF) as u8;
        n += 1;
    }
    // Via il padding di spazi a destra.
    while n > 0 && out[n - 1] == b' ' {
        n -= 1;
    }
    n
}

/// Decodifica il seriale (word 10-19, byte scambiati per word, come model).
fn decode_serial(words: &[u16; 256], out: &mut [u8; 20]) -> usize {
    let mut n = 0;
    for i in 0..10 {
        let w = words[10 + i];
        out[n] = (w >> 8) as u8;
        n += 1;
        out[n] = (w & 0xFF) as u8;
        n += 1;
    }
    // Via il padding di spazi a destra.
    while n > 0 && out[n - 1] == b' ' {
        n -= 1;
    }
    n
}

/// Probe di un singolo drive (0 = master, 1 = slave) sul canale.
fn probe_drive(ch: &AtaChannel, drive: u8) -> Probe {
    select(ch, drive);
    unsafe {
        io::outb(ch.cmd + 7, 0xEC); // IDENTIFY DEVICE
    }
    // Bus flottante (0xFF) o niente (0x00): assente, senza attese.
    let st = unsafe { io::inb(ch.cmd + 7) };
    if st == 0x00 || st == 0xFF {
        return Probe::Absent;
    }
    // Attendi BSY clear (bound: un drive impallato non appende il boot).
    let mut bsy = true;
    for _ in 0..TIMEOUT {
        let st = unsafe { io::inb(ch.cmd + 7) };
        if st & 0x80 == 0 {
            bsy = false;
            break;
        }
        core::hint::spin_loop();
    }
    if bsy {
        return Probe::Absent;
    }
    // ERR subito dopo BSY clear = niente (o drive guasto: comunque assente).
    let st = unsafe { io::inb(ch.cmd + 7) };
    if st & 0x01 != 0 {
        return Probe::Absent;
    }
    // Firma ATAPI nei registri cilindri: serve PACKET, non ATA (vedi ADR-0012).
    let mid = unsafe { io::inb(ch.cmd + 4) };
    let hi = unsafe { io::inb(ch.cmd + 5) };
    if mid == 0x14 && hi == 0xEB {
        return Probe::Atapi;
    }
    // Disco ATA: consuma le 256 word IDENTIFY (DRQ gia' verificato dai check).
    let mut words = [0u16; 256];
    read_identify(ch, &mut words);

    let lba48 = words[83] & (1 << 10) != 0;
    let sectors = if lba48 {
        (words[100] as u64)
            | ((words[101] as u64) << 16)
            | ((words[102] as u64) << 32)
            | ((words[103] as u64) << 48)
    } else {
        (words[60] as u64) | ((words[61] as u64) << 16)
    };
    let mut model = [0u8; 40];
    let model_len = decode_model(&words, &mut model);
    let mut serial = [0u8; 20];
    let serial_len = decode_serial(&words, &mut serial);
    // Topologia P2 (Fase 51): capability e geometria per S1/S2 di ArcaFS.
    // Word 169 bit 0 = DATA SET MANAGEMENT (TRIM) supportato.
    let trim = words[169] & 1 != 0;
    // Word 217 = rotation rate (1 = SSD, 0 = non riportato, else RPM).
    let rotation = words[217];
    // Word 106 bit 15/14 = 0/1 se valida; bit 13 = multipli logici per
    // fisico (exp in bit 0-3); word 117-118 = size logica in word.
    let w106 = words[106];
    let log_words = ((words[118] as u32) << 16) | words[117] as u32;
    let sec_logical = if w106 & 0xC000 == 0x4000 && log_words > 0 {
        log_words.saturating_mul(2)
    } else {
        512
    };
    let exp = (w106 & 0x000F) as u32;
    let sec_physical = if w106 & 0xC000 == 0x4000 && w106 & 0x2000 != 0 && exp <= 8 {
        sec_logical.saturating_mul(1 << exp)
    } else {
        sec_logical
    };
    Probe::Ata(DiskInfo {
        cmd: ch.cmd,
        drive,
        lba48,
        sectors,
        model,
        model_len,
        serial,
        serial_len,
        mdma_modes: words[63],
        udma_modes: words[88],
        trim,
        rotation,
        sec_logical,
        sec_physical,
        udma_neg: None,
    })
}

/// Rileva i dischi presenti (primario master/slave, poi secondario
/// master/slave) e li accoda in `out`. Ritorna quanti ATAPI skippati.
pub fn detect(out: &mut alloc::vec::Vec<DiskInfo>) -> usize {
    let mut atapi = 0;
    let channels = [PRIMARY, SECONDARY];
    for ch in &channels {
        software_reset(ch);
        for drive in 0..2 {
            match probe_drive(ch, drive) {
                Probe::Ata(info) => out.push(info),
                Probe::Atapi => atapi += 1,
                Probe::Absent => {}
            }
        }
    }
    atapi
}
