//! netecho — server di echo UDP/TCP nativo (N2+N3, ADR-0069).
//!
//! Apre un endpoint UDP sulla porta 7 e un endpoint TCP LISTEN sulla porta 8
//! presso il servizio `net`, polla entrambi i ring RX e rispecchia i
//! datagrammi/segmenti al mittente. Parte solo con `net=1`.

#![no_std]
#![no_main]

use civis;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use net::{Ipv6Addr, tcp};

// ── Costanti endpoint (da civis) ─────────────────────────────────────
use civis::{
    NET_EP_CLOSE, NET_EP_OPEN, NET_EP_RX_RING_OFF, NET_EP_RING_HDR, NET_EP_RING_SLOTS,
    NET_EP_SLOT_SIZE, NET_EP_TX, NET_EP_TX_RING_OFF, NET_ECHO_PORT, NET_PROTO_TCP, NET_PROTO_UDP,
};

static FUTEX: u32 = 0;

/// Porta di ascolto UDP (echo = 7, RFC 862).
const ECHO_PORT: u16 = 7;
/// Porta di ascolto TCP (secondo endpoint per N3).
const TCP_PORT: u16 = 8;

/// Flag TCP.
const FLAG_SYN: u8 = 0x02;
const FLAG_ACK: u8 = 0x10;

/// Metadata per slot: [len:2][addr:16][sport:2][dport:2][seq:4][ack:4][flags:1][payload].
const SLOT_META: usize = 31;

/// Stato di un endpoint echo.
struct EchoEp {
    ep_id: u16,
    ring_id: u32,
    ring_base: *mut u8,
    rx_head: u32,
    tx_tail: u32,
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

    // Apre endpoint UDP sulla porta 7.
    let mut udp = open_ep(net_chan, NET_PROTO_UDP as u64, ECHO_PORT, 0);
    // Apre endpoint TCP LISTEN sulla porta 8.
    let mut tcp = open_ep(net_chan, NET_PROTO_TCP as u64, TCP_PORT, civis::NET_EP_FLAG_LISTEN as u64);

    civis::println!(
        "[netecho] endpoint UDP aperto id={} porta={} ring={}",
        udp.ep_id, ECHO_PORT, udp.ring_id
    );
    civis::println!(
        "[netecho] endpoint TCP aperto id={} porta={} ring={}",
        tcp.ep_id, TCP_PORT, tcp.ring_id
    );

    loop {
        poll_ep(net_chan, &mut udp, false);
        poll_ep(net_chan, &mut tcp, true);
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
}

/// Apre un endpoint presso il servizio net.
fn open_ep(net_chan: u64, proto: u64, port: u16, flags: u64) -> EchoEp {
    let r = match civis::send(
        net_chan,
        NET_EP_OPEN,
        proto << 16 | port as u64,
        flags,
    ) {
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

    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    EchoEp {
        ep_id,
        ring_id,
        ring_base,
        rx_head: 0,
        tx_tail: 0,
    }
}

/// Polla un endpoint echo e gestisce i datagrammi/segmenti ricevuti.
/// `is_tcp`: true se l'endpoint è TCP (gestisce handshake SYN).
fn poll_ep(net_chan: u64, ep: &mut EchoEp, is_tcp: bool) {
    let ring = ep.ring_base;

    loop {
        let rx_head = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF) as *const u32) };
        let rx_tail = unsafe { read_volatile(ring.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        if rx_head == rx_tail {
            break;
        }
        let slot = (rx_head % NET_EP_RING_SLOTS as u32) as usize;
        let p = unsafe {
            ring.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
        };
        let total = unsafe { read_volatile(p as *const u16) } as usize;
        if total < SLOT_META || total > NET_EP_SLOT_SIZE {
            // Slot corrotto: salta.
            unsafe {
                write_volatile(
                    ring.add(NET_EP_RX_RING_OFF) as *mut u32,
                    rx_head.wrapping_add(1),
                )
            };
            ep.rx_head = rx_head.wrapping_add(1);
            continue;
        }
        let payload_len = total - SLOT_META;
        let mut src_addr = [0u8; 16];
        unsafe {
            core::ptr::copy_nonoverlapping(p.add(2), src_addr.as_mut_ptr(), 16);
        }
        let src_port = unsafe { read_volatile(p.add(18) as *const u16) };
        let dst_port = unsafe { read_volatile(p.add(20) as *const u16) };

        // Avanza head del RX.
        unsafe {
            write_volatile(
                ring.add(NET_EP_RX_RING_OFF) as *mut u32,
                rx_head.wrapping_add(1),
            )
        };
        ep.rx_head = rx_head.wrapping_add(1);

        let mut payload_buf = [0u8; NET_EP_SLOT_SIZE - SLOT_META];
        unsafe {
            core::ptr::copy_nonoverlapping(p.add(31), payload_buf.as_mut_ptr(), payload_len);
        }

        if is_tcp {
            // TCP: prova a parsificare come segmento.
            if let Some((tcp_hdr, _)) = tcp::parse(&payload_buf[..payload_len]) {
                if tcp_hdr.flags & FLAG_SYN != 0 {
                    // Handshake: rispondi con SYN+ACK.
                    let my_seq: u32 = 2000;
                    let their_seq = tcp_hdr.seq;
                    let mut buf = [0u8; NET_EP_SLOT_SIZE - SLOT_META];
                    let seg_len = tcp::write_checked(
                        &mut buf,
                        TCP_PORT,
                        src_port,
                        my_seq,
                        their_seq.wrapping_add(1),
                        FLAG_SYN | FLAG_ACK,
                        65535,
                        &Ipv6Addr::UNSPECIFIED,
                        &Ipv6Addr::from_bytes(src_addr),
                        &[],
                    );
                    write_tx_slot(ep, net_chan, &src_addr, src_port, my_seq, their_seq.wrapping_add(1), FLAG_SYN | FLAG_ACK, &buf[..seg_len]);
                    continue;
                }
            }
            // TCP dati non-SYN: rispecchia payload.
            write_tx_slot(ep, net_chan, &src_addr, src_port, 0, 0, 0, &payload_buf[..payload_len]);
        } else {
            // UDP: rispecchia payload.
            write_tx_slot(ep, net_chan, &src_addr, src_port, 0, 0, 0, &payload_buf[..payload_len]);
        }
    }
}

/// Scrive uno slot TX e notifica net di trasmettere.
fn write_tx_slot(
    ep: &EchoEp,
    net_chan: u64,
    dst_addr: &[u8; 16],
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
) {
    let total = SLOT_META + payload.len();
    if total > NET_EP_SLOT_SIZE {
        return;
    }
    let tx_head = unsafe { read_volatile(ep.ring_base.add(NET_EP_TX_RING_OFF) as *const u32) };
    let tx_tail_cur =
        unsafe { read_volatile(ep.ring_base.add(NET_EP_TX_RING_OFF + 4) as *const u32) };
    if tx_tail_cur.wrapping_sub(tx_head) >= NET_EP_RING_SLOTS as u32 {
        return; // ring pieno
    }
    let tx_slot = (tx_tail_cur % NET_EP_RING_SLOTS as u32) as usize;
    let tp = unsafe {
        ep.ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + tx_slot * NET_EP_SLOT_SIZE)
    };
    unsafe {
        write_volatile(tp as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(dst_addr.as_ptr(), tp.add(2), 16);
        write_volatile(tp.add(18) as *mut u16, dst_port);
        write_volatile(tp.add(20) as *mut u16, 0); // src_port (sarà sovrascritto dal servizio)
        write_volatile(tp.add(22) as *mut u32, seq);
        write_volatile(tp.add(26) as *mut u32, ack);
        write_volatile(tp.add(30) as *mut u8, flags);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), tp.add(31), payload.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ep.ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            tx_tail_cur.wrapping_add(1),
        )
    };

    civis::println!(
        "[netecho] TX slot={} seq={} ack={} flags={:02x}, invio NET_EP_TX",
        tx_slot, seq, ack, flags
    );
    let _ = civis::send(net_chan, NET_EP_TX, ep.ep_id as u64, tx_slot as u64);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[netecho] panic\n");
    civis::exit(1)
}
