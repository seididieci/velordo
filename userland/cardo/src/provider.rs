use super::*;
use super::ramfs::RamHandle;
use super::fat32::FileInfo;
use super::arca::{ArcaFs, ArcaHandle};
use alloc::boxed::Box;

/// Sink per l'output di readdir: ogni entry scrive il nome nel buffer del client.
pub trait EntrySink {
    fn emit(&mut self, name: &str);
}

/// Metadati di una directory entry (presentazione POSIX).
#[derive(Clone, Copy)]
pub struct Meta {
    pub size: u64,
    pub kind: u8, // 0 = file, 1 = dir, 2 = device
    pub readonly: bool,
    pub mtime: u64,
}

/// Spazio del filesystem (presentazione statvfs, Fase 52 P3): blocco in byte
/// + conteggi in blocchi. `bfree == bavail == u64::MAX` = illimitato
/// (ramfs memory-backed: vedi `SYS_MEMINFO`, mai disco).
#[derive(Clone, Copy)]
pub struct StatVfs {
    pub bsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
}

/// Provider di filesystem locale (ramfs, FAT, ArcaFS, ...).
/// La trait e' la presentazione POSIX: il core del FS resta nativo.
pub trait LocalFs {
    type Handle: Copy + PartialEq;

    fn open(&mut self, rel: &str, flags: u32) -> Result<Self::Handle, u64>;
    fn read(&mut self, h: Self::Handle, off: usize, buf: &mut [u8]) -> Result<usize, u64>;
    fn write(&mut self, h: Self::Handle, off: usize, buf: &[u8], append: bool) -> Result<usize, u64>;
    fn readdir(&mut self, rel: &str, out: &mut dyn EntrySink) -> Result<usize, u64>;
    fn stat(&mut self, rel: &str) -> Result<Meta, u64>;
    fn mkdir(&mut self, rel: &str) -> Result<(), u64>;
    fn remove(&mut self, rel: &str) -> Result<(), u64>;
    /// Rename S1.1: sposta `old` in `new` NELLO STESSO provider (il dispatch
    /// cross-mount rifiuta prima: mai chiamato tra mount diversi).
    fn rename(&mut self, old: &str, new: &str) -> Result<(), u64>;
    /// Symlink S1.1: crea il link `link` -> `target` (target opaco, mai
    /// risolto qui). `readlink` ne legge il target.
    fn symlink(&mut self, link: &str, target: &str) -> Result<(), u64>;
    fn readlink(&mut self, rel: &str) -> Result<alloc::string::String, u64>;
    /// chmod S1.1 (projection): registra la mode dove banale, altrove
    /// accetta no-op. Mai enforcement (Strato 0 come i mode default).
    fn chmod(&mut self, rel: &str, mode: u32) -> Result<(), u64>;
    /// Spazio del mount (Fase 52, P3: sensore per quota/swap futuri). Il
    /// `rel` e' ignorato (spazio del mount, non del path): la risoluzione
    /// mount resta negli handler, mai nel provider.
    fn statvfs(&mut self, rel: &str) -> Result<StatVfs, u64>;
}

/// Filesystem montato su un target. Le varianti tengono l'istanza viva;
/// `None` = spec registrata ma inattiva (sorgente assente all'ultimo tentativo).
pub enum MountedFs {
    Fat(Option<Fat32<IpcDisk>>),
    Local(Box<dyn LocalFsDyn>),
    /// ArcaFS nativo (Fase 54, P5: stub che rifiuta — A1 lo riempie).
    /// Variante propria (non `Local`) perche' il dispatch nativo `R_OBJ_*`
    /// futuro non passa dalla vista POSIX.
    Arca(ArcaFs),
}

/// Handle opaco per il dispatch dinamico (Fase 49): enum discriminata
/// by-value, niente `Box`, niente raw-pointer. Chiude il doppio contratto
/// `*const ()` delle Fasi 46-48 (Box documentato vs stack-pointer usato):
/// la type-confusion e' impossibile per costruzione (il `match` sul ramo
/// sbagliato ritorna errore), non esiste lifecycle da tracciare (entrambe
/// le `close` concrete sono no-op) e il per-op resta heap-free (regola
/// Fase 24).
#[derive(Clone, Copy, PartialEq)]
pub enum AnyHandle {
    Ram(RamHandle),
    Fat(FileInfo),
    /// ArcaFS (Fase 54, P5: mai costruito — `open` dello stub rifiuta).
    Arca(ArcaHandle),
}

/// Versione object-safe di LocalFs: gli handle viaggiano come `AnyHandle`
/// (Copy, sullo stack del chiamante). `open_dyn` apre per-path e ritorna
/// l'handle by-value (niente Box, niente lifecycle: le `close` concrete sono
/// no-op, la cache per-fd vive in `ftable` come prima).
pub trait LocalFsDyn {
    fn open_dyn(&mut self, rel: &str, flags: u32) -> Result<AnyHandle, u64>;
    fn read_dyn(&mut self, h: AnyHandle, off: usize, buf: &mut [u8]) -> Result<usize, u64>;
    fn write_dyn(&mut self, h: AnyHandle, off: usize, buf: &[u8], append: bool) -> Result<usize, u64>;
    fn readdir_dyn(&mut self, rel: &str, out: &mut dyn EntrySink) -> Result<usize, u64>;
    fn stat_dyn(&mut self, rel: &str) -> Result<Meta, u64>;
    fn mkdir_dyn(&mut self, rel: &str) -> Result<(), u64>;
    fn remove_dyn(&mut self, rel: &str) -> Result<(), u64>;
    fn rename_dyn(&mut self, old: &str, new: &str) -> Result<(), u64>;
    fn symlink_dyn(&mut self, link: &str, target: &str) -> Result<(), u64>;
    fn readlink_dyn(&mut self, rel: &str) -> Result<alloc::string::String, u64>;
    fn chmod_dyn(&mut self, rel: &str, mode: u32) -> Result<(), u64>;
    fn statvfs_dyn(&mut self, rel: &str) -> Result<StatVfs, u64>;
}
