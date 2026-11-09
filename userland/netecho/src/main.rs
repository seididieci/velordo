//! netecho — server di echo UDP nativo (N2, ADR-0069).
//!
//! Apre un endpoint UDP sulla porta 7 presso il servizio `net`, polla il
//! ring RX e rispedisce ogni datagramma al mittente. Parte solo con `net=1`.

#![no_std]
#![no_main]

use civis;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// ── Costanti endpoint (da civis) ─────────────────────────────────────
use civis::{
    NET_EP_OPEN, NET_EP_RX_RING_OFF, NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE,
    NET_EP_TX, NET_EP_TX_RING_OFF, NET_ECHO_PORT, NET_PROTO_UDP,
};

static FUTEX: u32 = 0;

/// Porta di ascolto (echo = 7, RFC 862).
const ECHO_PORT: u16 = 7;

/// Metadata per slot: [len:u16][addr:16][port:2][port:2][payload].
const SLOT_META: usize = 22;

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

    // Apre endpoint UDP sulla porta 7 (non ephemeral: porta fissa).
    let r = match civis::send(
        net_chan,
        NET_EP_OPEN,
        (NET_PROTO_UDP as u64) << 16 | ECHO_PORT as u64,
        0, // flags: nessuna ephemeral, porta specifica richiesta
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
    civis::println!(
        "[netecho] endpoint aperto id={} porta={} ring={}",
        ep_id,
        ECHO_PORT,
        ring_id
    );

    // Mappa lo shm dell'endpoint.
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            let _ = civis::print_string(b"[netecho] shm map fallito\n");
            civis::exit(1);
        }
    } as *mut u8;

    // Inizializza head/tail.
    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    let mut rx_head: u32 = 0;
    let mut tx_tail: u32 = 0;

    loop {
        // Legge head/tail del RX ring.
        rx_head = unsafe { read_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *const u32) };
        let rx_tail = unsafe { read_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *const u32) };

        while rx_head != rx_tail {
            let slot = (rx_head % NET_EP_RING_SLOTS as u32) as usize;
            let p = unsafe {
                ring_base.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
            };
            let total = unsafe { read_volatile(p as *const u16) } as usize;
            if total < SLOT_META || total > NET_EP_SLOT_SIZE {
                // Slot corrotto: salta.
                unsafe {
                    write_volatile(
                        ring_base.add(NET_EP_RX_RING_OFF) as *mut u32,
                        rx_head.wrapping_add(1),
                    )
                };
                rx_head = rx_head.wrapping_add(1);
                continue;
            }
            let payload_len = total - SLOT_META;
            let mut src_addr = [0u8; 16];
            unsafe {
                core::ptr::copy_nonoverlapping(p.add(2), src_addr.as_mut_ptr(), 16);
            }
            let src_port = unsafe { read_volatile(p.add(18) as *const u16) };
            civis::println!(
                "[netecho] RX datagramma totale={} payload_len={} da porta={}",
                total, payload_len, src_port
            );
            let mut payload = [0u8; NET_EP_SLOT_SIZE - SLOT_META];
            unsafe {
                core::ptr::copy_nonoverlapping(p.add(22), payload.as_mut_ptr(), payload_len);
            }
            // Avanza head del RX.
            unsafe {
                write_volatile(
                    ring_base.add(NET_EP_RX_RING_OFF) as *mut u32,
                    rx_head.wrapping_add(1),
                )
            };
            rx_head = rx_head.wrapping_add(1);

            // Echo: scrivi nello slot TX del proprio ring.
            // Formato TX: [len:u16][dst_addr:16][dst_port:2][payload].
            let tx_total = SLOT_META + payload_len;
            if tx_total <= NET_EP_SLOT_SIZE {
                let tx_head = unsafe {
                    read_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *const u32)
                };
                let tx_tail_cur = unsafe {
                    read_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *const u32)
                };
                if tx_tail_cur.wrapping_sub(tx_head) < NET_EP_RING_SLOTS as u32 {
                    let tx_slot = (tx_tail_cur % NET_EP_RING_SLOTS as u32) as usize;
                    let tp = unsafe {
                        ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR
                            + tx_slot * NET_EP_SLOT_SIZE)
                    };
                    unsafe {
                        write_volatile(tp as *mut u16, tx_total as u16);
                        core::ptr::copy_nonoverlapping(
                            src_addr.as_ptr(),
                            tp.add(2),
                            16,
                        );
                        write_volatile(tp.add(18) as *mut u16, src_port);
                        // dst_port per l'echo = src_port del datagramma ricevuto
                        write_volatile(tp.add(20) as *mut u16, src_port);
                        core::ptr::copy_nonoverlapping(
                            payload.as_ptr(),
                            tp.add(22),
                            payload_len,
                        );
                    }
                    fence(Ordering::Release);
                    unsafe {
                        write_volatile(
                            ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
                            tx_tail_cur.wrapping_add(1),
                        )
                    };
                    tx_tail = tx_tail_cur.wrapping_add(1);

                    // Notifica il servizio net di trasmettere.
                    civis::println!(
                        "[netecho] TX scritto slot={} payload_len={} dst_port={}, invio NET_EP_TX",
                        tx_slot, payload_len, src_port
                    );
                    let _ = civis::send(net_chan, NET_EP_TX, ep_id as u64, tx_slot as u64);
                }
            }
        }

        // Dorme brevemente prima del prossimo poll.
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[netecho] panic\n");
    civis::exit(1)
}
