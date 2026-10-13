//! Backend memmap2 per Velordo (S2.5b): snapshot privati in `Vec`.
//!
//! Il kernel mappa solo anonimo (niente file-backed, niente PROT_EXEC):
//! ogni `map*` legge il range via `File` in un `Vec` (stessa semantica del
//! fallback `Vec` che rustc usa su wasm32/miri per `map_copy_read_only`).
//! DIVERGENZA DOCUMENTATA (vs unix MAP_SHARED): le scritture NON raggiungono
//! mai il file (`flush` = Ok no-op); `map_exec`/`make_exec` = Err onesto
//! (niente eseguibile mappato). `len == 0` = file vuoto → Err come unix.
//! Coda oltre EOF = zeri (come le pagine oltre EOF nelle map private).

use std::fs::File;
use std::io::{self, Read};

pub struct MmapInner {
    data: Vec<u8>,
}

fn read_range(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory map must have a non-zero length",
        ));
    }
    // Niente seek su &File (try_clone onesto-Err: S2.2): offset 0 via
    // read_to_end (l'uso reale e' whole-file); offset>0 = Err onesto.
    if offset != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "offset di mappa oltre 0 assente su Velordo",
        ));
    }
    let mut data = Vec::new();
    // `&File: Read` (condiviso): rebind mutabile della ref (Copy).
    let mut f = file;
    f.read_to_end(&mut data)?;
    // resize taglia o estende a zeri (pagine oltre EOF nelle map private).
    data.resize(len, 0);
    Ok(data)
}

impl MmapInner {
    pub fn map(len: usize, file: &File, offset: u64, _populate: bool) -> io::Result<MmapInner> {
        read_range(file, offset, len).map(|data| MmapInner { data })
    }

    pub fn map_exec(
        _len: usize,
        _file: &File,
        _offset: u64,
        _populate: bool,
    ) -> io::Result<MmapInner> {
        // Niente PROT_EXEC mappato (solo codice di spawn): onesto.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "executable mapping assente su Velordo",
        ))
    }

    pub fn map_mut(len: usize, file: &File, offset: u64, populate: bool) -> io::Result<MmapInner> {
        Self::map(len, file, offset, populate)
    }

    pub fn map_copy(len: usize, file: &File, offset: u64, populate: bool) -> io::Result<MmapInner> {
        Self::map(len, file, offset, populate)
    }

    pub fn map_copy_read_only(
        len: usize,
        file: &File,
        offset: u64,
        populate: bool,
    ) -> io::Result<MmapInner> {
        Self::map(len, file, offset, populate)
    }

    pub fn map_anon(len: usize, _stack: bool) -> io::Result<MmapInner> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "memory map must have a non-zero length",
            ));
        }
        let mut data = Vec::new();
        data.resize(len, 0);
        Ok(MmapInner { data })
    }

    pub fn flush(&self, _offset: usize, _len: usize) -> io::Result<()> {
        // Snapshot privato: niente da scaricare (vedi divergenza sopra).
        Ok(())
    }

    pub fn flush_async(&self, _offset: usize, _len: usize) -> io::Result<()> {
        Ok(())
    }

    pub fn make_read_only(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn make_exec(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "executable mapping assente su Velordo",
        ))
    }

    pub fn make_mut(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    pub fn mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }
}
