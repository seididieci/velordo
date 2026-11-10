//! netecho — server di echo UDP/TCP nativo (N2+N3+N6.1c, ADR-0069).
//!
//! Apre un endpoint UDP sulla porta 7 e un endpoint TCP LISTEN sulla porta 8
//! presso il servizio `net`. Da N6.1c l'handshake TCP lo fa il **core**
//! (`libs/net`): il listener riceve `EV_ACCEPT` con `[ep_id:2][ring_id:4]`
//! del figlio, ne mappa il ring e rispecchia i dati; niente SYN+ACK
//! applicativo. Parte solo con `net=1`.

#![no_std]
#![no_main]

use civis;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// ── Costanti endpoint (da civis) ─────────────────────────────────────
use civis::{
    NET_EP_OPEN, NET_EP_RX_RING_OFF, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE,
    NET_EP_TX, NET_EP_TX_RING_OFF, NET_PROTO_TCP, NET_PROTO_UDP,
};

static FUTEX: u32 = 0;

/// Porta di ascolto UDP (echo = 7, RFC 862).
const ECHO_PORT: u16 = 7;
/// Porta di ascolto TCP.
const TCP_PORT: u16 = 8;
/// Connessioni servite in parallelo (deve stare dentro il backlog del core).
const MAX_CHILDREN: usize = 4;

/// Eventi trasportati nello slot ring (vedi `userland/net`).
const EV_DATA: u8 = 0;
const EV_SYNACK: u8 = 2;
const EV_FIN: u8 = 3;
const EV_RST: u8 = 4;
const EV_ACCEPT: u8 = 5;

/// Metadata per slot: [len:2][addr:16][sport:2][dport:2][event:1][payload].
const SLOT_META: usize = 23; // 2+16+2+2+1

/// Stato di un endpoint echo.
struct EchoEp {
    ep_id: u16,
    ring_base: *mut u8,
}

/// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let _ = civis::print_string(b"[netecho] avvio\n");

    // Risolve il servizio net.
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

    // Endpoint UDP sulla porta 7 e TCP LISTEN sulla porta 8.
    let udp = open_ep(net_chan, NET_PROTO_UDP as u64, ECHO_PORT, 0);
    let tcp = open_ep(
        net_chan,
        NET_PROTO_TCP as u64,
        TCP_PORT,
        civis::NET_EP_FLAG_LISTEN as u64,
    );
    civis::println!("[netecho] endpoint UDP aperto id={} porta={}", udp.ep_id, ECHO_PORT);
    civis::println!("[netecho] endpoint TCP aperto id={} porta={}", tcp.ep_id, TCP_PORT);

    let mut children: [Option<EchoEp>; MAX_CHILDREN] = [None, None, None, None];

    loop {
        poll_ep(net_chan, &udp, false, &mut children);
        poll_ep(net_chan, &tcp, true, &mut children);
        for i in 0..MAX_CHILDREN {
            if let Some(child) = children[i].as_ref().map(|c| EchoEp {
                ep_id: c.ep_id,
                ring_base: c.ring_base,
            }) {
                poll_ep(net_chan, &child, true, &mut children);
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
}

/// Apre un endpoint presso il servizio net e ne mappa il ring.
fn open_ep(net_chan: u64, proto: u64, port: u16, flags: u64) -> EchoEp {
    let r = match civis::send(net_chan, NET_EP_OPEN, proto << 16 | port as u64, flags) {
        Ok(r) => r,
        Err(_) => {
            let _ = civis::print_string(b"[netecho] open fallito (IPC)\n");
            civis::exit(1);
        }
    };
    if r.w0 == !0u64 {
        let _ = civis::print_string(b"[netecho] open rifiutato (policy)\n");
        civis::exit(1);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            let _ = civis::print_string(b"[netecho] shm map fallito\n");
            civis::exit(1);
        }
    } as *mut u8;
    init_ring(ring_base);
    EchoEp { ep_id, ring_base }
}

/// Azzera head/tail di entrambi i ring dell'endpoint.
fn init_ring(ring_base: *mut u8) {
    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }
}

/// Polla un endpoint echo e rispecchia datagrammi/segmenti ricevuti.
fn poll_ep(
    net_chan: u64,
    ep: &EchoEp,
    is_tcp: bool,
    children: &mut [Option<EchoEp>; MAX_CHILDREN],
) {
    let ring = ep.ring_base;
    let mut payload_buf = [0u8; NET_EP_SLOT_SIZE - SLOT_META];

    loop {
        let rx_head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
        let rx_tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        if rx_head == rx_tail {
            break;
        }
        let slot = (rx_head % NET_EP_RING_SLOTS as u32) as usize;
        let p = unsafe { ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
        let total = unsafe { read_volatile(p as *const u16) } as usize;
        // Consuma lo slot comunque (anche se corrotto).
        unsafe {
            write_volatile(
                ring.add(NET_EP_RX_RING_OFF) as *mut u32,
                rx_head.wrapping_add(1),
            )
        };
        if total < SLOT_META || total > NET_EP_SLOT_SIZE {
            continue;
        }
        let payload_len = total - SLOT_META;
        let mut src_addr = [0u8; 16];
        unsafe {
            core::ptr::copy_nonoverlapping(p.add(2), src_addr.as_mut_ptr(), 16);
            core::ptr::copy_nonoverlapping(p.add(SLOT_META), payload_buf.as_mut_ptr(), payload_len);
        }
        let src_port = unsafe { read_volatile(p.add(18) as *const u16) };
        let event = unsafe { read_volatile(p.add(22) as *const u8) };

        if !is_tcp {
            // UDP: rispecchia il datagramma.
            write_tx_slot(ep, net_chan, &src_addr, src_port, EV_DATA, &payload_buf[..payload_len]);
            continue;
        }

        match event {
            EV_ACCEPT => {
                // Nuova connessione: mappa il ring del figlio e servila.
                if payload_len < 6 {
                    continue;
                }
                let child_ep = u16::from_le_bytes([payload_buf[0], payload_buf[1]]);
                let ring_id = u32::from_le_bytes([
                    payload_buf[2],
                    payload_buf[3],
                    payload_buf[4],
                    payload_buf[5],
                ]);
                let Some(slot) = children.iter().position(|c| c.is_none()) else {
                    continue; // niente posto: la connessione resta al core
                };
                if let Ok(b) = civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
                    // NB: niente `init_ring` qui. Il ring del figlio è già
                    // inizializzato dal servizio, che può averci scritto dei
                    // byte prima che leggessimo l'EV_ACCEPT: azzerare
                    // head/tail butterebbe quei dati.
                    let base = b as *mut u8;
                    civis::println!("[netecho] connessione accettata ep={}", child_ep);
                    children[slot] = Some(EchoEp {
                        ep_id: child_ep,
                        ring_base: base,
                    });
                }
            }
            EV_DATA => {
                write_tx_slot(ep, net_chan, &src_addr, src_port, EV_DATA, &payload_buf[..payload_len]);
            }
            EV_FIN | EV_RST => {
                // Il peer ha chiuso (o reset): chiude la nostra direzione con
                // uno slot TX vuoto marcato FIN (convenzione N6.1c).
                write_tx_slot(ep, net_chan, &src_addr, src_port, EV_FIN, &[]);
            }
            EV_SYNACK => {}
            _ => {}
        }
    }
}

/// Scrive uno slot TX e notifica `net` di trasmettere.
fn write_tx_slot(
    ep: &EchoEp,
    net_chan: u64,
    dst_addr: &[u8; 16],
    dst_port: u16,
    event: u8,
    payload: &[u8],
) {
    let total = SLOT_META + payload.len();
    if total > NET_EP_SLOT_SIZE {
        return;
    }
    let ring = ep.ring_base;
    let tx_head = unsafe { read_volatile(ring.add(NET_EP_TX_RING_OFF) as *const u32) };
    let tx_tail_cur = unsafe { read_volatile(ring.add(NET_EP_TX_RING_OFF + 4) as *const u32) };
    if tx_tail_cur.wrapping_sub(tx_head) >= NET_EP_RING_SLOTS as u32 {
        return; // ring pieno
    }
    let tx_slot = (tx_tail_cur % NET_EP_RING_SLOTS as u32) as usize;
    let tp = unsafe { ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + tx_slot * NET_EP_SLOT_SIZE) };
    unsafe {
        write_volatile(tp as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(dst_addr.as_ptr(), tp.add(2), 16);
        write_volatile(tp.add(18) as *mut u16, dst_port);
        write_volatile(tp.add(20) as *mut u16, 0); // sorgente: la assegna il servizio
        write_volatile(tp.add(22) as *mut u8, event);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), tp.add(SLOT_META), payload.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            tx_tail_cur.wrapping_add(1),
        )
    };
    let _ = civis::send(net_chan, NET_EP_TX, ep.ep_id as u64, tx_slot as u64);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[netecho] panic\n");
    civis::exit(1)
}
