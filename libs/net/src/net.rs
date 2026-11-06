//! State machine IPv6/ICMPv6/NDP (ADR-0069).
//!
//! Pura: riceve frame, emette frame tramite `FrameSink`, segnala eventi.
//! Niente I/O, niente allocazioni per pacchetto, nessun panic su input.

use crate::addr::{Ipv6Addr, MacAddr};
use crate::config::Config;
use crate::eth::{self, ETHERTYPE_IPV6};
use crate::icmpv6::{self, TYPE_ECHO_REPLY, TYPE_ECHO_REQUEST, TYPE_NA, TYPE_NS, TYPE_RA, TYPE_REDIRECT};
use crate::ipv6::{self, HOP_LIMIT, NEXT_ICMPV6};
use crate::ndp;

const NEIGHBORS: usize = 8;
/// 1 s a 100 Hz.
const RS_INTERVAL: u64 = 100;
const ECHO_INTERVAL: u64 = 100;
const MAX_TRIES: u8 = 3;
/// 30 s a 100 Hz.
const NEIGHBOR_TTL: u64 = 3000;
const ECHO_ID: u16 = 0x5645; // "VE"
const ECHO_DATA: &[u8] = b"velordo";

/// Destinazione dei frame da trasmettere.
pub trait FrameSink {
    fn send(&mut self, frame: &[u8]);
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
    last: u64,
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
        for nb in self.neighbors.iter_mut() {
            if nb.valid && now.saturating_sub(nb.last) > NEIGHBOR_TTL {
                nb.valid = false;
            }
        }
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
        let Some((ip, icmp)) = ipv6::parse(payload) else {
            return;
        };
        if ip.next_header != NEXT_ICMPV6 {
            return;
        }
        if !icmpv6::checksum_ok(&ip.src, &ip.dst, icmp) {
            events(Event::Dropped("icmpv6 checksum"));
            return;
        }
        let Some(msg) = icmpv6::parse(icmp) else {
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
                self.on_na(&ip, msg.body, now);
            }
            TYPE_ECHO_REQUEST => self.on_echo_request(&eth, &ip, msg.body, sink),
            TYPE_ECHO_REPLY => self.on_echo_reply(&ip, msg.body, events),
            TYPE_REDIRECT => events(Event::Dropped("redirect")),
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

    fn on_na(&mut self, _ip: &ipv6::Ipv6Header, body: &[u8], now: u64) {
        let Some((target, _flags, ll)) = ndp::parse_na(body) else {
            return;
        };
        if let Some(m) = ll {
            self.neighbor_add(target, m, now);
        }
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

    fn neighbor_add(&mut self, addr: Ipv6Addr, mac: MacAddr, now: u64) {
        for nb in self.neighbors.iter_mut() {
            if nb.valid && nb.addr == addr {
                nb.mac = mac;
                nb.last = now;
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
        };
    }

    #[allow(dead_code)]
    fn neighbor_lookup(&self, addr: Ipv6Addr) -> Option<MacAddr> {
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
    }
    impl FrameSink for VecSink {
        fn send(&mut self, frame: &[u8]) {
            self.frames.push(frame.to_vec());
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
        let mut sink = VecSink { frames: Vec::new() };
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
        let mut sink = VecSink { frames: Vec::new() };
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
        let mut sink = VecSink { frames: Vec::new() };
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
        let mut sink = VecSink { frames: Vec::new() };
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
}
