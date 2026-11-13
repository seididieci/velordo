//! dns-resolve — risolve un nome di dominio tramite il servizio DNS.
//!
//! Client nativo (meccanismo, niente POSIX) che parla al servizio `dns`
//! attraverso un endpoint UDP su ring condiviso (ADR-0069, slot IPv4 via
//! `net_ep::Layout::IPV4`). Output testuale su console.
//!
//! Uso: `dns-resolve <nome> [aaaa]`

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;

use civis;
use civis::println;
use net;
use net_ep;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";

/// Indirizzo IPv4 di destinazione (il servizio dns ascolta in locale).
const DEST_IPV4: [u8; 4] = [127, 0, 0, 1];

/// Evento slot ring DNS (dati).
const EV_DATA: u8 = 0;

/// Attesa massima di una risposta (tick da 10 ms).
const MAX_TICKS: u32 = 300;

static FUTEX: u32 = 0;

// ── Console ────────────────────────────────────────────────────────

static mut CONSOLE_FD: i64 = -1;

fn console_write(data: &[u8]) {
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    civis::flush();
}

fn console_write_str(s: &str) {
    console_write(s.as_bytes());
}

// ── Slot ring (IPv4 via net_ep) ────────────────────────────────────

/// Scrive una query nello slot TX `slot` del ring condiviso.
/// Ritorna `true` se lo slot e' stato scritto.
fn write_tx_slot(
    ring_base: *mut u8,
    slot: usize,
    addr: &[u8],
    port: u16,
    query: &[u8],
) -> bool {
    let layout = net_ep::Layout::IPV4;
    let total = layout.total(query.len());
    if total > civis::NET_EP_SLOT_SIZE {
        return false;
    }
    let p = unsafe {
        ring_base
            .add(civis::NET_EP_TX_RING_OFF + civis::NET_EP_RING_HDR + slot * civis::NET_EP_SLOT_SIZE)
    };
    let win = unsafe { core::slice::from_raw_parts_mut(p, total) };
    if net_ep::write(win, layout, addr, port, 0, EV_DATA, query).is_none() {
        return false;
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    unsafe {
        core::ptr::write_volatile(
            ring_base.add(civis::NET_EP_TX_RING_OFF + 4) as *mut u32,
            (slot + 1) as u32,
        );
    }
    true
}

/// Legge il prossimo slot RX disponibile, copiando il payload in `out`.
/// Ritorna `Some(len)` se un payload e' stato letto, `None` se il ring e' vuoto.
fn read_rx_slot(ring_base: *mut u8, rx_head: &mut u32, out: &mut [u8]) -> Option<usize> {
    let layout = net_ep::Layout::IPV4;
    let (head, tail) = {
        let h = unsafe {
            core::ptr::read_volatile(ring_base.add(civis::NET_EP_RX_RING_OFF) as *const u32)
        };
        let t = unsafe {
            core::ptr::read_volatile(ring_base.add(civis::NET_EP_RX_RING_OFF + 4) as *const u32)
        };
        (h, t)
    };
    if head == tail {
        return None;
    }
    let idx = (head % civis::NET_EP_RING_SLOTS as u32) as usize;
    let q = unsafe {
        ring_base.add(
            civis::NET_EP_RX_RING_OFF
                + civis::NET_EP_RING_HDR
                + idx * civis::NET_EP_SLOT_SIZE,
        )
    };
    let total = unsafe { core::ptr::read_volatile(q as *const u16) } as usize;
    // `len` fuori range = corruzione: lo slot si consuma e si salta.
    if net_ep::check(layout, total, civis::NET_EP_SLOT_SIZE).is_none() {
        unsafe {
            core::ptr::write_volatile(
                ring_base.add(civis::NET_EP_RX_RING_OFF) as *mut u32,
                head.wrapping_add(1),
            );
        }
        *rx_head = head.wrapping_add(1);
        return None;
    }
    let win = unsafe { core::slice::from_raw_parts(q, total) };
    let slot = match net_ep::read(win, layout, total) {
        Some(s) => s,
        None => return None,
    };
    let n = slot.payload.len().min(out.len());
    out[..n].copy_from_slice(&slot.payload[..n]);
    // Consuma lo slot solo dopo aver copiato il payload.
    unsafe {
        core::ptr::write_volatile(
            ring_base.add(civis::NET_EP_RX_RING_OFF) as *mut u32,
            head.wrapping_add(1),
        );
    }
    *rx_head = head.wrapping_add(1);
    Some(n)
}

// ── DNS query builder ─────────────────────────────────────────────

/// Costruisce una query DNS tipo A o AAAA per `name`. Ritorna la lunghezza.
fn build_query(out: &mut [u8], name: &[u8], qtype: u16) -> usize {
    // Header (12 byte).
    out[0..2].copy_from_slice(&0x1234u16.to_be_bytes());
    out[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // RD=1
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // qdcount=1
    out[6..12].fill(0);

    let mut off = 12usize;
    for label in name.split(|b| *b == b'.') {
        out[off] = label.len() as u8;
        off += 1;
        out[off..off + label.len()].copy_from_slice(label);
        off += label.len();
    }
    out[off] = 0;
    off += 1;
    out[off..off + 2].copy_from_slice(&qtype.to_be_bytes()); // type
    off += 2;
    out[off..off + 2].copy_from_slice(&net::CLASS_IN.to_be_bytes()); // class IN
    off + 2
}

// ── IP formatting ─────────────────────────────────────────────────

/// Format IPv4 indirizzo in "x.x.x.x".
fn format_ipv4(addr: &[u8; 4]) -> String {
    alloc::format!("{}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3])
}

/// Format IPv6 indirizzo: `Display` di `libs/net` (RFC 5952, testato).
fn format_ipv6(addr: &[u8; 16]) -> String {
    alloc::format!("{}", net::Ipv6Addr(*addr))
}

// ── Risoluzione DNS ───────────────────────────────────────────────

/// Risolve il servizio DNS per nome (con retry).
fn resolve_dns() -> Result<u64, &'static str> {
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(civis::Service::Dns) {
            if c > 0 {
                return Ok(c as u64);
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    Err("DNS non trovato")
}

/// Risolve un nome e stampa i risultati su console.
/// Ritorna il codice di uscita (0 = ok, 2 = timeout, 3 = parse error).
fn resolve_and_print(dns_chan: u64, ep_id: u16, ring_base: *mut u8, domain: &[u8], qtype: u16) -> i32 {
    let mut rx_head = 0u32;

    let mut query = [0u8; 512];
    let qlen = build_query(&mut query, domain, qtype);

    // Scrive lo slot TX e notifica il servizio.
    if !write_tx_slot(ring_base, 0, &DEST_IPV4, civis::DNS_PORT, &query[..qlen]) {
        console_write_str("[dns-resolve] ERRORE: impossibile scrivere lo slot TX\n");
        return 1;
    }
    if civis::ipc::send(dns_chan, civis::DNS_TX, ep_id as u64, 0).is_err() {
        console_write_str("[dns-resolve] ERRORE: invio query fallito\n");
        return 1;
    }

    // Attende risposta.
    let mut response = [0u8; civis::NET_EP_SLOT_SIZE as usize];
    let mut got = None;
    for _tick in 0..MAX_TICKS {
        if let Some(len) = read_rx_slot(ring_base, &mut rx_head, &mut response) {
            got = Some(len);
            break;
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }

    let payload = match got {
        Some(len) => &response[..len],
        None => {
            console_write_str("[dns-resolve] ERRORE: nessuna risposta ricevuta\n");
            return 2;
        }
    };

    let (ipv4_addrs, ipv6_addrs) = match net::parse_response(payload) {
        Some((v4, v6)) => (v4, v6),
        None => {
            console_write_str("[dns-resolve] ERRORE: parse risposta fallito\n");
            return 3;
        }
    };

    let query_type = if qtype == net::TYPE_AAAA { "AAAA" } else { "A" };
    let domain_str = core::str::from_utf8(domain).unwrap_or("?");

    if ipv4_addrs.is_empty() && ipv6_addrs.is_empty() {
        // Nessun record: distingue il motivo (rcode) invece di un generico
        // "nessun record trovato".
        let rcode = net::parse_header(payload).map(|h| h.flags & 0xF).unwrap_or(0);
        match rcode {
            3 => console_write_str(&alloc::format!(
                "[dns-resolve] {} : nome inesistente (NXDOMAIN)\n",
                domain_str
            )),
            2 => console_write_str(&alloc::format!(
                "[dns-resolve] {} : errore del server (SERVFAIL)\n",
                domain_str
            )),
            _ => console_write_str(&alloc::format!(
                "[dns-resolve] {} : nessun record {} trovato\n",
                domain_str, query_type
            )),
        }
        return 0;
    }

    for addr in &ipv4_addrs {
        console_write_str(&alloc::format!(
            "[dns-resolve] {}: {} ({})\n",
            domain_str,
            format_ipv4(addr),
            query_type
        ));
    }
    for addr in &ipv6_addrs {
        console_write_str(&alloc::format!(
            "[dns-resolve] {}: {} ({})\n",
            domain_str,
            format_ipv6(addr),
            query_type
        ));
    }
    0
}

/// Entry: apre console, parse argv, delega.
fn real_main(sp: u64) -> ! {
    let console_fd = match civis::open(CONSOLE_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[dns-resolve] ERRORE: impossibile aprire console");
            civis::exit(1);
        }
    };
    unsafe { CONSOLE_FD = console_fd; }

    // Parse argv.
    let args = match civis::args_from_stack(sp) {
        Some(a) => a,
        None => {
            console_write_str("[dns-resolve] ERRORE: argv non leggibile\n");
            civis::exit(1);
        }
    };

    if args.argc() < 2 {
        console_write_str("[dns-resolve] uso: dns-resolve <nome> [aaaa]\n");
        civis::exit(1);
    }

    let domain = match args.get(1) {
        Some(d) => d,
        None => {
            console_write_str("[dns-resolve] ERRORE: argv[1] non valido\n");
            civis::exit(1);
        }
    };

    let qtype = if args.argc() > 2 {
        match args.get(2) {
            Some(q) if q == b"aaaa" || q == b"AAAA" => net::TYPE_AAAA,
            _ => net::TYPE_A,
        }
    } else {
        net::TYPE_A
    };

    // Risolve servizio DNS.
    let dns_chan = match resolve_dns() {
        Ok(c) => c,
        Err(_) => {
            console_write_str("[dns-resolve] ERRORE: servizio DNS non disponibile\n");
            civis::exit(1);
        }
    };

    // Apre endpoint UDP.
    let r = match civis::ipc::send(
        dns_chan,
        civis::DNS_OPEN,
        (civis::NET_PROTO_UDP as u64) << 16 | civis::DNS_PORT as u64,
        civis::NET_EP_FLAG_EPHEMERAL as u64,
    ) {
        Ok(r) => r,
        Err(_) => {
            console_write_str("[dns-resolve] ERRORE: open endpoint fallito\n");
            civis::exit(1);
        }
    };
    if r.w0 == !0u64 {
        console_write_str("[dns-resolve] ERRORE: open rifiutato (policy)\n");
        civis::exit(1);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;

    // Mappa ring shm.
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            console_write_str("[dns-resolve] ERRORE: shm map fallito\n");
            let _ = civis::ipc::send(dns_chan, civis::DNS_CLOSE, ep_id as u64, 0);
            civis::exit(1);
        }
    } as *mut u8;

    // Inizializza head/tail.
    unsafe {
        core::ptr::write_volatile(ring_base.add(civis::NET_EP_RX_RING_OFF) as *mut u32, 0);
        core::ptr::write_volatile(ring_base.add(civis::NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        core::ptr::write_volatile(ring_base.add(civis::NET_EP_TX_RING_OFF) as *mut u32, 0);
        core::ptr::write_volatile(ring_base.add(civis::NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    // Esegue la risoluzione.
    let code = resolve_and_print(dns_chan, ep_id, ring_base, domain, qtype);

    // Chiude endpoint.
    let _ = civis::ipc::send(dns_chan, civis::DNS_CLOSE, ep_id as u64, 0);

    civis::exit(code as i64)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write_str("[dns-resolve] panic\n");
    civis::exit(1)
}
