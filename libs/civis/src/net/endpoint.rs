//! Endpoint a capability del servizio `net`: apertura, invio, ricezione.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::{
    Error, NET_EP_CLOSE, NET_EP_FLAG_CONNECT, NET_EP_FLAG_EPHEMERAL, NET_EP_FLAG_LISTEN,
    NET_EP_OPEN, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_RX_RING_OFF, NET_EP_SLOT_SIZE,
    NET_EP_STATS, NET_EP_TX, NET_EP_TX_RING_OFF, NET_PROTO_TCP, NET_PROTO_UDP,
};

/// Layout degli slot del servizio `net` (IPv6, 23 byte di metadati).
const LAYOUT: net_ep::Layout = net_ep::Layout::IPV6;

/// Lunghezza del ring di un endpoint (RX + TX), la stessa che crea `net`.
const RING_LEN: usize = NET_EP_RING_HDR * 2 + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2;

/// Trasporto dell'endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Udp,
    Tcp,
}

impl Proto {
    const fn wire(self) -> u16 {
        match self {
            Proto::Udp => NET_PROTO_UDP as u16,
            Proto::Tcp => NET_PROTO_TCP as u16,
        }
    }
}

/// Come l'endpoint si lega al servizio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Porta effimera scelta dal servizio (UDP client).
    Ephemeral,
    /// Porta locale richiesta (UDP server).
    Bind(u16),
    /// TCP client: la connessione si apre al primo invio (N6.1c).
    Connect,
    /// TCP server: il core risponde al SYN, gli accept arrivano come
    /// [`Event::Accept`].
    Listen(u16),
}

impl Flow {
    /// `(porta locale, flag)` come li vuole `NET_EP_OPEN`.
    const fn wire(self) -> Option<(u16, u64)> {
        match self {
            Flow::Ephemeral => Some((0, NET_EP_FLAG_EPHEMERAL)),
            Flow::Bind(p) if p != 0 => Some((p, 0)),
            Flow::Connect => Some((0, NET_EP_FLAG_CONNECT)),
            Flow::Listen(p) if p != 0 => Some((p, NET_EP_FLAG_LISTEN)),
            // Porta 0 per bind/listen non ha senso: la sceglierebbe il
            // servizio, e il client non saprebbe dove ascolta.
            Flow::Bind(_) | Flow::Listen(_) => None,
        }
    }
}

/// Evento trasportato dallo slot (0 = dati).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Data,
    SynAck,
    Fin,
    Rst,
    Accept,
    /// Chiusura immediata della connessione, endpoint vivo (N6.3).
    Abort,
    /// Codice non noto a questo client: si ignora, mai panic.
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
            Event::Abort => 6,
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
            6 => Event::Abort,
            other => Event::Other(other),
        }
    }
}

/// Esito di un invio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Payload piu' grande dello slot.
    TooBig,
    /// Il servizio non ha preso in carico lo slot (buffer TCP pieno o ring RX
    /// del destinatario pieno): si ritenta lo stesso payload piu' tardi.
    Full,
    /// Il servizio non risponde (morto): l'endpoint non e' piu' usabile.
    Dead,
}

/// Uno slot ricevuto, copiato nel buffer del chiamante.
#[derive(Clone, Copy, Debug)]
pub struct Recv {
    /// Indirizzo del mittente.
    pub addr: [u8; 16],
    /// Porta del mittente.
    pub port: u16,
    /// Porta locale dell'endpoint.
    pub local: u16,
    pub event: Event,
    /// Lunghezza del payload nello slot (anche se troncato).
    pub len: usize,
    /// `len > buffer del chiamante`: i byte copiati sono solo i primi.
    pub truncated: bool,
}

/// Endpoint del servizio `net`.
///
/// Il ring e' mappato una volta all'apertura; l'endpoint vive finche' il
/// processo non fa [`Endpoint::close`] (o termina).
pub struct Endpoint {
    chan: u64,
    id: u16,
    ring: *mut u8,
    /// Cursore TX locale: indice dello slot da scrivere, avanza solo quando
    /// il servizio prende in carico il payload.
    tx_seq: u32,
    local_port: u16,
}

impl Endpoint {
    /// Apre un endpoint presso il servizio sul canale `chan`.
    pub fn open(chan: u64, proto: Proto, flow: Flow) -> Result<Self, Error> {
        let Some((port, flags)) = flow.wire() else {
            return Err(Error::Invalid);
        };
        let r = crate::send(
            chan,
            NET_EP_OPEN,
            (proto.wire() as u64) << 16 | port as u64,
            flags,
        )?;
        if r.w0 == !0u64 {
            return Err(Error::Invalid);
        }
        let ring_id = (r.w1 & 0xffff_ffff) as u32;
        let base = crate::shm_map(ring_id, 0, crate::PROT_READ | crate::PROT_WRITE)? as *mut u8;
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
        };
        ep.reset_rings();
        Ok(ep)
    }

    /// Adotta il ring di una connessione accettata (`Event::Accept`).
    ///
    /// **Non** azzera i contatori: il servizio puo' aver gia' scritto byte
    /// nel ring RX del figlio prima che il listener leggesse l'evento.
    pub fn attach(chan: u64, id: u16, ring_id: u32, local_port: u16) -> Result<Self, Error> {
        let base = crate::shm_map(ring_id, 0, crate::PROT_READ | crate::PROT_WRITE)? as *mut u8;
        Ok(Self {
            chan,
            id,
            ring: base,
            tx_seq: 0,
            local_port,
        })
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

    /// Invia `payload` a `addr:port` con l'evento `event`.
    ///
    /// Su [`SendError::Full`] lo slot non e' stato consumato: richiamare con
    /// lo **stesso** payload piu' tardi (il cursore non e' avanzato).
    pub fn send(
        &mut self,
        addr: [u8; 16],
        port: u16,
        event: Event,
        payload: &[u8],
    ) -> Result<(), SendError> {
        let total = LAYOUT.total(payload.len());
        if total > NET_EP_SLOT_SIZE {
            return Err(SendError::TooBig);
        }
        let slot = (self.tx_seq % NET_EP_RING_SLOTS as u32) as usize;
        let p = unsafe {
            self.ring
                .add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
        };
        let win = unsafe { core::slice::from_raw_parts_mut(p, total) };
        // Sorgente 0: la porta locale la mette il servizio.
        net_ep::write(win, LAYOUT, &addr, port, 0, event.wire(), payload)
            .ok_or(SendError::TooBig)?;
        fence(Ordering::Release);
        match crate::send(self.chan, NET_EP_TX, self.id as u64, slot as u64) {
            Ok(r) if r.w0 == 0 => {
                self.tx_seq = net_ep::next(self.tx_seq);
                Ok(())
            }
            // Servizio vivo ma buffer pieno: lo slot resta li', lo si riscrive.
            Ok(_) => Err(SendError::Full),
            Err(_) => Err(SendError::Dead),
        }
    }

    /// Copia il prossimo slot ricevuto in `buf` e consuma lo slot.
    ///
    /// `None` = ring RX vuoto. Gli slot corrotti (`len` fuori range) vengono
    /// consumati e saltati; se il payload e' piu' grande di `buf`, i byte in
    /// piu' si perdono e `truncated` lo dice (il chiamante decide).
    pub fn recv(&mut self, buf: &mut [u8]) -> Option<Recv> {
        loop {
            let (head, tail) = self.rx_positions();
            if head == tail {
                return None;
            }
            let slot = (head % NET_EP_RING_SLOTS as u32) as usize;
            let p = unsafe {
                self.ring
                    .add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
            };
            let total = unsafe { read_volatile(p as *const u16) } as usize;
            let out = match net_ep::check(LAYOUT, total, NET_EP_SLOT_SIZE) {
                None => None,
                Some(_) => {
                    let win = unsafe { core::slice::from_raw_parts(p, total) };
                    net_ep::read(win, LAYOUT, total).map(|s| {
                        let n = s.payload.len().min(buf.len());
                        buf[..n].copy_from_slice(&s.payload[..n]);
                        let mut addr = [0u8; 16];
                        addr.copy_from_slice(s.addr);
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

    /// Scarta tutto il ring RX (eventi stantii di una connessione chiusa).
    pub fn drain(&mut self) {
        let (_, tail) = self.rx_positions();
        self.set_rx_head(tail);
    }

    /// Contatori dell'endpoint `(rx, tx)`, o `None` se l'endpoint non esiste.
    pub fn stats(&mut self) -> Option<(u64, u64)> {
        match crate::send(self.chan, NET_EP_STATS, self.id as u64, 0) {
            Ok(r) if r.w0 != !0u64 => Some((r.w0, r.w1)),
            _ => None,
        }
    }

    /// Chiude l'endpoint presso il servizio e smappa il ring.
    ///
    /// Idempotente: dopo la prima chiamata il ring e' nullo e le successive
    /// non fanno nulla (utile per `Stream::shutdown` su un `&mut`).
    pub fn close(&mut self) {
        if self.ring.is_null() {
            return;
        }
        let _ = crate::send(self.chan, NET_EP_CLOSE, self.id as u64, 0);
        let _ = crate::munmap(self.ring as usize, RING_LEN);
        self.ring = core::ptr::null_mut();
    }

    fn reset_rings(&self) {
        unsafe {
            write_volatile(self.ring.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
            write_volatile(self.ring.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
            write_volatile(self.ring.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
            write_volatile(self.ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
        }
    }

    fn rx_positions(&self) -> (u32, u32) {
        unsafe {
            (
                read_volatile(self.ring.add(NET_EP_RX_RING_OFF) as *const u32),
                read_volatile(self.ring.add(NET_EP_RX_RING_OFF + 4) as *const u32),
            )
        }
    }

    fn advance_rx(&self, head: u32) {
        self.set_rx_head(net_ep::next(head));
    }

    fn set_rx_head(&self, v: u32) {
        unsafe {
            write_volatile(self.ring.add(NET_EP_RX_RING_OFF) as *mut u32, v);
        }
    }
}
