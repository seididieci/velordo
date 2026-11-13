//! dns — resolver DNS plain (N5+N6.0, ADR-0069).
//!
//! Servizio userspace che risolve nomi a indirizzi IPv6/IPv4 tramite query
//! DNS. Espone endpoint a capability (`DNS_EP_OPEN/TX/CLOSE/STATS`) per i
//! clienti; niente syscall di rete nel kernel, tutto in userspace.
//!
//! N5: la risoluzione usa una tabella statica (`example.com` A,
//! `ipv6.example.com` AAAA). N6.0: i nomi fuori tabella vengono inoltrati a
//! un resolver upstream via UDP plain, se `sys:dns.conf` lo configura.
//! Senza config il servizio resta in tabella statica (fail-closed) e risponde
//! NXDOMAIN; con config ma upstream irraggiungibile risponde SERVFAIL.
//! DoT/TLS arriva con N6.5 (`mode = dot`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use tls_net::TcpStream;

use civis;
use civis::net::{Endpoint, Event, Flow, Proto, SendError, PAYLOAD_MAX};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use net::{dns, Ipv6Addr};

use civis::{
    DNS_CLOSE, DNS_OPEN, DNS_PORT, DNS_STATS, DNS_TX, NET_ADDR_ROUTER, NET_EP_RX_RING_OFF,
    NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_TX_RING_OFF, NET_PROTO_UDP,
};

use net::{CLASS_IN, DNS_HEADER_LEN, FLAG_QR, FLAG_RD, TYPE_A, TYPE_AAAA};

// Metadata slot endpoint dei clienti DNS: layout unico in `net_ep` (N7.0,
// ADR-0074), IPv4 (11 byte).
const SLOT: net_ep::Layout = net_ep::Layout::IPV4;
const SLOT_META: usize = net_ep::Layout::IPV4.meta();
// Evento dati nello slot.
const EV_DATA: u8 = 0;
// Numero massimo di endpoint clienti concorrenti.
const MAX_ENDPOINTS: usize = 8;
// Dimensione massima di una query/risposta DNS accettata dall'upstream.
const MAX_DNS: usize = 512;
// Rcode: SERVFAIL (upstream configurato ma non raggiungibile/risposta invalida).
const RCODE_SERVFAIL: u16 = 2;
// Rcode: NXDOMAIN (nessun upstream configurato, nome fuori tabella).
const RCODE_NXDOMAIN: u16 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EpState {
    Free,
    Bound,
}

#[derive(Clone, Copy)]
struct Ep {
    state: EpState,
    ep_id: u16,
    local_port: u16,
    ring_id: u32,
    ring_base: *mut u8,
    peer_chan: u64,
    rx_pkts: u64,
    tx_pkts: u64,
}

impl Ep {
    const fn free() -> Self {
        Self {
            state: EpState::Free,
            ep_id: 0,
            local_port: 0,
            ring_id: 0,
            ring_base: core::ptr::null_mut(),
            peer_chan: 0,
            rx_pkts: 0,
            tx_pkts: 0,
        }
    }
}

/// Modalità di risoluzione: la config la dichiara, il servizio la applica.
/// `Static` = tabella N5 pura, `Plain` = inoltro UDP (N6.0), `Dot` = TLS
/// (N6.3): una connessione per query, mai fallback silenzioso al plain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Static,
    Plain,
    Dot,
}

/// Lunghezza massima del nome da verificare nel certificato (SNI).
const SNI_MAX: usize = 96;

/// Config del resolver, da `sys:dns.conf`. `Static` = N5 puro.
#[derive(Clone, Copy)]
struct Config {
    mode: Mode,
    upstream: Ipv6Addr,
    port: u16,
    timeout_ticks: u64,
    /// SNI per DoT (`sni = <nome>`), in chiaro: niente alloc nella config.
    sni: [u8; SNI_MAX],
    sni_len: usize,
    /// `mode = dot` dichiarato ma non utilizzabile (SNI o trust anchor
    /// mancanti): i nomi fuori tabella rispondono **SERVFAIL**. Mai NXDOMAIN:
    /// un resolver che degrada in silenzio mente al client.
    broken: bool,
}

impl Config {
    const fn off() -> Self {
        Self {
            mode: Mode::Static,
            upstream: Ipv6Addr::UNSPECIFIED,
            port: 0,
            timeout_ticks: 0,
            sni: [0; SNI_MAX],
            sni_len: 0,
            broken: false,
        }
    }

    /// `mode = dot` dichiarato ma inutilizzabile: fail-closed (SERVFAIL).
    const fn dot_broken() -> Self {
        Self {
            mode: Mode::Dot,
            upstream: Ipv6Addr::UNSPECIFIED,
            port: 0,
            timeout_ticks: 0,
            sni: [0; SNI_MAX],
            sni_len: 0,
            broken: true,
        }
    }

    /// L'inoltro **plain** è utilizzabile solo con upstream e porta validi.
    fn active(&self) -> bool {
        self.mode == Mode::Plain && !self.upstream.is_unspecified() && self.port != 0
    }

    /// Modalità DoT (anche nello stato `broken`: la dispatch resta SERVFAIL).
    fn dot(&self) -> bool {
        self.mode == Mode::Dot
    }

    /// Nome da verificare nel certificato, se c'è.
    fn sni(&self) -> Option<&str> {
        core::str::from_utf8(&self.sni[..self.sni_len]).ok()
    }
}

struct DnsService {
    net_chan: u64,
    /// Endpoint effimero presso `net` usato come client verso l'upstream
    /// plain (N6.0). `None` = tabella statica.
    up: Option<Endpoint>,
    cfg: Config,
    /// DoT (N6.3): endpoint TCP **persistente** (il ring si mappa una volta;
    /// la connessione si riapre per query con `TcpStream::abort`), trust
    /// anchor X.509 in DER e buffer record TLS nell'heap.
    dot: Option<TcpStream>,
    ca: Vec<u8>,
    tls_rbuf: Option<Box<[u8]>>,
    tls_wbuf: Option<Box<[u8]>>,
    /// Contatore dei transaction id verso l'upstream (rimappati: due client
    /// con lo stesso id non si confondono).
    txid: u16,
    endpoints: [Ep; MAX_ENDPOINTS],
    next_ep_id: u16,
}

impl DnsService {
    fn new() -> Self {
        Self {
            net_chan: 0,
            up: None,
            cfg: Config::off(),
            dot: None,
            ca: Vec::new(),
            tls_rbuf: None,
            tls_wbuf: None,
            txid: 0x4000,
            endpoints: [Ep::free(); MAX_ENDPOINTS],
            next_ep_id: 1,
        }
    }

    fn next_txid(&mut self) -> u16 {
        let id = self.txid;
        self.txid = self.txid.wrapping_add(1);
        if self.txid < 0x4000 {
            self.txid = 0x4000;
        }
        id
    }

    fn alloc_endpoint(&mut self, peer_chan: u64) -> Option<usize> {
        for i in 0..MAX_ENDPOINTS {
            if self.endpoints[i].state == EpState::Free {
                let ep_id = self.next_ep_id;
                self.next_ep_id = self.next_ep_id.wrapping_add(1);
                if self.next_ep_id == 0 {
                    self.next_ep_id = 1;
                }
                self.endpoints[i] = Ep {
                    state: EpState::Bound,
                    ep_id,
                    ..Ep::free()
                };
                self.endpoints[i].peer_chan = peer_chan;
                return Some(i);
            }
        }
        None
    }

    fn free_endpoint(&mut self, idx: usize) {
        let ep = self.endpoints[idx];
        if ep.ring_id != 0 {
            let ring_len = NET_EP_RING_HDR * 2 + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2;
            if !ep.ring_base.is_null() {
                let _ = civis::munmap(ep.ring_base as usize, ring_len);
            }
            // Fase 30.1: l'owner chiude il ring (i client che lo mappano
            // tengono il loro ref finche' non fanno `munmap`).
            let _ = civis::shm_destroy(ep.ring_id);
        }
        self.endpoints[idx] = Ep::free();
    }

    fn find_by_id(&self, ep_id: u16) -> Option<usize> {
        self.endpoints
            .iter()
            .position(|e| e.state != EpState::Free && e.ep_id == ep_id)
    }
}

/// Scrive uno slot nel ring RX (servizio → cliente).
fn ep_rx_write_full(
    ring: *mut u8,
    src_ipv4: &[u8; 4],
    src_port: u16,
    dst_port: u16,
    evento: u8,
    payload: &[u8],
) -> bool {
    let total = SLOT.total(payload.len());
    if total > NET_EP_SLOT_SIZE {
        return false;
    }
    let head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
    let tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
    let Some(index) = net_ep::push_index(head, tail, NET_EP_RING_SLOTS as u32) else {
        return false;
    };
    let slot = index as usize % NET_EP_RING_SLOTS;
    let p =
        unsafe { ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
    let win = unsafe { core::slice::from_raw_parts_mut(p, total) };
    if net_ep::write(win, SLOT, src_ipv4, src_port, dst_port, evento, payload).is_none() {
        return false;
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring.add(NET_EP_RX_RING_OFF + 4) as *mut u32,
            net_ep::next(tail),
        )
    };
    true
}

/// Legge uno slot dal ring TX (cliente → servizio).
fn ep_tx_read_full(
    ring: *mut u8,
    slot_idx: usize,
    buf: &mut [u8],
) -> Option<(usize, [u8; 4], u16, u16, u8)> {
    if slot_idx >= NET_EP_RING_SLOTS {
        return None;
    }
    let p =
        unsafe { ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot_idx * NET_EP_SLOT_SIZE) };
    let total = unsafe { read_volatile(p as *const u16) } as usize;
    let payload_len = net_ep::check(SLOT, total, NET_EP_SLOT_SIZE)?;
    if payload_len > buf.len() {
        return None;
    }
    let win = unsafe { core::slice::from_raw_parts(p, total) };
    let s = net_ep::read(win, SLOT, total)?;
    buf[..payload_len].copy_from_slice(s.payload);
    let mut dst_ipv4 = [0u8; 4];
    dst_ipv4.copy_from_slice(s.addr);
    Some((payload_len, dst_ipv4, s.port, s.local, s.event))
}

/// Avanza la tail del TX ring (slot consumato).
fn ep_tx_consume_raw(ring: *mut u8) {
    let head = unsafe { read_volatile(ring.add(NET_EP_TX_RING_OFF) as *const u32) };
    unsafe {
        write_volatile(
            ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            head.wrapping_add(1),
        )
    };
}

/// Costruisce una risposta DNS per una query.
///
/// `rcode` 0 = NOERROR. Se `a` è `Some`, aggiunge un record A; se `aaaa` è
/// `Some`, aggiunge un record AAAA. La domanda viene ricodificata dal nome.
fn build_response(
    out: &mut [u8],
    id: u16,
    name: &str,
    qtype: u16,
    a: Option<[u8; 4]>,
    aaaa: Option<[u8; 16]>,
    rcode: u16,
) -> Option<usize> {
    let ancount = (a.is_some() as u16) + (aaaa.is_some() as u16);
    let flags = FLAG_QR | FLAG_RD | (rcode & dns::FLAG_RCODE_MASK);
    let mut off = dns::write_header(out, id, flags, 1, ancount, 0, 0);
    off += dns::encode_question(&mut out[off..], name, qtype, CLASS_IN)?;

    if ancount > 0 {
        off += dns::encode_name(&mut out[off..], name)?;
    }
    if let Some(addr) = a {
        out[off..off + 2].copy_from_slice(&TYPE_A.to_be_bytes());
        out[off + 2..off + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
        out[off + 4..off + 8].copy_from_slice(&300u32.to_be_bytes());
        out[off + 8..off + 10].copy_from_slice(&4u16.to_be_bytes());
        out[off + 10..off + 14].copy_from_slice(&addr);
        off += 14;
    }
    if let Some(addr) = aaaa {
        out[off..off + 2].copy_from_slice(&TYPE_AAAA.to_be_bytes());
        out[off + 2..off + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
        out[off + 4..off + 8].copy_from_slice(&300u32.to_be_bytes());
        out[off + 8..off + 10].copy_from_slice(&16u16.to_be_bytes());
        out[off + 10..off + 26].copy_from_slice(&addr);
        off += 26;
    }
    Some(off)
}

/// Tabella statica N5.
fn lookup(name: &[u8], qtype: u16) -> Option<(Option<[u8; 4]>, Option<[u8; 16]>)> {
    if name == b"example.com" && qtype == TYPE_A {
        Some((Some([93, 184, 216, 34]), None))
    } else if name == b"ipv6.example.com" && qtype == TYPE_AAAA {
        Some((None, Some([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ])))
    } else {
        None
    }
}

// ── Upstream plain UDP (N6.0) ──────────────────────────────────────

/// Interroga `net` sugli indirizzi del nodo (`NET_ADDR`): 16 byte in due
/// parole. `None` se il selettore non ha indirizzo o il servizio è morto.
fn query_addr(net_chan: u64, selector: u64) -> Option<Ipv6Addr> {
    let r = civis::send(net_chan, civis::NET_ADDR, selector, 0).ok()?;
    if r.w0 == 0 && r.w1 == 0 {
        return None;
    }
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&r.w0.to_be_bytes());
    b[8..].copy_from_slice(&r.w1.to_be_bytes());
    Some(Ipv6Addr::from_bytes(b))
}

/// Legge `sys:dns.conf` dal bucket `sys`. File assente o invalido = tabella
/// statica: la config non è mai parziale (fail-closed).
///
/// Formato: `chiave = valore` per riga, `#` commento.
/// `upstream = auto|<ipv6>`, `port = 853|5353|53`, `mode = plain|dot|off`,
/// `timeout_ms = 2000`, `sni = <nome>` (obbligatorio con `dot`). `auto` =
/// default router corrente (test passt). La trust anchor DoT è l'oggetto
/// `sys:ca.der` (DER), letta all'avvio.
fn load_config(net_chan: u64) -> Config {
    let Ok(data) = civis::obj_get(b"sys", b"dns.conf") else {
        return Config::off();
    };
    let Ok(txt) = core::str::from_utf8(&data) else {
        return Config::off();
    };
    let mut upstream_txt: Option<&str> = None;
    let mut sni_txt: Option<&str> = None;
    let mut port = DNS_PORT;
    let mut mode = Mode::Plain;
    let mut timeout_ms = 2000u64;
    for line in txt.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Config::off();
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "upstream" => upstream_txt = Some(v),
            "port" => match v.parse::<u16>() {
                Ok(p) if p != 0 => port = p,
                _ => return Config::off(),
            },
            "mode" => match v {
                "plain" => mode = Mode::Plain,
                "dot" => mode = Mode::Dot,
                "off" | "static" => mode = Mode::Static,
                _ => return Config::off(),
            },
            "timeout_ms" => match v.parse::<u64>() {
                Ok(t) if t >= 100 => timeout_ms = t,
                _ => return Config::off(),
            },
            "sni" => sni_txt = Some(v),
            _ => {}
        }
    }
    // Da qui in poi un errore di config con `mode = dot` NON torna alla
    // tabella statica: è fail-closed (SERVFAIL sui nomi fuori tabella).
    let Some(u) = upstream_txt else {
        return if mode == Mode::Dot {
            let _ = civis::print_string(b"[dns] mode=dot senza upstream: SERVFAIL\n");
            Config::dot_broken()
        } else {
            Config::off()
        };
    };
    let upstream = if u == "auto" {
        match query_addr(net_chan, NET_ADDR_ROUTER) {
            Some(a) => a,
            None => {
                let _ = civis::print_string(b"[dns] upstream=auto senza router\n");
                return if mode == Mode::Dot {
                    Config::dot_broken()
                } else {
                    Config::off()
                };
            }
        }
    } else {
        match Ipv6Addr::parse(u) {
            Some(a) => a,
            None => {
                let _ = civis::print_string(b"[dns] upstream non valido\n");
                return if mode == Mode::Dot {
                    Config::dot_broken()
                } else {
                    Config::off()
                };
            }
        }
    };
    let timeout_ticks = (timeout_ms / 10).max(1);
    let mut cfg = Config {
        mode,
        upstream,
        port,
        timeout_ticks,
        sni: [0; SNI_MAX],
        sni_len: 0,
        broken: false,
    };
    if mode == Mode::Dot {
        let Some(sni) = sni_txt else {
            let _ = civis::print_string(b"[dns] mode=dot senza sni: SERVFAIL (fail-closed)\n");
            return Config::dot_broken();
        };
        if sni.is_empty() || sni.len() > SNI_MAX {
            let _ = civis::print_string(b"[dns] sni non valido: SERVFAIL (fail-closed)\n");
            return Config::dot_broken();
        }
        cfg.sni[..sni.len()].copy_from_slice(sni.as_bytes());
        cfg.sni_len = sni.len();
    }
    cfg
}

/// Apre l'endpoint UDP effimero presso `net` (lato client dell'inoltro).
///
/// Dal N7.0 apertura, ring e slot sono di `civis::net` (ADR-0074).
fn open_upstream(net_chan: u64) -> Option<Endpoint> {
    Endpoint::open(net_chan, Proto::Udp, Flow::Ephemeral).ok()
}

/// Drena il ring RX dell'upstream e cerca la risposta alla query `txid`:
/// verifica sorgente (indirizzo e porta configurati), `QR` e id. Copia il
/// payload in `out` ripristinando il transaction id del cliente.
fn up_take_reply(
    up: &mut Endpoint,
    upstream: Ipv6Addr,
    up_port: u16,
    txid: u16,
    client_id: u16,
    out: &mut [u8],
    buf: &mut [u8],
) -> Option<usize> {
    while let Some(r) = up.recv(buf) {
        if r.truncated
            || r.addr != upstream.0
            || r.port != up_port
            || r.len < DNS_HEADER_LEN
            || r.len > out.len()
        {
            continue;
        }
        let payload = &buf[..r.len];
        let Some(hdr) = dns::parse_header(payload) else {
            continue;
        };
        if hdr.id == txid && hdr.flags & FLAG_QR != 0 {
            out[..r.len].copy_from_slice(payload);
            out[0..2].copy_from_slice(&client_id.to_be_bytes());
            return Some(r.len);
        }
    }
    None
}

/// Inoltra `query` all'upstream (plain UDP) e attende la risposta entro il
/// timeout. **Bloccante sul processo** (single-flight, deadline breve): il
/// resolver non serve altri clienti mentre aspetta; il bound è la garanzia.
/// Ritorna la lunghezza della risposta in `out`.
fn forward_plain(
    svc: &mut DnsService,
    query: &[u8],
    client_id: u16,
    out: &mut [u8],
) -> Option<usize> {
    if !svc.cfg.active() || query.len() < DNS_HEADER_LEN || query.len() > MAX_DNS {
        return None;
    }
    let txid = svc.next_txid();
    let mut wire = [0u8; MAX_DNS];
    wire[..query.len()].copy_from_slice(query);
    wire[0..2].copy_from_slice(&txid.to_be_bytes());

    let (upstream, up_port) = (svc.cfg.upstream, svc.cfg.port);
    let Some(up) = svc.up.as_mut() else {
        return None;
    };
    match up.send(upstream.0, up_port, Event::Data, &wire[..query.len()]) {
        Ok(()) => {}
        Err(SendError::Full) => {
            let _ = civis::print_string(b"[dns] upstream: ring TX pieno\n");
            return None;
        }
        Err(_) => {
            let _ = civis::print_string(b"[dns] upstream: net morto\n");
            return None;
        }
    }

    let deadline = civis::get_ticks() as u64 + svc.cfg.timeout_ticks;
    let mut buf = [0u8; PAYLOAD_MAX];
    loop {
        if let Some(len) = up_take_reply(up, upstream, up_port, txid, client_id, out, &mut buf) {
            return Some(len);
        }
        let now = civis::get_ticks() as u64;
        if now >= deadline {
            let _ = civis::print_string(b"[dns] upstream timeout\n");
            return None;
        }
        let _ = civis::futex_wait(&FUTEX as *const u32 as usize, 0, now + 1);
    }
}

// ── Upstream DoT (N6.3) ────────────────────────────────────────────
//
// Una connessione per query (ADR-0072): niente keep-alive né resumption.
// L'**endpoint** TCP invece è persistente: il ring si mappa una volta e la
// connessione si riapre con `TcpStream::abort` (endpoint persistente: il
// ring non si ricrea per query).

/// Arma il client DoT: trust anchor da `sys:ca.der`, endpoint TCP verso
/// l'upstream, buffer record TLS nell'heap (16 KiB l'uno, mai sullo stack).
/// Qualsiasi fallimento = `broken` → SERVFAIL, mai tabella statica.
fn open_dot(svc: &mut DnsService) {
    match civis::obj_get(b"sys", b"ca.der") {
        Ok(ca) if !ca.is_empty() => svc.ca = ca,
        _ => {
            let _ = civis::print_string(b"[dns] DoT senza sys:ca.der: SERVFAIL (fail-closed)\n");
            svc.cfg = Config::dot_broken();
            return;
        }
    }
    let Some(sni) = svc.cfg.sni() else {
        svc.cfg = Config::dot_broken();
        return;
    };
    let Some(mut stream) = TcpStream::connect(svc.net_chan, svc.cfg.upstream.0, svc.cfg.port)
    else {
        let _ = civis::print_string(b"[dns] DoT: endpoint TCP non apribile\n");
        svc.cfg = Config::dot_broken();
        return;
    };
    stream.set_timeout(svc.cfg.timeout_ticks, svc.cfg.timeout_ticks);
    svc.tls_rbuf = Some(alloc::vec![0u8; tls::RECORD_READ_BUF].into_boxed_slice());
    svc.tls_wbuf = Some(alloc::vec![0u8; tls::RECORD_WRITE_BUF].into_boxed_slice());
    let ep = stream.ep_id();
    civis::println!(
        "[dns] upstream dot sni={} {}:{} ca={}B timeout={} tick (endpoint {})",
        sni,
        svc.cfg.upstream,
        svc.cfg.port,
        svc.ca.len(),
        svc.cfg.timeout_ticks,
        ep
    );
    svc.dot = Some(stream);
}

/// Scambio applicativo su una sessione aperta: query framed, risposta
/// framed, id del client ripristinato, `QR` obbligatorio.
fn dot_exchange(
    sess: &mut tls::Session<'_, &mut TcpStream>,
    frame: &[u8],
    client_id: u16,
    out: &mut [u8],
) -> Option<usize> {
    sess.write_all(frame).ok()?;
    let mut prefix = [0u8; dns::TCP_LEN_PREFIX];
    sess.read_exact(&mut prefix).ok()?;
    let len = dns::tcp_prefix_len(&prefix)?;
    if len < DNS_HEADER_LEN || len > out.len() {
        return None;
    }
    sess.read_exact(&mut out[..len]).ok()?;
    let hdr = dns::parse_header(&out[..len])?;
    if hdr.flags & FLAG_QR == 0 {
        return None;
    }
    out[0..2].copy_from_slice(&client_id.to_be_bytes());
    Some(len)
}

/// Inoltra `query` via DoT: handshake con **catena e hostname verificati**
/// (non `NoVerify`), framing RFC 7858, `close_notify`. `None` = SERVFAIL.
fn forward_dot(
    stream: &mut TcpStream,
    ca: &[u8],
    sni: &str,
    rbuf: &mut [u8],
    wbuf: &mut [u8],
    query: &[u8],
    client_id: u16,
    out: &mut [u8],
) -> Option<usize> {
    let mut frame = [0u8; MAX_DNS + dns::TCP_LEN_PREFIX];
    let flen = dns::frame_tcp(&mut frame, query)?;
    let cfg = tls::Config {
        server_name: sni,
        ca_der: ca,
        alpn: &[&b"dot"[..]],
    };
    let mut sess = match tls::connect::<&mut TcpStream, tls_net::SystemClock>(
        &mut *stream,
        &cfg,
        rbuf,
        wbuf,
    ) {
        Ok(s) => s,
        Err(e) => {
            civis::println!("[dns] dot: handshake fallito: {:?}", e);
            stream.abort();
            return None;
        }
    };
    let res = dot_exchange(&mut sess, &frame[..flen], client_id, out);
    let _ = sess.close();
    // Sempre: la connessione e' finita. `abort` libera lo slot TCP del core e
    // azzera `conn_id` (la prossima query ne apre una nuova) e scarta gli
    // eventi stantii della precedente: senza, l'EV_FIN del server arrivava
    // al primo read dell'handshake successivo e lo chiudeva (BrokenPipe).
    stream.abort();
    res
}

civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    if civis::service_register(civis::Service::Dns).is_err() {
        let _ = civis::print_string(b"[dns] service_register rifiutato\n");
        civis::exit(1);
    }
    let _ = civis::print_string(b"[dns] registered as service Dns\n");

    let mut svc = DnsService::new();
    let mut net_chan = 0u64;
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::Net) {
            if c > 0 {
                net_chan = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if net_chan == 0 {
        let _ = civis::print_string(b"[dns] servizio net non trovato\n");
        civis::exit(1);
    }
    svc.net_chan = net_chan;
    svc.cfg = load_config(net_chan);

    // DoT (N6.3): armato solo con config valida; altrimenti fail-closed.
    // L'endpoint UDP effimero è il lato *client* dell'inoltro plain (N6.0).
    // Senza upstream non si apre nulla: il servizio resta tabella statica
    // esattamente come in N5.
    if svc.cfg.dot() {
        if svc.cfg.broken {
            let _ = civis::print_string(
                b"[dns] DoT non armato: SERVFAIL sui nomi fuori tabella\n",
            );
        } else {
            open_dot(&mut svc);
        }
    } else if svc.cfg.active() {
        match open_upstream(net_chan) {
            Some(up) => {
                svc.up = Some(up);
                civis::println!(
                    "[dns] upstream plain {}:{} timeout={} tick",
                    svc.cfg.upstream,
                    svc.cfg.port,
                    svc.cfg.timeout_ticks
                );
            }
            None => {
                svc.cfg = Config::off();
                let _ = civis::print_string(b"[dns] upstream non apribile: tabella statica\n");
            }
        }
    } else {
        let _ = civis::print_string(b"[dns] tabella statica (nessun upstream)\n");
    }

    civis::signal_ready(1);
    civis::println!("[dns] resolver attivo, porta UDP {}", DNS_PORT);

    loop {
        let now = civis::get_ticks() as u64;
        while let Some(m) = civis::recv_poll() {
            handle_client(&mut svc, m);
        }
        let _ = civis::futex_wait(&FUTEX as *const u32 as usize, 0, (now + 1) as u64);
    }
}

fn handle_client(svc: &mut DnsService, m: civis::IpcMsg) {
    match m.tag {
        DNS_OPEN => {
            let proto = (m.w0 >> 16) as u16;
            let local_port = (m.w0 & 0xffff) as u16;
            if proto != NET_PROTO_UDP as u16 {
                let _ = civis::reply(DNS_OPEN, !0u64, 0);
                return;
            }
            let port = if local_port == 0 {
                let p = 49152u16.wrapping_add(svc.next_ep_id);
                svc.next_ep_id = svc.next_ep_id.wrapping_add(1);
                p
            } else {
                local_port
            };
            let Some(idx) = svc.alloc_endpoint(m.channel) else {
                let _ = civis::reply(DNS_OPEN, !0u64, 0);
                return;
            };
            let ring_len =
                (NET_EP_RING_HDR * 2 + NET_EP_RING_SLOTS * NET_EP_SLOT_SIZE * 2).next_power_of_two();
            let ring_id = match civis::shm_create(ring_len) {
                Ok(id) => id,
                Err(_) => {
                    svc.free_endpoint(idx);
                    let _ = civis::reply(DNS_OPEN, !0u64, 0);
                    return;
                }
            };
            let ring_base =
                match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
                    Ok(b) => b,
                    Err(_) => {
                        svc.free_endpoint(idx);
                        let _ = civis::reply(DNS_OPEN, !0u64, 0);
                        return;
                    }
                } as *mut u8;
            unsafe {
                write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
                write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
                write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
                write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
            }
            let ep = &mut svc.endpoints[idx];
            ep.local_port = port;
            ep.ring_id = ring_id;
            ep.ring_base = ring_base;
            let ep_id = ep.ep_id;
            let _ = civis::reply(
                DNS_OPEN,
                ep_id as u64,
                (ring_id as u64) | ((port as u64) << 32),
            );
        }
        DNS_TX => {
            let ep_id = m.w0 as u16;
            let slot_idx = m.w1 as usize;
            let Some(idx) = svc.find_by_id(ep_id) else {
                let _ = civis::reply(DNS_TX, !0u64, 0);
                return;
            };
            let ep = svc.endpoints[idx];
            let ring = ep.ring_base;
            let mut payload = [0u8; NET_EP_SLOT_SIZE - SLOT_META];
            let (payload_len, _, _, _, _) = match ep_tx_read_full(ring, slot_idx, &mut payload) {
                Some(v) => v,
                None => {
                    let _ = civis::reply(DNS_TX, !0u64, 0);
                    return;
                }
            };
            let query = &payload[..payload_len];

            let (id, name, qtype) = match query_question(query) {
                Some(v) => v,
                None => {
                    ep_tx_consume_raw(ring);
                    let _ = civis::reply(DNS_TX, 0, 0);
                    return;
                }
            };

            let mut resp = [0u8; MAX_DNS];
            let resp_len = match core::str::from_utf8(&name) {
                Ok(n) => match lookup(&name, qtype) {
                    Some((a, aaaa)) => {
                        build_response(&mut resp, id, n, qtype, a, aaaa, 0).unwrap_or(0)
                    }
                    // Nome fuori tabella → upstream. Plain UDP (N6.0) o DoT
                    // (N6.3, una connessione per query). Se l'upstream è
                    // configurato ma non risponde (o TLS/certificato
                    // falliscono) → SERVFAIL: mai NXDOMAIN, che sarebbe una
                    // risposta sbagliata data per buona.
                    None if svc.cfg.dot() => {
                        let mut len = 0;
                        let mut stream = svc.dot.take();
                        if let Some(st) = stream.as_mut() {
                            if let (Some(sni), Some(rbuf), Some(wbuf)) = (
                                svc.cfg.sni(),
                                svc.tls_rbuf.as_mut(),
                                svc.tls_wbuf.as_mut(),
                            ) {
                                if let Some(n) =
                                    forward_dot(st, &svc.ca, sni, rbuf, wbuf, query, id, &mut resp)
                                {
                                    len = n;
                                }
                            }
                        }
                        svc.dot = stream;
                        if len == 0 {
                            build_response(&mut resp, id, n, qtype, None, None, RCODE_SERVFAIL)
                                .unwrap_or(0)
                        } else {
                            len
                        }
                    }
                    None if svc.cfg.active() => match forward_plain(svc, query, id, &mut resp) {
                        Some(len) => len,
                        None => build_response(
                            &mut resp,
                            id,
                            n,
                            qtype,
                            None,
                            None,
                            RCODE_SERVFAIL,
                        )
                        .unwrap_or(0),
                    },
                    None => build_response(
                        &mut resp,
                        id,
                        n,
                        qtype,
                        None,
                        None,
                        RCODE_NXDOMAIN,
                    )
                    .unwrap_or(0),
                },
                // Nome non UTF-8: non è un nome DNS valido.
                Err(_) => 0,
            };

            if resp_len > 0 {
                if ep_rx_write_full(ring, &[0, 0, 0, 0], DNS_PORT, ep.local_port, EV_DATA, &resp[..resp_len])
                {
                    if ep.peer_chan != 0 {
                        let _ = civis::send_async(ep.peer_chan, DNS_TX, ep_id as u64, 0);
                    }
                }
            }
            ep_tx_consume_raw(ring);
            if let Some(i) = svc.find_by_id(ep_id) {
                svc.endpoints[i].tx_pkts = svc.endpoints[i].tx_pkts.saturating_add(1);
            }
            let _ = civis::reply(DNS_TX, 0, 0);
        }
        DNS_CLOSE => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                svc.free_endpoint(idx);
            }
            let _ = civis::reply(DNS_CLOSE, 0, 0);
        }
        DNS_STATS => {
            let ep_id = m.w0 as u16;
            if let Some(idx) = svc.find_by_id(ep_id) {
                let ep = svc.endpoints[idx];
                let _ = civis::reply(DNS_STATS, ep.rx_pkts, ep.tx_pkts);
            } else {
                let _ = civis::reply(DNS_STATS, !0u64, 0);
            }
        }
        civis::EXIT_NOTIFY => {
            let dead_chan = m.channel;
            for i in 0..MAX_ENDPOINTS {
                if svc.endpoints[i].state != EpState::Free
                    && svc.endpoints[i].peer_chan == dead_chan
                {
                    svc.free_endpoint(i);
                }
            }
        }
        _ => {}
    }
}

/// Estrae (id, nome, tipo) da una query DNS.
fn query_question(pkt: &[u8]) -> Option<(u16, Vec<u8>, u16)> {
    if pkt.len() < DNS_HEADER_LEN {
        return None;
    }
    let hdr = dns::parse_header(pkt)?;
    if hdr.qdcount == 0 {
        return None;
    }
    let (name, qtype, _qclass, _off) = dns::decode_question(pkt, DNS_HEADER_LEN)?;
    Some((hdr.id, name, qtype))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[dns] panic\n");
    civis::exit(1)
}

static FUTEX: u32 = 0;
