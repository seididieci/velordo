//! `TcpStream`: lo stream TCP del meccanismo visto come `tls::Transport`.
//!
//! Il trasporto vero — ring SPSC, cursore TX, backpressure, deadline,
//! pending — vive in `civis::net::Stream` (N7.0, ADR-0074): prima era qui, ma
//! lo stesso codice serve a ogni client TCP (DoT e, in futuro, la personalità
//! POSIX), e il punto unico evita che nascano di nuovo copie con invarianti
//! diverse.
//!
//! Qui resta il `newtype`, non la logica: la *orphan rule* vieta di
//! implementare `tls::Transport` (trait di un crate terzo) per
//! `civis::net::Stream` (tipo di un crate terzo) dentro `tls-net`; il tipo
//! locale intermedio e' l'unico modo, e costa solo la delegazione.
//!
//! Nota di contratto: `tls::Transport::read` non torna mai `Ok(0)` con buffer
//! non vuoto (sarebbe EOF silenzioso), quindi `Closed` e' un errore.

use civis::net::{Stream, StreamError};
use tls::{IoError, Transport};

/// Stream TCP blocking su un endpoint del servizio `net`.
pub struct TcpStream(Stream);

impl TcpStream {
    /// Apre un endpoint TCP CONNECT verso `dst:port` (il SYN parte al primo
    /// `write`: il core apre al primo `NET_EP_TX`, convenzione N6.1c).
    pub fn connect(net_chan: u64, dst: [u8; 16], port: u16) -> Option<Self> {
        Stream::connect(net_chan, dst, port).map(Self)
    }

    /// Deadline (tick) delle attese di lettura e scrittura.
    pub fn set_timeout(&mut self, read_ticks: u64, write_ticks: u64) {
        self.0.set_timeout(read_ticks, write_ticks);
    }

    /// Id dell'endpoint (diagnostica).
    pub fn ep_id(&self) -> u16 {
        self.0.ep_id()
    }

    /// Chiude la connessione lasciando vivo l'endpoint (reconnect per query).
    pub fn abort(&mut self) {
        self.0.abort();
    }

    /// Chiude l'endpoint presso `net`.
    pub fn shutdown(&mut self) {
        self.0.shutdown();
    }
}

impl Transport for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        match self.0.read(buf) {
            Ok(n) => Ok(n),
            Err(StreamError::Closed) => Err(IoError::Closed),
            // Timeout e servizio morto sono, per TLS, lo stesso: trasporto
            // che non risponde (mai confonderli con EOF).
            Err(StreamError::Timeout) | Err(StreamError::Dead) => Err(IoError::Failed),
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        match self.0.write(buf) {
            Ok(n) => Ok(n),
            Err(StreamError::Closed) => Err(IoError::Closed),
            Err(StreamError::Timeout) | Err(StreamError::Dead) => Err(IoError::Failed),
        }
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.0.flush();
        Ok(())
    }
}
