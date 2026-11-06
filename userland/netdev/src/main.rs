//! netdev — driver virtio-net (ADR-0069).
//!
//! Possiede il device, le virtqueue e il ring condiviso con il servizio
//! `net`. Non parla protocolli: copia i frame tra il device e il ring.
//! Il servizio `net` si attacca con `NET_ATTACH`; il driver mappa il ring
//! nella sua VA (`USER_NET_RING_VA`).

#![no_std]
#![no_main]

use civis;

mod virtio;

const ERR: u64 = !0u64;

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    if civis::service_register(civis::Service::NetDev).is_ok() {
        let _ = civis::print_string(b"[netdev] registered as service NetDev\n");
    }

    let mut dev = match virtio::init() {
        Ok(d) => Some(d),
        Err("assente") => {
            let _ = civis::print_string(b"[netdev] virtio-net assente\n");
            None
        }
        Err(cause) => {
            civis::println!("[netdev] init fallito: {}", cause);
            None
        }
    };

    civis::signal_ready(1);

    loop {
        match civis::recv() {
            Ok(msg) => match msg.tag {
                civis::NET_ATTACH => {
                    let (mac, ring_id) = match dev.as_ref() {
                        Some(d) => {
                            let m = d.mac();
                            (
                                u64::from_be_bytes([0, 0, m[0], m[1], m[2], m[3], m[4], m[5]]),
                                d.ring_id() as u64,
                            )
                        }
                        None => (0, 0),
                    };
                    if ring_id == 0 {
                        let _ = civis::reply(msg.tag, ERR, 0);
                        continue;
                    }
                    let _ = civis::reply(msg.tag, mac, ring_id);
                }
                civis::NET_POLL => {
                    let n = match dev.as_mut() {
                        Some(d) => d.poll_rx(),
                        None => 0,
                    };
                    let _ = civis::reply(msg.tag, n as u64, 0);
                }
                civis::NET_TX => {
                    let n = match dev.as_mut() {
                        Some(d) => d.tx(),
                        None => 0,
                    };
                    let _ = civis::reply(msg.tag, n as u64, 0);
                }
                civis::NET_INFO => {
                    let (link, mtu) = match dev.as_ref() {
                        Some(d) => (d.link_up() as u64, 1500),
                        None => (0, 0),
                    };
                    let _ = civis::reply(msg.tag, link, mtu);
                }
                civis::IRQ_NOTIFY_NET => {
                    if let Some(d) = dev.as_mut() {
                        d.handle_irq();
                        d.poll_rx();
                    }
                }
                civis::EXIT_NOTIFY => {}
                _ => {
                    if msg.req_id == 0 {
                        let _ = civis::reply(msg.tag, 0, 0);
                    }
                }
            },
            Err(_) => {}
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[netdev] panic\n");
    civis::exit(1)
}
