//! Pipe anonime Velordo (S2.4c1): riuso puro di `vfscli::sync::pipe` (Fase 42,
//! buffer server-side 8K in cardo). La sessione e' condivisa con `std::fs`
//! (i ring sono singleton per processo): i wrapper `pipe_raw/read/write/close`
//! vivono in `sys/fs/velordo.rs`.
//!
//! Superficie inherent (mai trait: `std::io::pipe` chiama questi per nome):
//! `read`/`read_vectored`/`is_read_vectored`/`read_to_end`/`read_buf` +
//! `write`/`write_vectored`/`is_write_vectored` + `try_clone` onesto-Err (DUP
//! e' handoff single-use, non clone: vedi `dup_grant` in S2.2).

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};

/// Estremita' di pipe (fd cardo come i file: stesso spazio, `close` onesto).
#[derive(Debug)]
pub struct Pipe(i64);

impl Pipe {
    /// Legge fino a `buf.len()` (0 = EOF; bloccante su vuota con writer vivi,
    /// come `read_fs`).
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        crate::sys::fs::velordo::pipe_read(self.0, buf)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut n = 0;
        for b in bufs.iter_mut() {
            let m = self.read(b)?;
            n += m;
            if m < b.len() {
                break;
            }
        }
        Ok(n)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        // Temp sullo stack (1 KiB): niente alloc per chiamata.
        let before = buf.len();
        loop {
            let mut tmp = [0u8; 1024];
            let n = self.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        Ok(buf.len() - before)
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        while cursor.written() < cursor.capacity() {
            let mut tmp = [0u8; 1024];
            let n = self.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            cursor.append(&tmp[..n]);
        }
        Ok(())
    }

    /// Scrive fino a `buf.len()` (bloccante su piena con lettori vivi).
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        crate::sys::fs::velordo::pipe_write(self.0, buf)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut n = 0;
        for b in bufs.iter() {
            let m = self.write(b)?;
            n += m;
            if m < b.len() {
                break;
            }
        }
        Ok(n)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn try_clone(&self) -> io::Result<Pipe> {
        // DUP e' handoff single-use al figlio, non clone indipendente.
        Err(io::Error::new(io::ErrorKind::Unsupported, "try_clone senza DUP indipendente"))
    }

    /// Chiude l'estremita' (writer chiusi = EOF ai lettori).
    pub fn close(self) {
        let _ = crate::sys::fs::velordo::pipe_close(self.0);
    }

    /// fd nativo (per `sys::process`: grant ai figli).
    pub fn fd(&self) -> i64 {
        self.0
    }

    /// Avvolge un fd esistente (estremita' parent in `spawn`, mai duplicata).
    pub(crate) fn from_fd(fd: i64) -> Pipe {
        Pipe(fd)
    }

    /// Tappo per `unsupported.rs` (`From<ChildPipe> for Stdio`, S2.4c1):
    /// irraggiungibile finche' `spawn` e' stub (nessuna pipe reale arriva al
    /// backend process). RIMOSSO in c2 col modulo process reale (il `From`
    /// diventa nostro, variante `Fd`).
    pub fn diverge(&self) -> ! {
        panic!("pipe Velordo in Stdio senza backend process (S2.4c2)")
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = crate::sys::fs::velordo::pipe_close(self.0);
    }
}

/// Crea una pipe: `(lettura, scrittura)`.
pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let (r, w) = crate::sys::fs::velordo::pipe_raw()?;
    Ok((Pipe(r), Pipe(w)))
}
