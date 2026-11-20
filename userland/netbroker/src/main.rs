//! netbroker — broker di rete per il codice non firmato (ADR-0080, fase 6).
//!
//! Perche' esiste: `net` e `siit` negano `Service::Net`/`Service::Siit` alle
//! immagini `Untrusted` (gate di **classe** misurata dal kernel, non una
//! tabella). Il codice non firmato resta pero' utile: naviga attraverso questo
//! broker, che e' un proxy con una API stretta.
//!
//! # Modello
//!
//! Per ogni cliente il broker apre un endpoint nel servizio `net` (con la
//! stessa `flow` chiesta dal cliente) e ne espone uno proprio a ring
//! (`BROKER_OPEN/TX/CLOSE/STATS`): il cliente vede un solo ring, il broker
//! inoltra nei due sensi.
//!
//! # Politica
//!
//! - **allowlist di porte**: le porte locali fuori allowlist sono negate
//!   (le effimere — client uscente — sono sempre libere);
//! - **rate limit per identita'** (finestra di tick, contatore in RAM);
//! - **audit in Vestigia** su open/deny/tx/close (mai nel data path caldo:
//!   `tx` logga solo il primo invio di ogni finestra per non affogare il log);
//! - **backpressure senza drop silenzioso**: `Full` = lo slot resta al
//!   cliente, che ritenta con lo stesso payload;
//! - **fail-closed**: senza `net` il broker non parte.

#![no_std]
#![no_main]

extern crate alloc;

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use broker_proto as proto;
use civis;
use custos_proto;
use civis::{
    BROKER_CLOSE, BROKER_OPEN, BROKER_STATS, BROKER_TX, NET_EP_CLOSE, NET_EP_FLAG_EPHEMERAL,
    NET_EP_OPEN, NET_EP_RING_BYTES, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_RX_RING_OFF,
    NET_EP_SLOT_SIZE, NET_EP_TX, NET_EP_TX_RING_OFF, NET_PROTO_IPV4, NET_PROTO_TCP, NET_PROTO_UDP,
    SIIT_EP_CLOSE, SIIT_EP_OPEN, SIIT_EP_TX,
};

static FUTEX: u32 = 0;

/// Clienti broker concorrenti.
const MAX_CLIENTS: usize = 8;

/// Rate limit: richieste ammesse per finestra (open + tx). Un trasferimento
/// legittimo via broker sta largamente sotto: il limite e' contro lo
/// scan/flood, non contro l'uso normale.
const RATE_MAX: u64 = 200;
/// Finestra del rate limit (tick PIT, 10 ms): 0,5 s.
const RATE_WINDOW: u64 = 50;

/// Lunghezza della regione ring di un endpoint (la stessa che crea `net`).
const RING_LEN: usize = NET_EP_RING_BYTES;

/// STATO DI UN CLIENTE: un ring broker verso il cliente, un endpoint `net`
/// verso la rete.
#[derive(Clone, Copy)]
struct Client {
    state: ClientState,
    /// Endpoint broker (ID che il cliente usa in TX/CLOSE/STATS).
    ep_id: u16,
    /// Ring broker (shm del broker, mappato qui e dal cliente).
    ring_id: u32,
    ring_base: *mut u8,
    peer_chan: u64,
    /// Endpoint `net` che porta il traffico (0 = non aperto).
    net_ep_id: u16,
    net_ring: *mut u8,
    net_port: u16,
    rx_pkts: u64,
    tx_pkts: u64,
    rate_count: u64,
    rate_start: u64,
    /// Da dove esce il traffico: `net` (IPv6) o `siit` (IPv4 tradotto, N4).
    /// Il layout degli slot dipende da qui (IPv6 vs IPv4).
    upstream: Upstream,
    /// `true` se il TX corrente ha gia' lasciato una traccia di audit nella
    /// finestra (evita una riga di log per datagramma).
    audit_tx: bool,
    /// Destinazioni globali consentite (decisione presa UNA volta all'open):
    /// vera per le immagini `System`/`ThirdParty` e per chi ha il consenso
    /// dell'utente + il grant admin (`CAP_NET_USE`). Loopback e link-local
    /// non la richiedono: sono la rete locale, gia' confinata al guest.
    pub_net: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Free,
    Bound,
}

/// Servizio a monte dell'inoltro: `net` parla IPv6, `siit` traduce verso IPv4
/// (prefisso 64:ff9b::/96). Il broker non traduce nulla: sceglie la porta
/// d'uscita e inoltra gli slot, che hanno layout diversi (`net-ep`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Upstream {
    Net,
    Siit,
}

impl Upstream {
    const fn layout(self) -> net_ep::Layout {
        match self {
            Upstream::Net => net_ep::Layout::IPV6,
            Upstream::Siit => net_ep::Layout::IPV4,
        }
    }

    const fn service(self) -> civis::Service {
        match self {
            Upstream::Net => civis::Service::Net,
            Upstream::Siit => civis::Service::Siit,
        }
    }

    /// Tag IPC del servizio a monte (protocollo `net` per IPv6, `SIIT_*` per
    /// l'uscita IPv4: stessi slot, tag diversi).
    const fn open_tag(self) -> u64 {
        match self {
            Upstream::Net => NET_EP_OPEN,
            Upstream::Siit => SIIT_EP_OPEN,
        }
    }

    const fn tx_tag(self) -> u64 {
        match self {
            Upstream::Net => NET_EP_TX,
            Upstream::Siit => SIIT_EP_TX,
        }
    }

    const fn close_tag(self) -> u64 {
        match self {
            Upstream::Net => NET_EP_CLOSE,
            Upstream::Siit => SIIT_EP_CLOSE,
        }
    }
}

impl Client {
    const fn free() -> Self {
        Self {
            state: ClientState::Free,
            ep_id: 0,
            ring_id: 0,
            ring_base: core::ptr::null_mut(),
            peer_chan: 0,
            net_ep_id: 0,
            net_ring: core::ptr::null_mut(),
            net_port: 0,
            rx_pkts: 0,
            tx_pkts: 0,
            rate_count: 0,
            rate_start: 0,
            upstream: Upstream::Net,
            audit_tx: false,
            pub_net: false,
        }
    }
}

// ── Ring (una sola forma: gli helper sono neutri rispetto al lato) ─────

/// Scrive uno slot nel ring RX (servizio → cliente).
fn rx_write(
    layout: net_ep::Layout,
    ring: *mut u8,
    src: &[u8],
    port: u16,
    local: u16,
    event: u8,
    payload: &[u8],
) -> bool {
    let total = layout.total(payload.len());
    if ring.is_null() || total > NET_EP_SLOT_SIZE {
        return false;
    }
    let head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
    let tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
    let Some(index) = net_ep::push_index(head, tail, NET_EP_RING_SLOTS as u32) else {
        return false;
    };
    let slot = index as usize % NET_EP_RING_SLOTS;
    let p = unsafe { ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
    let win = unsafe { core::slice::from_raw_parts_mut(p, total) };
    if net_ep::write(win, layout, src, port, local, event, payload).is_none() {
        return false;
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *mut u32, net_ep::next(tail));
    }
    true
}

/// Legge uno slot da un ring TX e lo copia in `out`; ritorna
/// `(len, addr, port, local, event)`.
fn tx_read(
    layout: net_ep::Layout,
    ring: *mut u8,
    slot_idx: usize,
    out: &mut [u8],
) -> Option<(usize, [u8; 16], u16, u16, u8)> {
    if ring.is_null() || slot_idx >= NET_EP_RING_SLOTS {
        return None;
    }
    let p = unsafe { ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot_idx * NET_EP_SLOT_SIZE) };
    let total = unsafe { read_volatile(p as *const u16) } as usize;
    let payload_len = net_ep::check(layout, total, NET_EP_SLOT_SIZE)?;
    if payload_len > out.len() {
        return None;
    }
    let win = unsafe { core::slice::from_raw_parts(p, total) };
    let s = net_ep::read(win, layout, total)?;
    out[..payload_len].copy_from_slice(s.payload);
    // `addr` ha sempre 16 byte: per IPv4 (layout SIIT) l'indirizzo vive negli
    // ultimi 4 (il resto resta zero), cosi' il resto del codice ha una forma
    // sola e non deve sapere quale uscita sta percorrendo.
    let mut addr = [0u8; 16];
    if s.addr.len() > addr.len() {
        return None;
    }
    let off = addr.len() - s.addr.len();
    addr[off..].copy_from_slice(s.addr);
    Some((payload_len, addr, s.port, s.local, s.event))
}

/// Scrive uno slot nel ring TX del servizio `net` (indice `slot_idx`).
fn net_tx_write(
    layout: net_ep::Layout,
    ring: *mut u8,
    slot_idx: usize,
    dst: &[u8],
    port: u16,
    local: u16,
    event: u8,
    payload: &[u8],
) -> bool {
    let total = layout.total(payload.len());
    if ring.is_null() || slot_idx >= NET_EP_RING_SLOTS || total > NET_EP_SLOT_SIZE {
        return false;
    }
    let p = unsafe { ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot_idx * NET_EP_SLOT_SIZE) };
    let win = unsafe { core::slice::from_raw_parts_mut(p, total) };
    if net_ep::write(win, layout, dst, port, local, event, payload).is_none() {
        return false;
    }
    fence(Ordering::Release);
    true
}

// ── Servizio ───────────────────────────────────────────────────────────

struct Broker {
    /// Canale verso il servizio `net` (0 = non risolto).
    net_chan: u64,
    /// Canale verso `siit` (0 = non risolto): uscita IPv4.
    siit_chan: u64,
    /// Canale verso `Custos` (0 = non risolto): decide le destinazioni
    /// globali per le immagini senza privilegio (ADR-0080, fase 6).
    custos_chan: u64,
    clients: [Client; MAX_CLIENTS],
    next_ep_id: u16,
}

impl Broker {
    const fn new() -> Self {
        Self {
            net_chan: 0,
            siit_chan: 0,
            custos_chan: 0,
            clients: [Client::free(); MAX_CLIENTS],
            next_ep_id: 1,
        }
    }

    fn alloc(&mut self, peer_chan: u64, now: u64) -> Option<usize> {
        for i in 0..MAX_CLIENTS {
            if self.clients[i].state == ClientState::Free {
                let ep_id = self.next_ep_id;
                self.next_ep_id = self.next_ep_id.wrapping_add(1).max(1);
                self.clients[i] = Client {
                    state: ClientState::Bound,
                    ep_id,
                    rate_start: now,
                    ..Client::free()
                };
                self.clients[i].peer_chan = peer_chan;
                return Some(i);
            }
        }
        None
    }

    fn free(&mut self, idx: usize) {
        let c = self.clients[idx];
        if c.net_ep_id != 0 {
            let up = c.upstream;
            let chan = match up {
                Upstream::Net => self.net_chan,
                Upstream::Siit => self.siit_chan,
            };
            if chan != 0 {
                let _ = civis::send(chan, up.close_tag(), c.net_ep_id as u64, 0);
            }
        }
        if !c.net_ring.is_null() {
            let _ = civis::munmap(c.net_ring as usize, RING_LEN);
        }
        if !c.ring_base.is_null() {
            let _ = civis::munmap(c.ring_base as usize, RING_LEN);
        }
        if c.ring_id != 0 {
            // Fase 30.1: l'owner chiude il ring; i clienti tengono il loro ref
            // finche' non fanno `munmap` (o muoiono).
            let _ = civis::shm_destroy(c.ring_id);
        }
        self.clients[idx] = Client::free();
    }

    fn by_ep(&self, ep_id: u16) -> Option<usize> {
        self.clients
            .iter()
            .position(|c| c.state == ClientState::Bound && c.ep_id == ep_id)
    }

    fn by_peer(&self, chan: u64) -> Option<usize> {
        self.clients
            .iter()
            .position(|c| c.state == ClientState::Bound && c.peer_chan == chan)
    }

    /// Canale del servizio a monte (`net` per IPv6, `siit` per IPv4).
    /// `Err` = servizio non disponibile: l'apertura e' negata (fail-closed).
    fn upstream_chan(&mut self, up: Upstream) -> Result<u64, ()> {
        let slot = match up {
            Upstream::Net => &mut self.net_chan,
            Upstream::Siit => &mut self.siit_chan,
        };
        if *slot == 0 {
            if let Ok(c) = civis::service_lookup(up.service()) {
                if c > 0 {
                    *slot = c as u64;
                }
            }
        }
        if *slot == 0 { Err(()) } else { Ok(*slot) }
    }

    /// `true` se `id` ha consenso dell'utente **e** grant admin per l'uso
    /// della rete globale (`CUSTOS_AUTHORIZE`). Fail-closed: Custos assente
    /// o muto = negato.
    fn authorized(&mut self, id: civis::ImageId) -> bool {
        if self.custos_chan == 0 {
            if let Ok(c) = civis::service_lookup(civis::Service::Custos) {
                if c > 0 {
                    self.custos_chan = c as u64;
                }
            }
        }
        if self.custos_chan == 0 {
            return false;
        }
        custos_proto::authorize(self.custos_chan, id.lo, id.hi, custos_proto::CAP_NET_USE, 0)
    }

    /// Rate limit per identita': `true` = richiesta ammessa.
    fn rate_ok(&mut self, idx: usize, now: u64) -> bool {
        let c = &mut self.clients[idx];
        if now.saturating_sub(c.rate_start) >= RATE_WINDOW {
            c.rate_start = now;
            c.rate_count = 0;
            c.audit_tx = false;
        }
        c.rate_count = c.rate_count.saturating_add(1);
        c.rate_count <= RATE_MAX
    }
}

civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    if civis::service_register(civis::Service::NetBroker).is_err() {
        let _ = civis::print_string(b"[netbroker] FAILED: service_register\n");
        civis::exit(1);
    }

    let mut broker = Broker::new();

    // Il broker inoltra verso `net`: senza il servizio non ha senso esistere
    // (fail-closed, mai un broker che finge di funzionare).
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::Net) {
            if c > 0 {
                broker.net_chan = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if broker.net_chan == 0 {
        let _ = civis::print_string(b"[netbroker] servizio net non trovato\n");
        civis::exit(1);
    }

    civis::signal_ready(1);
    civis::println!(
        "[netbroker] broker attivo, inoltro su net (canale {})",
        broker.net_chan
    );

    // Loop di servizio a **eventi** (E2, ADR-0084): si serve tutto quello che
    // c'e', poi si dorme finche' non arriva una richiesta di un cliente o un
    // hint di rete (`NET_EP_EVENT`, che copre anche `siit`). Niente tick: il
    // broker non ha lavoro periodico (i rate limit sono a finestra, non a
    // campione).
    loop {
        let now = now_tick();

        // Senso rete → cliente: drena i ring RX degli endpoint a monte.
        for i in 0..MAX_CLIENTS {
            if broker.clients[i].state == ClientState::Bound {
                drain_net(&mut broker, i);
            }
        }

        while let Some(m) = civis::recv_poll() {
            // Hint di rete: il dato sta nel ring, l'ha gia' drenato sopra (o
            // lo drenera' il giro dopo). Non e' una richiesta: niente reply.
            if m.tag == civis::NET_EP_EVENT {
                continue;
            }
            handle_msg(&mut broker, m, now);
        }

        // Dati arrivati durante il servizio (o hint perso a coda piena): si
        // continua senza dormire.
        if net_pending(&broker) {
            continue;
        }

        // Attesa a eventi su `net` (e su `siit`: stesso canale di hint).
        // `Dead` = servizio a monte morto: il broker non finge di funzionare.
        match civis::net::wait(broker.net_chan, 0) {
            civis::net::Wait::Dead => {
                let _ = civis::print_string(b"[netbroker] servizio net morto\n");
                civis::exit(1);
            }
            _ => {}
        }
    }
}

/// `true` se un endpoint a monte ha slot nel ring RX da drenare (E2).
fn net_pending(broker: &Broker) -> bool {
    for i in 0..MAX_CLIENTS {
        let c = &broker.clients[i];
        if c.state != ClientState::Bound || c.net_ring.is_null() {
            continue;
        }
        let head = unsafe { read_volatile(c.net_ring.add(NET_EP_RX_RING_OFF) as *const u32) };
        let tail = unsafe {
            read_volatile(c.net_ring.add(NET_EP_RX_RING_OFF + 4) as *const u32)
        };
        if head != tail {
            return true;
        }
    }
    false
}

/// Drena il ring RX di `net` del cliente `idx` e inoltra nel ring broker.
fn drain_net(broker: &mut Broker, idx: usize) {
    let ring = broker.clients[idx].net_ring;
    if ring.is_null() {
        return;
    }
    let layout = broker.clients[idx].upstream.layout();
    let client_ring = broker.clients[idx].ring_base;
    let peer_chan = broker.clients[idx].peer_chan;
    let ep_id = broker.clients[idx].ep_id;
    let mut head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
    let tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
    let mut moved = 0u64;
    while head != tail {
        let slot = (head % NET_EP_RING_SLOTS as u32) as usize;
        let p = unsafe { ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
        let total = unsafe { read_volatile(p as *const u16) } as usize;
        // Un solo parse: lo slot si legge una volta e si ricopia nel ring del
        // cliente. Slot corrotto (`len` fuori range) = consumato e saltato:
        // mai un frame parziale, mai un loop (la testa avanza comunque).
        let pushed = if net_ep::check(layout, total, NET_EP_SLOT_SIZE).is_some() {
            let win = unsafe { core::slice::from_raw_parts(p, total) };
            net_ep::read(win, layout, total)
                .map(|s| rx_write(layout, client_ring, s.addr, s.port, s.local, s.event, s.payload))
                .unwrap_or(false)
        } else {
            false
        };
        unsafe {
            write_volatile(ring.add(NET_EP_RX_RING_OFF) as *mut u32, head.wrapping_add(1));
        }
        head = head.wrapping_add(1);
        if pushed {
            moved += 1;
        }
    }
    if moved > 0 {
        broker.clients[idx].rx_pkts = broker.clients[idx].rx_pkts.saturating_add(moved);
        if peer_chan != 0 {
            // Sveglia non bloccante: il client legge il ring, non il messaggio.
            let _ = civis::send_async(peer_chan, BROKER_TX, ep_id as u64, 0);
        }
    }
}

fn handle_msg(broker: &mut Broker, m: civis::IpcMsg, now: u64) {
    match m.tag {
        BROKER_OPEN => open(broker, m, now),
        BROKER_TX => tx(broker, m, now),
        BROKER_CLOSE => {
            let ep_id = m.w0 as u16;
            if let Some(i) = broker.by_ep(ep_id) {
                let c = broker.clients[i];
                audit(b"close", c.peer_chan, c.net_port);
                broker.free(i);
            }
            let _ = civis::reply(BROKER_CLOSE, 0, 0);
        }
        BROKER_STATS => {
            let ep_id = m.w0 as u16;
            match broker.by_ep(ep_id) {
                Some(i) => {
                    let c = broker.clients[i];
                    let _ = civis::reply(BROKER_STATS, c.rx_pkts, c.tx_pkts);
                }
                None => {
                    let _ = civis::reply(BROKER_STATS, !0u64, 0);
                }
            }
        }
        civis::EXIT_NOTIFY => {
            // Il cliente e' morto: i suoi endpoint (net compreso) vanno via.
            let dead = m.channel;
            for i in 0..MAX_CLIENTS {
                if broker.clients[i].state == ClientState::Bound
                    && broker.clients[i].peer_chan == dead
                {
                    broker.free(i);
                }
            }
        }
        _ => {}
    }
}

/// `BROKER_OPEN`: allowlist + rate limit, poi un endpoint `net` per il cliente.
fn open(broker: &mut Broker, m: civis::IpcMsg, now: u64) {
    let proto_no = (m.w0 >> 16) as u16;
    let local_port = (m.w0 & 0xffff) as u16;
    let flags = m.w1;

    // Allowlist: una porta locale esplicita deve essere nella lista. Le
    // effimere (client uscente) le assegna `net` e sono sempre libere.
    let ephemeral = local_port == 0 || flags & NET_EP_FLAG_EPHEMERAL != 0;
    if !ephemeral && !proto::is_allowed_port(local_port) {
        audit(b"deny-port", m.channel, local_port);
        let _ = civis::reply(BROKER_OPEN, !0u64, 0);
        return;
    }
    // `NET_PROTO_IPV4` (255) sceglie l'uscita IPv4: il broker apre l'endpoint
    // presso `siit` (traduttore, N4) invece che presso `net`.
    let upstream = match proto_no {
        p if p == NET_PROTO_UDP as u16 || p == NET_PROTO_TCP as u16 => Upstream::Net,
        p if p == NET_PROTO_IPV4 as u16 => Upstream::Siit,
        _ => {
            let _ = civis::reply(BROKER_OPEN, !0u64, 0);
            return;
        }
    };
    // Identita' e classe del peer: senza identita' non c'e' una sessione da
    // attribuire (ne' rate limit ne' audit) e la richiesta e' negata.
    let (peer_id, class) = match civis::peer_info_class(m.channel) {
        Ok(v) => v,
        Err(_) => {
            let _ = civis::reply(BROKER_OPEN, !0u64, 0);
            return;
        }
    };
    // Destinazioni globali: privilegio del contenuto firmato (`System`), o
    // consenso dell'utente + grant admin per chi non lo e'. La decisione si
    // prende una volta per endpoint: un `CUSTOS_AUTHORIZE` per apertura, mai
    // nel data path.
    let pub_net = match class {
        civis::ImageClass::System | civis::ImageClass::ThirdParty => true,
        civis::ImageClass::Untrusted => broker.authorized(peer_id),
    };

    let idx = match broker.by_peer(m.channel) {
        Some(i) => i,
        None => match broker.alloc(m.channel, now) {
            Some(i) => i,
            None => {
                audit(b"deny-full", m.channel, local_port);
                let _ = civis::reply(BROKER_OPEN, !0u64, 0);
                return;
            }
        },
    };
    if !broker.rate_ok(idx, now) {
        audit(b"deny-rate", m.channel, local_port);
        let _ = civis::reply(BROKER_OPEN, !0u64, 0);
        return;
    }
    // Canale del servizio a monte (risolto una volta, poi in cache).
    if let Err(_) = broker.upstream_chan(upstream) {
        audit(b"deny-upstream", m.channel, local_port);
        let _ = civis::reply(BROKER_OPEN, !0u64, 0);
        return;
    }

    // Un cliente ha un endpoint alla volta: una seconda OPEN sullo stesso
    // canale riusa lo slot (chiude il precedente).
    if broker.clients[idx].ring_id != 0 {
        broker.free(idx);
        match broker.alloc(m.channel, now) {
            Some(i) => {
                broker.clients[i].peer_chan = m.channel;
                broker.clients[i].pub_net = pub_net;
                broker.clients[i].upstream = upstream;
                open_ring(broker, i, proto_no, local_port, flags, now)
            }
            None => {
                let _ = civis::reply(BROKER_OPEN, !0u64, 0);
            }
        }
        return;
    }
    broker.clients[idx].pub_net = pub_net;
    broker.clients[idx].upstream = upstream;
    open_ring(broker, idx, proto_no, local_port, flags, now);
}

/// Crea il ring broker e l'endpoint `net` dietro, poi risponde al cliente.
fn open_ring(broker: &mut Broker, idx: usize, proto_no: u16, local_port: u16, flags: u64, _now: u64) {
    let ring_id = match civis::shm_create(RING_LEN) {
        Ok(id) => id,
        Err(_) => {
            broker.free(idx);
            let _ = civis::reply(BROKER_OPEN, !0u64, 0);
            return;
        }
    };
    let Ok(base) = civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) else {
        broker.free(idx);
        let _ = civis::reply(BROKER_OPEN, !0u64, 0);
        return;
    };
    let ring = base as *mut u8;
    unsafe {
        write_volatile(ring.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }
    broker.clients[idx].ring_id = ring_id;
    broker.clients[idx].ring_base = ring;

    // L'endpoint vero, presso `net`. La classe del peer qui e' `System` (il
    // broker e' firmato `sys`), quindi il gate di classe di `net` passa: e' il
    // punto della catena in cui il privilegio vive nel broker, non nell'app.
    let up = broker.clients[idx].upstream;
    let chan = match broker.upstream_chan(up) {
        Ok(c) => c,
        Err(()) => {
            broker.free(idx);
            let _ = civis::reply(BROKER_OPEN, !0u64, 0);
            return;
        }
    };
    let r = match civis::send(chan, up.open_tag(), ((proto_no as u64) << 16) | local_port as u64, flags) {
        Ok(r) => r,
        Err(_) => {
            broker.free(idx);
            let _ = civis::reply(BROKER_OPEN, !0u64, 0);
            return;
        }
    };
    if r.w0 == !0u64 {
        audit(b"deny-net", broker.clients[idx].peer_chan, local_port);
        broker.free(idx);
        let _ = civis::reply(BROKER_OPEN, !0u64, 0);
        return;
    }
    let net_ep_id = r.w0 as u16;
    let net_ring_id = (r.w1 & 0xffff_ffff) as u32;
    let net_port = (r.w1 >> 32) as u16;
    let Ok(net_base) = civis::shm_map(net_ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) else {
        broker.free(idx);
        let _ = civis::reply(BROKER_OPEN, !0u64, 0);
        return;
    };
    broker.clients[idx].net_ep_id = net_ep_id;
    broker.clients[idx].net_ring = net_base as *mut u8;
    broker.clients[idx].net_port = net_port;

    let ep_id = broker.clients[idx].ep_id;
    audit(b"open", broker.clients[idx].peer_chan, net_port);
    let _ = civis::reply(BROKER_OPEN, ep_id as u64, (ring_id as u64) | ((net_port as u64) << 32));
}

/// `BROKER_TX`: inoltra lo slot del cliente nel ring TX di `net`.
fn tx(broker: &mut Broker, m: civis::IpcMsg, now: u64) {
    let ep_id = m.w0 as u16;
    let slot_idx = m.w1 as usize;
    let Some(idx) = broker.by_ep(ep_id) else {
        let _ = civis::reply(BROKER_TX, !0u64, 0);
        return;
    };
    if !broker.rate_ok(idx, now) {
        let _ = civis::reply(BROKER_TX, !0u64, 0);
        return;
    }
    let c = broker.clients[idx];
    if c.net_ep_id == 0 || c.net_ring.is_null() {
        let _ = civis::reply(BROKER_TX, !0u64, 0);
        return;
    }

    // Buffer sull'heap del servizio: 2 KiB sullo stack da 16 KiB sono troppi
    // insieme al resto del loop di inoltro.
    let layout = c.upstream.layout();
    let mut buf = alloc::vec![0u8; NET_EP_SLOT_SIZE];
    let Some((len, dst, dst_port, local, event)) = tx_read(layout, c.ring_base, slot_idx, &mut buf) else {
        let _ = civis::reply(BROKER_TX, !0u64, 0);
        return;
    };

    // Destinazione: loopback e link-local sono la rete locale del guest
    // (nessun instradamento), tutto il resto esce. Per chi non ha una
    // immagine privilegiata serve il consenso dell'utente + il grant admin,
    // deciso una volta all'apertura (`pub_net`).
    // L'upstream IPv4 (`siit`) porta solo i 4 byte dell'IPv4 nel campo
    // indirizzo: la forma dell'IPv6 SIIT completa esiste solo lato client, qui
    // la ricostruiamo da `addr_len`.
    let dst_local = if layout.addr_len() == 4 {
        v4_is_local(&dst[12..16])
    } else {
        dst_is_local(&dst)
    };
    if !c.pub_net && !dst_local {
        audit(b"deny-dst", c.peer_chan, dst_port);
        let _ = civis::reply(BROKER_TX, !0u64, 0);
        return;
    }

    // Inoltro: lo slot 0 del ring TX di `net` (un TX per endpoint alla volta,
    // il client e' seriale su BROKER_TX per costruzione).
    if !net_tx_write(
        layout,
        c.net_ring,
        0,
        &dst[..layout.addr_len()],
        dst_port,
        if local == 0 { c.net_port } else { local },
        event,
        &buf[..len],
    ) {
        // Ring dell'upstream non scrivibile (stato incoerente): il cliente
        // ritenta, lo slot suo non e' stato consumato.
        let _ = civis::reply(BROKER_TX, !0u64, 0);
        return;
    }
    let chan = match broker.upstream_chan(c.upstream) {
        Ok(ch) => ch,
        Err(()) => {
            let _ = civis::reply(BROKER_TX, !0u64, 0);
            return;
        }
    };
    match civis::send(chan, c.upstream.tx_tag(), c.net_ep_id as u64, 0) {
        Ok(r) if r.w0 == 0 => {
            // Preso in carico: il cliente puo' avanzare il cursore.
            consume_client_tx(c.ring_base);
            let cl = &mut broker.clients[idx];
            cl.tx_pkts = cl.tx_pkts.saturating_add(1);
            if !cl.audit_tx {
                cl.audit_tx = true;
                audit(b"tx", c.peer_chan, dst_port);
            }
            let _ = civis::reply(BROKER_TX, 0, 0);
        }
        // Backpressure o rifiuto dell'upstream: lo slot resta al cliente, che
        // ritenta lo stesso payload. Nessun drop silenzioso.
        Ok(_) => {
            let _ = civis::reply(BROKER_TX, !0u64, 0);
        }
        Err(_) => {
            let _ = civis::reply(BROKER_TX, !0u64, 0);
        }
    }
}

/// `true` se `addr` e' raggiungibile senza uscire verso Internet.
///
/// Sono "locali": il loopback, il link-local (fe80::/10), il multicast
/// (ff00::/8, NDP/MLD) e — per un indirizzo SIIT (`64:ff9b::/96`) — un IPv4 in
/// una gamma privata/di laboratorio (127/8, 10/8, 172.16/12, 192.168/16,
/// 169.254/16). Tutto il resto e' "rete globale": richiede consenso + grant
/// (`CAP_NET_USE`), quindi passa solo per le immagini fidate o per un utente
/// che ha acconsentito. La scelta e' deliberata: in un laboratorio le gammate
/// private sono la LAN, la rete pubblica e' la cosa da chiedere in permesso.
fn dst_is_local(addr: &[u8; 16]) -> bool {
    let loopback = addr[..15].iter().all(|&b| b == 0) && addr[15] == 1;
    let link_local = addr[0] == 0xfe && (addr[1] & 0xc0) == 0x80;
    let multicast = addr[0] == 0xff;
    if loopback || link_local || multicast {
        return true;
    }
    // Prefisso SIIT (RFC 6052, 64:ff9b::/96): conta l'IPv4 in coda.
    if addr[..6] == [0x64, 0xff, 0x9b, 0, 0, 0] && addr[6..12].iter().all(|&b| b == 0) {
        return v4_is_local(&addr[12..16]);
    }
    false
}

/// `true` se l'IPv4 (4 byte) e' in una gamma privata/di laboratorio.
fn v4_is_local(v4: &[u8]) -> bool {
    matches!(v4, [127, ..] | [10, ..] | [169, 254, ..] | [192, 168, ..])
        || (v4[0] == 172 && (v4[1] & 0xf0) == 16)
}

/// Avanza la tail del ring TX del cliente (slot consumato).
fn consume_client_tx(ring: *mut u8) {
    let head = unsafe { read_volatile(ring.add(NET_EP_TX_RING_OFF) as *const u32) };
    unsafe {
        write_volatile(ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32, head.wrapping_add(1));
    }
}

/// Audit: una riga nel bucket `netbroker` di Vestigia (best-effort: un log
/// che fallisce non deve mai fermare il traffico). Il soggetto e' il **canale**
/// del peer: `apex_info` sull'identita' lo fa il broker quando serve.
fn audit(op: &[u8], peer_chan: u64, port: u16) {
    let who = civis::peer_info(peer_chan).map(|id| alloc::format!("{:016x}{:016x}", id.hi, id.lo));
    let who = who.unwrap_or_else(|_| alloc::string::String::from("?"));
    let msg = alloc::format!(
        "{} peer={} port={}",
        core::str::from_utf8(op).unwrap_or("?"),
        who,
        port
    );
    let _ = civis::vestigia::log(b"netbroker", msg.as_bytes());
}

fn now_tick() -> u64 {
    let t = civis::get_ticks();
    if t < 0 { 0 } else { t as u64 }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    match info.location() {
        Some(l) => civis::println!("[netbroker] panic at {}:{}", l.file(), l.line()),
        None => {
            let _ = civis::print_string(b"[netbroker] panic\n");
        }
    }
    civis::exit(1)
}
