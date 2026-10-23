//! Parser FAT32 (Fase 9.2, scrivibile da Fase 20).
//!
//! Legge BPB, FAT, catene di cluster, directory e file 8.3 da una sorgente
//! settori `BlockSource` (Fase 16: client IPC verso block).
//! Generalizzato a qualunque dimensione di cluster (BytesPerSec x SPC).
//! Limiti: niente LFN (le entry 0x0F sono saltate), 8.3 names, niente
//! mkdir/rm su FAT (solo overwrite/create/grow, Fase 20).

extern crate alloc;

use alloc::format;

mod types;
mod mount;
mod fat;
mod dir;
mod file;

pub use types::*;

// ── Implementazione LocalFs per Fat32 (U0, provider trait) ─────────

impl<B: BlockSource> crate::provider::LocalFs for Fat32<B> {
    type Handle = FileInfo;

    fn open(&mut self, rel: &str, flags: u32) -> Result<Self::Handle, u64> {
        let path = rel.trim_matches('/');
        if path.is_empty() {
            return Err(crate::ERR_NOTFOUND);
        }
        // OPEN_CREATE: crea il file se non esiste (single source `civis::O_*`,
        // Fase 49: mai costanti magiche nel provider).
        if flags & civis::OPEN_CREATE != 0 && self.find(path).is_none() && !self.create_file(path) {
            return Err(crate::ERR_NOTFOUND);
        }
        let info = self.find(path).ok_or(crate::ERR_NOTFOUND)?;
        if info.is_dir {
            return Err(crate::ERR_ISDIR);
        }
        // OPEN_TRUNC assorbito qui (Fase 49, come `RamFs::open`): niente piu'
        // find+truncate fuori trait nell'handler. Fallimento = rifiuto, mai
        // truncate parziale dichiarato riuscito (contratto `truncate`).
        if flags & civis::OPEN_TRUNC != 0 {
            if !self.truncate(&info) {
                return Err(crate::ERR);
            }
            // La truncate cambia size/first_cluster: rileggi fresco.
            return self.find(path).ok_or(crate::ERR_NOTFOUND);
        }
        Ok(info)
    }

    fn read(&mut self, h: Self::Handle, off: usize, buf: &mut [u8]) -> Result<usize, u64> {
        if h.is_dir {
            return Err(crate::ERR_ISDIR);
        }
        let n = self.read_file(&h, off, buf.len(), buf);
        Ok(n)
    }

    fn write(&mut self, h: Self::Handle, off: usize, buf: &[u8], append: bool) -> Result<usize, u64> {
        if h.is_dir {
            return Err(crate::ERR_ISDIR);
        }
        // Semantica come ramfs: OPEN_APPEND ignora `off` e accoda a fine file;
        // altrimenti si scrive a `off`. `write_grow` e' un superset di
        // `write_file` (overwrite entro la size, crescita — e allocazione del
        // primo cluster per i file appena creati — oltre) come il vecchio
        // handler FAT faceva sempre.
        let off = if append { h.size as usize } else { off };
        Ok(self.write_grow(&h, off, buf))
    }

    fn readdir(&mut self, rel: &str, out: &mut dyn crate::provider::EntrySink) -> Result<usize, u64> {
        let entries = self.list_dir(rel);
        for e in &entries {
            out.emit(&e.name);
        }
        Ok(entries.len())
    }

    fn stat(&mut self, rel: &str) -> Result<crate::provider::Meta, u64> {
        match self.find(rel) {
            Some(info) => Ok(crate::provider::Meta {
                size: info.size as u64,
                kind: if info.is_dir { 1 } else { 0 },
                readonly: false, // FAT scrivibile dalla Fase 20 (write/grow).
                mtime: info.mtime,
            }),
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    fn statvfs(&mut self, _rel: &str) -> Result<crate::provider::StatVfs, u64> {
        // Spazio FAT (Fase 52, P3): blocchi = cluster (bsize reale), liberi
        // da FSInfo (clamp al totale: il bump e' best-effort e puo' derivare).
        // Senza FSInfo: nessuno spazio noto → ERR (mai numeri inventati).
        let total = self.cluster_total();
        let free = self.fsinfo_free().ok_or(crate::ERR)?;
        Ok(crate::provider::StatVfs {
            bsize: self.cluster_bytes() as u64,
            blocks: total,
            bfree: (free as u64).min(total),
            bavail: (free as u64).min(total),
        })
    }

    fn mkdir(&mut self, _rel: &str) -> Result<(), u64> {
        // mkdir su FAT e' fuori scope (niente unlink/mkdir).
        Err(crate::ERR_READONLY)
    }

    fn remove(&mut self, _rel: &str) -> Result<(), u64> {
        // remove su FAT e' fuori scope.
        Err(crate::ERR_READONLY)
    }

    fn rename(&mut self, _old: &str, _new: &str) -> Result<(), u64> {
        // rename su FAT e' fuori scope (namespace immutabile come mkdir).
        Err(crate::ERR_READONLY)
    }

    fn symlink(&mut self, _link: &str, _target: &str) -> Result<(), u64> {
        // FAT non ha symlink (fuori scope come rename).
        Err(crate::ERR_READONLY)
    }

    fn readlink(&mut self, _rel: &str) -> Result<alloc::string::String, u64> {
        Err(crate::ERR_NOTFOUND)
    }

    /// chmod S1.1 su FAT: accetta no-op (come Arca: i build non falliscono;
    /// l'attributo readonly reale resta fuori scope con mkdir/remove).
    fn chmod(&mut self, rel: &str, _mode: u32) -> Result<(), u64> {
        match self.find(rel) {
            Some(_) => Ok(()),
            None => Err(crate::ERR_NOTFOUND),
        }
    }
}

// ── Implementazione LocalFsDyn per Fat32 (Fase 48 wiring, Fase 49 handle
// unico): dispatch su `AnyHandle` discriminato — il ramo sbagliato e'
// errore, mai reinterpretazione (niente piu' `*const ()` + Box).
impl<B: BlockSource> crate::provider::LocalFsDyn for Fat32<B> {
    fn open_dyn(&mut self, rel: &str, flags: u32) -> Result<crate::provider::AnyHandle, u64> {
        <Self as crate::provider::LocalFs>::open(self, rel, flags).map(crate::provider::AnyHandle::Fat)
    }

    fn read_dyn(&mut self, h: crate::provider::AnyHandle, off: usize, buf: &mut [u8]) -> Result<usize, u64> {
        match h {
            crate::provider::AnyHandle::Fat(info) => {
                <Self as crate::provider::LocalFs>::read(self, info, off, buf)
            }
            _ => Err(crate::ERR_INVALID),
        }
    }

    fn write_dyn(
        &mut self,
        h: crate::provider::AnyHandle,
        off: usize,
        buf: &[u8],
        append: bool,
    ) -> Result<usize, u64> {
        match h {
            crate::provider::AnyHandle::Fat(info) => {
                <Self as crate::provider::LocalFs>::write(self, info, off, buf, append)
            }
            _ => Err(crate::ERR_INVALID),
        }
    }

    fn readdir_dyn(&mut self, rel: &str, out: &mut dyn crate::provider::EntrySink) -> Result<usize, u64> {
        <Self as crate::provider::LocalFs>::readdir(self, rel, out)
    }

    fn stat_dyn(&mut self, rel: &str) -> Result<crate::provider::Meta, u64> {
        <Self as crate::provider::LocalFs>::stat(self, rel)
    }

    fn statvfs_dyn(&mut self, rel: &str) -> Result<crate::provider::StatVfs, u64> {
        <Self as crate::provider::LocalFs>::statvfs(self, rel)
    }

    fn mkdir_dyn(&mut self, rel: &str) -> Result<(), u64> {
        <Self as crate::provider::LocalFs>::mkdir(self, rel)
    }

    fn remove_dyn(&mut self, rel: &str) -> Result<(), u64> {
        <Self as crate::provider::LocalFs>::remove(self, rel)
    }

    fn rename_dyn(&mut self, old: &str, new: &str) -> Result<(), u64> {
        <Self as crate::provider::LocalFs>::rename(self, old, new)
    }

    fn symlink_dyn(&mut self, link: &str, target: &str) -> Result<(), u64> {
        <Self as crate::provider::LocalFs>::symlink(self, link, target)
    }

    fn readlink_dyn(&mut self, rel: &str) -> Result<alloc::string::String, u64> {
        <Self as crate::provider::LocalFs>::readlink(self, rel)
    }

    fn chmod_dyn(&mut self, rel: &str, mode: u32) -> Result<(), u64> {
        <Self as crate::provider::LocalFs>::chmod(self, rel, mode)
    }
}
