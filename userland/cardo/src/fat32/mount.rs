use super::*;
use crate::*;

impl<B: BlockSource> Fat32<B> {
    pub fn mount(disk: B) -> Option<Fat32<B>> {        let mut boot = [0u8; 512];
        if !disk.read_sector(0, &mut boot) {
            return None;
        }
        if boot[510] != 0x55 || boot[511] != 0xAA {
            return None;
        }

        let bps = u16::from_le_bytes([boot[11], boot[12]]);
        let spc = boot[13];
        let rsvd = u16::from_le_bytes([boot[14], boot[15]]);
        let num_fats = boot[16];
        let fat_size = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);
        let root = u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]]);

        // Validazioni minime del BPB.
        if bps != 512 || spc == 0 || (spc & (spc - 1)) != 0 {
            return None;
        }
        if num_fats == 0 || fat_size == 0 || root < 2 {
            return None;
        }

        let fat_start = rsvd as u32;
        let data_start = rsvd as u32 + num_fats as u32 * fat_size;

        // Identità stabile del volume (Fase 16d, helper condiviso in civis:
        // stessi check di mount, usati anche dallo sniff per-nodo di block).
        let (vol_serial, vol_label) = match civis::fat_bpb_identity(&boot) {
            Some((s, l)) => (s, l),
            None => (None, [b' '; 11]),
        };

        Some(Fat32 {
            disk,
            bytes_per_sec: bps,
            spc,
            fat_start,
            num_fats,
            fat_size,
            data_start,
            root_cluster: root,
            vol_serial,
            vol_label,
            chain_memo: alloc::collections::BTreeMap::new(),
        })
    }

    /// Seriale volume (`UUID=`, maiuscolo hex 8 char) o `None` se assente.
    #[allow(dead_code)] // identita' FAT: riservata al match UUID (fuori gate)
    pub fn vol_serial(&self) -> Option<u32> {
        self.vol_serial
    }

    /// Label volume normalizzata (trim spazi) per match `LABEL=`.
    #[allow(dead_code)] // identita' FAT: riservata al match label (fuori gate)
    pub fn vol_label_trimmed(&self) -> &[u8] {
        let mut n = self.vol_label.len();
        while n > 0 && self.vol_label[n - 1] == b' ' {
            n -= 1;
        }
        &self.vol_label[..n]
    }

    pub(crate) fn cluster_bytes(&self) -> usize {
        self.bytes_per_sec as usize * self.spc as usize
    }

    /// Accesso alla sorgente settori (es. per invalidarla alla morte del
    /// server disco senza rimontare: la riconnessione e' lazy al prossimo read).
    pub fn disk(&self) -> &B {
        &self.disk
    }

    /// Settore della PRIMA copia FAT a `lba` (25: via cache del driver in
    /// `block`, niente memo locale — vedi campo `Fat32`).
    pub(crate) fn fat_sector(&self, lba: u32) -> Option<[u8; 512]> {
        let mut sec = [0u8; 512];
        if !self.disk.read_sector(lba as u64, &mut sec) {
            return None;
        }
        Some(sec)
    }
}
