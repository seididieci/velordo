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
use civis::hex;

use term::font::Font;
use term::grid::{color, Grid};
use term::surface::{self, Geom, Pixel, Surface, VgaText};
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

/// `true` se tutte le righe del buffer (stride in byte) sono byte-identiche
/// alla riga 0. E' la condizione in cui lo scroll puo' spostare i pixel
/// invece di ricomporre la banda (T4, ADR-0066).
fn rows_all_equal(buf: &[u8], stride: u32) -> bool {
    let stride = stride as usize;
    if stride == 0 || buf.len() < stride {
        return true;
    }
    let first = &buf[..stride];
    let mut off = stride;
    while off + stride <= buf.len() {
        if &buf[off..off + stride] != first {
            return false;
        }
        off += stride;
    }
    true
}

// ── Il terminale ───────────────────────────────────────────────────

/// Rettangolo di pixel toccati (half-open): il danno da trasferire al driver
/// (D7, ADR-0066). Unico punto di dichiarazione del danno: ogni cella disegnata
/// aggiunge il proprio bbox.
#[derive(Clone, Copy)]
struct Damage { x0: u32, y0: u32, x1: u32, y1: u32 }

impl Damage {
    /// Il sentinella di "nessun danno": x0/y0 al massimo cosi' il primo
    /// `add_rect` li abbassa al valore vero (con 0 l'`min` terrebbe l'origine
    /// e ogni rect partirebbe da (0,0)).
    fn none() -> Self { Self { x0: u32::MAX, y0: u32::MAX, x1: 0, y1: 0 } }
    fn is_empty(&self) -> bool { self.x0 >= self.x1 || self.y0 >= self.y1 }
    fn add_rect(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 { return; }
        let x1 = x.saturating_add(w);
        let y1 = y.saturating_add(h);
        self.x0 = self.x0.min(x);
        self.y0 = self.y0.min(y);
        self.x1 = self.x1.max(x1);
        self.y1 = self.y1.max(y1);
    }
    /// Espande il danno di 1 cella in tutte le direzioni (I3: sbordo glifo).
    /// `c0..=c1` / `r0..=r1` sono indici di cella inclusivi; il rettangolo
    /// risultante e' in pixel, half-open, clipped a `(surf_w, surf_h)`.
    fn add_cells(
        &mut self,
        geom: term::surface::Geom,
        surf_w: u32,
        surf_h: u32,
        c0: usize,
        r0: u32,
        c1: usize,
        r1: u32,
    ) {
        let cw = geom.cell_w as u32;
        let ch = geom.cell_h as u32;
        let ox = geom.origin_x;
        let oy = geom.origin_y;
        let last_col = (geom.cols.saturating_sub(1)) as u32;
        let last_row = (geom.rows.saturating_sub(1)) as u32;
        let x0 = ox + (c0.saturating_sub(1)) as u32 * cw;
        let y0 = oy + r0.saturating_sub(1) * ch;
        let x1 = (ox + ((c1 as u32 + 1).min(last_col) + 1) * cw).min(surf_w);
        let y1 = (oy + ((r1 + 1).min(last_row) + 1) * ch).min(surf_h);
        self.add_rect(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0));
    }
    /// Restituisce `(x,y,w,h)` se non vuoto, `None` altrimenti.
    fn take(&mut self) -> Option<(u32, u32, u32, u32)> {
        if self.is_empty() { return None; }
        let r = (self.x0, self.y0, self.x1 - self.x0, self.y1 - self.y0);
        *self = Self::none();
        Some(r)
    }
}

/// Lo stato del terminale: modello, decoder, surface.
struct Term {
    grid: Grid,
    decoder: Decoder,
    surface: Surface,
    font: Option<Font>,
    /// Stato del parser ESC/CSI:
    /// 0 = normale, 1 = ESC visto, 2 = CSI entry (ESC-[ visto),
    /// 3 = accumulo parametri CSI (numeri separati da `;`).
    esc: u8,
    /// Parametri CSI accumulati (es. `"38;2;255;0;0"` o `"0"`).
    csi_params: alloc::vec::Vec<u8>,
    /// Codepoint decodificati (buffer riusato).
    pending: alloc::vec::Vec<u32>,
    /// Ultima posizione del cursore disegnata (per ridisegnarla).
    cursor_drawn: (usize, usize),
    /// Ultimo stato visibile del cursore (per non ridisegnare la cella del
    /// cursore a ogni render se nulla e' cambiato: con il danno esplicito
    /// sarebbe un trasferimento per messaggio).
    cursor_drawn_visible: bool,
    /// Colore foreground corrente (truecolor RGBA packed).
    fg: u32,
    /// Colore background corrente (truecolor RGBA packed).
    bg: u32,
    /// Canale del driver `gpu` (0 = nessun driver raggiungibile).
    driver_chan: u64,
    /// La risoluzione *desiderata* dal guest (D4): `None` = "segue l'host".
    /// Quando l'host insiste (config change) la console cede e passa a
    /// `None`.
    desired: Option<(u32, u32)>,
    /// Copia del layer 0 (sfondo immagine) in formato BGRA, dimensione
    /// `width × height × 4`. Usata per ripristinare lo sfondo sotto le
    /// celle trasparenti o vuote senza ridisegnare tutto lo schermo.
    layer0: Option<alloc::vec::Vec<u8>>,
    /// Larghezza in pixel del layer 0 (stride = width * 4, nessun padding).
    layer0_width: u32,
    /// Offset X del layer 0 centrato nel framebuffer (D4).
    layer0_offset_x: u32,
    /// Offset Y del layer 0 centrato nel framebuffer (D4).
    layer0_offset_y: u32,
    /// Background precomposto (T3, ADR-0066): fondo di pagina + layer 0 in
    /// BGRA, dimensione framebuffer, stride = `width * 4`. Il render copia
    /// una banda da qui invece di rifare fill+blend per ogni cella.
    bg_layer: Option<alloc::vec::Vec<u8>>,
    /// `true` se tutte le righe della bg layer sono byte-identiche alla riga
    /// 0: e' la condizione in cui lo scroll puo' spostare i pixel (T4).
    bg_rows_equal: bool,
    /// Lo slot DMA del framebuffer corrente (D4: il riadattamento alloca
    /// l'altro slot e libera questo solo dopo che il nuovo e' presentato —
    /// un riadattamento fallito non deve lasciare la surface su un buffer
    /// liberato).
    fb_slot: u64,
    /// Danno accumulato (D7, ADR-0066): rettangolo di pixel da trasferire.
    damage: Damage,
    // Statistiche (T0, ADR-0066): diagnostica su seriale.
    stat_renders: u64,
    stat_cells: u64,
    stat_flushes: u64,
    stat_bytes: u64,
    stat_cyc_render: u64,
    stat_cyc_flush: u64,
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
        // 5) il fondo del buffer nuovo (colore di pagina + immagine
        //    ricentrata: gli offset sono relativi alla superficie) e
        //    **trasferisci i pixel all'host**: il PRESENT ha agganciato il
        //    buffer nuovo ma il device ha visto solo gli zeri dell'allocazione
        //    (il primo FLUSH, senza questo, arriverebbe al primo tasto:
        //    schermo nero fino a li').
        self.build_bg_layer();
        self.render();
        self.flush_display_full();
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
    ///
    /// D7 (ADR-0066): trasferisce solo il rettangolo di danno, non lo schermo
    /// intero. Valvola di sicurezza: `FULL_FLUSH_ONLY = true` → si torna al
    /// comportamento pre-D7.
    fn flush_display(&mut self) {
        // T0 (ADR-0066): statistica.
        let t0 = civis::rdtsc();
        if self.driver_chan == 0 {
            return;
        }
        if !matches!(self.surface, Surface::Pixel(_)) {
            return;
        }
        // D7 (ADR-0066): valvola di sicurezza. Se FULL_FLUSH_ONLY=true si torna
        // al comportamento pre-D7 (sempre full-screen). Con danno vuoto non si
        // manda nulla (nessuna IPC).
        const FULL_FLUSH_ONLY: bool = false;
        let f = if FULL_FLUSH_ONLY {
            term::disp::Flush::Full
        } else {
            match self.damage.take() {
                Some((x, y, w, h)) => term::disp::Flush::Rect { x, y, w, h },
                None => return, // niente da trasferire
            }
        };
        let (w0, w1) = term::disp::pack_flush(f);
        let _ = civis::send(self.driver_chan, civis::syscall_numbers::GPU_DISPLAY_FLUSH, w0, w1);
        self.stat_flushes += 1; // T0 (ADR-0066)
        // Byte effettivamente trasferiti: il full e' la superficie intera
        // (width*height*4), il rect e' la sua area.
        if let Surface::Pixel(p) = &self.surface {
            self.stat_bytes += match f {
                term::disp::Flush::Full => p.width as u64 * p.height as u64 * 4,
                term::disp::Flush::Rect { w, h, .. } => w as u64 * h as u64 * 4,
            };
        } // ADR-0066
        self.stat_cyc_flush += civis::rdtsc() - t0;
        if self.stat_flushes % 64 == 0 {
            civis::println!(
                "[console] stats: renders={} cells={} flushes={} B/flush={} cyc/render={} cyc/flush={}",
                self.stat_renders,
                self.stat_cells,
                self.stat_flushes,
                self.stat_bytes / self.stat_flushes.max(1),
                self.stat_cyc_render / self.stat_renders.max(1),
                self.stat_cyc_flush / self.stat_flushes.max(1)
            );
        }
    }

    /// Forza un trasferimento **full-screen** (D7: I4 — dopo PRESENT il device
    /// deve ricevere tutti i pixel, anche con danno vuoto). Usato all'avvio e
    /// a ogni resize (I4), e da `CONSOLE_REPAINT` (T4).
    fn flush_display_full(&mut self) {
        let t0 = civis::rdtsc();
        if self.driver_chan == 0 {
            return;
        }
        if !matches!(self.surface, Surface::Pixel(_)) {
            return;
        }
        let f = term::disp::Flush::Full;
        let (w0, w1) = term::disp::pack_flush(f);
        let _ = civis::send(self.driver_chan, civis::syscall_numbers::GPU_DISPLAY_FLUSH, w0, w1);
        // Azzera il danno: un full transfer copre tutto.
        self.damage = Damage::none();
        self.stat_flushes += 1; // T0 (ADR-0066)
        if let Surface::Pixel(p) = &self.surface {
            self.stat_bytes += p.width as u64 * p.height as u64 * 4; // ADR-0066
        }
        self.stat_cyc_flush += civis::rdtsc() - t0;
        if self.stat_flushes % 64 == 0 {
            civis::println!(
                "[console] stats: renders={} cells={} flushes={} B/flush={} cyc/render={} cyc/flush={}",
                self.stat_renders,
                self.stat_cells,
                self.stat_flushes,
                self.stat_bytes / self.stat_flushes.max(1),
                self.stat_cyc_render / self.stat_renders.max(1),
                self.stat_cyc_flush / self.stat_flushes.max(1)
            );
        }
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
                // CSI: accumuliamo i parametri (numeri e `;`) fino alla
                // lettera finale del comando (es. `m`, `D`, `C`, `K`).
                let c = cp as u8;
                if c.is_ascii_digit() || c == b';' {
                    self.csi_params.push(c);
                    return;
                }
                let cmd = c;
                self.esc = 0;
                let params = core::mem::take(&mut self.csi_params);
                match cmd {
                    b'D' => {
                        // cursore a sinistra (salta l'intero glifo largo)
                        let back = self.grid.span_back(self.grid.cursor.0, self.grid.cursor.1);
                        self.grid.cursor.0 -= back;
                    }
                    b'C' => {
                        // cursore a destra
                        if self.grid.cursor.0 + 1 < self.grid.cols() {
                            self.grid.cursor.0 += 1;
                        }
                    }
                    b'K' => {
                        // cancella fino a fine riga (se il cursore e' dentro
                        // un glifo largo, sparisce lo span intero)
                        let (col, row) = self.grid.cursor;
                        self.grid.erase_from(col, row, self.fg, self.bg);
                    }
                    b'm' => {
                        // Select Graphic Rendition: colori truecolor e 256.
                        self.handle_sgr(&params);
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
                // backspace: salta l'intero glifo largo
                let back = self.grid.span_back(self.grid.cursor.0, self.grid.cursor.1);
                self.grid.cursor.0 -= back;
            }
            0x0C => {
                // form feed: pulisce tutto, cursore a casa
                self.grid.clear();
                self.grid.cursor = (0, 0);
            }
            _ => {
                // Caratteri stampabili o glifi Nerd Font (lunghi piu' celle).
                let (mut col, mut row) = self.grid.cursor;

                // Quante celle occupa il glifo (1 per i caratteri normali).
                let mut advance = 1;
                if let Some(font) = self.font.as_mut() {
                    advance = font.glyph_width_cells(cp).max(1);
                }

                // Un glifo largo che non entra in fondo riga va a capo
                // **prima** di essere disegnato: altrimenti resterebbe
                // tagliato dal bordo.
                if col + advance > self.grid.cols() {
                    col = 0;
                    if row + 1 >= self.grid.rows() {
                        self.grid.scroll_up();
                    } else {
                        row += 1;
                    }
                }
                self.grid.put_wide(col, row, cp, advance, self.fg, self.bg);

                if col + advance >= self.grid.cols() {
                    // fine riga: il cursore va a capo (e scrolla se serve)
                    self.grid.cursor.0 = 0;
                    if row + 1 >= self.grid.rows() {
                        self.grid.scroll_up();
                    } else {
                        self.grid.cursor.1 = row + 1;
                    }
                } else {
                    self.grid.cursor = (col + advance, row);
                }
            }
        }
    }

    /// Ridisegna la zona sporca e il cursore.
    ///
    /// Ottimizzazione: solo le celle dirty vengono ritoccate. Per le celle
    /// vuote (cp == 0) o con background trasparente, prima si ripristina lo
    /// sfondo dal layer 0; se manca il layer 0, si riempie col colore di
    /// background corrente (cosi' i pixel del vecchio carattere non restano
    /// impressi). Poi si disegna il glifo (se presente).
    fn render(&mut self) {
        // T0 (ADR-0066): misura i cicli per render.
        let t0 = civis::rdtsc();
        // Estraggo i dati del layer 0 come puntatore+lunghezza (tipo Copy)
        // PRIMA di qualsiasi mut-borrow su self, per evitare conflitti di
        // borrow con restore_layer0_cell(&mut self, ...).
        let l0_data = self.layer0.as_ref().map(|v| (v.as_ptr() as *const u8, v.len()));

        // T3 (ADR-0066): con la bg layer pronta il render copia una banda di
        // background (memcpy) e disegna solo l'inchiostro. Puntatore+len+stride
        // estratti prima del mut-borrow della surface (come per `l0_data`).
        let bg_data: Option<(*const u8, usize, u32)> = match (&self.bg_layer, &self.surface) {
            (Some(v), Surface::Pixel(p)) => Some((v.as_ptr(), v.len(), p.width * 4)),
            _ => None,
        };

        let dirty = self.grid.take_dirty();
        if dirty.any {
            let (cols, rows) = (self.grid.cols(), self.grid.rows());
            let r0 = dirty.min_row.min(rows.saturating_sub(1));
            let r1 = dirty.max_row.min(rows.saturating_sub(1));
            let c0 = dirty.min_col.min(cols.saturating_sub(1));
            let c1 = dirty.max_col.min(cols.saturating_sub(1));
            if let Some((bg_ptr, bg_len, bg_stride)) = bg_data {
                // Allarga il bbox agli span interi: una banda che parte in
                // mezzo a un glifo largo cancellerebbe inchiostro che nessuno
                // ridisegna (Grid marca gia' lo span, ma il bbox puo' essere
                // stato ristretto da altre celle).
                let mut bc0 = c0;
                let mut bc1 = c1;
                for row in r0..=r1 {
                    let (s0, _) = self.grid.span_at(c0, row);
                    bc0 = bc0.min(s0);
                    let (s1, l1) = self.grid.span_at(c1, row);
                    bc1 = bc1.max((s1 + l1).saturating_sub(1));
                }
                let geom = self.surface.geom();
                let bg = unsafe { core::slice::from_raw_parts(bg_ptr, bg_len) };
                for row in r0..=r1 {
                    let x0 = geom.origin_x + (bc0 as u32) * geom.cell_w;
                    let y0 = geom.origin_y + (row as u32) * geom.cell_h;
                    let bw = ((bc1 - bc0 + 1) as u32) * geom.cell_w;
                    if let Surface::Pixel(p) = &mut self.surface {
                        p.copy_from(bg, bg_stride, x0, y0, bw, geom.cell_h);
                    }
                    self.damage.add_rect(x0, y0, bw, geom.cell_h);
                    for col in bc0..=bc1 {
                        // Le continuazioni le disegna la testa dello span.
                        if self.grid.cell(col, row).width == 0 {
                            continue;
                        }
                        let is_cursor =
                            (col, row) == self.grid.cursor_clamped() && self.grid.cursor_visible;
                        self.draw_cell_at(col, row, is_cursor, l0_data, true);
                        self.stat_cells += 1; // T0 (ADR-0066)
                    }
                }
            } else {
                // Fallback (VGA o bg layer non pronta): percorso per-cella.
                for row in r0..=r1 {
                    for col in c0..=c1 {
                        // Le continuazioni le disegna la **testa** dello span:
                        // qui non si ridipinge il background sopra il glifo
                        // largo (era il bug: l'icona restava di una cella).
                        if self.grid.cell(col, row).width == 0 {
                            continue;
                        }
                        let is_cursor =
                            (col, row) == self.grid.cursor_clamped() && self.grid.cursor_visible;
                        self.draw_cell_at(col, row, is_cursor, l0_data, false);
                        self.stat_cells += 1; // T0 (ADR-0066)
                    }
                }
            }
        }
        // Il cursore si e' spostato o ha cambiato visibilita': ridisegna la
        // cella del cursore solo se lo stato e' cambiato (con il danno
        // esplicito sarebbe un trasferimento per messaggio).
        let cur = self.grid.cursor_clamped();
        let cur_visible = self.grid.cursor_visible;
        if cur != self.cursor_drawn || cur_visible != self.cursor_drawn_visible {
            // Cancella la vecchia posizione del cursore (se diversa).
            if cur != self.cursor_drawn {
                let (cc, cr) = self.cursor_drawn;
                if cc < self.grid.cols() && cr < self.grid.rows() {
                    self.draw_cell_at(cc, cr, false, l0_data, false);
                }
            }
            // Disegna il cursore nella nuova posizione.
            let (cc, cr) = cur;
            if cc < self.grid.cols() && cr < self.grid.rows() {
                self.draw_cell_at(cc, cr, cur_visible, l0_data, false);
            }
            self.cursor_drawn = cur;
            self.cursor_drawn_visible = cur_visible;
        }
        // Nel fallback VGA il cursore e' quello hardware (CRTC).
        let (cc, cr) = cur;
        if matches!(self.surface, Surface::VgaText(_)) {
            unsafe { move_hw_cursor(cr, cc) };
        }
        self.stat_renders += 1; // T0 (ADR-0066)
        self.stat_cyc_render += civis::rdtsc() - t0;
    }

    /// Riempe una cella (o uno span di celle) col suo background (nessun
    /// glifo). Usato per le celle vuote quando non c'e' un layer 0 da
    /// ripristinare.
    fn paint_cell_bg(&mut self, col: usize, row: usize, span: usize, bg: u32) {
        let surface = match &mut self.surface {
            Surface::Pixel(p) => p,
            _ => return,
        };
        let cw = surface.geom.cell_w as u32;
        let ch = surface.geom.cell_h as u32;
        let x = surface.geom.origin_x + (col as u32) * cw;
        let y = surface.geom.origin_y + (row as u32) * ch;
        let w = cw * span.max(1) as u32;
        let color = surface::rgba_to_bgra(bg);
        for yy in 0..ch {
            for xx in 0..w {
                unsafe { surface.put(x + xx, y + yy, color) };
            }
        }
    }

    /// Disegna una cella sulla surface attiva (tutto lo span del glifo).
    fn draw_cell(
        &mut self,
        col: usize,
        row: usize,
        span: usize,
        cp: u32,
        fg: u32,
        bg: u32,
        cursor: bool,
    ) {
        // Nel path VGA il font non serve (il BIOS disegna il carattere).
        let font = self.font.as_mut();
        // Passa il layer 0 a put_char: i pixel BMP sovrapposti vengono
        // copiati sopra lo sfondo prima del glifo, cosi' il testo resta
        // sopra l'immagine senza cancellarla.
        let l0 = self.layer0.as_ref().map(|v| {
            // Righe = byte / (larghezza * 4): il pixel e' BGRA.
            let l0_h = (v.len() / (self.layer0_width.max(1) as usize * 4)) as u32;
            (v.as_slice(), self.layer0_width, l0_h, self.layer0_offset_x, self.layer0_offset_y)
        });
        self.surface
            .put_char(col, row, span, cp, fg, bg, cursor, font, l0);
    }

    /// Disegna la cella (col,row): se e' una continuazione disegna la testa
    /// dello span (una cella non si ridisegna da sola in mezzo a un glifo
    /// largo). `l0_data` e' il layer 0 gia' estratto (puntatore+lunghezza).
    /// D7 (ADR-0066): dichiara il danno subito dopo aver calcolato lo span.
    ///
    /// `bg_ready` (T3): la banda di background e' gia' stata copiata dalla bg
    /// layer, quindi per le celle col fondo di pagina (bg nero opaco) basta
    /// l'inchiostro. Le altre (bg colorato/trasparente) passano dal percorso
    /// completo, che resta l'unica verita' per alpha e layer 0.
    fn draw_cell_at(
        &mut self,
        col: usize,
        row: usize,
        cursor: bool,
        l0_data: Option<(*const u8, usize)>,
        bg_ready: bool,
    ) {
        let (start, _len) = self.grid.span_at(col, row);
        let cell = self.grid.cell(start, row);
        let span = cell.width.max(1) as usize;
        // D7 (ADR-0066): dichiara il danno subito dopo aver calcolato lo span
        // (una cella toccata e' una cella cambiata), con lo sbordo di 1 cella
        // e il clip alla superficie.
        let geom = self.surface.geom();
        let (sw, sh) = self.surface.pixel_size();
        self.damage.add_cells(geom, sw, sh, start, row as u32, start + span - 1, row as u32);
        // T3: background gia' corretto dalla banda -> solo inchiostro.
        // Copre anche `cp == 0` (la banda ha gia' messo il fondo giusto).
        if bg_ready && cell.bg == color::BLACK {
            if let Surface::Pixel(p) = &mut self.surface {
                p.draw_ink(start, row, span, cell.cp, cell.fg, cursor, self.font.as_mut());
            }
            return;
        }
        // Celle vuote (cp == 0): ripristina lo sfondo dal layer 0 (pixel BMP
        // dove sovrappone, nero altrove). Niente glifo. Senza layer 0 la
        // cella va riempita col suo bg (una cella "spazio" puo' avere un
        // background colorato).
        if cell.cp == 0 {
            if let Some((ptr, len)) = l0_data {
                let l0_slice = unsafe { core::slice::from_raw_parts(ptr, len) };
                self.restore_layer0_cell(start, row, span, l0_slice);
            } else {
                self.paint_cell_bg(start, row, span, cell.bg);
            }
            return;
        }
        // Celle con carattere: put_char gestisce layer 0 (BMP come sfondo) +
        // glifo in un colpo solo.
        self.draw_cell(start, row, span, cell.cp, cell.fg, cell.bg, cursor);
    }

    /// Select Graphic Rendition (ESC[...m): truecolor (`38;2` / `48;2`) e 256
    /// colori (`38;5` / `48;5`). I parametri non riconosciuti vengono ignorati.
    ///
    /// Quando il backend e' VGA testuale (nessun alpha, 16 colori), il
    /// truecolor viene scalato all'indice VGA piu' vicino da
    /// `surface::nearest_vga`: non viene ignorato.
    fn handle_sgr(&mut self, params: &[u8]) {
        // Dividi per `;` e parse i numeri.
        let mut nums: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
        let mut cur: u32 = 0;
        let mut has_num = false;
        for &b in params {
            if b == b';' {
                if has_num {
                    nums.push(cur);
                    cur = 0;
                    has_num = false;
                }
            } else if b.is_ascii_digit() {
                cur = cur.saturating_mul(10).saturating_add((b - b'0') as u32);
                has_num = true;
            }
        }
        if has_num {
            nums.push(cur);
        }

        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => {
                    // Reset a default.
                    self.fg = color::DEFAULT_FG;
                    self.bg = color::DEFAULT_BG;
                }
                38 => {
                    // Foreground: il token successivo specifica il tipo.
                    if i + 1 < nums.len() {
                        match nums[i + 1] {
                            2 => {
                                // truecolor: R G B seguono.
                                if i + 4 < nums.len() {
                                    self.fg = surface::rgba(
                                        nums[i + 2] as u8,
                                        nums[i + 3] as u8,
                                        nums[i + 4] as u8,
                                        0xFF,
                                    );
                                    i += 4;
                                }
                            }
                            5 => {
                                // 256 colori: un indice segue.
                                if i + 2 < nums.len() {
                                    self.fg = surface::color256(nums[i + 2] as u8);
                                    i += 2;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                48 => {
                    // Background: come 38 ma per bg.
                    if i + 1 < nums.len() {
                        match nums[i + 1] {
                            2 => {
                                if i + 4 < nums.len() {
                                    self.bg = surface::rgba(
                                        nums[i + 2] as u8,
                                        nums[i + 3] as u8,
                                        nums[i + 4] as u8,
                                        0xFF,
                                    );
                                    i += 4;
                                }
                            }
                            5 => {
                                if i + 2 < nums.len() {
                                    self.bg = surface::color256(nums[i + 2] as u8);
                                    i += 2;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Ridisegna il fondo dell'intero framebuffer: il **colore di pagina**
    /// (anche nei margini fuori dalla griglia), poi il layer 0 (BMP) centrato,
    /// Costruisce la **bg layer** (T3, ADR-0066): fondo di pagina + immagine
    /// centrata, composti una volta sola in un buffer BGRA grande quanto il
    /// framebuffer. Il render poi copia una banda da qui (memcpy) invece di
    /// rifare fill+blend per ogni cella.
    ///
    /// Calcola e memorizza gli offset di centratura per `restore_layer0_cell`
    /// (cambiano al resize: gli offset sono relativi alla superficie).
    fn build_bg_layer(&mut self) {
        let (w_fb, h_fb, stride) = match &self.surface {
            Surface::Pixel(p) => (p.width, p.height, p.stride),
            Surface::VgaText(_) => return,
        };
        let page = surface::rgba_to_bgra(color::BLACK);
        let bg_stride = w_fb * 4;
        let mut v = alloc::vec![0u8; (bg_stride as usize) * (h_fb as usize)];
        // Fondo di pagina a parole (u32).
        {
            let words = v.len() / 4;
            let ptr = v.as_mut_ptr() as *mut u32;
            for i in 0..words {
                unsafe { core::ptr::write(ptr.add(i), page) };
            }
        }
        // Immagine centrata (D4): offset relativi alla superficie.
        let w_img = self.layer0_width as usize;
        let (ptr, len) = match self.layer0.as_ref() {
            Some(v) => (v.as_ptr() as *const u8, v.len()),
            None => (core::ptr::null(), 0),
        };
        let mut ox = 0u32;
        let mut oy = 0u32;
        let mut h_img = 0usize;
        if w_img > 0 && len > 0 {
            h_img = len / (w_img * 4);
            ox = ((w_fb as usize).saturating_sub(w_img) / 2) as u32;
            oy = ((h_fb as usize).saturating_sub(h_img) / 2) as u32;
            self.layer0_offset_x = ox;
            self.layer0_offset_y = oy;
            let layer0 = unsafe { core::slice::from_raw_parts(ptr, len) };
            let n = (w_img as u32).min(w_fb.saturating_sub(ox));
            if n > 0 {
                for yy in 0..h_img {
                    let sy = oy as usize + yy;
                    if sy >= h_fb as usize {
                        break;
                    }
                    let off = yy * w_img * 4;
                    surface::Pixel::blend_row_bgra(
                        &mut v,
                        bg_stride,
                        ox,
                        sy as u32,
                        n,
                        &layer0[off..off + n as usize * 4],
                    );
                }
            }
        }
        self.bg_rows_equal = rows_all_equal(&v, bg_stride);
        let _ = civis::println!(
            "[console] layer 0: fb={}x{} img={}x{} offset=({},{}) stride={} bg_rows_equal={}",
            w_fb, h_fb, w_img, h_img, ox, oy, stride, self.bg_rows_equal
        );
        // Copia nella surface (il buffer resta come bg layer per il render).
        if let Surface::Pixel(p) = &mut self.surface {
            p.copy_from(&v, bg_stride, 0, 0, w_fb, h_fb);
        }
        self.bg_layer = Some(v);
    }

    /// Ripristina una cella (o uno span di celle) dal layer 0: prima il colore
    /// di pagina su tutta la zona (cancella il carattere precedente e i
    /// margini fuori dall'immagine), poi l'overlap del BMP composto con il
    /// suo alpha.
    fn restore_layer0_cell(&mut self, col: usize, row: usize, span: usize, layer0: &[u8]) {
        let bg = surface::rgba_to_bgra(color::BLACK);
        let w_img = self.layer0_width;
        let ox = self.layer0_offset_x;
        let oy = self.layer0_offset_y;
        let h_img = if w_img == 0 {
            0
        } else {
            (layer0.len() / (w_img as usize * 4)) as u32
        };
        let surface = match &mut self.surface {
            Surface::Pixel(p) => p,
            _ => return,
        };
        let cw = surface.geom.cell_w as u32;
        let ch = surface.geom.cell_h as u32;
        let x = surface.geom.origin_x + (col as u32) * cw;
        let y = surface.geom.origin_y + (row as u32) * ch;
        let span_px = cw * span.max(1) as u32;

        // Fondo di pagina (clippato alla superficie).
        let w = span_px.min(surface.width.saturating_sub(x));
        let h = ch.min(surface.height.saturating_sub(y));
        surface.fill_rect(x, y, w, h, bg);

        // Overlap con il BMP, riga per riga (l'offset sorgente e' in BYTE:
        // `w_img * 4` per riga — senza il `* 4` si copiava bene solo la prima
        // riga e il resto sbavava in verticale).
        let overlap_x_start = x.max(ox);
        let overlap_x_end = (x + span_px).min(ox + w_img);
        let overlap_y_start = y.max(oy);
        let overlap_y_end = (y + ch).min(oy + h_img);
        if overlap_x_start < overlap_x_end && overlap_y_start < overlap_y_end {
            let n = overlap_x_end - overlap_x_start;
            for sy in overlap_y_start..overlap_y_end {
                if sy >= surface.height {
                    break;
                }
                let sy0 = (sy - oy) as usize;
                let src_off = sy0 * w_img as usize * 4 + ((overlap_x_start - ox) as usize) * 4;
                let bytes = n as usize * 4;
                if src_off + bytes > layer0.len() {
                    break;
                }
                surface.blend_bgra_span(
                    overlap_x_start,
                    sy,
                    n,
                    &layer0[src_off..src_off + bytes],
                );
            }
        }
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
    let grid = Grid::new(geom.cols, geom.rows);
    // Nessuna modifica: Grid::new pone il cursore a (0, 0) —
    // il testo scorre dall'alto verso il basso (nuove righe in fondo).

    let mut term = Term {
        grid,
        decoder: Decoder::new(),
        surface,
        font,
        esc: 0,
        csi_params: alloc::vec::Vec::new(),
        pending: alloc::vec::Vec::with_capacity(64),
        cursor_drawn: (0, 0),
        cursor_drawn_visible: false,
        fg: color::DEFAULT_FG,
        bg: color::DEFAULT_BG,
        driver_chan,
        desired: None,
        layer0: None,
        layer0_width: 0,
        layer0_offset_x: 0,
        layer0_offset_y: 0,
        bg_layer: None,
        bg_rows_equal: false,
        fb_slot: civis::DMA_SLOT_DEVICE,
        // D7 (ADR-0066): nessun danno iniziale.
        damage: Damage::none(),
        // T0: statistica (ADR-0066).
        stat_renders: 0, stat_cells: 0, stat_flushes: 0,
        stat_bytes: 0, stat_cyc_render: 0, stat_cyc_flush: 0,
    };

    // Lo sfondo e' un layer: caricato PRIMA del banner, altrimenti il testo
    // resterebbe sotto l'immagine. Senza `/bg.bmp` resta il colore di pagina.
    match load_bmp("/bg.bmp") {
        Some((l0, w, h)) => {
            let _ = civis::println!("[console] layer 0: {}x{} bg loaded OK", w, h);
            term.layer0_width = w;
            term.layer0 = Some(l0);
        }
        None => {
            let _ = civis::println!(
                "[console] layer 0: /bg.bmp non trovato o non valido, solo colore di pagina"
            );
        }
    }
    // Fondo: colore di pagina + immagine centrata (se c'e').
    term.build_bg_layer();
    term.flush_display_full();

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
            hex[i * 2] = hex::encode_nibble(digest[i] >> 4);
            hex[i * 2 + 1] = hex::encode_nibble(digest[i] & 0x0F);
        }
        let s = core::str::from_utf8(&hex).unwrap_or("?");
        civis::println!("[console] fb blake2s={}", s);
    }
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

/// Carica un BMP 32-bit non compresso e lo restituisce in formato BGRA,
/// con le righe ribaltate (il BMP è bottom-up, lo schermo è top-down).
fn load_bmp(path: &str) -> Option<(alloc::vec::Vec<u8>, u32, u32)> {
    let fd = civis::open(path, 0).ok()?;
    let mut data = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = civis::read_fs(fd, &mut buf, 4096).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let _ = civis::close(fd);

    if data.len() < 14 {
        return None;
    }
    if &data[0..2] != b"BM" {
        return None;
    }

    let pixel_offset =
        u32::from_le_bytes([data[10], data[11], data[12], data[13]]) as usize;
    let dib_size =
        u32::from_le_bytes([data[14], data[15], data[16], data[17]]) as usize;
    if dib_size < 40 {
        return None;
    }

    let width = u32::from_le_bytes([data[18], data[19], data[20], data[21]]);
    let height = u32::from_le_bytes([data[22], data[23], data[24], data[25]]);
    let bpp = u16::from_le_bytes([data[28], data[29]]);
    let compression =
        u32::from_le_bytes([data[30], data[31], data[32], data[33]]);

    if bpp != 32 {
        return None;
    }
    if compression != 0 && compression != 3 {
        return None;
    }

    let row_size = width as usize * 4;
    let total = width as usize * height as usize * 4;
    if data.len() < pixel_offset + total {
        return None;
    }

    let mut out = alloc::vec![0u8; total];
    // Il BMP memorizza le righe bottom-up: la prima riga del file è la più
    // in basso sullo schermo. Ribaltiamo durante la copia.
    for y in 0..height {
        let src_row = pixel_offset + (height - 1 - y) as usize * row_size;
        let dst_row = y as usize * row_size;
        out[dst_row..dst_row + row_size]
            .copy_from_slice(&data[src_row..src_row + row_size]);
    }

    Some((out, width, height))
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
