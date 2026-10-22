//! console — il **console server** (D3, ADR-0060).
//!
//! Due cose in un servizio:
//! - **il terminale**: modello a celle (UTF-8), font TrueType rasterizzato a
//!   runtime, e le surface che mettono i caratteri sullo schermo (framebuffer
//!   pixel o VGA testuale);
//! - **il device di testo** `/dev/console`: ci scrivono il login, l'eco della
//!   tastiera (`porta`) e i programmi.
//!
//! I **pixel sono della console**: alloca il buffer device (slot DMA), lo
//! renderizza e chiede al driver `gpu` di presentarlo (`/dev/display`).
//! Il driver non sa disegnare: e' solo il device.
//!
//! Fallback: se il device pixel non c'e' (o il font non e' leggibile), la
//! console scrive nel buffer testo della VGA (80x25, font hardware) — cioe'
//! il comportamento di sempre.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use civis;

use term::font::Font;
use term::grid::Grid;
use term::surface::{Geom, Pixel, Surface, VgaText};
use term::utf8::Decoder;

// ── Ring I/O (Fase 10.2) ─────────────────────────────────────────
// Le finestre CLI_* sono mappate da cardo (map_in) con i ring del client
// a ogni relay DEV (zero-copy); i ring propri del server non cambiano mai.

const REQ_RING_VA: u64 = civis::CLI_REQ_VA;
const RESP_RING_VA: u64 = civis::CLI_RESP_VA;
use civis::{req_frame_read, resp_frame_write};

// ── Tag IPC del device (DocsD: single source in `syscall-numbers`) ───
use civis::{DEV_CLOSE, DEV_OPEN, DEV_READ, DEV_WRITE};

/// Valore di errore IPC.
const ERR: u64 = !0u64;

/// Indirizzo fisico del buffer testo VGA (mappato dalla console).
const VGA_TEXT_PHYS: u64 = 0xB8000;
/// VA dove la console mappa il buffer testo VGA.
const VGA_TEXT_VA: u64 = 0x4000_0010_3000;
/// Dimensione della finestra VGA testuale (80x25 celle 8x16).
const VGA_COLS: usize = 80;
const VGA_ROWS: usize = 25;
/// Path del font TrueType nel volume root (oggetto ArcaFS `font.ttf`).
const FONT_PATH: &str = "/font.ttf";
/// Dimensione del font in pixel (fissa: la griglia si adatta alla
/// risoluzione, il font no).
const FONT_PX: u32 = 16;
/// Porte CRTC (cursore hardware) per il fallback VGA.
const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;

// ── Il terminale ───────────────────────────────────────────────────

/// Lo stato del terminale: modello, decoder, surface.
struct Term {
    grid: Grid,
    decoder: Decoder,
    surface: Surface,
    font: Option<Font>,
    /// Stato del parser ESC (0 = normale, 1 = ESC visto, 2 = ESC-[).
    esc: u8,
    /// Codepoint decodificati (buffer riusato).
    pending: alloc::vec::Vec<u32>,
    /// Ultima posizione del cursore disegnata (per ridisegnarla).
    cursor_drawn: (usize, usize),
    /// Colore foreground corrente (indice VGA).
    fg: u8,
    /// Colore background corrente.
    bg: u8,
    /// Canale del driver `gpu` (0 = nessun driver raggiungibile).
    driver_chan: u64,
    /// La risoluzione *desiderata* dal guest (D4): `None` = "segue l'host".
    /// Quando l'host insiste (config change) la console cede e passa a
    /// `None`.
    desired: Option<(u32, u32)>,
    /// Lo slot DMA del framebuffer corrente (D4: il riadattamento alloca
    /// l'altro slot e libera questo solo dopo che il nuovo e' presentato —
    /// un riadattamento fallito non deve lasciare la surface su un buffer
    /// liberato).
    fb_slot: u64,
}

impl Term {
    /// Mangia i byte e aggiorna il modello.
    fn feed(&mut self, bytes: &[u8]) {
        self.decoder.push(bytes, &mut self.pending);
        // `drain` svuota il buffer riusandone la capacita': i codepoint
        // processati NON devono restare nel buffer (il feed successivo li
        // rielaborerebbe, ripetendo gli a-capo e facendo scorrere la griglia).
        let mut cps = core::mem::take(&mut self.pending);
        for cp in cps.drain(..) {
            self.put(cp);
        }
        self.pending = cps;
        self.render();
        // Il path non-virgl di QEMU copia la risorsa nella superficie del
        // display **solo al flush**: senza, lo schermo resta nero.
        self.flush_display();
    }

    /// **Il terminale si riadatta** a una nuova misura del display (D4):
    /// buffer device nuovo (contiguo), risorsa nuova, griglia ricalcolata —
    /// il testo viene ricopiato dallo scrollback.
    ///
    /// Ritorna `true` se il riadattamento e' andato a buon fine.
    fn resize_to(&mut self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 || self.driver_chan == 0 {
            return false;
        }
        let m = match self.font.as_ref() {
            Some(f) => f.metrics(),
            None => return false,
        };
        // 1) il buffer device NUOVO: lo slot alternato (il vecchio resta
        //    valido finche' il nuovo non e' presentato — un riadattamento
        //    fallito non deve lasciare la surface su un buffer liberato).
        let bytes = (w as u64) * (h as u64) * 4;
        let pages = ((bytes + 4095) / 4096) as usize;
        // Il bound dei device buffer lo conosce `civis` (single source): se la
        // misura non ci sta lo si dice **prima** della syscall, con il perche'
        // (un resize dell'host oltre il bound e' legittimo: si tiene la
        // risoluzione corrente e il device scala il scanout nella finestra).
        if pages > civis::DMA_DEVICE_PAGES_MAX {
            civis::println!(
                "[console] display: {}x{} troppo grande ({} pagine, max {})",
                w,
                h,
                pages,
                civis::DMA_DEVICE_PAGES_MAX
            );
            return false;
        }
        let new_slot = if self.fb_slot == civis::DMA_SLOT_DEVICE {
            civis::DMA_SLOT_DEVICE + 1
        } else {
            civis::DMA_SLOT_DEVICE
        };
        let (phys, va) = match civis::dma_alloc_slot(pages, new_slot) {
            Ok(v) => v,
            Err(_) => {
                let _ = civis::print_string(b"[console] display: alloc del nuovo buffer fallita\n");
                return false;
            }
        };
        // 2) le geometrie: la superficie (w×h) e la griglia (celle intere,
        //    resto centrato).
        let grid_geom = Geom::for_surface(
            w,
            h,
            m.cell_w as u32,
            m.cell_h as u32,
            m.ascent_px,
        );
        let surface_geom = term::disp::Geometry {
            width: w,
            height: h,
            stride: w * 4,
            format: term::disp::FORMAT_B8G8R8A8,
        };
        // 3) il driver presenta il buffer nuovo (la risorsa vecchia viene
        //    rilasciata e rifatta dal driver: una risorsa ha dimensione fissa).
        let reply = match civis::send(
            self.driver_chan,
            civis::syscall_numbers::GPU_DISPLAY_PRESENT,
            phys,
            surface_geom.pack(),
        ) {
            Ok(r) => r,
            Err(_) => return false,
        };
        if reply.w0 != 0 {
            // Il driver non ha accettato il nuovo buffer: si libera e si
            // resta con quello vecchio (la surface resta valida).
            let _ = civis::dma_free_slot(new_slot);
            let _ = civis::print_string(b"[console] display: PRESENT del nuovo buffer fallito\n");
            return false;
        }
        // 4) la nuova superficie e la griglia (il testo ricopiato); il
        //    vecchio buffer e' liberato solo ora.
        let _ = civis::dma_free_slot(self.fb_slot);
        self.fb_slot = new_slot;
        self.surface = Surface::Pixel(Pixel {
            ptr: va as *mut u8,
            width: w,
            height: h,
            stride: surface_geom.stride,
            geom: grid_geom,
        });
        self.grid.resize(grid_geom.cols, grid_geom.rows);
        // 5) ridisegna tutto (il resize ha segnato la zona sporca intera).
        self.render();
        true
    }

    /// **Il guest sceglie la risoluzione** (D4, modeset): la finestra
    /// dell'host la segue. `(0, 0)` = "segui l'host" (torna alla policy
    /// dell'host). Nel fallback VGA non c'e' modeset: `-1`.
    fn set_mode(&mut self, w: u32, h: u32) -> Result<(), ()> {
        if !matches!(self.surface, Surface::Pixel(_)) {
            return Err(());
        }
        if w == 0 || h == 0 {
            self.desired = None;
            return Ok(());
        }
        // La misura desiderata si registra **solo** se il riadattamento
        // riesce: un modeset rifiutato non deve lasciare una desired che il
        // terminale non sta mostrando.
        if self.resize_to(w, h) {
            self.desired = Some((w, h));
            Ok(())
        } else {
            Err(())
        }
    }

    /// Chiede al driver il refresh della superficie (solo nel path pixel).
    ///
    /// IPC **diretto** col driver: il cardo e' single-threaded e la console
    /// lo sta bloccando mentre serve `/dev/console` (una scrittura FS qui
    /// dentro si deadlockerebbe). Inoltre i ring client e server condividono
    /// la stessa finestra `CLI_*`: il canale diretto evita anche la
    /// sovrapposizione.
    fn flush_display(&mut self) {
        if self.driver_chan == 0 {
            return;
        }
        if !matches!(self.surface, Surface::Pixel(_)) {
            return;
        }
        let _ = civis::send(self.driver_chan, civis::syscall_numbers::GPU_DISPLAY_FLUSH, 0, 0);
    }

    /// Un codepoint nel modello (movimento del cursore, scroll, editing).
    fn put(&mut self, cp: u32) {
        match self.esc {
            1 => {
                // ESC [ ...
                self.esc = if cp == b'[' as u32 { 2 } else { 0 };
                return;
            }
            2 => {
                self.esc = 0;
                match cp as u8 {
                    b'D' => {
                        // cursore a sinistra
                        if self.grid.cursor.0 > 0 {
                            self.grid.cursor.0 -= 1;
                        }
                    }
                    b'C' => {
                        // cursore a destra
                        if self.grid.cursor.0 + 1 < self.grid.cols() {
                            self.grid.cursor.0 += 1;
                        }
                    }
                    b'K' => {
                        // cancella fino a fine riga
                        let col = self.grid.cursor.0;
                        for c in col..self.grid.cols() {
                            self.grid.put(c, self.grid.cursor.1, b' ' as u32, self.fg, self.bg);
                        }
                    }
                    _ => {}
                }
                return;
            }
            _ => {}
        }
        match cp {
            0x1B => {
                self.esc = 1;
            }
            0x0A => {
                // newline: la riga successiva, scroll se siamo in fondo
                let row = self.grid.cursor.1;
                if row + 1 >= self.grid.rows() {
                    self.grid.scroll_up();
                } else {
                    self.grid.cursor.1 = row + 1;
                }
                self.grid.cursor.0 = 0;
            }
            0x0D => {
                self.grid.cursor.0 = 0;
            }
            0x08 => {
                if self.grid.cursor.0 > 0 {
                    self.grid.cursor.0 -= 1;
                }
            }
            0x0C => {
                // form feed: pulisce tutto, cursore a casa
                self.grid.clear();
                self.grid.cursor = (0, 0);
            }
            0x20..=0x7E => {
                let (col, row) = self.grid.cursor;
                self.grid.put(col, row, cp, self.fg, self.bg);
                if col + 1 >= self.grid.cols() {
                    // fine riga: va a capo (e scrolla se necessario)
                    self.grid.cursor.0 = 0;
                    if row + 1 >= self.grid.rows() {
                        self.grid.scroll_up();
                    } else {
                        self.grid.cursor.1 = row + 1;
                    }
                } else {
                    self.grid.cursor.0 = col + 1;
                }
            }
            _ => {
                // Stampabile ma fuori ASCII (icone Nerd, accenti): la
                // superficie pixel lo sa disegnare, la VGA no (diventa '?').
                let (col, row) = self.grid.cursor;
                self.grid.put(col, row, cp, self.fg, self.bg);
                if col + 1 >= self.grid.cols() {
                    self.grid.cursor.0 = 0;
                    if row + 1 >= self.grid.rows() {
                        self.grid.scroll_up();
                    } else {
                        self.grid.cursor.1 = row + 1;
                    }
                } else {
                    self.grid.cursor.0 = col + 1;
                }
            }
        }
    }

    /// Ridisegna la zona sporca e il cursore.
    fn render(&mut self) {
        let dirty = self.grid.take_dirty();
        if dirty.any {
            let (cols, rows) = (self.grid.cols(), self.grid.rows());
            for row in dirty.min_row..=dirty.max_row.min(rows.saturating_sub(1)) {
                for col in dirty.min_col..=dirty.max_col.min(cols.saturating_sub(1)) {
                    let cell = self.grid.cell(col, row);
                    let is_cursor = (col, row) == self.grid.cursor_clamped() && self.grid.cursor_visible;
                    self.draw_cell(col, row, cell.cp, cell.fg, cell.bg, is_cursor);
                }
            }
        }
        // Il cursore si e' spostato: la cella vecchia va ridisegnata.
        let cur = self.grid.cursor_clamped();
        if cur != self.cursor_drawn {
            let (cc, cr) = self.cursor_drawn;
            if cc < self.grid.cols() && cr < self.grid.rows() {
                let cell = self.grid.cell(cc, cr);
                self.draw_cell(cc, cr, cell.cp, cell.fg, cell.bg, false);
            }
            self.cursor_drawn = cur;
        }
        // La cella del cursore (con il cursore dentro).
        let (cc, cr) = self.cursor_drawn;
        if cc < self.grid.cols() && cr < self.grid.rows() {
            let cell = self.grid.cell(cc, cr);
            self.draw_cell(cc, cr, cell.cp, cell.fg, cell.bg, self.grid.cursor_visible);
        }
        // Nel fallback VGA il cursore e' quello hardware (CRTC).
        if matches!(self.surface, Surface::VgaText(_)) {
            unsafe { move_hw_cursor(cr, cc) };
        }
    }

    /// Disegna una cella sulla surface attiva.
    fn draw_cell(&mut self, col: usize, row: usize, cp: u32, fg: u8, bg: u8, cursor: bool) {
        // Nel path VGA il font non serve (il BIOS disegna il carattere).
        let font = self.font.as_mut();
        self.surface
            .put_char(col, row, cp, fg, bg, cursor, font);
    }
}

// ── Il font: lettura dal volume ────────────────────────────────────

/// Fonte di byte per il file del font: **la RAM**.
///
/// Il font e' letto tutto (2.4 MiB) all'avvio, non a pezzi durante il
/// servizio: la console non puo' fare I/O sul FS mentre serve `/dev/console`
/// (il cardo e' single-threaded e la aspetta: un letture FS li' dentro
/// deadlockerebbe — console e cardo si bloccherebbero a vicenda). Con il
/// font in RAM il rasterizer non tocca piu' il FS, e le icone Nerd (che
/// stanno fuori dal range ASCII) restano disponibili.
struct MemSource {
    data: alloc::vec::Vec<u8>,
}

impl term::font::Source for MemSource {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Option<usize> {
        let start = off as usize;
        if start >= self.data.len() {
            return Some(0);
        }
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        Some(n)
    }
}

// ── Entry point ─────────────────────────────────────────────────────

/// Assicura il mount "/dev/console" presso cardo (stessa via del vecchio
/// console server): un tentativo, idempotente (replace-on-register).
fn ensure_console_registered() {
    civis::ensure_fs_mount(|| civis::fs_register(b"/dev/console"));
}

/// Il cursore hardware del VGA (fallback): porta CRTC.
unsafe fn move_hw_cursor(row: usize, col: usize) {
    let offset = (row * VGA_COLS + col) as u16;
    unsafe {
        outb(CRTC_INDEX, 0x0E);
        outb(CRTC_DATA, (offset >> 8) as u8);
        outb(CRTC_INDEX, 0x0F);
        outb(CRTC_DATA, (offset & 0xFF) as u8);
    }
}

/// Scrive un byte su una porta I/O (ring 3 consentito dal TSS, ADR-0006).
unsafe fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nostack, nomem));
    }
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    // 1. Registra il servizio: il grant del device buffer (slot DMA >= 1) e'
    //    legato a questo owner (ADR-0057-style).
    if civis::service_register(civis::Service::Console).is_ok() {
        let _ = civis::print_string(b"[console] registered as service Console\n");
    }

    // 2. Risolve il driver `gpu` (per nome): e' lui che sa cosa puo'
    //    mostrare (device pixel o fallback VGA).
    let driver_chan = match civis::service_lookup(civis::Service::Gpu) {
        Ok(c) if c > 0 => c as u64,
        _ => 0,
    };

    // 3. Costruisce la surface: pixel (virtio-gpu) o VGA testuale.
    let mut font: Option<Font> = None;
    let surface = if driver_chan != 0 {
        setup_pixel(driver_chan, &mut font)
    } else {
        None
    };
    let surface = match surface {
        Some(s) => s,
        None => {
            let _ = civis::print_string(b"[console] display pixel non disponibile: VGA testuale\n");
            setup_vga_text()
        }
    };

    // 4. Il modello: la griglia segue la surface.
    let geom = surface.geom();
    let mut grid = Grid::new(geom.cols, geom.rows);
    // Il prompt parte in fondo (come il terminale di sempre).
    grid.cursor = (0, geom.rows.saturating_sub(1));

    let mut term = Term {
        grid,
        decoder: Decoder::new(),
        surface,
        font,
        esc: 0,
        pending: alloc::vec::Vec::with_capacity(64),
        cursor_drawn: (0, 0),
        fg: 0x0F,
        bg: 0x00,
        driver_chan,
        desired: None,
        fb_slot: civis::DMA_SLOT_DEVICE,
    };

    // 5. Banner sul terminale (il framebuffer o la VGA) + il seriale (sink
    //    di debug): la console si annuncia come il vecchio console server.
    term.feed(b"Velordo console server\n");
    let _ = civis::print_string(b"[console] server up\n");
    log_render_marker(&term);

    // 6. Avvisa il parent (init): il supervisore lo usa (Fase 14).
    civis::signal_ready(1);

    // 7. Registra /dev/console con cardo.
    ensure_console_registered();
    let _ = civis::print_string(b"[console] registered /dev/console with cardo\n");

    // 8. Loop IPC: il device di testo.
    loop {
        match civis::recv() {
            Ok(msg) => {
                match msg.tag {
                    DEV_OPEN => {
                        if msg.w0 == civis::DEV_CONSOLE {
                            let _ = civis::reply(msg.tag, 0, 0);
                        } else {
                            let _ = civis::reply(msg.tag, ERR, 0);
                        }
                    }

                    DEV_READ => {
                        // Output-only: EOF immediato (frame vuoto + 0).
                        unsafe { resp_frame_write(RESP_RING_VA, &[]); }
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    DEV_WRITE => {
                        let count = msg.w1 as usize;
                        if count > 0 {
                            let mut data = alloc::vec::Vec::with_capacity(count);
                            data.resize(count, 0);
                            unsafe { req_frame_read(REQ_RING_VA, &mut data, count); }
                            term.feed(&data);
                        }
                        let _ = civis::reply(msg.tag, msg.w1, 0);
                    }

                    DEV_CLOSE => {
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    civis::syscall_numbers::GPU_DISPLAY_CHANGED => {
                        // L'host ha cambiato la finestra (config change MSI-X):
                        // la console cede (policy D1) e si riadatta — testo
                        // conservato. NOTA: e' una notifica fire-and-forget:
                        // NON si risponde (altrimenti driver e console
                        // rimbalzano le risposte a vicenda).
                        let g = term::disp::Geometry::unpack(msg.w1);
                        if g.valid() {
                            // L'host insiste: il guest cede la scelta.
                            term.desired = None;
                            // Se la misura e' gia' quella che il terminale
                            // mostra (l'host ha solo confermato) non si
                            // rialloca niente: un buffer contiguo non e'
                            // gratis.
                            let already = matches!(
                                &term.surface,
                                Surface::Pixel(p)
                                    if p.width == g.width && p.height == g.height
                            );
                            if already {
                                continue;
                            }
                            if term.resize_to(g.width, g.height) {
                                civis::println!(
                                    "[console] display: {}x{} grid={}x{} (adattato, testo conservato)",
                                    g.width,
                                    g.height,
                                    term.grid.cols(),
                                    term.grid.rows()
                                );
                            } else {
                                let _ = civis::print_string(b"[console] display: riadattamento fallito\n");
                            }
                        }
                    }

                    civis::syscall_numbers::CONSOLE_QUERY => {
                        let _ = civis::reply(msg.tag, term.grid.cols() as u64, term.grid.rows() as u64);
                    }

                    civis::syscall_numbers::CONSOLE_SET_MODE => {
                        // Il guest sceglie la risoluzione (modeset).
                        let r = term.set_mode(msg.w0 as u32, msg.w1 as u32);
                        let _ = civis::reply(msg.tag, if r.is_ok() { 0 } else { ERR }, 0);
                    }

                    civis::syscall_numbers::CONSOLE_STATE => {
                        // Il contenuto del terminale: celle non vuote + CRC
                        // del testo (la prova automatica che il testo ha
                        // sopravvissuto a un re-adattamento).
                        let _ = civis::reply(
                            msg.tag,
                            term.grid.non_empty() as u64,
                            term.grid.text_crc() as u64,
                        );
                    }

                    civis::EXIT_NOTIFY => {
                        // cardo morto e rinato: re-registra il console.
                        ensure_console_registered();
                    }

                    _ => {
                        // Solo le richieste (req_id == 0) vanno risposte: una
                        // risposta async ricevuta qui non va rimbalzata.
                        if msg.req_id == 0 {
                            let _ = civis::reply(msg.tag, 0, 0);
                        }
                    }
                }
            }
            Err(_) => {}
        }
    }
}

/// Prepara la surface pixel: legge il descrittore, alloca il buffer device,
/// carica il font e presenta il buffer al driver.
fn setup_pixel(driver_chan: u64, font: &mut Option<Font>) -> Option<Surface> {
    // Cosa sa fare il driver? (backend + geometria)
    let reply = civis::send(driver_chan, civis::syscall_numbers::GPU_DISPLAY_QUERY, 0, 0).ok()?;
    if reply.w0 != term::disp::BACKEND_VIRTIO_GPU as u64 {
        return None;
    }
    let geom = term::disp::Geometry::unpack(reply.w1);
    if !geom.valid() {
        return None;
    }

    // Il font (16 px): le metriche danno la cella.
    let f = match load_font() {
        Some(f) => f,
        None => {
            let _ = civis::print_string(b"[console] font non leggibile: niente surface pixel\n");
            return None;
        }
    };
    let m = f.metrics();
    *font = Some(f);

    // Il buffer device: contiguo, nelle mani della console.
    let bytes = (geom.width as u64) * (geom.height as u64) * 4;
    let pages = ((bytes + 4095) / 4096) as usize;
    let (phys, va) = civis::dma_alloc_slot(pages, civis::DMA_SLOT_DEVICE).ok()?;
    let _ = civis::print_string(b"[console] buffer device allocato\n");

    // La griglia: dalla risoluzione e dalla cella del font. Celle intere,
    // resto centrato (D4): il terminale resta centrato nella finestra.
    let cells = Geom::for_surface(
        geom.width,
        geom.height,
        m.cell_w as u32,
        m.cell_h as u32,
        m.ascent_px,
    );
    let _ = civis::println!(
        "[console] font: MapleMono-NF {}px cell={}x{}",
        FONT_PX,
        m.cell_w,
        m.cell_h
    );

    // Presenta il buffer al driver (il device lo mappa sullo scanout).
    let reply = civis::send(
        driver_chan,
        civis::syscall_numbers::GPU_DISPLAY_PRESENT,
        phys,
        geom.pack(),
    )
    .ok()?;
    if reply.w0 != 0 {
        let _ = civis::print_string(b"[console] PRESENT fallito\n");
        return None;
    }

    let _ = civis::println!(
        "[console] surface: pixel {}x{} grid={}x{}",
        geom.width,
        geom.height,
        cells.cols,
        cells.rows
    );
    Some(Surface::Pixel(Pixel {
        ptr: va as *mut u8,
        width: geom.width,
        height: geom.height,
        stride: geom.stride,
        geom: cells,
    }))
}

/// Marker deterministico del rendering: BLAKE2s-256 dell'ultima riga di
/// pixel (li' sta il banner). Stessa uscita a ogni boot (il rasterizer e' in
/// punto fisso): serve da prova di regressione del rendering.
fn log_render_marker(term: &Term) {
    if let Surface::Pixel(p) = &term.surface {
        // Hash dell'**intero** framebuffer: deterministico (il rasterizer e'
        // in punto fisso) e copre ogni pixel disegnato — il marker di
        // regressione del rendering. Stessa opera sul host (stesso crate)
        // per il confronto bit-a-bit.
        let total = p.stride as usize * p.height as usize;
        let buf = unsafe { core::slice::from_raw_parts(p.ptr, total) };
        let digest = blake2s::blake2s(buf);
        let mut hex = [0u8; 64];
        for i in 0..32 {
            hex[i * 2] = hex_digit(digest[i] >> 4);
            hex[i * 2 + 1] = hex_digit(digest[i] & 0x0F);
        }
        let s = core::str::from_utf8(&hex).unwrap_or("?");
        civis::println!("[console] fb blake2s={}", s);
    }
}

/// Un nibble come carattere esadecimale.
fn hex_digit(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'a' + n - 10 }
}

/// La surface di fallback: il buffer testo della VGA (80x25).
fn setup_vga_text() -> Surface {
    let _ = civis::map_physical(VGA_TEXT_PHYS, VGA_TEXT_VA, 1);
    let _ = civis::println!("[console] surface: vga-text {}x{}", VGA_COLS, VGA_ROWS);
    Surface::VgaText(VgaText {
        ptr: VGA_TEXT_VA as *mut u8,
        cols: VGA_COLS,
        rows: VGA_ROWS,
    })
}

/// Carica il font TrueType dal volume root (tutto in RAM: vedi `MemSource`).
fn load_font() -> Option<Font> {
    let data = read_whole_file(FONT_PATH)?;
    Font::load(Box::new(MemSource { data }), FONT_PX).ok()
}

/// Legge un file per intero (il FS risponde a chunk: si itera).
fn read_whole_file(path: &str) -> Option<alloc::vec::Vec<u8>> {
    let fd = civis::open(path, 0).ok()?;
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    // Il buffer e' sull'heap: lo stack utente e' piccolo (4 frame).
    let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(16 * 1024);
    let cap = buf.capacity();
    buf.resize(cap, 0);
    loop {
        let n = match civis::read_fs(fd, &mut buf, cap) {
            Ok(n) => n,
            Err(_) => {
                let _ = civis::close(fd);
                return None;
            }
        };
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    let _ = civis::close(fd);
    Some(out)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[console] panic\n");
    civis::exit(1)
}
