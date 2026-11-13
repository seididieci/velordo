//! Stream TCP su un endpoint del servizio `net`.
//!
//! L'endpoint resta aperto per tutta la vita del processo e il ring si mappa
//! una volta: ricrearlo per connessione non serve. `abort` chiude la
//! *connessione* lasciando vivo l'endpoint (serve al reconnect per-query del
//! DoT: FIN/TIME_WAIT non si aspettano).

use alloc::vec::Vec;

use super::{Endpoint, Event, Flow, Proto, SendError};
use crate::NET_EP_SLOT_SIZE;

/// Attesa massima di default per un record in arrivo (10 s a 100 Hz).
pub const DEFAULT_READ_TICKS: u64 = 1000;
/// Attesa massima di default per far posto nel buffer di invio TCP.
pub const DEFAULT_WRITE_TICKS: u64 = 300;

/// Chunk di scrittura: <= `SEND_BUF` del core TCP (4096) meno il margine.
/// Il servizio non riporta l'accettazione parziale (solo 0/!0), quindi un
/// chunk che entra intero nel buffer rende la semantica di stream esatta.
const WRITE_CHUNK: usize = 1024;

/// Capacita' di un payload in uno slot (metadati IPv6 esclusi).
const RX_CAP: usize = NET_EP_SLOT_SIZE - net_ep::Layout::IPV6.meta();

static FUTEX: u32 = 0;

/// Errore di stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// Il peer (o l'endpoint) ha chiuso: EOF.
    Closed,
    /// Deadline scaduta (peer muto o buffer di invio sempre pieno).
    Timeout,
    /// Il servizio `net` non risponde piu'.
    Dead,
}

/// Stream TCP blocking sopra un endpoint.
pub struct Stream {
    ep: Endpoint,
    dst: [u8; 16],
    port: u16,
    /// Payload dell'ultimo slot ricevuto (uno slot per lettura).
    buf: Vec<u8>,
    plen: usize,
    ppos: usize,
    eof: bool,
    read_ticks: u64,
    write_ticks: u64,
}

impl Stream {
    /// Apre un endpoint TCP CONNECT verso `dst:port` e ne mappa il ring.
    ///
    /// Non apre la connessione: la apre il primo `write` (il core fa SYN al
    /// primo `NET_EP_TX`, convenzione N6.1c).
    pub fn connect(chan: u64, dst: [u8; 16], port: u16) -> Option<Self> {
        if port == 0 {
            return None;
        }
        let ep = Endpoint::open(chan, Proto::Tcp, Flow::Connect).ok()?;
        let mut buf = Vec::new();
        buf.resize(RX_CAP, 0);
        Some(Self {
            ep,
            dst,
            port,
            buf,
            plen: 0,
            ppos: 0,
            eof: false,
            read_ticks: DEFAULT_READ_TICKS,
            write_ticks: DEFAULT_WRITE_TICKS,
        })
    }

    /// Imposta le deadline (tick) delle attese di lettura e scrittura.
    pub fn set_timeout(&mut self, read_ticks: u64, write_ticks: u64) {
        self.read_ticks = read_ticks.max(1);
        self.write_ticks = write_ticks.max(1);
    }

    pub fn ep_id(&self) -> u16 {
        self.ep.id()
    }

    pub fn local_port(&self) -> u16 {
        self.ep.local_port()
    }

    /// Chiude la connessione lasciando vivo l'endpoint: la prossima `write`
    /// ne apre una nuova. Scarta gli eventi stantii della precedente.
    pub fn abort(&mut self) {
        let _ = self.ep.send(self.dst, self.port, Event::Abort, &[]);
        self.ep.drain();
        self.reset_stream();
    }

    /// Chiude l'endpoint presso `net` (fine vita del processo).
    pub fn shutdown(&mut self) {
        self.ep.close();
    }

    /// Legge almeno un byte (blocking fino alla deadline), `Closed` a EOF.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, StreamError> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.ppos < self.plen {
            return Ok(self.take(out));
        }
        if self.eof {
            return Err(StreamError::Closed);
        }
        let deadline = crate::get_ticks() as u64 + self.read_ticks;
        loop {
            let r = self.ep.recv(self.buf.as_mut_slice());
            if let Some(r) = r {
                self.plen = r.len.min(self.buf.len());
                self.ppos = 0;
                match r.event {
                    Event::Data if self.plen > 0 => return Ok(self.take(out)),
                    Event::SynAck => continue, // handshake: lo chiude il core
                    Event::Fin | Event::Rst => {
                        self.eof = true;
                        return Err(StreamError::Closed);
                    }
                    _ => continue,
                }
            }
            if crate::get_ticks() as u64 >= deadline {
                return Err(StreamError::Timeout);
            }
            Self::wait_tick();
        }
    }

    /// Scrive fino a `WRITE_CHUNK` byte; ritorna quanti ne ha presi in carico.
    pub fn write(&mut self, data: &[u8]) -> Result<usize, StreamError> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.eof {
            return Err(StreamError::Closed);
        }
        let chunk = data.len().min(WRITE_CHUNK);
        let deadline = crate::get_ticks() as u64 + self.write_ticks;
        loop {
            match self.ep.send(self.dst, self.port, Event::Data, &data[..chunk]) {
                Ok(()) => return Ok(chunk),
                Err(SendError::Full) => {
                    // Backpressure (buffer di invio TCP pieno): stesso slot al
                    // tick dopo. Se sfonda la deadline e' un errore vero.
                    if crate::get_ticks() as u64 >= deadline {
                        return Err(StreamError::Timeout);
                    }
                    Self::wait_tick();
                }
                Err(SendError::Dead) => return Err(StreamError::Dead),
                // Impossibile con `WRITE_CHUNK`; se accadesse, non e' un timeout.
                Err(SendError::TooBig) => return Err(StreamError::Closed),
            }
        }
    }

    /// `net` trasmette al tick: non c'e' un buffer applicativo da svuotare.
    pub fn flush(&mut self) {}

    fn reset_stream(&mut self) {
        self.plen = 0;
        self.ppos = 0;
        self.eof = false;
    }

    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.plen - self.ppos);
        out[..n].copy_from_slice(&self.buf[self.ppos..self.ppos + n]);
        self.ppos += n;
        n
    }

    fn wait_tick() {
        let _ = crate::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (crate::get_ticks() + 1) as u64,
        );
    }
}
