use crate::*;

/// Sorgente di settori da 512 byte (LBA assoluti nel nodo montato).
/// Implementata dal client IPC verso block (Fase 16, `ipc_disk.rs`):
/// il parser non distingue (il driver ATA locale e' stato rimosso in Fase 16).
pub trait BlockSource {
    fn read_sector(&self, lba: u64, buf: &mut [u8; 512]) -> bool;
    /// Scrive un settore (Fase 20, FAT scrivibile): write-through, nessun
    /// caching — ogni write torna solo a settore stabile su disco.
    fn write_sector(&self, lba: u64, data: &[u8; 512]) -> bool;
    /// 24.2 — run di `n` settori contigui (default: loop sui singoli;
    /// `IpcDisk` li trasferisce in 1 IPC da ≤7 + 1 comando PIO + 1 flush).
    /// `out`/`data` lunghi almeno `n*512` byte.
    fn read_sectors(&self, lba: u64, n: usize, out: &mut [u8]) -> bool {
        if out.len() < n * 512 {
            return false;
        }
        for k in 0..n {
            let mut sec = [0u8; 512];
            if !self.read_sector(lba + k as u64, &mut sec) {
                return false;
            }
            out[k * 512..(k + 1) * 512].copy_from_slice(&sec);
        }
        true
    }
    /// 24.2 — come `read_sectors` in scrittura (write-through per run:
    /// il flush chiude l'intero run, vedi `AtaDisk::write_sectors`).
    fn write_sectors(&self, lba: u64, n: usize, data: &[u8]) -> bool {
        if data.len() < n * 512 {
            return false;
        }
        for k in 0..n {
            let mut sec = [0u8; 512];
            sec.copy_from_slice(&data[k * 512..(k + 1) * 512]);
            if !self.write_sector(lba + k as u64, &sec) {
                return false;
            }
        }
        true
    }
}

pub(crate) const EOC: u32 = 0x0FFFFFF8;   // valori >= questo = fine catena
pub(crate) const BAD_CLUSTER: u32 = 0x0FFFFFF7;

pub struct DirEntry {
    /// Nome 8.3 senza padding ne' estensione separata ("HELLO.TXT").
    pub name: String,
    pub attr: u8,
    pub first_cluster: u32,
    pub size: u32,
    /// Secondi epoch da WrtTime/WrtDate (Fase 50, P1 orologio): 0 =
    /// sconosciuto (campi DOS assurdi o azzerati, mai inventato).
    pub mtime: u64,
    /// Offset in byte dell'entry da 32 B nel data stream della directory
    /// (serve a 20.3 per aggiornare size/first_cluster sul posto).
    pub entry_off: usize,
}
#[derive(Clone, Copy, PartialEq)]
pub struct FileInfo {
    pub first_cluster: u32,
    pub size: u32,
    /// true se directory (da attr, serve a R_STAT; Fase 19.2).
    pub is_dir: bool,
    /// Secondi epoch da WrtTime/WrtDate (Fase 50): copiato dal `find`, 0 =
    /// sconosciuto. La cache per-fd in ftable lo congela all'open (come
    /// size: i metadati freschi si rileggono via R_STAT, mai via fd).
    pub mtime: u64,
    /// Cluster iniziale della directory CONTENITRICE + offset in byte
    /// dell'entry (Fase 20.3: update size/first_cluster sul posto).
    pub dir_cluster: u32,
    pub entry_off: usize,
}

pub struct Fat32<B: BlockSource> {
    pub(crate) disk: B,
    pub(crate) bytes_per_sec: u16,
    pub(crate) spc: u8,
    pub(crate) fat_start: u32,   // LBA della prima FAT
    /// Numero di copie FAT (Fase 20.3: le scritture aggiornano TUTTE).
    pub(crate) num_fats: u8,
    /// Settori per FAT (capacita' ~ fat_size*128 cluster, bound di scan).
    pub(crate) fat_size: u32,
    pub(crate) data_start: u32,  // LBA dei dati (cluster 2)
    pub(crate) root_cluster: u32,
    /// Seriale volume FAT32 (`vol_id`, BPB+67 LE32 nel layout standard firma
    /// `0x29`@66; `fat_bpb_identity` accetta anche il legacy firma@67).
    /// `None` = assente: niente identità stabile da questo nodo (Fase 16d).
    pub(crate) vol_serial: Option<u32>,
    /// Label volume BPB+71 (11 byte raw, padding spazi): identità `LABEL=`.
    pub(crate) vol_label: [u8; 11],
    // Nota: nessun memo settoriale qui (il 24.1 `fat_memo` e' stato rimosso in
    // 25: la cache settoriale write-through vive in `block`, unico
    // proprietario dei blocchi — un secondo strato cacherebbe gli stessi 512 B
    // due volte. Ogni `read_sector` attraversa IPC+PIO o la cache del driver).
    /// Memo delle cluster-chain per first_cluster (S2.5d): `read_file`
    /// camminerebbe da 0 a ogni slice (O(n) a read, O(n^2) sul file — un
    /// file da 99 MB non finirebbe mai). Build pigra al primo read, drop su
    /// `write_grow` (l'unica mutazione possibile: mkdir/remove fuori scope).
    /// Chiave = first_cluster (stabile a mount vivo: niente defrag online).
    pub(crate) chain_memo: alloc::collections::BTreeMap<u32, alloc::vec::Vec<u32>>,
}

pub(crate) const ATTR_DIR: u8 = 0x10;
pub(crate) const ATTR_VOLUME: u8 = 0x08;
