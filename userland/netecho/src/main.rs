//! netecho — server di echo UDP/TCP nativo (N2+N3+N6.1c, ADR-0069).
//!
//! Apre un endpoint UDP sulla porta 7 e un endpoint TCP LISTEN sulla porta 8
//! presso il servizio `net`. Da N6.1c l'handshake TCP lo fa il **core**
//! (`libs/net`): il listener riceve `EV_ACCEPT` con `[ep_id:2][ring_id:4]`
//! del figlio, ne adotta il ring e rispecchia i dati; niente SYN+ACK
//! applicativo. Parte solo con `net=1`.
//!
//! Dal N7.0 il protocollo degli slot e le aperture sono in `net-ep` /
//! `civis::net` (ADR-0074): qui resta solo la politica dell'echo.

#![no_std]
#![no_main]

use civis;
use civis::net::{Endpoint, Event, Flow, Proto, Recv, PAYLOAD_MAX};

static FUTEX: u32 = 0;

/// Porta di ascolto UDP (echo = 7, RFC 862).
const ECHO_PORT: u16 = 7;
/// Porta di ascolto TCP.
const TCP_PORT: u16 = 8;
/// Connessioni servite in parallelo (deve stare dentro il backlog del core).
const MAX_CHILDREN: usize = 4;

/// Che cosa fare degli eventi di un endpoint.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Udp,
    /// TCP in ascolto: gli `Accept` diventano figli.
    Listener,
    /// Connessione accettata: echo e chiusura, mai `Accept`.
    Child,
}

/// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let _ = civis::print_string(b"[netecho] avvio\n");

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
        let _ = civis::print_string(b"[netecho] servizio net non trovato\n");
        civis::exit(1);
    }

    let mut udp = match Endpoint::open(net_chan, Proto::Udp, Flow::Bind(ECHO_PORT)) {
        Ok(ep) => ep,
        Err(_) => {
            let _ = civis::print_string(b"[netecho] endpoint UDP rifiutato\n");
            civis::exit(1);
        }
    };
    let mut tcp = match Endpoint::open(net_chan, Proto::Tcp, Flow::Listen(TCP_PORT)) {
        Ok(ep) => ep,
        Err(_) => {
            let _ = civis::print_string(b"[netecho] endpoint TCP rifiutato\n");
            civis::exit(1);
        }
    };
    civis::println!("[netecho] endpoint UDP aperto id={} porta={}", udp.id(), ECHO_PORT);
    civis::println!("[netecho] endpoint TCP aperto id={} porta={}", tcp.id(), TCP_PORT);

    let mut children: [Option<Endpoint>; MAX_CHILDREN] = [None, None, None, None];
    let mut buf = [0u8; PAYLOAD_MAX];

    loop {
        poll(&mut udp, Role::Udp, &mut children, &mut buf);
        poll(&mut tcp, Role::Listener, &mut children, &mut buf);
        for i in 0..MAX_CHILDREN {
            // Il figlio esce dallo slot mentre lo si serve: `Role::Child` non
            // aggiunge figli, quindi nessuno puo' occupare il posto libero
            // prima del rientro.
            let Some(mut child) = children[i].take() else {
                continue;
            };
            poll(&mut child, Role::Child, &mut children, &mut buf);
            children[i] = Some(child);
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
}

/// Consuma gli slot ricevuti e rispecchia (echo) quello che arriva.
fn poll(
    ep: &mut Endpoint,
    role: Role,
    children: &mut [Option<Endpoint>; MAX_CHILDREN],
    buf: &mut [u8],
) {
    while let Some(r) = ep.recv(buf) {
        match role {
            Role::Udp => echo(ep, &r, buf),
            Role::Listener | Role::Child => match r.event {
                Event::Accept => {
                    if matches!(role, Role::Child) {
                        continue;
                    }
                    attach(ep, children, &r, buf);
                }
                Event::Data => echo(ep, &r, buf),
                // Il peer ha chiuso (o reset): chiude la nostra direzione con
                // uno slot TX vuoto marcato FIN (convenzione N6.1c).
                Event::Fin | Event::Rst => {
                    let _ = ep.send(r.addr, r.port, Event::Fin, &[]);
                }
                _ => {}
            },
        }
    }
}

/// Adotta il ring della connessione accettata (`[ep_id:2][ring_id:4]`).
fn attach(
    listener: &mut Endpoint,
    children: &mut [Option<Endpoint>; MAX_CHILDREN],
    r: &Recv,
    buf: &[u8],
) {
    if r.len < 6 || r.truncated {
        return;
    }
    let id = u16::from_le_bytes([buf[0], buf[1]]);
    let ring_id = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
    let Some(slot) = children.iter().position(|c| c.is_none()) else {
        return; // niente posto: la connessione resta al core
    };
    if let Ok(ep) = Endpoint::attach(listener.chan(), id, ring_id, listener.local_port()) {
        civis::println!("[netecho] connessione accettata ep={}", ep.id());
        children[slot] = Some(ep);
    }
}

/// Rimanda indietro il payload ricevuto.
///
/// A ring pieno o servizio morto l'echo rinuncia al datagramma: e' un echo,
/// non un trasporto affidabile (il client ritenta o scade).
fn echo(ep: &mut Endpoint, r: &Recv, buf: &[u8]) {
    if !r.truncated {
        let _ = ep.send(r.addr, r.port, Event::Data, &buf[..r.len]);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[netecho] panic\n");
    civis::exit(1)
}
