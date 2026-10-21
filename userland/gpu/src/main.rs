//! gpu — **driver** del display (D3, ADR-0060).
//!
//! Prima (L0b) questo servizio faceva due cose: driver virtio-gpu *e*
//! console (VGA testuale). D3 le separa:
//! - `gpu` (qui): enumera `virtio-gpu-pci`, programma BAR/MSI-X, e pubblica
//!   il device **`/dev/display`** (il protocollo in `term::disp`). Non sa
//!   cosa sono caratteri, celle o font: non disegna nulla.
//! - `console` (servizio nuovo): possiede i pixel, tiene il modello a celle
//!   e serve `/dev/console`.
//!
//! I pixel sono del **client**: il driver riceve l'indirizzo fisico del
//! buffer e lo programma sul device (risorsa 2D + scanout). Nessuna mappatura
//! cross-process, nessun buffer nel driver.

#![no_std]
#![no_main]

extern crate alloc;
use civis;

// ── Ring I/O (Fase 10.2) ─────────────────────────────────────────
// Le finestre CLI_* sono mappate da cardo (map_in) con i ring del client
// a ogni relay DEV (zero-copy); i ring propri del server non cambiano mai.

const REQ_RING_VA: u64 = civis::CLI_REQ_VA;
const RESP_RING_VA: u64 = civis::CLI_RESP_VA;
// Geometria ring (A1) + frame helpers (A2): single source in `civis`.
use civis::{req_frame_read, resp_frame_write};

// ── Tag IPC del device (DocsD: single source in `syscall-numbers`) ───
use civis::{DEV_CLOSE, DEV_OPEN, DEV_READ, DEV_WRITE};

/// Valore di errore IPC.
const ERR: u64 = !0u64;

mod virtio;
mod virtio_gpu;

// ── Entry point ──────────────────────────────────────────────────────

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    // 1. Registra il servizio: serve al grant MMIO (ADR-0057) e al routing
    //    degli interrupt (config change MSI-X).
    if civis::service_register(civis::Service::Gpu).is_ok() {
        let _ = civis::print_string(b"[gpu] registered as service Gpu\n");
    }

    // 2. Init del transport virtio (L0b): BAR, MSI-X, controlq. Se il
    //    device non c'e' il driver resta vivo e pubblica un display
    //    "VGA testuale": la console sceglie il fallback.
    let mut dev = match virtio::init() {
        Ok(d) => Some(d),
        Err("assente") => {
            let _ = civis::print_string(b"[gpu] virtio-gpu assente: display VGA testuale\n");
            None
        }
        Err(cause) => {
            civis::println!("[gpu] virtio-gpu init fallito: {}", cause);
            None
        }
    };

    // 3. Avvisa il parent (init) di essere pronto (SVC_READY): il supervisore
    //    init-restart lo usa (Fase 14).
    civis::signal_ready(1);

    // 4. Il driver e' raggiunto in diretta (IPC per nome): niente device FS
    //    (il cardo e' single-threaded: un driver che lo blocca mentre serve
    //    un cliente si deadlocka — vedi ADR-0060).

    // 5. Loop IPC: il device /dev/display (Open/Read/Write) + le notify.
    // `presented` = l'ultima geometria presentata (per il flush).
    let mut presented: Option<(u32, u32, u32)> = None;
    loop {
        match civis::recv() {
            Ok(msg) => {
                match msg.tag {
                    // ── Protocollo display (IPC diretto dalla console) ──
                    civis::syscall_numbers::GPU_DISPLAY_QUERY => {
                        let (backend, geom) = descriptor(&mut dev);
                        let _ = civis::reply(msg.tag, backend as u64, geom.pack());
                    }

                    civis::syscall_numbers::GPU_DISPLAY_PRESENT => {
                        // w0 = fisico del buffer, w1 = geometria impacchettata.
                        let geom = term::disp::Geometry::unpack(msg.w1);
                        let ok = handle_present(&mut dev, msg.w0, geom, &mut presented);
                        let _ = civis::reply(msg.tag, if ok { 0 } else { ERR }, 0);
                    }

                    civis::syscall_numbers::GPU_DISPLAY_FLUSH => {
                        let ok = handle_flush(&mut dev, &mut presented);
                        let _ = civis::reply(msg.tag, if ok { 0 } else { ERR }, 0);
                    }

                    DEV_OPEN => {
                        // Il driver non serve piu' device FS (il display e' in
                        // diretta): qualunque altra richiesta e' un errore.
                        let _ = civis::reply(msg.tag, ERR, 0);
                    }

                    DEV_READ | DEV_WRITE | DEV_CLOSE => {
                        let _ = civis::reply(msg.tag, ERR, 0);
                    }

                    civis::IRQ_NOTIFY_GPU => {
                        // MSI-X del config change (L0b): il driver osserva e
                        // logga. Il re-sync (D4) ricreera' la risorsa.
                        if let Some(d) = dev.as_mut() {
                            d.handle_irq();
                        }
                    }

                    civis::EXIT_NOTIFY => {
                        // cardo morto e rinato: il driver non serve piu' nessun
                        // device FS (il display e' in diretta) — niente da fare.
                    }

                    _ => {
                        let _ = civis::reply(msg.tag, 0, 0);
                    }
                }
            }
            Err(_) => {}
        }
    }
}

/// Il descrittore del display: backend + geometria del device.
fn descriptor(dev: &mut Option<virtio::Device>) -> (u8, term::disp::Geometry) {
    match dev.as_mut() {
        Some(dev) => {
            // La risoluzione corrente del device (il guest non sceglie ancora
            // la sua: D4).
            let info = dev.display_info();
            let (w, h) = match info {
                Some(i) => (i.width, i.height),
                None => (0, 0),
            };
            (
                term::disp::BACKEND_VIRTIO_GPU,
                term::disp::Geometry {
                    width: w,
                    height: h,
                    stride: w * 4,
                    format: term::disp::FORMAT_B8G8R8A8,
                },
            )
        }
        None => (term::disp::BACKEND_VGA_TEXT, term::disp::Geometry::default()),
    }
}

/// `FLUSH`: il device copia la risorsa nella superficie (path non-virgl:
/// senza flush lo schermo resta nero). La console lo manda dopo ogni render.
fn handle_flush(dev: &mut Option<virtio::Device>, presented: &mut Option<(u32, u32, u32)>) -> bool {
    let dev = match dev.as_mut() {
        Some(d) => d,
        None => return false,
    };
    let (w, h, stride) = match presented {
        Some(p) => *p,
        None => return false,
    };
    let _ = stride;
    // Prima i pixel (transfer), poi il refresh della superficie (flush).
    if !dev.transfer_to_host_2d(1, w, h) {
        return false;
    }
    dev.flush(1, w, h)
}

/// `PRESENT`: crea la risorsa 2D, le aggancia al buffer del client e la
/// imposta come scanout.
fn handle_present(
    dev: &mut Option<virtio::Device>,
    phys: u64,
    geom: term::disp::Geometry,
    presented: &mut Option<(u32, u32, u32)>,
) -> bool {
    let dev = match dev.as_mut() {
        Some(d) => d,
        None => return false,
    };
    if !geom.valid() || phys & 0xFFF != 0 {
        return false;
    }
    const RESOURCE_ID: u32 = 1;
    if !dev.create_2d(RESOURCE_ID, geom.format, geom.width, geom.height) {
        return false;
    }
    let len = geom.stride * geom.height;
    if !dev.attach_backing(RESOURCE_ID, phys, len) {
        return false;
    }
    if !dev.set_scanout(0, RESOURCE_ID, geom.width, geom.height) {
        return false;
    }
    *presented = Some((geom.width, geom.height, geom.stride));
    let _ = dev.flush(RESOURCE_ID, geom.width, geom.height);
    civis::println!(
        "[gpu] scanout: {}x{} stride={} (resource {})",
        geom.width,
        geom.height,
        geom.stride,
        RESOURCE_ID
    );
    true
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[gpu] panic\n");
    civis::exit(1)
}
