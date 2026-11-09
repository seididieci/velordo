//! netudp — probe di rete UDP (N2, test fuori gate).
//!
//! Apre un endpoint UDP ephemeral presso il servizio `net`, invia un
//! datagramma a `::1:7` (netecho), valida l'eco ricevuta e segnala
//! TEST_DONE ad init. Eseguito solo con `net=1`.

#![no_std]
#![no_main]

use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// ── Costanti endpoint (da civis) ─────────────────────────────────────
use civis::{
    NET_EP_CLOSE, NET_EP_FLAG_EPHEMERAL, NET_EP_OPEN, NET_EP_RX_RING_OFF, NET_EP_RING_HDR,
    NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_STATS, NET_EP_TX, NET_EP_TX_RING_OFF,
    NET_ECHO_PORT, NET_PROTO_UDP,
};

static FUTEX: u32 = 0;

/// Indirizzo destinatario: `::1` (loopback).
const DST_ADDR: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
];
/// Porta destinatario: echo server.
const DST_PORT: u16 = 7;
/// Payload di test.
const TEST_PAYLOAD: &[u8] = b"velordo-netudp-probe";
/// Numero massimo di tentativi di ricezione.
const MAX_RETRIES: u32 = 2000;
/// Metadata slot ring (N2+N3): [len:2][addr:16][sport:2][dport:2][seq:4][ack:4][flags:1].
const SLOT_META: usize = 31;

/// Entry point.
civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    let mut all_ok = true;

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
        println!("[netudp] FAIL: servizio net non trovato");
        finish(false);
    }

    // Apre endpoint UDP ephemeral.
    let r = match civis::send(
        net_chan,
        NET_EP_OPEN,
        (NET_PROTO_UDP as u64) << 16, // porta 0 = ephemeral
        NET_EP_FLAG_EPHEMERAL as u64,
    ) {
        Ok(r) => r,
        Err(_) => {
            println!("[netudp] FAIL: open fallito (IPC)");
            finish(false);
        }
    };
    if r.w0 == !0u64 {
        println!("[netudp] FAIL: open rifiutato (policy)");
        finish(false);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let local_port = (r.w1 >> 32) as u16;
    println!(
        "[netudp] endpoint aperto id={} porta={} ring={}",
        ep_id, local_port, ring_id
    );

    // Mappa lo shm dell'endpoint.
    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            println!("[netudp] FAIL: shm map fallito");
            finish(false);
        }
    } as *mut u8;

    // Inizializza head/tail.
    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    // Invia datagramma a ::1:7.
    // Formato TX slot: [len:2][addr:16][sport:2][dport:2][seq:4][ack:4][flags:1][payload].
    let meta = SLOT_META; // 31
    let total = meta + TEST_PAYLOAD.len();
    if total > NET_EP_SLOT_SIZE {
        println!("[netudp] FAIL: payload troppo grande per lo slot");
        finish(false);
    }

    // Scrive nello slot TX 0.
    let p = unsafe {
        ring_base.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR)
    };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(DST_ADDR.as_ptr(), p.add(2), 16);
        write_volatile(p.add(18) as *mut u16, DST_PORT);
        write_volatile(p.add(20) as *mut u16, local_port);
        write_volatile(p.add(22) as *mut u32, 0); // seq (UDP: 0)
        write_volatile(p.add(26) as *mut u32, 0); // ack (UDP: 0)
        write_volatile(p.add(30) as *mut u8, 0);  // flags (UDP: 0)
        core::ptr::copy_nonoverlapping(TEST_PAYLOAD.as_ptr(), p.add(31), TEST_PAYLOAD.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            1, // tail = 1 (head è 0)
        )
    };

    // Notifica il servizio net di trasmettere (slot 0).
    let _ = civis::send(net_chan, NET_EP_TX, ep_id as u64, 0);
    println!("[netudp] datagramma inviato a ::1:7 (slot 0, len={})", TEST_PAYLOAD.len());

    // Aspetta l'eco sul RX ring.
    let mut rx_head: u32 = 0;
    let mut echo_ok = false;
    let mut retries = 0u32;
    for _ in 0..MAX_RETRIES {
        retries += 1;
        let rx_tail = unsafe { read_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *const u32) };
        if rx_head != rx_tail {
            let slot = (rx_head % NET_EP_RING_SLOTS as u32) as usize;
            let p = unsafe {
                ring_base.add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
            };
            let total = unsafe { read_volatile(p as *const u16) } as usize;
            let expected_total = SLOT_META + TEST_PAYLOAD.len();
            println!(
                "[netudp] RX slot totale={} payload_len_atteso={}",
                total, expected_total
            );
            if total >= expected_total {
                let mut src_addr = [0u8; 16];
                unsafe {
                    core::ptr::copy_nonoverlapping(p.add(2), src_addr.as_mut_ptr(), 16);
                }
                let src_port = unsafe { read_volatile(p.add(18) as *const u16) };
                let dst_port = unsafe { read_volatile(p.add(20) as *const u16) };
                let payload_start = 31;
                println!(
                    "[netudp] RX da porta={} dst_port={} payload_len={}",
                    src_port, dst_port, total - SLOT_META
                );
                if src_port == DST_PORT {
                    let mut match_ok = true;
                    for i in 0..TEST_PAYLOAD.len() {
                        let b = unsafe { read_volatile(p.add(payload_start + i)) };
                        if b != TEST_PAYLOAD[i] {
                            match_ok = false;
                            break;
                        }
                    }
                    if match_ok {
                        echo_ok = true;
                        println!("[netudp] payload corrispondente: echo valido");
                    } else {
                        println!("[netudp] payload NON corrispondente");
                    }
                } else {
                    println!("[netudp] src_port ({}) != DST_PORT ({}), ignoro", src_port, DST_PORT);
                }
            } else {
                println!("[netudp] slot troppo corto, salto");
            }
            // Avanza head.
            unsafe {
                write_volatile(
                    ring_base.add(NET_EP_RX_RING_OFF) as *mut u32,
                    rx_head.wrapping_add(1),
                )
            };
            rx_head = rx_head.wrapping_add(1);
            break;
        }
        // Dorme e riprova.
        if retries % 500 == 0 {
            println!("[netudp] in attesa echo... retries={}", retries);
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }

    println!("[netudp] fine attesa: retries={} echo_ok={}", retries, echo_ok);

    if echo_ok {
        println!("[netudp] echo da ::1:7 ricevuto e validato: PASS");
    } else {
        println!("[netudp] echo da ::1:7 non ricevuto o non valido: FAIL");
        all_ok = false;
    }

    // Chiude l'endpoint.
    let _ = civis::send(net_chan, NET_EP_CLOSE, ep_id as u64, 0);

    // Statistiche (opzionale).
    let _ = civis::send(net_chan, NET_EP_STATS, ep_id as u64, 0);

    finish(all_ok);
}

/// Termina il probe e segnala TEST_DONE ad init.
fn finish(ok: bool) -> ! {
    if ok {
        println!("[netudp] risultato: PASS");
    } else {
        println!("[netudp] risultato: FAIL");
    }
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, if ok { 1 } else { 0 }, 0);
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[netudp] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
