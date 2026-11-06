//! net — servizio di rete nativo (ADR-0069).
//!
//! Registra `Service::Net`, si attacca al driver `netdev` (`NET_ATTACH`),
//! drena il ring RX, passa i frame al core `net` e scrive i frame di uscita
//! nel ring TX. Per N1 non espone ancora endpoint ai client: fa SLAAC/NDP e
//! un ping6 di prova verso il router.

#![no_std]
#![no_main]

use civis;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use net::{Config, Event, FrameSink, MacAddr, Net};

static FUTEX: u32 = 0;

/// Scrive i frame nel ring TX condiviso (producer).
struct Sink {
    ring: *mut u8,
}

impl FrameSink for Sink {
    fn send(&mut self, frame: &[u8]) {
        if frame.len() > civis::NET_SLOT_SIZE - 2 {
            return;
        }
        let head = unsafe { read_volatile(self.ring.add(civis::NET_TX_RING_OFF) as *const u32) };
        let tail =
            unsafe { read_volatile(self.ring.add(civis::NET_TX_RING_OFF + 4) as *const u32) };
        if tail.wrapping_sub(head) >= civis::NET_RING_SLOTS as u32 {
            return;
        }
        let slot = (tail % civis::NET_RING_SLOTS as u32) as usize;
        let p = unsafe {
            self.ring.add(
                civis::NET_TX_RING_OFF + civis::NET_RING_HDR + slot * civis::NET_SLOT_SIZE,
            )
        };
        unsafe {
            write_volatile(p as *mut u16, frame.len() as u16);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), p.add(2), frame.len());
        }
        fence(Ordering::Release);
        unsafe {
            write_volatile(
                self.ring.add(civis::NET_TX_RING_OFF + 4) as *mut u32,
                tail.wrapping_add(1),
            )
        };
    }
}

fn log_event(e: Event) {
    match e {
        Event::LinkUp => {
            let _ = civis::print_string(b"[net] link up\n");
        }
        Event::Ra { router, prefix } => match prefix {
            Some(p) => civis::println!(
                "[net] RA from {} prefix={:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}::/64",
                router,
                p[0],
                p[1],
                p[2],
                p[3],
                p[4],
                p[5],
                p[6],
                p[7]
            ),
            None => civis::println!("[net] RA from {} (senza prefix)", router),
        },
        Event::Address(a) => civis::println!("[net] address {}", a),
        Event::EchoReply { from, seq } => {
            civis::println!("[net] echo reply from {} seq={}", from, seq)
        }
        Event::EchoTimeout => {
            let _ = civis::print_string(b"[net] echo timeout\n");
        }
        Event::Dropped(r) => civis::println!("[net] drop: {}", r),
    }
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    if civis::service_register(civis::Service::Net).is_ok() {
        let _ = civis::print_string(b"[net] registered as service Net\n");
    }

    // Risolve il driver (init lo spawana prima del servizio).
    let mut driver = 0u64;
    for _ in 0..100 {
        if let Ok(c) = civis::service_lookup(civis::Service::NetDev) {
            if c > 0 {
                driver = c as u64;
                break;
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    if driver == 0 {
        let _ = civis::print_string(b"[net] netdev non trovato\n");
        civis::exit(1);
    }

    let r = match civis::send(driver, civis::NET_ATTACH, 1, 0) {
        Ok(r) => r,
        Err(_) => {
            let _ = civis::print_string(b"[net] attach fallito\n");
            civis::exit(1);
        }
    };
    if r.w0 == !0u64 {
        let _ = civis::print_string(b"[net] attach rifiutato\n");
        civis::exit(1);
    }
    let mac = MacAddr::from_u64(r.w0);
    let ring_id = r.w1 as u32;
    let ring_base = match civis::shm_map(
        ring_id,
        0,
        civis::PROT_READ | civis::PROT_WRITE,
    ) {
        Ok(b) => b,
        Err(_) => {
            let _ = civis::print_string(b"[net] shm map fallita\n");
            civis::exit(1);
        }
    };
    let ring = ring_base as *mut u8;
    let link = match civis::send(driver, civis::NET_INFO, 0, 0) {
        Ok(r) => r.w0 != 0,
        Err(_) => false,
    };

    let mut net = Net::new(Config {
        mac,
        ra_accept: true,
        ra_router: None,
        ping: true,
        ping_target: None,
        seed: civis::rdtsc(),
    });
    let mut sink = Sink { ring };
    let mut events = |e: Event| log_event(e);

    let now = civis::get_ticks() as u64;
    if link {
        net.link_up(now, &mut sink, &mut events);
    }
    civis::signal_ready(1);

    loop {
        let now = civis::get_ticks() as u64;
        // Client (N1: nessuno).
        while let Some(_m) = civis::recv_poll() {}
        // Poll del driver: drena il used ring nel ring condiviso.
        let _ = civis::send(driver, civis::NET_POLL, 0, 0);
        // Drena il ring RX.
        let head = unsafe { read_volatile(ring.add(civis::NET_RX_RING_OFF) as *const u32) };
        let tail =
            unsafe { read_volatile(ring.add(civis::NET_RX_RING_OFF + 4) as *const u32) };
        let mut h = head;
        while h != tail {
            let slot = (h % civis::NET_RING_SLOTS as u32) as usize;
            let p = unsafe {
                ring.add(
                    civis::NET_RX_RING_OFF + civis::NET_RING_HDR + slot * civis::NET_SLOT_SIZE,
                )
            };
            let len = unsafe { read_volatile(p as *const u16) } as usize;
            if len > 0 && len <= civis::NET_SLOT_SIZE - 2 {
                let frame = unsafe { core::slice::from_raw_parts(p.add(2), len) };
                net.on_frame(frame, now, &mut sink, &mut events);
            }
            h = h.wrapping_add(1);
        }
        unsafe {
            write_volatile(ring.add(civis::NET_RX_RING_OFF) as *mut u32, h);
        }
        // Timer (RS/echo retry).
        net.on_tick(now, &mut sink, &mut events);
        // TX: il driver consuma il ring.
        let _ = civis::send(driver, civis::NET_TX, 0, 0);
        // Dorme un tick.
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (now + 1) as u64,
        );
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[net] panic\n");
    civis::exit(1)
}
