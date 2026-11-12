//! Endpoint TCP del servizio `net` visto come stream (blocking).
//!
//! Il protocollo e' quello di `net` (ADR-0069): ring SPSC in shm con
//! `NET_EP_RING_SLOTS` slot da `NET_EP_SLOT_SIZE`. Due dettagli che qui
//! diventano contratto:
//!
//! - **slot TX**: `[len:2][addr:16][dport:2][sport:2][evento:1][payload]`.
//!   `NET_EP_TX` risponde `w0 = 0` se il payload e' stato preso in carico,
//!   `!0` se il buffer di invio TCP e' pieno (si ritenta lo stesso slot al
//!   tick dopo: e' backpressure, non errore).
//! - **slot TX con evento `EV_ABORT`**: chiude la *connessione* lasciando
//!   vivo l'endpoint (`conn_id = NO_CONN`). E' cio' che serve al reconnect
//!   per-query del DoT senza aspettare FIN/TIME_WAIT.
//!
//! L'endpoint resta aperto per tutta la vita del processo: il ring si mappa
//! una volta (endpoint persistente: ricrearlo per query non serve).

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use civis::{
    NET_EP_CLOSE, NET_EP_FLAG_CONNECT, NET_EP_OPEN, NET_EP_RING_SLOTS, NET_EP_RX_RING_OFF,
    NET_EP_RING_HDR, NET_EP_SLOT_SIZE, NET_EP_TX, NET_EP_TX_RING_OFF, NET_PROTO_TCP,
};
use tls::{IoError, Transport};

/// Attesa massima di default per un record in arrivo (10 s a 100 Hz).
pub const DEFAULT_READ_TICKS: u64 = 1000;
/// Attesa massima di default per far posto nel buffer di invio TCP.
pub const DEFAULT_WRITE_TICKS: u64 = 300;

/// Chunk di scrittura: <= `SEND_BUF` del core TCP (4096) meno il margine.
/// Il servizio non riporta l'accettazione parziale (solo 0/!0), quindi un
/// chunk che entra intero nel buffer rende la semantica di stream esatta.
const WRITE_CHUNK: usize = 1024;

/// Metadata slot di `net`: `[len:2][addr:16][dport:2][sport:2][evento:1]`.
const SLOT_META: usize = 23;

// Eventi (convenzione condivisa con `userland/net`).
const EV_DATA: u8 = 0;
const EV_SYNACK: u8 = 2;
const EV_FIN: u8 = 3;
const EV_RST: u8 = 4;
/// Chiusura immediata della connessione, endpoint vivo (vedi modulo).
const EV_ABORT: u8 = 6;

static FUTEX: u32 = 0;

/// Stream TCP su un endpoint del servizio `net`.
pub struct TcpStream {
    net_chan: u64,
    ep_id: u16,
    ring: *mut u8,
    dst: [u8; 16],
    port: u16,
    /// Indice di slot TX (modulo `NET_EP_RING_SLOTS`).
    tx_seq: u32,
    rx_head: u32,
    /// Payload di uno slot RX piu' grande della richiesta: il resto va qui
    /// (semantica di stream, mai byte persi).
    pending: Vec<u8>,
    ppos: usize,
    eof: bool,
    read_ticks: u64,
    write_ticks: u64,
}

impl TcpStream {
    /// Apre un endpoint TCP CONNECT verso `dst:port` e ne mappa il ring.
    ///
    /// Non apre la connessione: la apre il primo `write` (il core fa SYN al
    /// primo `NET_EP_TX`, come da convenzione N6.1c).
    pub fn connect(net_chan: u64, dst: [u8; 16], port: u16) -> Option<Self> {
        if port == 0 {
            return None;
        }
        let r = civis::send(
            net_chan,
            NET_EP_OPEN,
            (NET_PROTO_TCP as u64) << 16,
            NET_EP_FLAG_CONNECT as u64,
        )
        .ok()?;
        if r.w0 == !0u64 {
            return None;
        }
        let ring_id = (r.w1 & 0xffff_ffff) as u32;
        let ring = civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE).ok()? as *mut u8;
        unsafe {
            write_volatile(ring.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
            write_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
            write_volatile(ring.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
            write_volatile(ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
        }
        Some(Self {
            net_chan,
            ep_id: r.w0 as u16,
            ring,
            dst,
            port,
            tx_seq: 0,
            rx_head: 0,
            pending: Vec::new(),
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

    /// Id dell'endpoint (diagnostica).
    pub fn ep_id(&self) -> u16 {
        self.ep_id
    }

    /// Chiude la connessione lasciando vivo l'endpoint: la prossima `write`
    /// ne apre una nuova. Scarta gli eventi stantii della precedente.
    pub fn abort(&mut self) {
        let slot = (self.tx_seq % NET_EP_RING_SLOTS as u32) as usize;
        write_slot(self.ring, slot, &self.dst, self.port, EV_ABORT, &[]);
        self.tx_seq = self.tx_seq.wrapping_add(1);
        let _ = civis::send(self.net_chan, NET_EP_TX, self.ep_id as u64, slot as u64);
        self.drain_rx();
        self.reset_stream();
    }

    /// Chiude l'endpoint presso `net` (fine vita del processo).
    pub fn shutdown(&mut self) {
        let _ = civis::send(self.net_chan, NET_EP_CLOSE, self.ep_id as u64, 0);
    }

    /// Svuota lo stato di stream (eventi e byte della connessione precedente).
    fn reset_stream(&mut self) {
        self.pending.clear();
        self.ppos = 0;
        self.eof = false;
    }

    /// Consuma senza consegnare tutto cio' che e' nel ring RX.
    fn drain_rx(&mut self) {
        let tail = unsafe { read_volatile(self.ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        self.rx_head = tail;
        unsafe {
            write_volatile(self.ring.add(NET_EP_RX_RING_OFF) as *mut u32, tail);
        }
    }

    fn take(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.pending.len() - self.ppos);
        buf[..n].copy_from_slice(&self.pending[self.ppos..self.ppos + n]);
        self.ppos += n;
        n
    }

    fn wait_tick() {
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
}

impl Transport for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if self.ppos < self.pending.len() {
            return Ok(self.take(buf));
        }
        if self.eof {
            return Err(IoError::Closed);
        }
        let deadline = civis::get_ticks() as u64 + self.read_ticks;
        loop {
            let tail =
                unsafe { read_volatile(self.ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
            if self.rx_head != tail {
                let slot = (self.rx_head % NET_EP_RING_SLOTS as u32) as usize;
                let p = unsafe {
                    self.ring
                        .add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
                };
                let total = unsafe { read_volatile(p as *const u16) } as usize;
                let event = if total >= SLOT_META {
                    unsafe { read_volatile(p.add(22)) }
                } else {
                    0xff
                };
                let len = total.saturating_sub(SLOT_META);
                self.pending.clear();
                self.ppos = 0;
                if event == EV_DATA && len > 0 {
                    self.pending
                        .extend_from_slice(unsafe { core::slice::from_raw_parts(p.add(SLOT_META), len) });
                }
                unsafe {
                    write_volatile(
                        self.ring.add(NET_EP_RX_RING_OFF) as *mut u32,
                        self.rx_head.wrapping_add(1),
                    )
                };
                self.rx_head = self.rx_head.wrapping_add(1);
                match event {
                    EV_DATA if len > 0 => return Ok(self.take(buf)),
                    EV_SYNACK => continue, // handshake: lo ha gia' chiuso il core
                    EV_RST | EV_FIN => {
                        self.eof = true;
                        return Err(IoError::Closed);
                    }
                    _ => continue,
                }
            }
            if civis::get_ticks() as u64 >= deadline {
                return Err(IoError::Failed);
            }
            Self::wait_tick();
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.eof {
            return Err(IoError::Closed);
        }
        let chunk = buf.len().min(WRITE_CHUNK);
        let deadline = civis::get_ticks() as u64 + self.write_ticks;
        loop {
            let slot = (self.tx_seq % NET_EP_RING_SLOTS as u32) as usize;
            write_slot(self.ring, slot, &self.dst, self.port, EV_DATA, &buf[..chunk]);
            match civis::send(self.net_chan, NET_EP_TX, self.ep_id as u64, slot as u64) {
                Ok(r) if r.w0 == 0 => {
                    self.tx_seq = self.tx_seq.wrapping_add(1);
                    return Ok(chunk);
                }
                _ => {
                    // Buffer di invio TCP pieno (backpressure): stesso slot
                    // al tick dopo. Se sfonda la deadline e' un errore vero.
                    if civis::get_ticks() as u64 >= deadline {
                        return Err(IoError::Failed);
                    }
                    Self::wait_tick();
                }
            }
        }
    }

    fn flush(&mut self) -> Result<(), IoError> {
        // `net` trasmette al tick: non c'e' un buffer applicativo da svuotare.
        Ok(())
    }
}

/// Scrive lo slot TX `slot`: `[len][addr][dport][sport][evento][payload]`.
fn write_slot(ring: *mut u8, slot: usize, dst: &[u8; 16], port: u16, event: u8, payload: &[u8]) {
    let total = SLOT_META + payload.len();
    let p = unsafe { ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(dst.as_ptr(), p.add(2), 16);
        write_volatile(p.add(18) as *mut u16, port);
        write_volatile(p.add(20) as *mut u16, 0); // sorgente: la sceglie `net`
        write_volatile(p.add(22) as *mut u8, event);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(SLOT_META), payload.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            (slot + 1) as u32,
        )
    };
}