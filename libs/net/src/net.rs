//! State machine IPv6/ICMPv6/NDP (ADR-0069).
//!
//! Pura: riceve frame, emette frame tramite `FrameSink`, segnala eventi.
//! Niente I/O, niente allocazioni per pacchetto, nessun panic su input.

use crate::addr::{Ipv6Addr, MacAddr};
use crate::config::Config;
use crate::eth::{self, ETHERTYPE_IPV6};
use crate::icmpv6::{self, TYPE_ECHO_REPLY, TYPE_ECHO_REQUEST, TYPE_NA, TYPE_NS, TYPE_RA, TYPE_REDIRECT};
use crate::ipv6::{self, HOP_LIMIT, NEXT_ICMPV6, NEXT_TCP, NEXT_UDP};
use crate::ndp;
use crate::tcp;
use crate::udp;

const NEIGHBORS: usize = 8;
/// Connessioni TCP concorrenti nel core (N6.1b).
const TCP_CONNS: usize = 16;
/// Figli simultanei per un listener (N6.1c).
const TCP_BACKLOG: usize = 4;
/// 1 s a 100 Hz.
const RS_INTERVAL: u64 = 100;
const ECHO_INTERVAL: u64 = 100;
const MAX_TRIES: u8 = 3;
/// 30 s a 100 Hz: oltre, la entry è STALE (MAC ancora usabile, da verificare).
const NEIGHBOR_TTL: u64 = 3000;
/// 1 s a 100 Hz: intervallo fra due Neighbor Solicitation di verifica.
const NEIGHBOR_PROBE: u64 = 100;
/// Massimo numero di NS senza risposta prima di scartare la entry.
const NEIGHBOR_MAX_PROBES: u8 = 3;
const ECHO_ID: u16 = 0x5645; // "VE"
const ECHO_DATA: &[u8] = b"velordo";

/// Destinazione dei frame da trasmettere.
pub trait FrameSink {
    /// Emette un frame Ethernet sul link.
    fn send(&mut self, frame: &[u8]);
    /// Consegna un datagramma UDP localmente (loopback o indirizzo proprio):
    /// nessun frame Ethernet viene emesso. Default: no-op.
    fn deliver(&mut self, _src: &Ipv6Addr, _dst: &Ipv6Addr, _src_port: u16, _dst_port: u16, _payload: &[u8]) {}
    /// Segnala un cambiamento su una connessione TCP identificata dall'**id**
    /// di connessione (N6.1b/c). Il dato vero si legge con `Net::tcp_read`;
    /// qui arriva solo la notifica, così il sink non deve chiamare `Net`.
    fn tcp_signal(&mut self, _id: u8, _signal: TcpSignal) {}
}

/// Evento di una connessione TCP verso il proprietario dell'endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpSignal {
    /// Una SYN è arrivata su un listener: è nato un figlio in SYN_RECV.
    /// Porta la **porta locale** del listener (il servizio crea l'endpoint
    /// del figlio e manda `EV_ACCEPT`).
    Accepted(u16),
    /// Handshake completato (`EV_SYNACK` lato servizio).
    Established,
    /// Ci sono byte da leggere (`EV_DATA`).
    Data,
    /// Il peer ha chiuso la sua direzione: EOF (`EV_FIN`).
    Fin,
    /// Connessione abortita/reset (`EV_RST`).
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    LinkUp,
    Ra {
        router: Ipv6Addr,
        prefix: Option<[u8; 8]>,
    },
    Address(Ipv6Addr),
    EchoReply {
        from: Ipv6Addr,
        seq: u16,
    },
    EchoTimeout,
    Dropped(&'static str),
}

#[derive(Clone, Copy, Default)]
struct Neighbor {
    addr: Ipv6Addr,
    mac: MacAddr,
    valid: bool,
    /// Ultima conferma di raggiungibilità (RA/NS/NA): oltre `NEIGHBOR_TTL`
    /// la entry è STALE, non morta.
    last: u64,
    /// NS di verifica inviate senza risposta.
    probes: u8,
    /// Istante dell'ultima NS di verifica.
    probed: u64,
}

pub struct Net {
    cfg: Config,
    mac: MacAddr,
    link_local: Ipv6Addr,
    global: Option<Ipv6Addr>,
    router: Option<Ipv6Addr>,
    router_mac: Option<MacAddr>,
    prefix: Option<[u8; 8]>,
    neighbors: [Neighbor; NEIGHBORS],
    pending_echo: Option<(u16, u16)>,
    echo_target: Option<Ipv6Addr>,
    echo_sent: u8,
    rs_sent: u8,
    last_rs: u64,
    last_echo: u64,
    rng: u64,
    link_up: bool,
    echo_done: bool,
    tcp_seq_counter: u32,
    tcp: [Option<tcp::Conn>; TCP_CONNS],
    /// Buffer di serializzazione dei segmenti in uscita: sull'heap, perché
    /// 1,4 KiB di stack per chiamata sbordano lo stack user da 16 KiB.
    tcp_tx: alloc::vec::Vec<u8>,
}

impl Net {
    pub fn new(cfg: Config) -> Self {
        let mut n = Self {
            cfg,
            mac: cfg.mac,
            link_local: Ipv6Addr::UNSPECIFIED,
            global: None,
            router: None,
            router_mac: None,
            prefix: None,
            neighbors: [Neighbor::default(); NEIGHBORS],
            pending_echo: None,
            echo_target: None,
            echo_sent: 0,
            rs_sent: 0,
            last_rs: 0,
            last_echo: 0,
            rng: cfg.seed | 1,
            link_up: false,
            echo_done: false,
            tcp_seq_counter: 0x1000,
            tcp: core::array::from_fn(|_| None),
            tcp_tx: alloc::vec![0u8; 1500 - ipv6::IPV6_HDR - eth::ETH_HDR],
        };
        // IID randomizzato (RFC 4941) con bit U/L = locale.
        let iid = n.next_rand() | 0x0200_0000_0000_0000;
        n.link_local = Ipv6Addr::from_link_local_iid(iid);
        n
    }

    pub fn mac(&self) -> MacAddr {
        self.mac
    }

    pub fn link_local(&self) -> Ipv6Addr {
        self.link_local
    }

    pub fn global(&self) -> Option<Ipv6Addr> {
        self.global
    }

    pub fn router(&self) -> Option<Ipv6Addr> {
        self.router
    }

    pub fn router_mac(&self) -> Option<MacAddr> {
        self.router_mac
    }

    /// Ritorna il prossimo numero di sequenza TCP (contatore semplice).
    pub fn tcp_next_seq(&mut self) -> u32 {
        let s = self.tcp_seq_counter;
        self.tcp_seq_counter = self.tcp_seq_counter.wrapping_add(1);
        s
    }

    /// Ritorna `true` se `addr` è uno dei nostri indirizzi (link-local, global, loopback).
    pub fn is_our_address(&self, addr: Ipv6Addr) -> bool {
        addr == self.link_local || self.global == Some(addr) || addr.is_loopback()
    }

    /// Invia un datagramma UDP a `dst`.
    ///
    /// Seleziona l'indirizzo sorgente, risolve il MAC di prossimo hop (cache vicinato
    /// per link-local, MAC router per off-link) oppure consegna localmente via
    /// `sink.deliver()` per indirizzi propri/loopback. Ritorna `true` se il datagramma
    /// è stato accettato (inviato o consegnato localmente), `false` se non c'è rotta.
    pub fn send_udp(
        &mut self,
        dst: Ipv6Addr,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
        sink: &mut dyn FrameSink,
    ) -> bool {
        let total = udp::UDP_HDR + payload.len();
        if total > 1500 - ipv6::IPV6_HDR - eth::ETH_HDR {
            return false; // troppo grande per un frame Ethernet
        }

        // Indirizzo sorgente.
        let src = if dst.is_loopback() || dst == self.link_local {
            self.link_local
        } else if dst.is_link_local() {
            self.link_local
        } else if let Some(g) = self.global {
            g
        } else {
            self.link_local
        };

        // Consegna locale per indirizzi propri/loopback: nessun frame Ethernet.
        if dst.is_loopback() || self.is_our_address(dst) {
            sink.deliver(&src, &dst, src_port, dst_port, payload);
            return true;
        }

        // Risoluzione del MAC di prossimo hop.
        let dst_mac = if dst.is_link_local() {
            match self.neighbor_lookup(dst) {
                Some(m) => m,
                None => return false, // nessun vicino noto
            }
        } else {
            // Off-link: via router.
            match (self.router, self.router_mac) {
                (Some(_), Some(m)) => m,
                _ => return false, // nessun router
            }
        };

        // Costruisce il frame Ethernet + IPv6 + UDP.
        let mut frame = [0u8; 1514];
        let mut off = eth::write(&mut frame, dst_mac, self.mac, ETHERTYPE_IPV6);
        let mut udp_buf = [0u8; 1500 - ipv6::IPV6_HDR - eth::ETH_HDR];
        let udp_len = udp::write_checked(
            &mut udp_buf,
            src_port,
            dst_port,
            &src,
            &dst,
            payload,
        );
        off += ipv6::write(
            &mut frame[off..],
            NEXT_UDP,
            HOP_LIMIT,
            src,
            dst,
            udp_len as u16,
        );
        frame[off..off + udp_len].copy_from_slice(&udp_buf[..udp_len]);
        sink.send(&frame[..off + udp_len]);
        true
    }

    /// Invia un segmento TCP a `dst` con i flag e le sequenze specificate.
    /// Stessa logica di source-selection e risoluzione next-hop di `send_udp`.
    /// Ritorna `true` se il segmento è stato accettato (inviato o consegnato localmente).
    pub fn send_tcp(
        &mut self,
        dst: Ipv6Addr,
        src_port: u16,
        dst_port: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        window: u16,
        payload: &[u8],
        sink: &mut dyn FrameSink,
    ) -> bool {
        let total = tcp::TCP_HDR + payload.len();
        if total > 1500 - ipv6::IPV6_HDR - eth::ETH_HDR {
            return false; // troppo grande per un frame Ethernet
        }

        // Indirizzo sorgente (stessa logica di send_udp).
        let src = if dst.is_loopback() || dst == self.link_local {
            self.link_local
        } else if dst.is_link_local() {
            self.link_local
        } else if let Some(g) = self.global {
            g
        } else {
            self.link_local
        };

        // Consegna locale per indirizzi propri/loopback: nessun frame Ethernet.
        if dst.is_loopback() || self.is_our_address(dst) {
            // Serializza il segmento TCP nello slot di delivery.
            let mut tcp_buf = [0u8; 1500 - ipv6::IPV6_HDR - eth::ETH_HDR];
            let tcp_len = tcp::write_checked(
                &mut tcp_buf,
                src_port,
                dst_port,
                seq,
                ack,
                flags,
                window,
                &src,
                &dst,
                payload,
            );
            sink.deliver(&src, &dst, src_port, dst_port, &tcp_buf[..tcp_len]);
            return true;
        }

        // Risoluzione del MAC di prossimo hop (stessa logica di send_udp).
        let dst_mac = if dst.is_link_local() {
            match self.neighbor_lookup(dst) {
                Some(m) => m,
                None => return false, // nessun vicino noto
            }
        } else {
            // Off-link: via router.
            match (self.router, self.router_mac) {
                (Some(_), Some(m)) => m,
                _ => return false, // nessun router
            }
        };

        // Costruisce il frame Ethernet + IPv6 + TCP.
        let mut frame = [0u8; 1514];
        let mut off = eth::write(&mut frame, dst_mac, self.mac, ETHERTYPE_IPV6);
        let mut tcp_buf = [0u8; 1500 - ipv6::IPV6_HDR - eth::ETH_HDR];
        let tcp_len = tcp::write_checked(
            &mut tcp_buf,
            src_port,
            dst_port,
            seq,
            ack,
            flags,
            window,
            &src,
            &dst,
            payload,
        );
        off += ipv6::write(
            &mut frame[off..],
            NEXT_TCP,
            HOP_LIMIT,
            src,
            dst,
            tcp_len as u16,
        );
        frame[off..off + tcp_len].copy_from_slice(&tcp_buf[..tcp_len]);
        sink.send(&frame[..off + tcp_len]);
        true
    }

    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Link up: invia subito un Router Solicitation.
    pub fn link_up(&mut self, now: u64, sink: &mut dyn FrameSink, events: &mut dyn FnMut(Event)) {
        if self.link_up {
            return;
        }
        self.link_up = true;
        events(Event::LinkUp);
        self.send_rs(now, sink);
    }

    /// Timer periodico (1 tick = 10 ms): RS/echo retry, aging cache.
    pub fn on_tick(&mut self, now: u64, sink: &mut dyn FrameSink, events: &mut dyn FnMut(Event)) {
        if !self.link_up {
            return;
        }
        if self.router.is_none()
            && self.rs_sent < MAX_TRIES
            && now.saturating_sub(self.last_rs) >= RS_INTERVAL
        {
            self.send_rs(now, sink);
        }
        if self.router.is_some()
            && self.cfg.ping
            && !self.echo_done
            && self.echo_sent < MAX_TRIES
            && now.saturating_sub(self.last_echo) >= ECHO_INTERVAL
        {
            self.send_echo(now, sink);
        }
        if self.echo_sent >= MAX_TRIES && !self.echo_done && self.pending_echo.is_some() {
            self.pending_echo = None;
            self.echo_done = true;
            events(Event::EchoTimeout);
        }
        // Aging (RFC 4861 §7.3.3 semplificato): oltre il TTL la entry diventa
        // STALE, non muore. Il MAC resta usabile (rifiutare l'invio blocca il
        // traffico per sempre se il peer non parla più: un RA non arriva a
        // comando), ma si verifica con NS; senza risposta dopo
        // `NEIGHBOR_MAX_PROBES` la entry si scarta.
        let mut to_probe = [Ipv6Addr::UNSPECIFIED; NEIGHBORS];
        let mut n_probe = 0usize;
        for nb in self.neighbors.iter_mut() {
            if !nb.valid || now.saturating_sub(nb.last) <= NEIGHBOR_TTL {
                continue;
            }
            if now.saturating_sub(nb.probed) < NEIGHBOR_PROBE {
                continue;
            }
            if nb.probes >= NEIGHBOR_MAX_PROBES {
                nb.valid = false;
                continue;
            }
            nb.probes += 1;
            nb.probed = now;
            to_probe[n_probe] = nb.addr;
            n_probe += 1;
        }
        for addr in &to_probe[..n_probe] {
            self.send_ns(*addr, sink);
        }
    }

    /// Neighbor Solicitation verso il solicited-node multicast di `target`.
    fn send_ns(&mut self, target: Ipv6Addr, sink: &mut dyn FrameSink) {
        let dst = target.solicited_node_multicast();
        let dst_mac = MacAddr::multicast_for(&dst);
        let mut frame = [0u8; 128];
        let mut off = eth::write(&mut frame, dst_mac, self.mac, ETHERTYPE_IPV6);
        let mut icmp = [0u8; 64];
        let len = ndp::write_ns(&mut icmp, target, self.mac, &self.link_local, &dst);
        off += ipv6::write(
            &mut frame[off..],
            NEXT_ICMPV6,
            255,
            self.link_local,
            dst,
            len as u16,
        );
        frame[off..off + len].copy_from_slice(&icmp[..len]);
        sink.send(&frame[..off + len]);
    }

    /// Frame ricevuto dal driver.
    pub fn on_frame(
        &mut self,
        frame: &[u8],
        now: u64,
        sink: &mut dyn FrameSink,
        events: &mut dyn FnMut(Event),
    ) {
        let Some((eth, payload)) = eth::parse(frame) else {
            return;
        };
        if eth.ethertype != ETHERTYPE_IPV6 {
            return;
        }
        let Some((ip, payload)) = ipv6::parse(payload) else {
            return;
        };
        match ip.next_header {
            NEXT_ICMPV6 => {
                if !icmpv6::checksum_ok(&ip.src, &ip.dst, payload) {
                    events(Event::Dropped("icmpv6 checksum"));
                    return;
                }
                let Some(msg) = icmpv6::parse(payload) else {
                    return;
                };
                match msg.typ {
                    TYPE_RA => {
                        if ip.hop_limit != 255 {
                            events(Event::Dropped("ra hop"));
                            return;
                        }
                        self.on_ra(&eth, &ip, msg.body, now, sink, events);
                    }
                    TYPE_NS => {
                        if ip.hop_limit != 255 {
                            events(Event::Dropped("ns hop"));
                            return;
                        }
                        self.on_ns(&eth, &ip, msg.body, now, sink);
                    }
                    TYPE_NA => {
                        if ip.hop_limit != 255 {
                            events(Event::Dropped("na hop"));
                            return;
                        }
                        self.on_na(&eth, &ip, msg.body, now);
                    }
                    TYPE_ECHO_REQUEST => self.on_echo_request(&eth, &ip, msg.body, sink),
                    TYPE_ECHO_REPLY => self.on_echo_reply(&ip, msg.body, events),
                    TYPE_REDIRECT => events(Event::Dropped("redirect")),
                    _ => {}
                }
            }
            NEXT_UDP => {
                if ip.dst.is_loopback() || self.is_our_address(ip.dst) {
                    if let Some((udp, body)) = udp::parse(payload) {
                        if udp::checksum_ok(&ip.src, &ip.dst, payload) {
                            sink.deliver(&ip.src, &ip.dst, udp.src_port, udp.dst_port, body);
                        } else {
                            events(Event::Dropped("udp checksum"));
                        }
                    }
                }
            }
            NEXT_TCP => {
                if ip.dst.is_loopback() || self.is_our_address(ip.dst) {
                    self.tcp_input(&ip.src, &ip.dst, payload, now, sink, events, 0);
                }
            }
            _ => {}
        }
    }

    fn on_ra(
        &mut self,
        eth: &eth::EthHeader,
        ip: &ipv6::Ipv6Header,
        body: &[u8],
        now: u64,
        sink: &mut dyn FrameSink,
        events: &mut dyn FnMut(Event),
    ) {
        if !self.cfg.ra_accept {
            events(Event::Dropped("ra disabilitato"));
            return;
        }
        if !ip.src.is_link_local() {
            events(Event::Dropped("ra src non link-local"));
            return;
        }
        if let Some(allowed) = self.cfg.ra_router {
            if allowed != ip.src {
                events(Event::Dropped("ra guard"));
                return;
            }
        }
        let Some(ra) = ndp::parse_ra(body) else {
            return;
        };
        if ra.router_lifetime == 0 {
            return;
        }
        self.router = Some(ip.src);
        self.router_mac = ra.src_ll.or(Some(eth.src));
        if let Some((p, plen)) = ra.prefix {
            if plen == 64 {
                self.prefix = Some(p);
                let g = Ipv6Addr::from_prefix_iid(&p, self.link_local.iid());
                if self.global != Some(g) {
                    self.global = Some(g);
                    events(Event::Address(g));
                }
            }
        }
        if let Some(m) = self.router_mac {
            self.neighbor_add(ip.src, m, now);
        }
        events(Event::Ra {
            router: ip.src,
            prefix: self.prefix,
        });
        if self.cfg.ping && !self.echo_done && self.echo_sent == 0 {
            self.send_echo(now, sink);
        }
    }

    fn on_ns(
        &mut self,
        _eth: &eth::EthHeader,
        ip: &ipv6::Ipv6Header,
        body: &[u8],
        now: u64,
        sink: &mut dyn FrameSink,
    ) {
        let Some((target, src_ll)) = ndp::parse_ns(body) else {
            return;
        };
        if let Some(m) = src_ll {
            self.neighbor_add(ip.src, m, now);
        }
        let ours = target == self.link_local || self.global == Some(target);
        if !ours {
            return;
        }
        let dst_mac = src_ll
            .unwrap_or_else(|| MacAddr::multicast_for(&target.solicited_node_multicast()));
        let mut frame = [0u8; 128];
        let mut off = eth::write(&mut frame, dst_mac, self.mac, ETHERTYPE_IPV6);
        let mut icmp = [0u8; 64];
        let len = ndp::write_na(&mut icmp, target, 0x60, self.mac, &target, &ip.src);
        off += ipv6::write(
            &mut frame[off..],
            NEXT_ICMPV6,
            255,
            target,
            ip.src,
            len as u16,
        );
        frame[off..off + len].copy_from_slice(&icmp[..len]);
        sink.send(&frame[..off + len]);
    }

    /// Neighbor Advertisement: conferma la raggiungibilità di `target`.
    /// Senza l'opzione target-link-layer (RFC 4861 §4.4 la rende
    /// obbligatoria nelle NA sollecitate, ma non tutti la mettono: passt no)
    /// si usa il MAC sorgente del frame Ethernet, che per una NA sollecitata
    /// è il link-layer del target.
    fn on_na(
        &mut self,
        eth: &eth::EthHeader,
        _ip: &ipv6::Ipv6Header,
        body: &[u8],
        now: u64,
    ) {
        let Some((target, _flags, ll)) = ndp::parse_na(body) else {
            return;
        };
        self.neighbor_add(target, ll.unwrap_or(eth.src), now);
    }

    fn on_echo_request(
        &mut self,
        eth: &eth::EthHeader,
        ip: &ipv6::Ipv6Header,
        body: &[u8],
        sink: &mut dyn FrameSink,
    ) {
        if !self.cfg.ping || body.len() < 4 {
            return;
        }
        let id = u16::from_be_bytes([body[0], body[1]]);
        let seq = u16::from_be_bytes([body[2], body[3]]);
        let data = &body[4..];
        let mut frame = [0u8; 1600];
        let mut off = eth::write(&mut frame, eth.src, self.mac, ETHERTYPE_IPV6);
        let mut icmp = [0u8; 1500];
        let len = icmpv6::write_echo(
            &mut icmp,
            TYPE_ECHO_REPLY,
            id,
            seq,
            data,
            &ip.dst,
            &ip.src,
        );
        off += ipv6::write(
            &mut frame[off..],
            NEXT_ICMPV6,
            HOP_LIMIT,
            ip.dst,
            ip.src,
            len as u16,
        );
        frame[off..off + len].copy_from_slice(&icmp[..len]);
        sink.send(&frame[..off + len]);
    }

    fn on_echo_reply(
        &mut self,
        ip: &ipv6::Ipv6Header,
        body: &[u8],
        events: &mut dyn FnMut(Event),
    ) {
        if body.len() < 4 {
            return;
        }
        let id = u16::from_be_bytes([body[0], body[1]]);
        let seq = u16::from_be_bytes([body[2], body[3]]);
        if self.pending_echo == Some((id, seq)) {
            self.pending_echo = None;
            self.echo_done = true;
            events(Event::EchoReply {
                from: ip.src,
                seq,
            });
        }
    }

    fn send_rs(&mut self, now: u64, sink: &mut dyn FrameSink) {
        self.rs_sent = self.rs_sent.saturating_add(1);
        self.last_rs = now;
        let dst = Ipv6Addr::ALL_ROUTERS;
        let dst_mac = MacAddr::multicast_for(&dst);
        let mut frame = [0u8; 128];
        let mut off = eth::write(&mut frame, dst_mac, self.mac, ETHERTYPE_IPV6);
        let mut icmp = [0u8; 64];
        let len = ndp::write_rs(&mut icmp, self.mac, &self.link_local, &dst);
        off += ipv6::write(
            &mut frame[off..],
            NEXT_ICMPV6,
            255,
            self.link_local,
            dst,
            len as u16,
        );
        frame[off..off + len].copy_from_slice(&icmp[..len]);
        sink.send(&frame[..off + len]);
    }

    fn send_echo(&mut self, now: u64, sink: &mut dyn FrameSink) {
        let Some(target) = self.cfg.ping_target.or(self.router) else {
            return;
        };
        let Some(router_mac) = self.router_mac else {
            return;
        };
        let src = if target.is_link_local() {
            self.link_local
        } else {
            self.global.unwrap_or(self.link_local)
        };
        self.echo_sent = self.echo_sent.saturating_add(1);
        self.last_echo = now;
        let seq = self.echo_sent as u16;
        let mut frame = [0u8; 1600];
        let mut off = eth::write(&mut frame, router_mac, self.mac, ETHERTYPE_IPV6);
        let mut icmp = [0u8; 1500];
        let len = icmpv6::write_echo(
            &mut icmp,
            TYPE_ECHO_REQUEST,
            ECHO_ID,
            seq,
            ECHO_DATA,
            &src,
            &target,
        );
        off += ipv6::write(
            &mut frame[off..],
            NEXT_ICMPV6,
            HOP_LIMIT,
            src,
            target,
            len as u16,
        );
        frame[off..off + len].copy_from_slice(&icmp[..len]);
        self.pending_echo = Some((ECHO_ID, seq));
        self.echo_target = Some(target);
        sink.send(&frame[..off + len]);
    }

    // ── TCP (N6.1b/c): tabella connessioni, input, output ───────────
    //
    // Le connessioni si identificano con un **id** (indice di slot). La
    // chiave non può essere la porta locale: un listener e i suoi figli
    // condividono la porta (il client ha chiamato *quella*), e distinguerli
    // per porta renderebbe impossibile servire due client in parallelo.

    /// Indirizzo sorgente per uscire verso `dst` (stessa regola di UDP/TCP).
    /// Il loopback usa `::1`: è l'indirizzo a cui il peer ha indirizzato i
    /// suoi segmenti, e quindi quello che si aspetta di vedere come mittente.
    fn src_for(&self, dst: Ipv6Addr) -> Ipv6Addr {
        if dst.is_loopback() {
            Ipv6Addr::LOOPBACK
        } else if dst == self.link_local || dst.is_link_local() {
            self.link_local
        } else if let Some(g) = self.global {
            g
        } else {
            self.link_local
        }
    }

    fn tcp_free_slot(&self) -> Option<usize> {
        self.tcp.iter().position(|c| c.is_none())
    }

    /// Connessione (non in ascolto) su `local_port` con questo peer.
    fn tcp_match(&self, local_port: u16, remote: &Ipv6Addr, remote_port: u16) -> Option<usize> {
        self.tcp.iter().position(|c| {
            c.as_ref().is_some_and(|c| {
                c.local_port == local_port
                    && c.state() != tcp::State::Listen
                    && c.remote_port == remote_port
                    && c.remote_addr == *remote
            })
        })
    }

    /// Listener su `local_port`.
    fn tcp_listener(&self, local_port: u16) -> Option<usize> {
        self.tcp.iter().position(|c| {
            c.as_ref()
                .is_some_and(|c| c.local_port == local_port && c.state() == tcp::State::Listen)
        })
    }

    /// Apre una connessione attiva verso `dst:dst_port`. Ritorna l'id.
    pub fn tcp_connect(&mut self, dst: Ipv6Addr, dst_port: u16, local_port: u16) -> Option<u8> {
        if local_port == 0 || self.tcp_match(local_port, &dst, dst_port).is_some() {
            return None;
        }
        let slot = self.tcp_free_slot()?;
        let iss = self.tcp_next_seq();
        let src = self.src_for(dst);
        let mut c = tcp::Conn::new_client(local_port);
        c.set_local_addr(src);
        c.open(dst, dst_port, iss, 0);
        self.tcp[slot] = Some(c);
        Some(slot as u8)
    }

    /// Apre un listener su `local_port`. Ritorna l'id.
    pub fn tcp_listen(&mut self, local_port: u16) -> Option<u8> {
        if local_port == 0 || self.tcp_listener(local_port).is_some() {
            return None;
        }
        let slot = self.tcp_free_slot()?;
        let mut c = tcp::Conn::new_listener(local_port);
        c.set_local_addr(self.link_local);
        self.tcp[slot] = Some(c);
        Some(slot as u8)
    }

    fn tcp_conn(&self, id: u8) -> Option<&tcp::Conn> {
        self.tcp.get(id as usize).and_then(|c| c.as_ref())
    }

    /// Stato della connessione `id`.
    pub fn tcp_state(&self, id: u8) -> Option<tcp::State> {
        self.tcp_conn(id).map(|c| c.state())
    }

    /// Byte leggibili sulla connessione `id`.
    pub fn tcp_ready(&self, id: u8) -> usize {
        self.tcp_conn(id).map(|c| c.readable()).unwrap_or(0)
    }

    /// Bufferizza `data` per l'invio. Ritorna i byte accettati.
    pub fn tcp_write(&mut self, id: u8, data: &[u8], now: u64) -> usize {
        match self.tcp.get_mut(id as usize).and_then(|c| c.as_mut()) {
            Some(c) => c.write(data, now),
            None => 0,
        }
    }

    /// Legge i byte ricevuti in ordine.
    pub fn tcp_read(&mut self, id: u8, out: &mut [u8]) -> usize {
        match self.tcp.get_mut(id as usize).and_then(|c| c.as_mut()) {
            Some(c) => c.read(out),
            None => 0,
        }
    }

    /// Chiusura ordinata (FIN).
    pub fn tcp_close(&mut self, id: u8) {
        if let Some(c) = self.tcp.get_mut(id as usize).and_then(|c| c.as_mut()) {
            c.close();
        }
    }

    /// Abort: RST e slot libero.
    pub fn tcp_abort(&mut self, id: u8) {
        if let Some(c) = self.tcp.get_mut(id as usize).and_then(|c| c.as_mut()) {
            c.abort();
        }
        if let Some(slot) = self.tcp.get_mut(id as usize) {
            *slot = None;
        }
    }

    /// Emette un segmento TCP (già serializzato) verso `dst`.
    fn emit_tcp(&mut self, src: Ipv6Addr, dst: Ipv6Addr, seg: &[u8], sink: &mut dyn FrameSink) -> bool {
        let dst_mac = if dst.is_link_local() {
            match self.neighbor_lookup(dst) {
                Some(m) => m,
                None => return false,
            }
        } else {
            match (self.router, self.router_mac) {
                (Some(_), Some(m)) => m,
                _ => return false,
            }
        };
        let mut frame = [0u8; 1514];
        let mut off = eth::write(&mut frame, dst_mac, self.mac, ETHERTYPE_IPV6);
        off += ipv6::write(&mut frame[off..], NEXT_TCP, HOP_LIMIT, src, dst, seg.len() as u16);
        if off + seg.len() > frame.len() {
            return false;
        }
        frame[off..off + seg.len()].copy_from_slice(seg);
        sink.send(&frame[..off + seg.len()]);
        true
    }

    /// Serializza/emette un segmento TCP: se la destinazione è nostra lo
    /// rientra in `tcp_input` (loopback, nessun frame), altrimenti emette il
    /// frame Ethernet. `depth` limita la ricorsione del loopback.
    fn tcp_send(
        &mut self,
        src: Ipv6Addr,
        dst: Ipv6Addr,
        seg: &[u8],
        now: u64,
        sink: &mut dyn FrameSink,
        events: &mut dyn FnMut(Event),
        depth: u8,
    ) {
        if dst.is_loopback() || self.is_our_address(dst) {
            if depth < 3 {
                self.tcp_input(&src, &dst, seg, now, sink, events, depth + 1);
            }
            return;
        }
        self.emit_tcp(src, dst, seg, sink);
    }

    /// Segmento in ingresso (NEXT_TCP): checksum, connessione per (porta,
    /// peer) o listener, `Conn::on_segment`, risposta ed eventi.
    fn tcp_input(
        &mut self,
        src: &Ipv6Addr,
        dst: &Ipv6Addr,
        payload: &[u8],
        now: u64,
        sink: &mut dyn FrameSink,
        events: &mut dyn FnMut(Event),
        depth: u8,
    ) {
        let Some((hdr, body)) = tcp::parse(payload) else {
            return;
        };
        if !tcp::checksum_ok(src, dst, payload) {
            events(Event::Dropped("tcp checksum"));
            return;
        }

        // Connessione esistente, oppure listener che accetta una SYN.
        let slot = match self.tcp_match(hdr.dst_port, src, hdr.src_port) {
            Some(i) => Some(i),
            None => {
                let Some(l) = self.tcp_listener(hdr.dst_port) else {
                    // Nessuno in ascolto: RST (mai in risposta a un RST).
                    if hdr.flags & tcp::FLAG_RST == 0 {
                        let mut seg = [0u8; 20];
                        let len = tcp::write_checked(
                            &mut seg,
                            hdr.dst_port,
                            hdr.src_port,
                            0,
                            hdr.seq.wrapping_add(1),
                            tcp::FLAG_RST | tcp::FLAG_ACK,
                            0,
                            dst,
                            src,
                            &[],
                        );
                        let seg_copy = seg[..len].to_vec();
                        self.tcp_send(*dst, *src, &seg_copy, now, sink, events, depth);
                    }
                    return;
                };
                if hdr.flags & tcp::FLAG_SYN == 0 {
                    return; // un listener risponde solo alle SYN
                }
                // Backlog: al massimo `TCP_BACKLOG` figli per listener.
                let children = self.tcp.iter().flatten().filter(|c| {
                    c.local_port == hdr.dst_port && c.state() != tcp::State::Listen
                }).count();
                if children >= TCP_BACKLOG {
                    if hdr.flags & tcp::FLAG_RST == 0 {
                        let mut seg = [0u8; 20];
                        let len = tcp::write_checked(
                            &mut seg,
                            hdr.dst_port,
                            hdr.src_port,
                            0,
                            hdr.seq.wrapping_add(1),
                            tcp::FLAG_RST | tcp::FLAG_ACK,
                            0,
                            dst,
                            src,
                            &[],
                        );
                        let seg_copy = seg[..len].to_vec();
                        self.tcp_send(*dst, *src, &seg_copy, now, sink, events, depth);
                    }
                    return;
                }
                let Some(free) = self.tcp_free_slot() else {
                    return;
                };
                let iss = self.tcp_next_seq();
                let mut child = tcp::Conn::new_client(hdr.dst_port);
                child.adopt(*dst, *src, hdr.src_port);
                child.open_passive(*src, hdr.src_port, iss, hdr.seq, now);
                self.tcp[free] = Some(child);
                // Il figlio, entrando in SYN_RECV, produce SYN+ACK da solo.
                sink.tcp_signal(free as u8, TcpSignal::Accepted(hdr.dst_port));
                Some(free)
            }
        };
        let Some(i) = slot else { return };

        let Some(mut conn) = self.tcp[i].take() else {
            return;
        };
        conn.set_local_addr(*dst);
        let out = conn.on_segment(&hdr, body, now);
        let id = i as u8;
        let closed = conn.closed;
        if closed {
            self.tcp[i] = None;
        } else {
            self.tcp[i] = Some(conn);
        }
        if let Some(tx) = out.tx {
            let mut seg = [0u8; 20];
            let len = tcp::write(
                &mut seg,
                tx.src_port,
                tx.dst_port,
                tx.seq,
                tx.ack,
                tx.flags,
                tx.window,
                0,
            );
            let c = crate::tcp::codec::checksum_tcp(dst, src, &seg[..len]);
            seg[16..18].copy_from_slice(&c.to_be_bytes());
            let seg_copy = seg[..len].to_vec();
            self.tcp_send(*dst, *src, &seg_copy, now, sink, events, depth);
        }
        if out.established {
            sink.tcp_signal(id, TcpSignal::Established);
        }
        if out.data.is_some() {
            sink.tcp_signal(id, TcpSignal::Data);
        }
        if out.fin_recv {
            sink.tcp_signal(id, TcpSignal::Fin);
        }
        if out.rst {
            sink.tcp_signal(id, TcpSignal::Reset);
        }
    }

    /// Drena la coda di trasmissione di tutte le connessioni: SYN, dati,
    /// ritrasmissioni e FIN. Va chiamato a ogni tick e dopo una `tcp_write`.
    pub fn tcp_poll(&mut self, now: u64, sink: &mut dyn FrameSink, events: &mut dyn FnMut(Event)) {
        for i in 0..TCP_CONNS {
            let Some(mut conn) = self.tcp[i].take() else {
                continue;
            };
            let id = i as u8;
            if conn.closed {
                sink.tcp_signal(id, TcpSignal::Reset);
                continue;
            }
            let dst = conn.remote_addr;
            if conn.state() == tcp::State::Listen {
                // Un listener non trasmette: resta in tabella.
                self.tcp[i] = Some(conn);
                continue;
            }
            // L'indirizzo locale dipende dal next-hop e serve al checksum:
            // va impostato prima di serializzare.
            let src = self.src_for(dst);
            conn.set_local_addr(src);
            let mut buf = core::mem::take(&mut self.tcp_tx);
            let seg = conn.next_tx(&mut buf, now);
            let closed = conn.closed;
            if !closed {
                self.tcp[i] = Some(conn);
            }
            self.tcp_tx = buf;
            if let Some(info) = seg {
                let seg_copy = self.tcp_tx[..info.len].to_vec();

                self.tcp_send(src, dst, &seg_copy, now, sink, events, 0);
            }
            if closed {
                sink.tcp_signal(id, TcpSignal::Reset);
            }
        }
    }

    fn neighbor_add(&mut self, addr: Ipv6Addr, mac: MacAddr, now: u64) {
        for nb in self.neighbors.iter_mut() {
            if nb.valid && nb.addr == addr {
                nb.mac = mac;
                nb.last = now;
                nb.probes = 0;
                nb.probed = now;
                return;
            }
        }
        for nb in self.neighbors.iter_mut() {
            if !nb.valid {
                *nb = Neighbor {
                    addr,
                    mac,
                    valid: true,
                    last: now,
                    probes: 0,
                    probed: now,
                };
                return;
            }
        }
        let mut oldest = 0usize;
        for i in 1..NEIGHBORS {
            if self.neighbors[i].last < self.neighbors[oldest].last {
                oldest = i;
            }
        }
        self.neighbors[oldest] = Neighbor {
            addr,
            mac,
            valid: true,
            last: now,
            probes: 0,
            probed: now,
        };
    }

    pub fn neighbor_lookup(&self, addr: Ipv6Addr) -> Option<MacAddr> {
        self.neighbors
            .iter()
            .find(|nb| nb.valid && nb.addr == addr)
            .map(|nb| nb.mac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    struct VecSink {
        frames: Vec<Vec<u8>>,
        signals: Vec<(u8, TcpSignal)>,
        loops: Vec<(u16, u16, Vec<u8>)>,
        deliver_tcp: bool,
    }
    impl FrameSink for VecSink {
        fn send(&mut self, frame: &[u8]) {
            self.frames.push(frame.to_vec());
        }
        fn tcp_signal(&mut self, id: u8, signal: TcpSignal) {
            self.signals.push((id, signal));
        }
        fn deliver(&mut self, src: &Ipv6Addr, dst: &Ipv6Addr, sp: u16, dp: u16, payload: &[u8]) {
            if self.deliver_tcp {
                let mut seg = alloc::vec![0u8; tcp::TCP_HDR + payload.len()];
                let n = tcp::write_checked(
                    &mut seg, sp, dp, 0, 0, tcp::FLAG_ACK, 0, src, dst, payload,
                );
                self.loops.push((sp, dp, seg[..n].to_vec()));
            }
        }
    }

    fn cfg() -> Config {
        Config {
            mac: MacAddr([0x02, 0, 0, 0, 0, 0x01]),
            ra_accept: true,
            ra_router: None,
            ping: true,
            ping_target: None,
            seed: 0x1234_5678_9abc_def0,
        }
    }

    fn wrap_ipv6(eth_dst: MacAddr, eth_src: MacAddr, src: Ipv6Addr, dst: Ipv6Addr, icmp: &[u8]) -> Vec<u8> {
        let mut f = alloc::vec![0u8; 14 + 40 + icmp.len()];
        let off = eth::write(&mut f, eth_dst, eth_src, ETHERTYPE_IPV6);
        let off2 = ipv6::write(&mut f[off..], NEXT_ICMPV6, 255, src, dst, icmp.len() as u16);
        f[off + off2..].copy_from_slice(icmp);
        f
    }

    fn ra_frame(router: Ipv6Addr, router_mac: MacAddr, prefix: [u8; 8]) -> Vec<u8> {
        // ICMPv6 header (4) + RA fisso (12) + source LL (8) + prefix info (32).
        let mut icmp = alloc::vec![0u8; 4 + 12 + 8 + 32];
        icmp[0] = TYPE_RA;
        icmp[1] = 0;
        icmp[4] = 64; // cur hop limit
        icmp[5] = 0;
        icmp[6..8].copy_from_slice(&1800u16.to_be_bytes()); // router lifetime
        // source link-layer (le opzioni iniziano dopo i 12 byte fissi: 4+12=16)
        icmp[16] = ndp::OPT_SOURCE_LL;
        icmp[17] = 1;
        icmp[18..24].copy_from_slice(&router_mac.0);
        // prefix info
        let p = 24;
        icmp[p] = ndp::OPT_PREFIX_INFO;
        icmp[p + 1] = 4;
        icmp[p + 2] = 64; // prefix length
        icmp[p + 3] = 0xc0; // on-link + autonomous
        icmp[p + 4..p + 8].copy_from_slice(&3600u32.to_be_bytes());
        icmp[p + 8..p + 12].copy_from_slice(&1800u32.to_be_bytes());
        icmp[p + 16..p + 24].copy_from_slice(&prefix);
        let c = crate::checksum::icmpv6(&router, &Ipv6Addr::ALL_NODES, &icmp);
        icmp[2..4].copy_from_slice(&c.to_be_bytes());
        wrap_ipv6(
            MacAddr::multicast_for(&Ipv6Addr::ALL_NODES),
            router_mac,
            router,
            Ipv6Addr::ALL_NODES,
            &icmp,
        )
    }

    #[test]
    fn ra_slaac_e_router() {
        let mut n = Net::new(cfg());
        let mut sink = VecSink { frames: Vec::new(), signals: Vec::new(), loops: Vec::new(), deliver_tcp: false };
        let mut evs = Vec::new();
        n.link_up(0, &mut sink, &mut |e| evs.push(e));
        assert_eq!(sink.frames.len(), 1, "RS inviato al link up");
        let router = Ipv6Addr::from_link_local_iid(0x0200_0000_0000_0002);
        let rmac = MacAddr([0x02, 0, 0, 0, 0, 0x02]);
        let prefix = [0x2a, 0x01, 0x0e, 0x11, 0x24, 0x30, 0x7a, 0xf0];
        let f = ra_frame(router, rmac, prefix);
        n.on_frame(&f, 10, &mut sink, &mut |e| evs.push(e));
        assert_eq!(n.router(), Some(router));
        let g = n.global().expect("indirizzo globale da SLAAC");
        assert_eq!(&g.0[..8], &prefix);
        assert!(evs.iter().any(|e| matches!(e, Event::Ra { .. })));
        assert!(evs.iter().any(|e| matches!(e, Event::Address(_))));
        // L'echo verso il router e' partito (ping=true).
        assert!(sink.frames.len() >= 2);
    }

    #[test]
    fn ra_guard_rifiuta_router_non_autorizzato() {
        let mut c = cfg();
        c.ra_router = Some(Ipv6Addr::from_link_local_iid(0x0200_0000_0000_0009));
        let mut n = Net::new(c);
        let mut sink = VecSink { frames: Vec::new(), signals: Vec::new(), loops: Vec::new(), deliver_tcp: false };
        let mut evs = Vec::new();
        n.link_up(0, &mut sink, &mut |e| evs.push(e));
        let router = Ipv6Addr::from_link_local_iid(0x0200_0000_0000_0002);
        let f = ra_frame(router, MacAddr([0x02, 0, 0, 0, 0, 0x02]), [0x2a; 8]);
        n.on_frame(&f, 10, &mut sink, &mut |e| evs.push(e));
        assert_eq!(n.router(), None);
        assert!(evs.iter().any(|e| matches!(e, Event::Dropped("ra guard"))));
    }

    #[test]
    fn echo_reply_completa_il_ping() {
        let mut n = Net::new(cfg());
        let mut sink = VecSink { frames: Vec::new(), signals: Vec::new(), loops: Vec::new(), deliver_tcp: false };
        let mut evs = Vec::new();
        n.link_up(0, &mut sink, &mut |e| evs.push(e));
        let router = Ipv6Addr::from_link_local_iid(0x0200_0000_0000_0002);
        let rmac = MacAddr([0x02, 0, 0, 0, 0, 0x02]);
        let f = ra_frame(router, rmac, [0x2a; 8]);
        n.on_frame(&f, 10, &mut sink, &mut |e| evs.push(e));
        // Costruisce l'echo reply per l'ultimo echo request inviato.
        let req = sink.frames.last().unwrap().clone();
        let (_, ip_payload) = eth::parse(&req).unwrap();
        let (ip, icmp) = ipv6::parse(ip_payload).unwrap();
        let msg = icmpv6::parse(icmp).unwrap();
        let id = u16::from_be_bytes([msg.body[0], msg.body[1]]);
        let seq = u16::from_be_bytes([msg.body[2], msg.body[3]]);
        let mut reply = alloc::vec![0u8; 8 + 4];
        let len = icmpv6::write_echo(
            &mut reply,
            TYPE_ECHO_REPLY,
            id,
            seq,
            b"pong",
            &ip.dst,
            &ip.src,
        );
        let frame = wrap_ipv6(rmac, n.mac(), ip.dst, ip.src, &reply[..len]);
        n.on_frame(&frame, 20, &mut sink, &mut |e| evs.push(e));
        assert!(evs.iter().any(|e| matches!(e, Event::EchoReply { .. })));
    }

    #[test]
    fn ns_per_il_nostro_indirizzo_produce_na() {
        let mut n = Net::new(cfg());
        let mut sink = VecSink { frames: Vec::new(), signals: Vec::new(), loops: Vec::new(), deliver_tcp: false };
        let mut evs = Vec::new();
        n.link_up(0, &mut sink, &mut |e| evs.push(e));
        let ll = n.link_local();
        let peer = Ipv6Addr::from_link_local_iid(0x0200_0000_0000_0003);
        let pmac = MacAddr([0x02, 0, 0, 0, 0, 0x03]);
        let mut icmp = alloc::vec![0u8; 8 + 16 + 8];
        let len = ndp::write_ns(&mut icmp, ll, pmac, &peer, &ll.solicited_node_multicast());
        let frame = wrap_ipv6(
            MacAddr::multicast_for(&ll.solicited_node_multicast()),
            pmac,
            peer,
            ll.solicited_node_multicast(),
            &icmp[..len],
        );
        let before = sink.frames.len();
        n.on_frame(&frame, 10, &mut sink, &mut |e| evs.push(e));
        assert_eq!(sink.frames.len(), before + 1, "NA inviato");
        let na = sink.frames.last().unwrap();
        let (_, ip_payload) = eth::parse(na).unwrap();
        let (_, icmp) = ipv6::parse(ip_payload).unwrap();
        let msg = icmpv6::parse(icmp).unwrap();
        assert_eq!(msg.typ, TYPE_NA);
    }

    #[test]
    fn neighbor_stale_resta_usabile_e_si_verifica() {
        let mut c = cfg();
        c.ping = false;
        let mut n = Net::new(c);
        let mut sink = VecSink { frames: Vec::new(), signals: Vec::new(), loops: Vec::new(), deliver_tcp: false };
        let router = Ipv6Addr::parse("fe80::1").unwrap();
        let rmac = MacAddr([0x02, 0, 0, 0, 0, 0x02]);
        n.link_up(0, &mut sink, &mut |_| {});
        let ra = ra_frame(router, rmac, [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0]);
        n.on_frame(&ra, 0, &mut sink, &mut |_| {});
        assert_eq!(n.neighbor_lookup(router), Some(rmac));

        // Oltre il TTL la entry è STALE: il MAC resta usabile e parte una NS.
        sink.frames.clear();
        n.on_tick(NEIGHBOR_TTL + 1, &mut sink, &mut |_| {});
        assert_eq!(n.neighbor_lookup(router), Some(rmac));
        let mut has_ns = false;
        for f in &sink.frames {
            let (_, ip_payload) = eth::parse(f).unwrap();
            let (_, icmp) = ipv6::parse(ip_payload).unwrap();
            if icmpv6::parse(icmp).map(|m| m.typ) == Some(TYPE_NS) {
                has_ns = true;
            }
        }
        assert!(has_ns, "NS di verifica inviata");

        // La NA di risposta può non avere l'opzione target link-layer (passt
        // non la mette): il MAC si prende dall'Ethernet sorgente e la entry
        // torna fresca (nessuna nuova NS al tick successivo).
        let mut na = alloc::vec![0u8; 8 + 16];
        na[0] = TYPE_NA;
        na[4] = 0x60;
        na[8..24].copy_from_slice(&router.0);
        let c = crate::checksum::icmpv6(&router, &n.link_local(), &na);
        na[2..4].copy_from_slice(&c.to_be_bytes());
        let frame = wrap_ipv6(n.mac(), rmac, router, n.link_local(), &na);
        n.on_frame(&frame, NEIGHBOR_TTL + 2, &mut sink, &mut |_| {});
        assert_eq!(n.neighbor_lookup(router), Some(rmac));
        sink.frames.clear();
        n.on_tick(NEIGHBOR_TTL + 2 + NEIGHBOR_PROBE, &mut sink, &mut |_| {});
        assert!(sink.frames.is_empty(), "entry fresca: nessuna NS");

        // Nessuna risposta: dopo NEIGHBOR_MAX_PROBES la entry si scarta.
        let base = 2 * NEIGHBOR_TTL + 3;
        for k in 0..=NEIGHBOR_MAX_PROBES as u64 {
            n.on_tick(base + k * NEIGHBOR_PROBE, &mut sink, &mut |_| {});
        }
        assert_eq!(n.neighbor_lookup(router), None);
    }

    // ── TCP wiring (N6.1b/c) ────────────────────────────────────────

    /// Net con link up e nessun ping: serve solo il TCP.
    fn tcp_net() -> Net {
        let mut c = cfg();
        c.ping = false;
        let mut n = Net::new(c);
        let mut sink = new_sink();
        n.link_up(0, &mut sink, &mut |_| {});
        n
    }

    fn new_sink() -> VecSink {
        VecSink {
            frames: Vec::new(),
            signals: Vec::new(),
            loops: Vec::new(),
            deliver_tcp: false,
        }
    }

    /// Chiama `tcp_poll` più volte (loopback sincrono).
    fn poll_n(n: &mut Net, sink: &mut VecSink, now: u64, max: usize) {
        for _ in 0..max {
            n.tcp_poll(now, sink, &mut |_| {});
        }
    }

    #[test]
    fn tcp_loopback_handshake_e_dati() {
        let mut n = tcp_net();
        let mut sink = new_sink();
        let srv = n.tcp_listen(9000).expect("listener");
        poll_n(&mut n, &mut sink, 0, 2);
        let cli = n.tcp_connect(Ipv6Addr::LOOPBACK, 9000, 9001).expect("connect");

        poll_n(&mut n, &mut sink, 0, 6);

        assert_eq!(n.tcp_state(cli), Some(tcp::State::Established));
        assert_eq!(n.tcp_state(srv), Some(tcp::State::Listen), "il listener resta in ascolto");
        assert!(
            sink.frames.is_empty(),
            "loopback non deve emettere frame Ethernet"
        );

        // Il figlio accettato è una connessione distinta dall'id del listener.
        let child = sink
            .signals
            .iter()
            .find(|(_, s)| matches!(s, TcpSignal::Accepted(_)))
            .map(|(id, _)| *id)
            .expect("Accepted");
        assert_ne!(child, srv);
        assert_eq!(n.tcp_state(child), Some(tcp::State::Established));

        // Dati client -> figlio del server.
        assert_eq!(n.tcp_write(cli, b"ciao mondo", 0), 10);
        poll_n(&mut n, &mut sink, 0, 4);
        let mut buf = [0u8; 32];
        let got = n.tcp_read(child, &mut buf);
        assert_eq!(&buf[..got], b"ciao mondo");

        // Echo: il figlio risponde e il client legge.
        assert_eq!(n.tcp_write(child, b"echo!", 0), 5);
        poll_n(&mut n, &mut sink, 0, 4);
        let got = n.tcp_read(cli, &mut buf);
        assert_eq!(&buf[..got], b"echo!");

        assert!(sink
            .signals
            .iter()
            .any(|(id, s)| *id == cli && *s == TcpSignal::Established));
        assert!(sink
            .signals
            .iter()
            .any(|(id, s)| *id == child && *s == TcpSignal::Data));
    }

    #[test]
    fn tcp_scrittura_prima_dell_handshake() {
        // Un client che scrive subito dopo `connect` (come il probe net3):
        // i dati vanno bufferizzati e partono a handshake chiuso.
        let mut n = tcp_net();
        let mut sink = new_sink();
        let _srv = n.tcp_listen(9400).expect("listener");
        poll_n(&mut n, &mut sink, 0, 2);
        let cli = n.tcp_connect(Ipv6Addr::LOOPBACK, 9400, 9401).expect("connect");
        assert_eq!(n.tcp_write(cli, b"early", 0), 5);
        poll_n(&mut n, &mut sink, 0, 8);
        assert_eq!(n.tcp_state(cli), Some(tcp::State::Established));
        let child = sink
            .signals
            .iter()
            .find(|(_, s)| matches!(s, TcpSignal::Accepted(_)))
            .map(|(id, _)| *id)
            .expect("Accepted");
        let mut buf = [0u8; 16];
        let got = n.tcp_read(child, &mut buf);
        assert_eq!(&buf[..got], b"early", "i dati bufferizzati devono partire");
    }

    #[test]
    fn tcp_porta_chiusa_risponde_rst() {
        let mut n = tcp_net();
        let mut sink = new_sink();
        let cli = n.tcp_connect(Ipv6Addr::LOOPBACK, 9, 9002).expect("connect");
        poll_n(&mut n, &mut sink, 0, 4);
        assert!(
            sink.signals
                .iter()
                .any(|(id, s)| *id == cli && *s == TcpSignal::Reset),
            "il client deve ricevere un reset: {:?}",
            sink.signals
        );
        assert!(n.tcp_state(cli).is_none(), "slot liberato");
    }

    #[test]
    fn tcp_close_ordinato_loopback() {
        let mut n = tcp_net();
        let mut sink = new_sink();
        let srv = n.tcp_listen(9100).expect("listener");
        poll_n(&mut n, &mut sink, 0, 2);
        let cli = n.tcp_connect(Ipv6Addr::LOOPBACK, 9100, 9101).expect("connect");
        poll_n(&mut n, &mut sink, 0, 6);
        let child = sink
            .signals
            .iter()
            .find(|(_, s)| matches!(s, TcpSignal::Accepted(_)))
            .map(|(id, _)| *id)
            .expect("Accepted");
        assert_eq!(n.tcp_state(cli), Some(tcp::State::Established));
        assert_eq!(n.tcp_state(child), Some(tcp::State::Established));
        assert_eq!(n.tcp_state(srv), Some(tcp::State::Listen));

        // Chiusura del client: FIN -> il figlio va in CLOSE_WAIT e segnala Fin.
        n.tcp_close(cli);
        poll_n(&mut n, &mut sink, 0, 4);
        assert_eq!(n.tcp_state(child), Some(tcp::State::CloseWait));
        assert!(sink
            .signals
            .iter()
            .any(|(id, s)| *id == child && *s == TcpSignal::Fin));

        // Anche il server chiude: il client passa in TIME_WAIT.
        n.tcp_close(child);
        poll_n(&mut n, &mut sink, 0, 6);
        assert_eq!(n.tcp_state(cli), Some(tcp::State::TimeWait));
    }

    #[test]
    fn tcp_listener_backlog_bounded() {
        let mut n = tcp_net();
        let mut sink = new_sink();
        let srv = n.tcp_listen(9200).expect("listener");
        poll_n(&mut n, &mut sink, 0, 2);
        // Più client di quanti il backlog consente (TCP_BACKLOG).
        let mut ok = 0;
        for k in 0..(TCP_BACKLOG as u16 + 2) {
            let port = 9300 + k;
            let c = n.tcp_connect(Ipv6Addr::LOOPBACK, 9200, port).expect("connect");
            poll_n(&mut n, &mut sink, 0, 3);
            if n.tcp_state(c) == Some(tcp::State::Established) {
                ok += 1;
            }
        }
        assert_eq!(ok, TCP_BACKLOG, "il backlog limita i figli simultanei");
        assert_eq!(n.tcp_state(srv), Some(tcp::State::Listen));
    }

    #[test]
    fn tcp_ritrasmissione_su_link_reale() {
        // Percorso non-loopback: il segmento va al router (se c'è) e senza
        // risposta l'RTO ritrasmette.
        let mut c = cfg();
        c.ping = false;
        let mut n = Net::new(c);
        let mut sink = new_sink();
        n.link_up(0, &mut sink, &mut |_| {});
        let router = Ipv6Addr::parse("fe80::1").unwrap();
        let rmac = MacAddr([0x02, 0, 0, 0, 0, 0x02]);
        let ra = ra_frame(router, rmac, [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0]);
        n.on_frame(&ra, 0, &mut sink, &mut |_| {});

        let dst = Ipv6Addr::parse("2001:db8::42").unwrap();
        assert!(n.tcp_connect(dst, 853, 9102).is_some());
        sink.frames.clear();
        n.tcp_poll(0, &mut sink, &mut |_| {});
        assert_eq!(sink.frames.len(), 1, "SYN emesso verso il router");
        let (_, ip_payload) = eth::parse(&sink.frames[0]).unwrap();
        let (_, ip) = ipv6::parse(ip_payload).unwrap();
        let (hdr, _) = tcp::parse(ip).unwrap();
        assert_ne!(hdr.flags & tcp::FLAG_SYN, 0);

        // Nessuna risposta: dopo l'RTO rieccolo.
        sink.frames.clear();
        n.tcp_poll(101, &mut sink, &mut |_| {});
        assert_eq!(sink.frames.len(), 1, "SYN ritrasmesso");
    }
}
