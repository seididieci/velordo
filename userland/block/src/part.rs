//! Tabella partizioni MBR/GPT (Fase 16 + Fase 55, A1).
//!
//! MBR: voci primarie (4 a `0x1BE`, 16 byte l'una): tipo, LBA iniziale e
//! dimensione. Niente catene extended/logical (futuro documentato).
//! GPT: header EFI a LBA1 + entry da 128 B (Fase 55, chiusura: offset UEFI
//! reali — revisione/correzioni dei placeholder A1); guard protective-MBR
//! `0xEE` (CRC32 saltato e backup header ignorato per A1, solo diagnostica
//! futura).

use alloc::vec::Vec;
use super::*;

/// Voce di partizione primaria.
pub struct PartInfo {
    /// Byte tipo MBR (0x0B/0x0C = FAT32, 0x83 = Linux, ...).
    pub ptype: u8,
    /// Primo LBA della partizione.
    pub start: u32,
    /// Settori della partizione.
    pub sectors: u32,
}

/// Tipo GPT ArcaFS (placeholder, da registrare con UUID alias prima del
/// rilascio). In A1 il match sul tipo NON e' enforced: qualunque entry non
/// vuota viene esposta (come MBR) — il mount decide sul magic `ACFS`.
#[allow(dead_code)] // placeholder documentato per il match GPT futuro
pub const ARCAFS_TYPE_GUID: [u8; 16] = [
    0x41, 0x52, 0x43, 0x41, 0x46, 0x53, 0x2D, 0x41,
    0x31, 0x2D, 0x30, 0x30, 0x30, 0x30, 0x30, 0x31,
];

/// Partizione GPT (coordinate fisiche LBA48, Fase 55).
pub struct PartLoc {
    /// Primo settore della partizione (LBA48).
    pub start: u64,
    /// Numero di settori.
    pub sectors: u64,
}

/// Firma MBR a fine settore.
const MBR_SIG0: u8 = 0x55;
const MBR_SIG1: u8 = 0xAA;

/// Magic GPT "EFI PART" (8 byte a offset 0 dell'header).
const GPT_MAGIC: [u8; 8] = [0x45, 0x46, 0x49, 0x20, 0x50, 0x41, 0x52, 0x54];

/// Tipo protective-MBR (campo tipo della prima voce, byte 450 = 446+4 del
/// settore 0): segnale "questo disco e' GPT". Attenzione: byte 446 e' il
/// boot flag (0x00), NON il tipo — il guard va a 450 (bug storico Fase 55).
const PARTITION_TYPE_EFI: u8 = 0xEE;

/// Header GPT (campi che A1 usa, offset UEFI dallo start di LBA1).
pub struct GptHeader {
    /// Primo LBA usabile (offset 40, u64).
    pub first_usable: u64,
    /// Ultimo LBA usabile (offset 48, u64).
    pub last_usable: u64,
    /// LBA di partenza dell'array entry (offset 72, u64).
    pub entries_lba: u64,
    /// Numero di entry (offset 80, u32).
    pub entries_count: u32,
}

/// Parsa le voci primarie dal settore 0. Ritorna le partizioni non vuote
/// (tipo != 0 e size != 0), al massimo 4.
pub fn parse_mbr(sector0: &[u8; 512], out: &mut Vec<PartInfo>) {
    if sector0[510] != MBR_SIG0 || sector0[511] != MBR_SIG1 {
        return;
    }
    for i in 0..4 {
        let base = 0x1BE + i * 16;
        let ptype = sector0[base + 4];
        let start = u32::from_le_bytes([
            sector0[base + 8],
            sector0[base + 9],
            sector0[base + 10],
            sector0[base + 11],
        ]);
        let sectors = u32::from_le_bytes([
            sector0[base + 12],
            sector0[base + 13],
            sector0[base + 14],
            sector0[base + 15],
        ]);
        if ptype != 0 && sectors != 0 {
            out.push(PartInfo { ptype, start, sectors });
        }
    }
}

/// Parsa il GPT header a LBA1 (offset UEFI reali). Ritorna None se non e'
/// un header valido (magic, current LBA == 1, entry da 128 B — A1 accetta
/// solo entry standard; CRC saltato per A1).
pub fn parse_gpt_header(sector1: &[u8; 512]) -> Option<GptHeader> {
    // Magic "EFI PART" a offset 0.
    if sector1[0..8] != GPT_MAGIC {
        return None;
    }
    // Offset 24: current LBA (u64) — deve essere 1.
    let current_lba = u64::from_le_bytes([
        sector1[24], sector1[25], sector1[26], sector1[27],
        sector1[28], sector1[29], sector1[30], sector1[31],
    ]);
    if current_lba != 1 {
        return None;
    }
    // Offset 40/48: first/last usable LBA (u64).
    let first_usable = u64::from_le_bytes([
        sector1[40], sector1[41], sector1[42], sector1[43],
        sector1[44], sector1[45], sector1[46], sector1[47],
    ]);
    let last_usable = u64::from_le_bytes([
        sector1[48], sector1[49], sector1[50], sector1[51],
        sector1[52], sector1[53], sector1[54], sector1[55],
    ]);
    // Offset 72: partition entry LBA (u64).
    let entries_lba = u64::from_le_bytes([
        sector1[72], sector1[73], sector1[74], sector1[75],
        sector1[76], sector1[77], sector1[78], sector1[79],
    ]);
    // Offset 80: numero entry (u32). Offset 84: size entry (u32, A1 = 128).
    let entries_count = u32::from_le_bytes([
        sector1[80], sector1[81], sector1[82], sector1[83],
    ]);
    let entry_size = u32::from_le_bytes([
        sector1[84], sector1[85], sector1[86], sector1[87],
    ]);
    if entry_size != 128 || entries_count == 0 {
        return None;
    }
    Some(GptHeader { first_usable, last_usable, entries_lba, entries_count })
}

/// Legge l'entry GPT `i` (128 B, anche a cavallo di due settori) dall'array
/// che parte a `entries_lba`. Ritorna None su errore IO.
fn read_gpt_entry(disk: &block::AtaDisk, entries_lba: u64, i: u64) -> Option<[u8; 128]> {
    let byte_off = i * 128;
    let sec = entries_lba + byte_off / 512;
    let off = (byte_off % 512) as usize;
    let mut out = [0u8; 128];
    if off + 128 <= 512 {
        let mut s = [0u8; 512];
        if !disk.read_sector(sec, &mut s) {
            return None;
        }
        out.copy_from_slice(&s[off..off + 128]);
    } else {
        // A cavallo: coda di un settore + testa del successivo.
        let mut s0 = [0u8; 512];
        let mut s1 = [0u8; 512];
        if !disk.read_sector(sec, &mut s0) || !disk.read_sector(sec + 1, &mut s1) {
            return None;
        }
        let k = 512 - off;
        out[..k].copy_from_slice(&s0[off..]);
        out[k..].copy_from_slice(&s1[..128 - k]);
    }
    Some(out)
}

/// Parsa una entry GPT da 128 B: type GUID 0..16 (zero = vuota), first LBA a
/// offset 32 (u64), last LBA a offset 40 (u64). Ritorna None se vuota o
/// invalida (first > last, fuori area usabile, zero settori).
fn parse_gpt_entry(e: &[u8; 128], first_usable: u64, last_usable: u64) -> Option<PartLoc> {
    if e[0..16] == [0u8; 16] {
        return None;
    }
    let first_lba = u64::from_le_bytes([
        e[32], e[33], e[34], e[35], e[36], e[37], e[38], e[39],
    ]);
    let last_lba = u64::from_le_bytes([
        e[40], e[41], e[42], e[43], e[44], e[45], e[46], e[47],
    ]);
    if first_lba > last_lba || first_lba < first_usable || last_lba > last_usable {
        return None;
    }
    let sectors = last_lba - first_lba + 1;
    if sectors == 0 {
        return None;
    }
    Some(PartLoc { start: first_lba, sectors })
}

/// Parsa le entry GPT da un disco. Legge LBA0 (protective-MBR), LBA1 (header),
/// poi l'array entry (max 128 voci). Ritorna le partizioni valide.
pub fn parse_gpt(disk: &block::AtaDisk, lba0: &[u8; 512], out: &mut Vec<PartLoc>) {
    // Guard protective-MBR: tipo prima voce (byte 450) = 0xEE → disco GPT
    if lba0[446 + 4] != PARTITION_TYPE_EFI {
        return;
    }

    // Leggi LBA1 (GPT header)
    let mut sec1 = [0u8; 512];
    if !disk.read_sector(1, &mut sec1) {
        return;
    }

    let hdr = match parse_gpt_header(&sec1) {
        Some(h) => h,
        None => return,
    };

    // Scansiona le entry GPT (max 128)
    for i in 0..hdr.entries_count.min(128) as u64 {
        let e = match read_gpt_entry(disk, hdr.entries_lba, i) {
            Some(e) => e,
            None => continue,
        };
        // A1: match sul tipo NON enforced (vedi ARCAFS_TYPE_GUID) — qualunque
        // entry non vuota e valida viene esposta; il mount decide sul magic.
        if let Some(p) = parse_gpt_entry(&e, hdr.first_usable, hdr.last_usable) {
            out.push(p);
        }
    }
}

/// Parsa MBR e GPT da un disco. Ritorna le partizioni trovate (MBR o GPT, mai entrambi).
/// Se il tipo della prima voce di LBA0 (byte 450) == `0xEE` → GPT;
/// altrimenti → MBR legacy (o disco non partizionato).
pub fn parse_partitions(disk: &block::AtaDisk, lba0: &[u8; 512]) -> PartitionResult {
    // Controlla se e' un disco GPT (protective-MBR)
    if lba0[446 + 4] == PARTITION_TYPE_EFI {
        let mut parts = Vec::new();
        parse_gpt(disk, lba0, &mut parts);
        return PartitionResult::Gpt(parts);
    }

    // Altrimenti MBR legacy (o disco non partizionato)
    let mut parts = Vec::new();
    parse_mbr(lba0, &mut parts);
    PartitionResult::Mbr(parts)
}

/// Risultato del parse: MBR o GPT.
pub enum PartitionResult {
    Mbr(Vec<PartInfo>),
    Gpt(Vec<PartLoc>),
}
