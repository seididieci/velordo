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

/// Assicura il mount "/dev/display" presso cardo (stessa via di
/// `/dev/console`): un tentativo, idempotente grazie al
/// replace-on-register in cardo. Va rifatto se cardo muore (EXIT_NOTIFY).
fn ensure_display_registered() {
    civis::ensure_fs_mount(|| civis::fs_register(b"/dev/display"));
}

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

    // 4. Pubblica il device del display.
    ensure_display_registered();
    let _ = civis::print_string(b"[gpu] registered /dev/display with cardo\n");

    // 5. Loop IPC: il device /dev/display (Open/Read/Write) + le notify.
    loop {
        match civis::recv() {
            Ok(msg) => {
                match msg.tag {
                    DEV_OPEN => {
                        // Il solo device del driver: il display.
                        if msg.w0 == civis::DEV_DISPLAY {
                            let _ = civis::reply(msg.tag, 0, 0);
                        } else {
                            let _ = civis::reply(msg.tag, ERR, 0);
                        }
                    }

                    DEV_READ => {
                        // Il descrittore del display (formato `term::disp`).
                        let d = descriptor(&mut dev);
                        unsafe { resp_frame_write(RESP_RING_VA, &d); }
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    DEV_WRITE => {
                        // msg.w0 = fd, msg.w1 = count. Il comando (Present)
                        // e' nel request ring del client.
                        let count = msg.w1 as usize;
                        let mut ok = false;
                        if count > 0 {
                            let mut data = alloc::vec::Vec::with_capacity(count);
                            data.resize(count, 0);
                            unsafe { req_frame_read(REQ_RING_VA, &mut data, count); }
                            ok = handle_present(&mut dev, &data);
                        }
                        if ok {
                            let _ = civis::reply(msg.tag, msg.w1, 0);
                        } else {
                            let _ = civis::reply(msg.tag, ERR, 0);
                        }
                    }

                    DEV_CLOSE => {
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    civis::IRQ_NOTIFY_GPU => {
                        // MSI-X del config change (L0b): il driver osserva e
                        // logga. Il re-sync (D4) ricreera' la risorsa.
                        if let Some(d) = dev.as_mut() {
                            d.handle_irq();
                        }
                    }

                    civis::EXIT_NOTIFY => {
                        // cardo morto e rinato: re-registra il display.
                        ensure_display_registered();
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

/// Il descrittore del display (formato `term::disp`): backend, geometria,
/// stride, formato.
fn descriptor(dev: &mut Option<virtio::Device>) -> [u8; term::disp::DESCRIPTOR_LEN] {
    let mut d = term::disp::Descriptor::default();
    match dev.as_mut() {
        Some(dev) => {
            // La risoluzione corrente del device (il guest non sceglie ancora
            // la sua: D4).
            let info = dev.display_info();
            let (w, h) = match info {
                Some(i) => (i.width, i.height),
                None => (0, 0),
            };
            d.backend = term::disp::BACKEND_VIRTIO_GPU;
            d.width = w;
            d.height = h;
            d.stride = w * 4;
            d.format = term::disp::FORMAT_B8G8R8A8;
        }
        None => {
            d.backend = term::disp::BACKEND_VGA_TEXT;
        }
    }
    term::disp::descriptor_bytes(&d)
}

/// `PRESENT`: crea la risorsa 2D, le aggancia al buffer del client e la
/// imposta come scanout.
fn handle_present(dev: &mut Option<virtio::Device>, data: &[u8]) -> bool {
    let dev = match dev.as_mut() {
        Some(d) => d,
        None => return false,
    };
    let p = match term::disp::parse_present(data) {
        Some(p) => p,
        None => return false,
    };
    const RESOURCE_ID: u32 = 1;
    if !dev.create_2d(RESOURCE_ID, p.format, p.width, p.height) {
        return false;
    }
    let len = p.stride * p.height;
    if !dev.attach_backing(RESOURCE_ID, p.phys, len) {
        return false;
    }
    if !dev.set_scanout(0, RESOURCE_ID, p.width, p.height) {
        return false;
    }
    let _ = dev.flush(RESOURCE_ID, p.width, p.height);
    civis::println!(
        "[gpu] scanout: {}x{} stride={} (resource {})",
        p.width,
        p.height,
        p.stride,
        RESOURCE_ID
    );
    true
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[gpu] panic\n");
    civis::exit(1)
}
