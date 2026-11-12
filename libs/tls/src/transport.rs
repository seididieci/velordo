//! Trasporto iniettato: TLS non sa cosa sia un socket.
//!
//! Nel sistema reale il trasporto e' l'endpoint TCP del servizio `net` (ring
//! SPSC + IPC); nei test host e' un buffer in memoria. `embedded-tls` vuole
//! `embedded_io::{Read, Write}` blocking: l'adapter `Adapter<T>` fa da ponte,
//! cosi' il confine del crate resta un trait nostro e piccolo.

use core::fmt;

/// Errore di trasporto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoError {
    /// Il peer ha chiuso la connessione (EOF).
    Closed,
    /// Errore o timeout del trasporto.
    Failed,
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IoError::Closed => f.write_str("trasporto chiuso dal peer"),
            IoError::Failed => f.write_str("errore di trasporto"),
        }
    }
}

impl core::error::Error for IoError {}

impl embedded_io::Error for IoError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            IoError::Closed => embedded_io::ErrorKind::BrokenPipe,
            IoError::Failed => embedded_io::ErrorKind::Other,
        }
    }
}

/// Sorgente/sink di byte per TLS: stream **blocking**, come `read(2)`.
///
/// Contratto: `read` blocca finche' almeno un byte e' disponibile e ritorna
/// `Err(IoError::Closed)` a EOF (mai `Ok(0)` con buffer non vuoto: in
/// `embedded-io` quello sarebbe EOF silenzioso). `write` puo' accettare meno
/// byte del richiesto (finestra TCP piena): il chiamante ritenta.
pub trait Transport {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError>;
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError>;

    /// Consegna quanto eventualmente bufferizzato dall'implementazione.
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

/// Ponte fra `Transport` e `embedded_io` (blocking).
pub struct Adapter<T: Transport>(pub T);

/// Un trasporto preso per riferimento e' ancora un trasporto: permette a
/// `connect(&mut stream, …)` di lasciare il socket **al chiamante** (che lo
/// riusa per la connessione successiva, es. DoT una connessione per query).
impl<T: Transport + ?Sized> Transport for &mut T {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        (**self).read(buf)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        (**self).write(buf)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        (**self).flush()
    }
}

impl<T: Transport> embedded_io::ErrorType for Adapter<T> {
    type Error = IoError;
}

impl<T: Transport> embedded_io::Read for Adapter<T> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.0.read(buf)
    }
}

impl<T: Transport> embedded_io::Write for Adapter<T> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_io::{Read, Write};

    /// Trasporto su due buffer: legge da `rx`, scrive in `tx`.
    struct Buffers {
        rx: alloc::vec::Vec<u8>,
        rpos: usize,
        tx: alloc::vec::Vec<u8>,
    }

    impl Transport for Buffers {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
            if self.rpos >= self.rx.len() {
                return Err(IoError::Closed);
            }
            let n = buf.len().min(self.rx.len() - self.rpos);
            buf[..n].copy_from_slice(&self.rx[self.rpos..self.rpos + n]);
            self.rpos += n;
            Ok(n)
        }

        fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    #[test]
    fn adapter_read_write_roundtrip() {
        let mut a = Adapter(Buffers {
            rx: alloc::vec![1, 2, 3, 4, 5],
            rpos: 0,
            tx: alloc::vec::Vec::new(),
        });
        let mut buf = [0u8; 3];
        assert_eq!(a.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, &[1, 2, 3]);
        assert_eq!(a.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[4, 5]);
        // EOF: mai Ok(0) con buffer non vuoto.
        assert_eq!(a.read(&mut buf).unwrap_err(), IoError::Closed);
        a.write_all(b"ciao").unwrap();
        assert_eq!(&a.0.tx, b"ciao");
    }

    #[test]
    fn adapter_empty_read_is_zero() {
        let mut a = Adapter(Buffers {
            rx: alloc::vec![9],
            rpos: 0,
            tx: alloc::vec::Vec::new(),
        });
        assert_eq!(a.read(&mut []).unwrap(), 0);
    }
}
