//! Client dell'endpoint del broker di rete `NetBroker` (ADR-0080, fase 6).
//! Stesso modello di `civis::net::endpoint` (ring SPSC in shm, cursore TX
//! locale, backpressure senza drop silenzioso) ma verso il servizio
//! `Service::NetBroker`: il codice `Untrusted` non ottiene mai
//! `Service::Net`/`Service::Siit`; tutto il suo traffico passa per il broker.
//! Il broker applica allowlist di porte, rate limit per identita' e
//! consenso per immagine, e fa audit in Vestigia.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::Error;
use broker_proto as proto;
use net_ep;
use syscall_numbers as sys;

/// Lunghezza del ring di un endpoint broker (stesso di `net`).
const RING_LEN: usize = sys::NET_EP_RING_HDR * 2
    + sys::NET_EP_RING_SLOTS * sys::NET_EP_SLOT_SIZE * 2;

/// Protocollo (e uscita) dell'endpoint broker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Udp,
    Tcp,
    /// Uscita IPv4 attraverso il traduttore SIIT/NAT64 (N4): il broker apre
    /// l'endpoint presso `siit`, con il layout degli slot IPv4.
    Ipv4,
}

impl Proto {
    const fn wire(self) -> u16 {
        match self {
            Proto::Udp => sys::NET_PROTO_UDP as u16,
            Proto::Tcp => sys::NET_PROTO_TCP as u16,
            Proto::Ipv4 => sys::NET_PROTO_IPV4 as u16,
        }
    }

    const fn layout(self) -> net_ep::Layout {
        match self {
            Proto::Ipv4 => net_ep::Layout::IPV4,
            Proto::Udp | Proto::Tcp => net_ep::Layout::IPV6,
        }
    }
}

/// Come l'endpoint si lega al broker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Ephemeral,
    Bind(u16),
    Connect,
}

impl Flow {
    const fn wire(self) -> Option<(u16, u64)> {
        match self {
            Flow::Ephemeral => Some((0, sys::NET_EP_FLAG_EPHEMERAL)),
            Flow::Bind(p) if p != 0 => Some((p, 0)),
            Flow::Connect => Some((0, sys::NET_EP_FLAG_CONNECT)),
            Flow::Bind(_) => None,
        }
    }
}

/// Evento trasportato dallo slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Data,
    SynAck,
    Fin,
    Rst,
    Accept,
    Other(u8),
}

impl Event {
    pub const fn wire(self) -> u8 {
        match self {
            Event::Data => 0,
            Event::SynAck => 2,
            Event::Fin => 3,
            Event::Rst => 4,
            Event::Accept => 5,
            Event::Other(v) => v,
        }
    }
    pub const fn from_wire(v: u8) -> Self {
        match v {
            0 => Event::Data,
            2 => Event::SynAck,
            3 => Event::Fin,
            4 => Event::Rst,
            5 => Event::Accept,
            other => Event::Other(other),
        }
    }
}

/// Esito di un invio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    TooBig,
    Full,
    Dead,
}

/// Uno slot ricevuto.
#[derive(Clone, Copy, Debug)]
pub struct Recv {
    pub addr: [u8; 16],
    pub port: u16,
    pub local: u16,
    pub event: Event,
    pub len: usize,
    pub truncated: bool,
}

/// Endpoint del broker di rete.
pub struct BrokerEndpoint {
    chan: u64,
    id: u16,
    ring: *mut u8,
    tx_seq: u32,
    local_port: u16,
    /// Layout degli slot: dipende dall'uscita scelta all'apertura.
    layout: net_ep::Layout,
    /// Lunghezza dell'indirizzo in uno slot (`Recv::addr` e' sempre 16 byte:
    /// per IPv4 i primi 12 sono zero).
    addr_len: usize,
}

impl BrokerEndpoint {
    /// Apre un endpoint presso il broker sul canale `chan`.
    pub fn open(chan: u64, proto: Proto, flow: Flow) -> Result<Self, Error> {
        let Some((port, flags)) = flow.wire() else {
            return Err(Error::Invalid);
        };
        let r = crate::send(
            chan,
            proto::BROKER_OPEN,
            (proto.wire() as u64) << 16 | port as u64,
            flags,
        )?;
        if r.w0 == !0u64 {
            return Err(Error::Invalid);
        }
        let ring_id = (r.w1 & 0xffff_ffff) as u32;
        let base = crate::shm_map(ring_id, 0, crate::PROT_READ | crate::PROT_WRITE)?
            as *mut u8;
        let ep = Self {
            chan,
            id: r.w0 as u16,
            ring: base,
            tx_seq: 0,
            local_port: if port == 0 {
                (r.w1 >> 32) as u16
            } else {
                port
            },
            layout: proto.layout(),
            addr_len: proto.layout().addr_len(),
        };
        ep.reset_rings();
        Ok(ep)
    }

    pub fn id(&self) -> u16 {
        self.id
    }
    pub fn local_port(&self) -> u16 {
        self.local_port
    }
    pub fn chan(&self) -> u64 {
        self.chan
    }

    /// Invia `payload` a `addr:port`.
    pub fn send(
        &mut self,
        addr: [u8; 16],
        port: u16,
        event: Event,
        payload: &[u8],
    ) -> Result<(), SendError> {
        let total = self.layout.total(payload.len());
        if total > sys::NET_EP_SLOT_SIZE {
            return Err(SendError::TooBig);
        }
        let slot = (self.tx_seq % sys::NET_EP_RING_SLOTS as u32) as usize;
        let p = unsafe {
            self.ring.add(
                sys::NET_EP_TX_RING_OFF + sys::NET_EP_RING_HDR + slot * sys::NET_EP_SLOT_SIZE,
            )
        };
        let win = unsafe { core::slice::from_raw_parts_mut(p, total) };
        // `addr` ha sempre 16 byte; per l'uscita IPv4 contano gli ultimi 4
        // (la stessa convenzione di [`Recv::addr`]: un indirizzo SIIT
        // `64:ff9b::a.b.c.d` porta l'IPv4 in coda).
        net_ep::write(
            win,
            self.layout,
            &addr[addr.len() - self.addr_len..],
            port,
            0,
            event.wire(),
            payload,
        )
        .ok_or(SendError::TooBig)?;
        fence(Ordering::Release);
        match crate::send(self.chan, proto::BROKER_TX, self.id as u64, slot as u64) {
            Ok(r) if r.w0 == 0 => {
                self.tx_seq = net_ep::next(self.tx_seq);
                Ok(())
            }
            Ok(_) => Err(SendError::Full),
            Err(_) => Err(SendError::Dead),
        }
    }

    /// Copia il prossimo slot ricevuto in `buf` e consuma lo slot.
    pub fn recv(&mut self, buf: &mut [u8]) -> Option<Recv> {
        loop {
            let (head, tail) = self.rx_positions();
            if head == tail {
                return None;
            }
            let slot = (head % sys::NET_EP_RING_SLOTS as u32) as usize;
            let p = unsafe {
                self.ring.add(
                    sys::NET_EP_RX_RING_OFF + sys::NET_EP_RING_HDR + slot * sys::NET_EP_SLOT_SIZE,
                )
            };
            let total = unsafe { read_volatile(p as *const u16) } as usize;
            let out = match net_ep::check(self.layout, total, sys::NET_EP_SLOT_SIZE) {
                None => None,
                Some(_) => {
                    let win = unsafe { core::slice::from_raw_parts(p, total) };
                    net_ep::read(win, self.layout, total).map(|s| {
                        let n = s.payload.len().min(buf.len());
                        buf[..n].copy_from_slice(&s.payload[..n]);
                        let mut addr = [0u8; 16];
                        // IPv4: l'indirizzo sta negli ultimi 4 byte (il resto
                        // resta zero), cosi' `Recv::addr` ha una forma sola.
                        let off = 16 - s.addr.len();
                        addr[off..].copy_from_slice(s.addr);
                        Recv {
                            addr,
                            port: s.port,
                            local: s.local,
                            event: Event::from_wire(s.event),
                            len: s.payload.len(),
                            truncated: s.payload.len() > buf.len(),
                        }
                    })
                }
            };
            self.advance_rx(head);
            if let Some(r) = out {
                return Some(r);
            }
        }
    }

    /// Statistiche dell'endpoint.
    pub fn stats(&mut self) -> Option<(u64, u64)> {
        match crate::send(self.chan, proto::BROKER_STATS, self.id as u64, 0) {
            Ok(r) if r.w0 != !0u64 => Some((r.w0, r.w1)),
            _ => None,
        }
    }

    /// Chiude l'endpoint presso il broker e smappa il ring.
    pub fn close(&mut self) {
        if self.ring.is_null() {
            return;
        }
        let _ = crate::send(self.chan, proto::BROKER_CLOSE, self.id as u64, 0);
        let _ = crate::munmap(self.ring as usize, RING_LEN);
        self.ring = core::ptr::null_mut();
    }

    fn reset_rings(&self) {
        unsafe {
            write_volatile(self.ring.add(sys::NET_EP_RX_RING_OFF) as *mut u32, 0);
            write_volatile(self.ring.add(sys::NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
            write_volatile(self.ring.add(sys::NET_EP_TX_RING_OFF) as *mut u32, 0);
            write_volatile(self.ring.add(sys::NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
        }
    }

    fn rx_positions(&self) -> (u32, u32) {
        unsafe {
            (
                read_volatile(self.ring.add(sys::NET_EP_RX_RING_OFF) as *const u32),
                read_volatile(self.ring.add(sys::NET_EP_RX_RING_OFF + 4) as *const u32),
            )
        }
    }

    fn advance_rx(&self, head: u32) {
        self.set_rx_head(net_ep::next(head));
    }

    fn set_rx_head(&self, v: u32) {
        unsafe {
            write_volatile(self.ring.add(sys::NET_EP_RX_RING_OFF) as *mut u32, v);
        }
    }
}
