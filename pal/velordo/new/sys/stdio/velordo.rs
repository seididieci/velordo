//! Console Velordo (S1.3): stdout/stderr su SYS_WRITE (maniglia console →
//! seriale), stdin EOF. Forma di `stdio/unsupported.rs` con write reale.
//!
//! S2.4c3: redirect fd per i figli spawnati (`Command` con pipe): `_start`
//! legge `VELORDO_FD0/1/2` dall'env e arma `STDIO_FD`; qui ogni write/read
//! controlla lo slot (-1 = console/default, -2 = null, >= 0 = fd cardo).
//! `Stderr` e' un tipo proprio (non alias): stderr va allo slot 2.

use crate::sys::pal::write_console as pal_write;
use crate::io::{self, IoSlice, IoSliceMut};
use crate::sync::atomic::AtomicI64;

/// fd stdio: -1 console/default, -2 null (discard/EOF), >= 0 fd cardo.
// NOTE: scritto una volta da `_start` (pre-main, single-thread) e letto
// sempre: niente lock, solo atomici.
static STDIO_FD: [AtomicI64; 3] =
    [AtomicI64::new(-1), AtomicI64::new(-1), AtomicI64::new(-1)];

/// Arma lo slot stdio (0/1/2) da `_start` (S2.4c3). Ignora slot invalidi.
pub fn set_stdio_fd(slot: usize, fd: i64) {
    if slot < 3 {
        STDIO_FD[slot].store(fd, crate::sync::atomic::Ordering::Relaxed);
    }
}

fn stdio_fd(slot: usize) -> i64 {
    STDIO_FD[slot].load(crate::sync::atomic::Ordering::Relaxed)
}

pub struct Stdin;
pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // S2.4c3: fd redirectato (figlio con stdin piped) o EOF.
        match stdio_fd(0) {
            fd if fd >= 0 => crate::sys::fs::velordo::pipe_read(fd, buf),
            _ => Ok(0),
        }
    }

    #[inline]
    fn read_vectored(&mut self, _bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        Ok(0)
    }

    #[inline]
    fn is_read_vectored(&self) -> bool {
        false
    }

    // Niente override read_exact/to_end/to_string: i default chiamano
    // `read` (fd o EOF) ed ereditano il redirect gratis.
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

/// Scrive un buffer sullo slot stdio `slot` (1 = stdout, 2 = stderr):
/// console, discard (-2 = Ok tutto), o fd cardo.
fn write_slot(slot: usize, buf: &[u8]) -> io::Result<usize> {
    match stdio_fd(slot) {
        fd if fd >= 0 => crate::sys::fs::velordo::pipe_write(fd, buf),
        -2 => Ok(buf.len()),
        _ => Ok(pal_write(buf)),
    }
}

impl io::Write for Stdout {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_slot(1, buf)
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut n = 0;
        for b in bufs {
            n += write_slot(1, b)?;
        }
        Ok(n)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        // SYS_WRITE puo' scrivere corto: loop fino a tutto (mai perdita
        // silenziosa a meta' riga). Vale anche per gli fd (stessa forma).
        let mut done = 0;
        while done < buf.len() {
            let n = self.write(&buf[done..])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
            }
            done += n;
        }
        Ok(())
    }

    #[inline]
    fn write_all_vectored(&mut self, bufs: &mut [IoSlice<'_>]) -> io::Result<()> {
        for b in bufs {
            self.write_all(b)?;
        }
        Ok(())
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // seriale senza buffering: niente da scaricare
    }
}

impl Stderr {
    pub const fn new() -> Stderr {
        Stderr
    }
}

impl io::Write for Stderr {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_slot(2, buf)
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut n = 0;
        for b in bufs {
            n += write_slot(2, b)?;
        }
        Ok(n)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let n = self.write(&buf[done..])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
            }
            done += n;
        }
        Ok(())
    }

    #[inline]
    fn write_all_vectored(&mut self, bufs: &mut [IoSlice<'_>]) -> io::Result<()> {
        for b in bufs {
            self.write_all(b)?;
        }
        Ok(())
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub const STDIN_BUF_SIZE: usize = 0;

pub fn is_ebadf(_err: &io::Error) -> bool {
    true
}

pub fn panic_output() -> Option<impl crate::io::Write> {
    // S2.5d: rtabort! scriveva nel vuoto (None = aborti runtime silenti,
    // codice 134 senza messaggio — undebuggabili). Stderr diretto: niente
    // lock, niente alloc, solo raw write sullo slot 2 (console).
    Some(Stderr::new())
}
