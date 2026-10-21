//! gpu — Terminale video in userspace (Fase 8.2 + 9.4 + 15.4, R4: ex-console).
//!
//! E' l'UNICO processo che disegna sul frame buffer VGA e pubblica il device
//! di output `/dev/console` (DEV_WRITE disegna i byte). La tastiera vive
//! altrove (Fase 15): `kbd` pubblica scancode raw su `/dev/kbd`, `porta`
//! decodifica, fa echo scrivendo qui e serve i byte cotti su
//! `/dev/input/keyboard`.
//!
//! Il cursore hardware VGA (CRTC 0x3D4/0x3D5) segue il punto di scrittura.

#![no_std]
#![no_main]

extern crate alloc;
use civis;

mod virtio;
mod virtio_gpu;

/// Costante copiata da `vmm_user.rs` (il crate kernel non e' linkato qui).
const USER_VGA: u64 = 0x4000_0010_0000;

/// Indirizzo fisico del frame buffer VGA (single source in `vela::gpu`).
use vela::gpu::VGA_PHYS;

/// Dimensioni del buffer VGA (single source in `vela::gpu`).
use vela::gpu::{VGA_COLS, VGA_ROWS};

/// Porte CRTC per il cursore hardware VGA.
/// Porte CRTC (single source in `vela::gpu`).
use vela::gpu::{CRTC_DATA, CRTC_INDEX};

// ── IPC tags + device type (DocsD: single source in `syscall-numbers`) ─
use civis::{DEV_CLOSE, DEV_CONSOLE, DEV_OPEN, DEV_READ, DEV_WRITE};

/// Valore di errore IPC.
const ERR: u64 = !0u64;

// ── VGA buffer ───────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct ScreenChar {
    ascii: u8,
    color: u8,
}

#[repr(C)]
struct Buffer {
    chars: [[ScreenChar; VGA_COLS]; VGA_ROWS],
}

/// Colore unico del terminale (bianco su nero): un solo writer, colori coerenti.
const COLOR: u8 = 0x0F;

/// Porta di output 8-bit (ring 3 consentito via TSS I/O bitmap, ADR-0006).
unsafe fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nostack, nomem));
    }
}

/// Sposta il cursore hardware VGA alla cella (row, col).
unsafe fn move_hw_cursor(row: usize, col: usize) {
    let offset = (row * VGA_COLS + col) as u16;
    unsafe {
        outb(CRTC_INDEX, 0x0E);
        outb(CRTC_DATA, (offset >> 8) as u8);
        outb(CRTC_INDEX, 0x0F);
        outb(CRTC_DATA, (offset & 0xFF) as u8);
    }
}

/// Scrive un singolo byte sul frame buffer VGA a `USER_VGA`.
/// Safety: `vga` deve puntare a un buffer VGA valido mappato in questo
/// processo.
unsafe fn vga_write_byte(vga: *mut Buffer, row: usize, col: usize, byte: u8) {
    let ch = ScreenChar { ascii: byte, color: COLOR };
    unsafe {
        core::ptr::write_volatile(&mut (*vga).chars[row][col], ch);
    }
}

/// Pulisce una riga intera.
unsafe fn vga_clear_row(vga: *mut Buffer, row: usize) {
    for col in 0..VGA_COLS {
        unsafe { vga_write_byte(vga, row, col, b' ') };
    }
}

/// Scorrimento: copia le righe 1..24 verso l'alto e pulisce l'ultima.
unsafe fn vga_scroll(vga: *mut Buffer) {
    for row in 1..VGA_ROWS {
        for col in 0..VGA_COLS {
            let ch = unsafe { core::ptr::read_volatile(&(*vga).chars[row][col]) };
            unsafe { core::ptr::write_volatile(&mut (*vga).chars[row - 1][col], ch) };
        }
    }
    unsafe { vga_clear_row(vga, VGA_ROWS - 1) };
}

/// Alimenta UN byte del flusso terminale (Fase 43b, editing di linea della
/// shell): mini-parser ESC con stato `esc` (0 = normale, 1 = visto ESC,
/// 2 = visto ESC-`[`). I comandi supportati sono `ESC[D` (cursore sinistra
/// SENZA cancellare: il vecchio `\x08` cancella e non basta per l'editing
/// mid-line), `ESC[C` (destra), `ESC[K` (spazi da cursore a fine riga,
/// cursore fermo). Sequenze ignote = ignorate. Lo stato persiste tra le
/// DEV_WRITE (una sequenza puo' spezzarsi tra due write).
unsafe fn feed_byte(vga: *mut Buffer, byte: u8, cursor: &mut usize, esc: &mut u8) {
    match *esc {
        0 => {
            if byte == 0x1b {
                *esc = 1;
            } else {
                unsafe { vga_write_char(vga, byte, cursor) };
            }
        }
        1 => {
            *esc = if byte == b'[' { 2 } else { 0 };
        }
        _ => {
            *esc = 0;
            match byte {
                b'D' => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        unsafe { move_hw_cursor(VGA_ROWS - 1, *cursor) };
                    }
                }
                b'C' => {
                    if *cursor < VGA_COLS - 1 {
                        *cursor += 1;
                        unsafe { move_hw_cursor(VGA_ROWS - 1, *cursor) };
                    }
                }
                b'K' => {
                    let mut col = *cursor;
                    while col < VGA_COLS {
                        unsafe { vga_write_byte(vga, VGA_ROWS - 1, col, b' ') };
                        col += 1;
                    }
                    unsafe { move_hw_cursor(VGA_ROWS - 1, *cursor) };
                }
                _ => {}
            }
        }
    }
}

/// Scrive un carattere ASCII sul VGA (ultima riga) con scroll automatico.
/// Aggiorna il cursore software e sposta il cursore hardware a seguire.
unsafe fn vga_write_char(vga: *mut Buffer, byte: u8, cursor: &mut usize) {
    match byte {
        b'\x0c' => {
            // Form feed (Fase 18.1, builtin `clear`): pulisci tutto e home.
            for row in 0..VGA_ROWS {
                unsafe { vga_clear_row(vga, row) };
            }
            *cursor = 0;
        }
        b'\n' => {
            unsafe { vga_scroll(vga) };
            *cursor = 0;
        }
        b'\r' => *cursor = 0,
        b'\x08' => {
            if *cursor > 0 {
                *cursor -= 1;
                unsafe { vga_write_byte(vga, VGA_ROWS - 1, *cursor, b' ') };
            }
        }
        0x20..=0x7e => {
            let row = VGA_ROWS - 1;
            let col = *cursor;
            unsafe { vga_write_byte(vga, row, col, byte) };
            *cursor += 1;
            if *cursor >= VGA_COLS {
                unsafe { vga_scroll(vga) };
                *cursor = 0;
            }
        }
        _ => {}
    }
    unsafe { move_hw_cursor(VGA_ROWS - 1, *cursor) };
}

// ── Ring I/O (Fase 10.2) ─────────────────────────────────────────
// Le finestre CLI_* sono mappate da cardo (map_in) con i ring del client
// a ogni relay DEV (zero-copy); i ring propri del server non cambiano mai.

const REQ_RING_VA: u64 = civis::CLI_REQ_VA;
const RESP_RING_VA: u64 = civis::CLI_RESP_VA;
// Geometria ring (A1) + frame helpers (A2): single source in `civis`.
use civis::{req_frame_read, resp_frame_write};

// ── Entry point ──────────────────────────────────────────────────────

/// Assicura il mount "/dev/console" presso cardo (Fase 14, t28 + Fase 15):
/// attende Fs via soli lookup, poi UN tentativo (vedi corpo). Stessa funzione
/// a boot e su EXIT_NOTIFY. Unbounded come `fs_chan`. Idempotente grazie al
/// replace-on-register in cardo.
fn ensure_mounted() {
    civis::ensure_fs_mount(|| civis::fs_register(b"/dev/console"));
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    // 1. Mappa il frame buffer VGA.
    let _ = civis::map_physical(VGA_PHYS, USER_VGA, 1);
    let vga = USER_VGA as *mut Buffer;

    // 2. Pulisci il VGA.
    for row in 0..VGA_ROWS {
        unsafe { vga_clear_row(vga, row) };
    }

    // 3. Stampa banner (il terminale parte in fondo, come un prompt).
    let msg = b"Velordo console server";
    let mut cursor = 0usize;
    // Stato parser ESC (Fase 43b): persiste tra le DEV_WRITE.
    let mut esc: u8 = 0;
    for &b in msg {
        unsafe { vga_write_char(vga, b, &mut cursor) };
    }
    let _ = civis::print_string(b"[gpu] server up\n");

    // 4b. Registra il servizio Console per nome (ADR-0008): init lo usa per
    // la supervisione (service_pid) e i client potrebbero risolverlo.
    if civis::service_register(civis::Service::Gpu).is_ok() {
        let _ = civis::print_string(b"[gpu] registered as service Gpu\n");
    }

    // 4b-bis. Trasporto virtio-gpu modern (L0b, ADR-0057): la registrazione
    // qui sopra e' il grant per SYS_MAP_MMIO. Ogni fallimento = si resta su
    // VGA, il loop IPC e /dev/console non dipendono dal device.
    let mut virtio = match virtio::init() {
        Ok(dev) => Some(dev),
        Err("assente") => {
            let _ = civis::print_string(b"[gpu] virtio-gpu assente: resto su VGA\n");
            None
        }
        Err(cause) => {
            civis::println!("[gpu] virtio-gpu init fallito: {}", cause);
            None
        }
    };

    // 4c. Avvisa il parent (init) di essere pronto (SVC_READY fire-and-forget):
    // serve al supervisore init-restart (Fase 14). SUBITO dopo la registrazione
    // del servizio (non dopo /dev/console, che richiede cardo non ancora nato:
    // init aspetta questo ack a boot e attendere dopo sarebbe deadlock).
    // Fire-and-forget in `civis` (A3): retry bounded, mai hang.
    civis::signal_ready(1);

    // 5. Registra /dev/console con cardo (IPC FS_REGISTER via civis::fs_register,
    //    che prima alloca e registra la pagina FS per-processo).
    //    ensure_mounted: stessa funzione a boot e su EXIT_NOTIFY (t28).
    ensure_mounted();
    let _ = civis::print_string(b"[gpu] registered /dev/console with cardo\n");

    // 6. Loop IPC: solo richieste DEV sul device di output (+ EXIT_NOTIFY).
    //    Niente piu' tastiera qui (Fase 15: kbd + porta).
    loop {
        match civis::recv() {
            Ok(msg) => {
                match msg.tag {
                    DEV_OPEN => {
                        // msg.w0 = device type
                        if msg.w0 == DEV_CONSOLE {
                            let _ = civis::reply(msg.tag, 0, 0);
                        } else {
                            let _ = civis::reply(msg.tag, ERR, 0);
                        }
                    }

                    DEV_READ => {
                        // Output-only: EOF immediato (frame vuoto + 0), come
                        // /dev/null. Frame SEMPRE (anche vuoto): il client
                        // distingue "0 byte" da "ring vuoto" solo dal frame.
                        unsafe { resp_frame_write(RESP_RING_VA, &[]); }
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    DEV_WRITE => {
                        // msg.w0 = fd, msg.w1 = count. I byte da disegnare sono
                        // nella request ring del client (mappata da cardo via map_in).
                        let count = msg.w1 as usize;
                        if count > 0 {
                            let mut data = alloc::vec::Vec::with_capacity(count);
                            data.resize(count, 0);
                            unsafe { req_frame_read(REQ_RING_VA, &mut data, count); }
                            for b in &data {
                                unsafe { feed_byte(vga, *b, &mut cursor, &mut esc) };
                            }
                        }
                        let _ = civis::reply(msg.tag, msg.w1, 0);
                    }

                    DEV_CLOSE => {
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    civis::IRQ_NOTIFY_GPU => {
                        // MSI-X del config change (L0b): ISR -> events_read ->
                        // events_clear -> nuova risoluzione. Nessuna reply.
                        if let Some(dev) = virtio.as_mut() {
                            dev.handle_irq();
                        }
                    }

                    civis::EXIT_NOTIFY => {
                        // cardo morto e rinato (t28): re-mount. Nessuno stato
                        // per-client da purgare; mai rispondere alle notifiche.
                        ensure_mounted();
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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[gpu] panic\n");
    civis::exit(1)
}
