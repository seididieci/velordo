//! console — il **console server** (D3, ADR-0060).
//!
//! Due cose in un servizio:
//! - **il terminale**: modello a celle (UTF-8), font TrueType rasterizzato a
//!   runtime, e le surface che mettono i caratteri sullo schermo (framebuffer
//!   pixel o VGA testuale);
//! - **le sessioni di terminale** (`littera`, D8c1/D8c2): il testo arriva
//!   dall'endpoint di sessione (`SESS_*`), il **seriale** ne e' il mirror (il
//!   canale live del gate e del debug headless). Nessun device di testo:
//!   `/dev/console` non esiste piu'.
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
use term::grid::{color, Cell};
use term::surface::{self, Geom, Pixel, Surface, VgaText};

use littera::{Sessions, Step};
use musivum;

// Tag delle finestre del DE (D8b, ADR-0088): il servizio console le gestisce
// direttamente; i clienti le usano tramite `civis::win`.
use civis::{WIN_CLOSE, WIN_COMMIT, WIN_OPEN, WIN_RAISE, WIN_SET_RECT, WIN_SET_SURFACE, WIN_STATS};
// Tag delle sessioni di terminale (D8c1, ADR-0089).
use civis::{
    SESS_ATTACH_DEFAULT, SESS_BIND, SESS_CLOSE, SESS_CREATE, SESS_DUMP, SESS_ECHO, SESS_FOCUS,
    SESS_INFO, SESS_READ, SESS_RESIZE, SESS_STATE, SESS_STATS, SESS_TEST_INPUT, SESS_WHOAMI,
    SESS_WRITE,
};

/// Valore di errore IPC.
const ERR: u64 = !0u64;

/// D8c1 — il numero di byte della mailbox di sessione (una pagina, vedi
/// `sess-proto`): identico al `MAILBOX_BYTES` che il client mappa/smappa.
const SESS_MAILBOX_BYTES: usize = sess_proto::MAILBOX_BYTES;

/// D8c2 — tetto del mirror seriale **per sessione** (rete di sicurezza: una
/// write mirrorata da un client senza budget, es. la personalita' POSIX, non
/// deve poter sommergere la UART). Il budget *per comando* (256 KiB) resta
/// della shell — e' lei che sa cosa e' un comando.
const MIRROR_SESSION_MAX: usize = 1024 * 1024;

/// D8c2 — avviso di tetto raggiunto (una volta per sessione).
const MIRROR_TRUNCATED_MSG: &[u8] = b"\n[mirror seriale: tetto di sessione raggiunto]\n";

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

/// T4 (ADR-0066): valvola di sicurezza. `true` = non si fa MAI lo shift dei
/// pixel (si ricompone sempre): e' il comportamento pre-D7.
const NO_SHIFT: bool = false;

/// T4: un feed piu' lungo di questa soglia e' "in blocco" (output di un
/// comando). Lo scroll ricompone invece di spostare i pixel: con il render
/// differito a fine feed i pixel intermedi non esistono, e uno shift di N
/// righe costerebbe N memcpy dell'intera griglia.
///
/// La soglia deve stare sopra il prompt della shell nativa (ANSI + orologio,
/// ~80 B) e sotto un blocco di output (4 KiB): 256 B separa i due casi.
const BULK_FEED_BYTES: usize = 256;

/// T4/bulk: in un feed "in blocco" il render+flush e' differito ogni N
/// write. Senza un timer nel kernel (niente `recv` con timeout) il batch
/// finale lo flusha la prima write interattiva successiva (il prompt), che
/// rende subito. Un output lungo passa da N flush dell'intera griglia a N/8.
const BULK_FLUSH_EVERY: u32 = 8;

/// D8b — dimensione massima della superficie di una finestra ospite. Lo shm
/// del kernel ha un tetto di 64 frame (256 KiB); oltre, `shm_create` fallisce.
const WIN_SURFACE_MAX: usize = 64 * 4096;

// ── Il terminale ───────────────────────────────────────────────────

/// Privilegio >= ThirdParty (il DE e i servizi firmati): l'unico check che
/// apre le operazioni "da DE" (`SESS_BIND`, sessioni non legate, ...).
fn privileged(class: civis::ImageClass) -> bool {
    matches!(
        class,
        civis::ImageClass::ThirdParty | civis::ImageClass::System
    )
}

/// Il rettangolo di pixel di un blocco di celle, con lo sbordo di 1 cella
/// (I3: il glifo puo' uscire dalla cella) e il clip alla superficie.
/// `c0..=c1` / `r0..=r1` sono indici di cella inclusivi.
///
/// Il danno accumulato e' del compositor (`musivum::Damage`): qui si calcola
/// solo la geometria, che dipende dal font e dalla griglia.
fn cells_rect(
    geom: term::surface::Geom,
    surf_w: u32,
    surf_h: u32,
    c0: usize,
    r0: u32,
    c1: usize,
    r1: u32,
) -> musivum::Rect {
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
    musivum::Rect::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

/// Il **display + server** del terminale. Il testo (modello, parser ANSI,
/// scrollback, input, eco) vive nelle **sessioni** (`littera`): qui resta
/// tutto cio' che serve a mostrare la sessione in focus e a servire la
/// capability di sessione.
struct Term {
    /// Le sessioni (1:1 con le tile del DE): la 0 e' lo schermo. In D8c1 si
    /// disegna solo quella in focus.
    sessions: Sessions,
    /// Mailbox **per attachment** create dal server (owner): `(canale, pid,
    /// shm_id, ptr, map_len)`. La chiave e' il **canale**, non la sessione
    /// (D8c2): due clienti della stessa sessione hanno buffer distinti, come
    /// il ring FS e' per processo. Con un buffer condiviso la write di uno
    /// puo' essere sovrascritta fra il suo "copia" e il "copia" del server.
    /// La mappatura **RW** serve a leggere la OUT e scrivere la IN; va
    /// smappata (a pagina) prima di `shm_destroy`.
    sess_mailboxes: alloc::vec::Vec<(u64, i64, u32, *mut u8, usize)>,
    /// D8c2 — mirror seriale: budget residuo **per sessione** (T4/bulk) e
    /// "gia' troncato" (l'avviso una volta sola). Lo consuma la sessione in
    /// focus: e' il canale live del gate e del debug headless.
    mirror_left: [usize; littera::MAX_SESSIONS],
    mirror_truncated: [bool; littera::MAX_SESSIONS],
    surface: Surface,
    font: Option<Font>,
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
    /// Il compositor del DE (D8a): possiede il fondo (wallpaper) e il danno.
    /// Con una tile sola non compone nulla: la tile scrive direttamente nella
    /// surface del device (`Backing::Direct`) e il compositor si limita a
    /// sottrarne l'area al fondo.
    comp: musivum::Compositor,
    /// L'id della tile del terminale nel compositor (registrata all'avvio).
    tile: u32,
    /// Finestre ospite aperte dai clienti (D8b): (win_id, canale richiedente, pid owner).
    /// Serve per chiudere tutte le finestre di un processo alla sua morte (EXIT_NOTIFY).
    win_owner: alloc::vec::Vec<(u32, u64, i64)>,
    /// Superfici delle finestre ospite (shm create dal server, mappate RO):
    /// (win_id, shm_id, ptr, map_len, w, h). `map_len` e' la lunghezza
    /// mappata **a pagine** (per `munmap` alla chiusura).
    win_surfaces: alloc::vec::Vec<(u32, u32, *mut u8, usize, u32, u32)>,
    /// `true` se tutte le righe della bg layer sono byte-identiche alla riga
    /// 0: e' la condizione in cui lo scroll puo' spostare i pixel (T4).
    bg_rows_equal: bool,
    /// T4: valvola di sicurezza (default `NO_SHIFT`). `true` = mai shift.
    no_shift: bool,
    /// T4 (solo test): bg layer uniforme (senza immagine) per esercitare lo
    /// shift anche con il wallpaper presente.
    test_uniform_bg: bool,
    /// Lo slot DMA del framebuffer corrente (D4: il riadattamento alloca
    /// l'altro slot e libera questo solo dopo che il nuovo e' presentato —
    /// un riadattamento fallito non deve lasciare la surface su un buffer
    /// liberato).
    fb_slot: u64,
    // Statistiche (T0, ADR-0066): diagnostica su seriale.
    stat_renders: u64,
    stat_cells: u64,
    stat_flushes: u64,
    stat_bytes: u64,
    stat_cyc_render: u64,
    stat_cyc_flush: u64,
}

impl Term {
    /// Larghezza (celle) del glifo `cp`: 1 senza font (host/VGA), il valore
    /// del font per le icone Nerd (glifi lunghi piu' celle).
    fn glyph_advance(&mut self, cp: u32) -> usize {
        match self.font.as_mut() {
            Some(f) => f.glyph_width_cells(cp).max(1),
            None => 1,
        }
    }

    /// D8c2 (ADR-0090) — **mirror seriale** del server: la sessione in focus
    /// e' il canale live nudo su cui il gate fa regex byte-exact. Stessa
    /// chiamata che facevano i client (`print_string` = `SYS_WRITE` raw),
    /// quindi gli stessi byte; ora pero' da un posto solo (niente doppie
    /// scritture). `MIRROR_SESSION_MAX` e' una rete di sicurezza per sessione:
    /// il budget *per comando* resta del client che sa cosa e' un comando.
    fn mirror_serial(&mut self, idx: usize, bytes: &[u8]) {
        if idx != self.sessions.focus || idx >= self.mirror_left.len() {
            return;
        }
        if self.mirror_left[idx] > 0 {
            let n = bytes.len().min(self.mirror_left[idx]);
            let _ = civis::print_string(&bytes[..n]);
            self.mirror_left[idx] -= n;
            if n == bytes.len() {
                return;
            }
        }
        if !self.mirror_truncated[idx] {
            self.mirror_truncated[idx] = true;
            let _ = civis::print_string(MIRROR_TRUNCATED_MSG);
        }
    }

    /// Manda i byte alla sessione `idx`: decode -> modello (con scroll) ->
    /// render+flush **solo se la sessione e' quella disegnata**. `mirror` =
    /// la write entra nel canale seriale (D8c2): eco, ridisegni e test no
    /// (`SESS_WRITE.w1 = 1`).
    fn feed_session(&mut self, idx: usize, bytes: &[u8], mirror: bool) {
        if mirror {
            self.mirror_serial(idx, bytes);
        }
        // T4: un feed lungo e' output di un comando (blocco): lo scroll
        // ricompone invece di spostare i pixel (vedi `scroll_session`).
        let bulk = bytes.len() > BULK_FEED_BYTES;
        let mut cps = match self.sessions.get_mut(idx) {
            Some(s) => {
                s.bulk_feed = bulk;
                s.decode(bytes)
            }
            None => return,
        };
        // `drain` svuota il buffer riusandone la capacita': i codepoint
        // processati NON devono restare nel buffer (il feed successivo li
        // rielaborerebbe, ripetendo gli a-capo e facendo scorrere la griglia).
        for cp in cps.drain(..) {
            self.put_cp(idx, cp);
        }
        if let Some(s) = self.sessions.get_mut(idx) {
            s.bulk_feed = false;
            s.give_pending(cps);
        }
        // Solo la sessione in focus ha pixel da aggiornare.
        if idx != self.sessions.focus {
            return;
        }
        let bw = match self.sessions.get_mut(idx) {
            Some(s) => s.bulk_writes,
            None => return,
        };
        if bulk {
            // T4/bulk: render+flush differiti ogni N write. Il batch finale
            // lo flusha la write interattiva successiva (il prompt).
            let next = bw + 1;
            if next >= BULK_FLUSH_EVERY {
                if let Some(s) = self.sessions.get_mut(idx) {
                    s.bulk_writes = 0;
                }
                self.render();
                self.flush_display();
            } else if let Some(s) = self.sessions.get_mut(idx) {
                s.bulk_writes = next;
            }
        } else {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.bulk_writes = 0;
            }
            self.render();
            // Il path non-virgl di QEMU copia la risorsa nella superficie del
            // display **solo al flush**: senza, lo schermo resta nero.
            self.flush_display();
        }
    }

    /// Un codepoint nelle sessione `idx`. Se il modello chiede uno scroll,
    /// lo esegue `scroll_session` (pixel + modello) e si riprova **con lo
    /// stesso codepoint**: la sessione ricorda l'azione da completare.
    fn put_cp(&mut self, idx: usize, cp: u32) {
        let advance = self.glyph_advance(cp);
        loop {
            let step = match self.sessions.get_mut(idx) {
                Some(s) => s.put(cp, advance),
                None => return,
            };
            match step {
                Step::Done => return,
                Step::NeedScroll => self.scroll_session(idx),
            }
        }
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
        // D8c1: il riadattamento del display ridimensiona **la sessione 0**
        // (lo schermo). Le altre sessioni hanno la misura della loro tile:
        // arrivera' con le tile multiple (D8e).
        self.sessions.cols = grid_geom.cols;
        self.sessions.rows = grid_geom.rows;
        if let Some(s) = self.sessions.get_mut(0) {
            s.resize(grid_geom.cols, grid_geom.rows);
        }
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
    /// lo sta bloccando mentre serve una sessione (una scrittura FS qui
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
            // D8a: il compositor compone il danno — con una tile `Direct` e'
            // una sottrazione (i pixel della tile sono gia' nella surface) —
            // e restituisce il rettangolo da trasferire.
            let dst = match &self.surface {
                Surface::Pixel(p) => {
                    musivum::Surface::new(p.ptr, p.width, p.height, p.stride)
                }
                Surface::VgaText(_) => return,
            };
            match self.comp.compose(&dst) {
                Some(r) => term::disp::Flush::Rect { x: r.x, y: r.y, w: r.w, h: r.h },
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
        self.comp.clear_damage();
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

    /// D8b — ripristina il fondo e ridisegna le celle del terminale sotto un
    /// rect **in pixel** (`screen`). La tile e' `Backing::Direct`: il
    /// compositor non sa ricostruire i pixel del terminale, quindi quando una
    /// finestra si chiude (o cambia area) l'area va riscritta dal wallpaper e
    /// dal modello a celle. Il guest viene poi ricomposto sopra da
    /// `flush_display`.
    ///
    /// Il ripristino del fondo e' per **pixel** (non per celle): la griglia e'
    /// centrata (`origin_y` puo' essere 4), quindi i margini del rect non
    /// cadono su nessuna cella e la sola banda di celle lascerebbe li' i pixel
    /// del guest.
    fn repaint_tile_area(&mut self, screen: musivum::Rect) {
        let (sw, sh) = self.surface.pixel_size();
        if sw == 0 || sh == 0 || screen.is_empty() {
            return;
        }
        let r = screen.intersect(musivum::Rect::new(0, 0, sw, sh));
        if r.is_empty() {
            return;
        }

        // 1. il fondo (wallpaper) sull'intero rect in pixel, margini inclusi.
        let wp = self.comp.wallpaper().map(|w| {
            (w.data().as_ptr(), w.data().len(), w.stride())
        });
        let page = surface::rgba_to_bgra(color::BLACK);
        if let Surface::Pixel(p) = &mut self.surface {
            match wp {
                Some((ptr, len, stride)) => {
                    let bg = unsafe { core::slice::from_raw_parts(ptr, len) };
                    p.copy_from(bg, stride, r.x, r.y, r.w, r.h);
                }
                None => p.fill_rect(r.x, r.y, r.w, r.h, page),
            }
        }

        // 2. le celle sotto il rect (inchiostro + bg espliciti).
        let geom = self.surface.geom();
        let (cw, ch) = (geom.cell_w as u32, geom.cell_h as u32);
        if cw == 0 || ch == 0 {
            return;
        }
        let (ox, oy) = (geom.origin_x, geom.origin_y);
        let x1 = r.right().saturating_sub(1);
        let y1 = r.bottom().saturating_sub(1);
        let c0 = r.x.saturating_sub(ox) / cw;
        let c1 = x1.saturating_sub(ox) / cw;
        let r0 = r.y.saturating_sub(oy) / ch;
        let r1 = y1.saturating_sub(oy) / ch;
        if let Some(s) = self.sessions.focus_mut() {
            s.grid.mark_rect(c0 as usize, r0 as usize, c1 as usize, r1 as usize);
        }
        self.render();

        // 3. il danno copre il rect ripristinato (il compositor poi ricompone
        //    le finestre sopra).
        self.comp.add_damage(r);
    }

    /// D8b — ricompone la tile sotto una finestra e trasferisce.
    fn refresh_windows(&mut self, screen: musivum::Rect) {
        self.repaint_tile_area(screen);
        self.flush_display();
    }

    /// D8b — chiude una finestra: libera la superficie (shm) e la toglie dal
    /// compositor, poi ridisegna il terminale sotto il suo vecchio rect.
    fn close_window(&mut self, win: u32) {
        let old = self.comp.get(win).map(|w| w.visible_rect());
        if let Some(pos) = self.win_owner.iter().position(|(id, _, _)| *id == win) {
            self.win_owner.remove(pos);
        }
        // Prima si smappa (il compositor non deve piu' leggere quei frame),
        // poi si distrugge: senza il `munmap` il riferimento della mappatura
        // resterebbe appeso e i frame non verrebbero mai liberati.
        if let Some(spos) = self.win_surfaces.iter().position(|(id, _, _, _, _, _)| *id == win) {
            let (_id, shm_id, ptr, map_len, _w, _h) = self.win_surfaces.remove(spos);
            let _ = civis::munmap(ptr as usize, map_len);
            let _ = civis::shm_destroy(shm_id);
        }
        let _ = self.comp.close(win);
        if let Some(r) = old {
            self.refresh_windows(r);
        }
    }

    /// D8b — chiude tutte le finestre del processo `pid` (morte dell'owner).
    fn close_windows_of(&mut self, pid: i64) {
        // Al massimo `MAX_WINDOWS` finestre: il ciclo non puo' girare a vuoto.
        for _ in 0..musivum::MAX_WINDOWS {
            let win = match self
                .win_owner
                .iter()
                .find(|(_, _, p)| *p == pid)
                .map(|(id, _, _)| *id)
            {
                Some(w) => w,
                None => break,
            };
            self.close_window(win);
        }
    }

    // ── Sessioni di terminale (D8c1) e mailbox per attachment (D8c2) ──

    /// Il puntatore (RW) alla mailbox del **canale** `chan`: un attachment,
    /// un buffer. Mai condiviso con un altro cliente della stessa sessione.
    fn sess_mb(&self, chan: u64) -> Option<*mut u8> {
        self.sess_mailboxes
            .iter()
            .find(|(c, _, _, _, _)| *c == chan)
            .map(|(_, _, _, ptr, _)| *ptr)
    }

    /// La mailbox del canale, creandola al primo attach. Ritorna lo `shm` da
    /// consegnare al client, o `None` — fail-loud: il chiamante risponde `!0`,
    /// mai un id che il client non puo' mappare.
    fn ensure_mailbox(&mut self, chan: u64, pid: i64) -> Option<u32> {
        if let Some((_, _, shm, _, _)) = self.sess_mailboxes.iter().find(|(c, ..)| *c == chan) {
            return Some(*shm);
        }
        let shm = civis::shm_create(SESS_MAILBOX_BYTES).ok()?;
        let va = match civis::shm_map(shm, 0, civis::PROT_READ | civis::PROT_WRITE) {
            Ok(v) => v,
            Err(_) => {
                let _ = civis::shm_destroy(shm);
                return None;
            }
        };
        self.sess_mailboxes
            .push((chan, pid, shm, va as *mut u8, SESS_MAILBOX_BYTES));
        Some(shm)
    }

    /// Smonta e distrugge la mailbox di un canale. Si smappa **a pagina**
    /// prima di `shm_destroy`: senza, il riferimento della mappatura resta
    /// appeso e i frame non tornano mai (lezione D8b: `[munmap]` nel gate).
    fn drop_mailbox(&mut self, chan: u64) {
        if let Some(pos) = self
            .sess_mailboxes
            .iter()
            .position(|(c, _, _, _, _)| *c == chan)
        {
            let (_, _, shm, ptr, len) = self.sess_mailboxes.remove(pos);
            let _ = civis::munmap(ptr as usize, len);
            let _ = civis::shm_destroy(shm);
        }
    }

    /// Morte di un processo: chiude le sessioni di cui era owner (mai la 0) e
    /// libera le mailbox dei suoi attachment. Le sessioni si chiudono per id
    /// (`purge_pid` le elenca), le mailbox per **pid**: alla notifica il
    /// canale e' gia' invalidato e non si puo' piu' risalire al peer.
    fn purge_sessions_of(&mut self, pid: i64) {
        for id in self.sessions.purge_pid(pid) {
            let _ = self.sessions.close(id);
        }
        while let Some(pos) = self.sess_mailboxes.iter().position(|(_, p, ..)| *p == pid) {
            let (_, _, shm, ptr, len) = self.sess_mailboxes.remove(pos);
            let _ = civis::munmap(ptr as usize, len);
            let _ = civis::shm_destroy(shm);
        }
    }

    /// La classe dell'immagine del chiamante (mai una dichiarazione del peer).
    fn peer_class(&self, chan: u64) -> civis::ImageClass {
        civis::peer_info_class(chan)
            .map(|(_, c)| c)
            .unwrap_or(civis::ImageClass::Untrusted)
    }

    /// Il chiamante puo' parlare alla sessione `sess`: ci e' attaccato, oppure
    /// e' privilegiato.
    fn sess_allowed(&self, chan: u64, sess: u32, class: civis::ImageClass) -> bool {
        self.sessions.attached(chan) == Some(sess as usize) || privileged(class)
    }

    /// Area della griglia in pixel: `(x, y, w, h)` clippata alla surface.
    fn grid_px_rect(&self) -> (u32, u32, u32, u32) {
        let g = self.surface.geom();
        let (sw, sh) = self.surface.pixel_size();
        let x = g.origin_x;
        let y = g.origin_y;
        let w = (g.cols as u32 * g.cell_w).min(sw.saturating_sub(x));
        let h = (g.rows as u32 * g.cell_h).min(sh.saturating_sub(y));
        (x, y, w, h)
    }

    /// CRC32 del framebuffer visibile (T4, solo test): righe `width*4` byte,
    /// lo stride padding non si hasha. Ritorna `(crc, byte hashati)`.
    fn frame_crc(&self) -> (u32, usize) {
        let Surface::Pixel(p) = &self.surface else {
            return (0, 0);
        };
        let row_bytes = p.width as usize * 4;
        let mut crc = 0xFFFF_FFFFu32;
        let mut hashed = 0usize;
        for y in 0..p.height as usize {
            let row = unsafe {
                core::slice::from_raw_parts(
                    (p.ptr as *const u8).add(y * p.stride as usize),
                    row_bytes,
                )
            };
            for &b in row {
                crc ^= b as u32;
                crc = crc.wrapping_mul(0x0100_0193);
            }
            hashed += row_bytes;
        }
        (crc, hashed)
    }

    /// CRC32 di una riga di celle (T4, solo test): `cell_h` pixel rows a
    /// partire da `origin_y + row*cell_h`, `width*4` byte per riga.
    fn row_crc(&self, row: usize) -> (u32, usize) {
        let Surface::Pixel(p) = &self.surface else {
            return (0, 0);
        };
        let g = p.geom;
        if row >= g.rows {
            return (0, 0);
        }
        let y0 = g.origin_y as usize + row * g.cell_h as usize;
        let row_bytes = p.width as usize * 4;
        let mut crc = 0xFFFF_FFFFu32;
        let mut hashed = 0usize;
        for yy in 0..g.cell_h as usize {
            let y = y0 + yy;
            if y >= p.height as usize {
                break;
            }
            let row = unsafe {
                core::slice::from_raw_parts(
                    (p.ptr as *const u8).add(y * p.stride as usize),
                    row_bytes,
                )
            };
            for &b in row {
                crc ^= b as u32;
                crc = crc.wrapping_mul(0x0100_0193);
            }
            hashed += row_bytes;
        }
        (crc, hashed)
    }

    /// Scroll della sessione `idx` (T4, ADR-0066).
    ///
    /// Se la sessione e' **disegnata** e lo sfondo e' riga-invariante e il feed
    /// e' interattivo, sposta i pixel; altrimenti ricompone il modello
    /// (`mark_all`) e il render successivo ridisegna tutto.
    ///
    /// Lo shift e' possibile solo se i pixel sono aggiornati: per questo il
    /// feed "in blocco" (output di un comando) ricompone — il render e'
    /// differito a fine feed e i pixel intermedi non esistono.
    fn scroll_session(&mut self, idx: usize) {
        let visible = idx == self.sessions.focus;
        let bulk = self.sessions.get(idx).map(|s| s.bulk_feed).unwrap_or(false);
        let can_shift = visible
            && !self.no_shift
            && !bulk
            && self.bg_rows_equal
            && self.comp.wallpaper().is_some()
            && matches!(self.surface, Surface::Pixel(_));
        if !can_shift {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.grid.scroll_up();
            }
            return;
        }
        // I6: prima si disegna il pendente (i pixel del testo appena scritto
        // devono esistere prima di essere spostati).
        // I6b: il cursore non deve finire nello shift (diventerebbe un blocco
        // fantasma una riga sopra): lo si cancella prima, ridisegnando la sua
        // cella con cursor_visible = false, poi si ripristina il flag.
        let cursor_was_visible = match self.sessions.get(idx) {
            Some(s) => s.grid.cursor_visible,
            None => return,
        };
        if let Some(s) = self.sessions.get_mut(idx) {
            s.grid.cursor_visible = false;
        }
        self.render();
        if let Some(s) = self.sessions.get_mut(idx) {
            s.grid.cursor_visible = cursor_was_visible;
        }
        let (gx, gy, gw, gh) = self.grid_px_rect();
        let cell_h = self.surface.geom().cell_h;
        if let Surface::Pixel(p) = &mut self.surface {
            // `h` = tutta la griglia: la funzione lascia invariate le ultime
            // `dy` righe (la nuova ultima riga di celle, che il renderer
            // ridisegna). Passare `gh - cell_h` lascerebbe ferme DUE righe.
            p.shift_rows_up(gx, gy, gw, gh, cell_h);
        }
        if let Some(s) = self.sessions.get_mut(idx) {
            s.grid.scroll_up_after_shift();
        }
        self.comp.add_damage(musivum::Rect::new(gx, gy, gw, gh));
    }

    // ── Letture della griglia in focus (il render disegna solo quella) ──
    fn fgrid_span_at(&self, col: usize, row: usize) -> (usize, usize) {
        self.sessions
            .focus_session()
            .map(|s| s.grid.span_at(col, row))
            .unwrap_or((col, 1))
    }

    fn fgrid_cell(&self, col: usize, row: usize) -> Cell {
        self.sessions
            .focus_session()
            .map(|s| s.grid.cell(col, row))
            .unwrap_or_default()
    }

    fn fgrid_cell_width(&self, col: usize, row: usize) -> u8 {
        self.fgrid_cell(col, row).width
    }

    fn fgrid_cursor(&self) -> (usize, usize) {
        self.sessions
            .focus_session()
            .map(|s| s.grid.cursor_clamped())
            .unwrap_or((0, 0))
    }

    fn fgrid_cursor_visible(&self) -> bool {
        self.sessions
            .focus_session()
            .map(|s| s.grid.cursor_visible)
            .unwrap_or(false)
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
        // T4 (solo test): con la bg uniforme il percorso legacy non deve
        // comporre il wallpaper, o shift e repaint darebbero pixel diversi.
        let l0_data = if self.test_uniform_bg {
            None
        } else {
            self.layer0.as_ref().map(|v| (v.as_ptr() as *const u8, v.len()))
        };

        // T3 (ADR-0066): con la bg layer pronta il render copia una banda di
        // background (memcpy) e disegna solo l'inchiostro. Puntatore+len+stride
        // estratti prima del mut-borrow della surface (come per `l0_data`).
        let bg_data: Option<(*const u8, usize, u32)> = match (self.comp.wallpaper(), &self.surface) {
            (Some(wp), Surface::Pixel(_)) => {
                Some((wp.data().as_ptr(), wp.data().len(), wp.stride()))
            }
            _ => None,
        };

        let focus = self.sessions.focus;
        let dirty = match self.sessions.get_mut(focus) {
            Some(s) => s.grid.take_dirty(),
            None => return,
        };
        if dirty.any {
            let (cols, rows) = (self.sessions.cols, self.sessions.rows);
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
                    let (s0, _) = self.fgrid_span_at(c0, row);
                    bc0 = bc0.min(s0);
                    let (s1, l1) = self.fgrid_span_at(c1, row);
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
                    self.comp.add_damage(musivum::Rect::new(x0, y0, bw, geom.cell_h));
                    for col in bc0..=bc1 {
                        // Le continuazioni le disegna la testa dello span.
                        if self.fgrid_cell_width(col, row) == 0 {
                            continue;
                        }
                        let is_cursor =
                            (col, row) == self.fgrid_cursor() && self.fgrid_cursor_visible();
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
                        if self.fgrid_cell_width(col, row) == 0 {
                            continue;
                        }
                        let is_cursor =
                            (col, row) == self.fgrid_cursor() && self.fgrid_cursor_visible();
                        self.draw_cell_at(col, row, is_cursor, l0_data, false);
                        self.stat_cells += 1; // T0 (ADR-0066)
                    }
                }
            }
        }
        // Il cursore si e' spostato o ha cambiato visibilita': ridisegna la
        // cella del cursore solo se lo stato e' cambiato (con il danno
        // esplicito sarebbe un trasferimento per messaggio).
        let cur = self.fgrid_cursor();
        let cur_visible = self.fgrid_cursor_visible();
        let (drawn, drawn_visible) = match self.sessions.get(focus) {
            Some(s) => (s.cursor_drawn, s.cursor_drawn_visible),
            None => return,
        };
        if cur != drawn || cur_visible != drawn_visible {
            // Cancella la vecchia posizione del cursore (se diversa).
            if cur != drawn {
                let (cc, cr) = drawn;
                if cc < self.sessions.cols && cr < self.sessions.rows {
                    self.draw_cell_at(cc, cr, false, l0_data, false);
                }
            }
            // Disegna il cursore nella nuova posizione.
            let (cc, cr) = cur;
            if cc < self.sessions.cols && cr < self.sessions.rows {
                self.draw_cell_at(cc, cr, cur_visible, l0_data, false);
            }
            if let Some(s) = self.sessions.get_mut(focus) {
                s.cursor_drawn = cur;
                s.cursor_drawn_visible = cur_visible;
            }
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
        // sopra l'immagine senza cancellarla. T4 (solo test): con la bg
        // uniforme il wallpaper non si compone (coerenza con la banda).
        let l0 = if self.test_uniform_bg {
            None
        } else {
            self.layer0.as_ref().map(|v| {
                // Righe = byte / (larghezza * 4): il pixel e' BGRA.
                let l0_h = (v.len() / (self.layer0_width.max(1) as usize * 4)) as u32;
                (v.as_slice(), self.layer0_width, l0_h, self.layer0_offset_x, self.layer0_offset_y)
            })
        };
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
        let (start, _len) = self.fgrid_span_at(col, row);
        let cell = self.fgrid_cell(start, row);
        let span = cell.width.max(1) as usize;
        // D7 (ADR-0066): dichiara il danno subito dopo aver calcolato lo span
        // (una cella toccata e' una cella cambiata), con lo sbordo di 1 cella
        // e il clip alla superficie.
        let geom = self.surface.geom();
        let (sw, sh) = self.surface.pixel_size();
        self.comp.add_damage(cells_rect(
            geom,
            sw,
            sh,
            start,
            row as u32,
            start + span - 1,
            row as u32,
        ));
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
        if !self.test_uniform_bg && w_img > 0 && len > 0 {
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
        // D8a: il fondo e' del compositor — una copia sola per tutto il DE, e
        // le tile ne leggono la fetta (`Wallpaper::view`). Il buffer resta la
        // sorgente pristina per il render (T3).
        let wp = musivum::Wallpaper::new(v, w_fb, h_fb);
        self.bg_rows_equal = self.test_uniform_bg || wp.rows_equal();
        let _ = civis::println!(
            "[console] layer 0: fb={}x{} img={}x{} offset=({},{}) stride={} bg_rows_equal={}",
            w_fb, h_fb, w_img, h_img, ox, oy, stride, self.bg_rows_equal
        );
        // Copia nella surface: la superficie mostra il fondo dal primo frame.
        let (bg_ptr, bg_len, bg_stride) = (wp.data().as_ptr(), wp.data().len(), wp.stride());
        if let Surface::Pixel(p) = &mut self.surface {
            let bg = unsafe { core::slice::from_raw_parts(bg_ptr, bg_len) };
            p.copy_from(bg, bg_stride, 0, 0, w_fb, h_fb);
        }
        self.comp.set_wallpaper(wp);
        // D8a: una sola tile, grande quanto lo schermo, che scrive
        // **direttamente** nel buffer del device (`Backing::Direct`). Il
        // compositor non copia nulla: sottrae la sua area al fondo, che qui
        // coincide con tutto lo schermo — il wallpaper resta la sorgente che
        // la tile legge per comporre le proprie bande (T3), non un layer
        // ridipinto a ogni frame.
        let screen = musivum::Rect::new(0, 0, w_fb, h_fb);
        self.comp.set_screen(screen);
        if self.tile == 0 {
            self.tile = self.comp.open(musivum::Window {
                id: 0,
                owner: 0,
                rect: screen,
                clip: screen,
                backing: musivum::Backing::Direct,
                layer: musivum::Layer::Tiles,
                z: 0,
                visible: true,
            });
        } else {
            self.comp.set_rect(self.tile, screen);
            self.comp.set_clip(self.tile, screen);
        }
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
/// servizio: la console non puo' fare I/O sul FS mentre serve i clienti
/// (il cardo e' single-threaded e la aspetta: una lettura FS li' dentro
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

    // 4. Le sessioni: la 0 e' lo **schermo** (la griglia segue la surface) e
    //    nasce qui, col terminale che vediamo. Il cursore parte da (0, 0): il
    //    testo scorre dall'alto verso il basso (nuove righe in fondo).
    let geom = surface.geom();

    // D8a: la superficie del compositor (0x0 nel fallback VGA: il compositor
    // non si usa li').
    let (w_surf, h_surf) = surface.pixel_size();

    let mut term = Term {
        sessions: Sessions::new(geom.cols, geom.rows),
        sess_mailboxes: alloc::vec::Vec::new(),
        mirror_left: [MIRROR_SESSION_MAX; littera::MAX_SESSIONS],
        mirror_truncated: [false; littera::MAX_SESSIONS],
        surface,
        font,
        driver_chan,
        desired: None,
        layer0: None,
        layer0_width: 0,
        layer0_offset_x: 0,
        layer0_offset_y: 0,
        // D8a: il compositor del DE. La tile si registra in `build_bg_layer`.
        comp: musivum::Compositor::new(musivum::Rect::new(0, 0, w_surf, h_surf)),
        tile: 0,
        win_owner: alloc::vec::Vec::new(),
        win_surfaces: alloc::vec::Vec::new(),
        bg_rows_equal: false,
        no_shift: NO_SHIFT,
        test_uniform_bg: false,
        fb_slot: civis::DMA_SLOT_DEVICE,
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
    // 4b. La mailbox non si crea qui: e' **dell'attachment** (D8c2), nasce
    //     alla prima `SESS_*` che la chiede e il server la distrugge alla
    //     chiusura o alla morte del peer. La sessione 0 esiste da subito (e'
    //     il terminale di boot) ma non ha un buffer condiviso.

    // Fondo: colore di pagina + immagine centrata (se c'e').
    term.build_bg_layer();
    term.flush_display_full();

    // 5. Banner sul terminale (il framebuffer o la VGA) + il seriale (sink
    //    di debug): la console si annuncia come il vecchio console server.
    //    Il banner **non** entra nel mirror (`false`): il log resta quello di
    //    sempre (`[console] server up`).
    term.feed_session(0, b"Velordo console server\n", false);
    let _ = civis::print_string(b"[console] server up\n");
    log_render_marker(&term);

    // 6. Avvisa il parent (init): il supervisore lo usa (Fase 14).
    civis::signal_ready(1);

    // 7. Nessun device da registrare (D8c2): `/dev/console` non esiste piu'.
    //    Il testo viaggia sull'endpoint di sessione, per capability.

    // 8. Loop IPC: sessioni di terminale, finestre, display.
    loop {
        match civis::recv() {
            Ok(msg) => {
                match msg.tag {
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
                                    term.sessions.cols,
                                    term.sessions.rows
                                );
                            } else {
                                let _ = civis::print_string(b"[console] display: riadattamento fallito\n");
                            }
                        }
                    }

                    civis::syscall_numbers::CONSOLE_QUERY => {
                        let _ = civis::reply(msg.tag, term.sessions.cols as u64, term.sessions.rows as u64);
                    }

                    civis::syscall_numbers::CONSOLE_SET_MODE => {
                        // Il guest sceglie la risoluzione (modeset).
                        let r = term.set_mode(msg.w0 as u32, msg.w1 as u32);
                        let _ = civis::reply(msg.tag, if r.is_ok() { 0 } else { ERR }, 0);
                    }

                    civis::syscall_numbers::CONSOLE_STATE => {
                        // Il contenuto del terminale: celle non vuote + CRC
                        // del testo (la prova automatica che il testo ha
                        // sopravvissuto a un re-adattamento). `w0` nella
                        // richiesta = limite: 0 = tutte le celle, N = le
                        // **ultime** N (il resize che cresce riporta a video
                        // lo scrollback: il testo di prima resta in coda, vedi
                        // `Grid::text_crc_last`).
                        let (non_empty, crc) = term
                            .sessions
                            .focus_session()
                            .map(|s| s.state(msg.w0 as usize))
                            .unwrap_or((0, 0));
                        let _ = civis::reply(msg.tag, non_empty as u64, crc as u64);
                    }

                    civis::syscall_numbers::CONSOLE_FRAME_CRC => {
                        // T4 (solo test): CRC del framebuffer visibile.
                        let (crc, bytes) = term.frame_crc();
                        let _ = civis::reply(msg.tag, crc as u64, bytes as u64);
                    }

                    civis::syscall_numbers::CONSOLE_ROW_CRC => {
                        // T4 (solo test): CRC di una riga di celle.
                        let (crc, bytes) = term.row_crc(msg.w0 as usize);
                        let _ = civis::reply(msg.tag, crc as u64, bytes as u64);
                    }

                    civis::syscall_numbers::CONSOLE_REPAINT => {
                        // T4 (solo test): riferimento "full repaint".
                        if let Some(s) = term.sessions.focus_mut() {
                            s.grid.mark_all();
                        }
                        term.render();
                        term.flush_display_full();
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    civis::syscall_numbers::CONSOLE_TEST_MODE => {
                        // T4 (solo test): bg uniforme e/o niente shift.
                        term.test_uniform_bg = (msg.w0 & 1) != 0;
                        term.no_shift = (msg.w0 & 2) != 0;
                        term.build_bg_layer();
                        if let Some(s) = term.sessions.focus_mut() {
                            s.grid.mark_all();
                        }
                        term.render();
                        term.flush_display_full();
                        let _ = civis::reply(msg.tag, 0, 0);
                    }

                    // ── Finestre del DE (D8b, ADR-0088) ─────────────────
                    WIN_OPEN => {
                        // w0 = tile handle, w1 = size hint w<<16|h.
                        // D8c1: `w0 = 0` significa **"la tile della mia
                        // sessione"** — risolta per attestazione del pid
                        // (nessun numero magico passato allo spawn). In D8c1
                        // si disegna solo la sessione in focus, quindi la
                        // richiesta e' ammessa solo da chi e' attaccato a
                        // quella. L'id esplicito della tile (t73) resta
                        // accettato; ogni altra tile e' negata (policy in
                        // D8f/D8g).
                        let hint = msg.w1 as u32;
                        let tile = if msg.w0 == 0 {
                            match term.sessions.attached(msg.channel) {
                                Some(sid) if sid == term.sessions.focus => term.tile,
                                _ => {
                                    let _ = civis::reply(WIN_OPEN, !0, 0);
                                    continue;
                                }
                            }
                        } else {
                            msg.w0 as u32
                        };
                        let w = (hint >> 16) as u32;
                        let h = (hint & 0xFFFF) as u32;
                        let (sw, sh) = term.surface.pixel_size();
                        let ok = tile != 0
                            && tile == term.tile
                            && sw > 0
                            && sh > 0
                            && w > 0
                            && h > 0
                            && w <= sw
                            && h <= sh;
                        if !ok {
                            let _ = civis::reply(WIN_OPEN, !0, 0);
                            continue;
                        }
                        let bytes = w as usize * h as usize * 4;
                        if bytes > WIN_SURFACE_MAX {
                            let _ = civis::println!(
                                "[console] win: superficie {}x{} ({} B) oltre il limite {}",
                                w, h, bytes, WIN_SURFACE_MAX
                            );
                            let _ = civis::reply(WIN_OPEN, !0, 0);
                            continue;
                        }
                        let owner_pid = civis::peer_pid(msg.channel).unwrap_or(-1);
                        // Il server (console) crea la superficie ed e' l'owner dello
                        // shm; il client la mappa RW, il server la tiene RO per il
                        // blit (i frame sono gli stessi: il client scrive, il
                        // compositor legge).
                        let shm_id = match civis::shm_create(bytes) {
                            Ok(id) => id,
                            Err(_) => {
                                let _ = civis::reply(WIN_OPEN, !0, 0);
                                continue;
                            }
                        };
                        let va = match civis::shm_map(shm_id, 0, civis::PROT_READ) {
                            Ok(v) => v,
                            Err(_) => {
                                let _ = civis::shm_destroy(shm_id);
                                let _ = civis::reply(WIN_OPEN, !0, 0);
                                continue;
                            }
                        };
                        let ptr = va as *mut u8;
                        let map_len = ((bytes + 4095) / 4096) * 4096;
                        let rect = musivum::Rect::new(0, 0, w, h);
                        let clip = musivum::Rect::new(0, 0, sw, sh);
                        // `visible: false` finche' il cliente non fa il primo
                        // COMMIT: aprire non deve dipingere un rect nero.
                        let id = term.comp.open(musivum::Window {
                            id: 0,
                            owner: owner_pid as u32,
                            rect,
                            clip,
                            backing: musivum::Backing::Owned(musivum::Surface::new(
                                ptr, w, h, w * 4,
                            )),
                            layer: musivum::Layer::TileGuests,
                            z: 0,
                            visible: false,
                        });
                        if id == 0 {
                            let _ = civis::shm_destroy(shm_id);
                            let _ = civis::reply(WIN_OPEN, !0, 0);
                            continue;
                        }
                        term.win_owner.push((id, msg.channel, owner_pid));
                        term.win_surfaces.push((id, shm_id, ptr, map_len, w, h));
                        // Il danno dell'apertura e' inutile (non visibile): il
                        // primo COMMIT lo dichiara.
                        term.comp.clear_damage();
                        let _ = civis::reply(WIN_OPEN, id as u64, shm_id as u64);
                    }

                    WIN_COMMIT => {
                        // w0 = win id, w1 = rect danneggiato (0 = tutta la finestra).
                        // Il client ha riempito la sua superficie (shm condivisa in
                        // RW): il compositor la blitta nel device buffer e flusha.
                        let win = msg.w0 as u32;
                        if term.comp.get(win).is_none() {
                            let _ = civis::reply(WIN_COMMIT, !0, 0);
                            continue;
                        }
                        // Primo commit: la finestra diventa visibile.
                        let _ = term.comp.set_visible(win, true);
                        if let Some(w) = term.comp.get(win) {
                            term.comp.add_damage(w.visible_rect());
                        }
                        term.flush_display();
                        let _ = civis::reply(WIN_COMMIT, 0, 0);
                    }

                    WIN_CLOSE => {
                        // w0 = win id.
                        let win = msg.w0 as u32;
                        if term.comp.get(win).is_some() {
                            term.close_window(win);
                            let _ = civis::reply(WIN_CLOSE, 0, 0);
                        } else {
                            let _ = civis::reply(WIN_CLOSE, !0, 0);
                        }
                    }

                    WIN_STATS => {
                        let (vis, tot) = term.comp.stats();
                        let _ = civis::reply(WIN_STATS, vis as u64, tot as u64);
                    }

                    WIN_SET_RECT | WIN_SET_SURFACE | WIN_RAISE => {
                        // Non implementati in D8b (arrivano con floating/D8f).
                        let _ = civis::reply(msg.tag, !0, 0);
                    }

                    // ── Sessioni di terminale (D8c1, ADR-0089) ──────────
                    SESS_CREATE => {
                        // w0 = 0 legata al chiamante / 1 non legata (>= ThirdParty).
                        let class = term.peer_class(msg.channel);
                        if msg.w0 != 0 && !privileged(class) {
                            let _ = civis::reply(SESS_CREATE, !0, 0);
                            continue;
                        }
                        let owner = if msg.w0 == 0 {
                            civis::peer_pid(msg.channel).ok()
                        } else {
                            None
                        };
                        let id = match term.sessions.create(owner) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_CREATE, !0, 0);
                                continue;
                            }
                        };
                        let shm = match term.ensure_mailbox(msg.channel, owner.unwrap_or(-1)) {
                            Some(s) => s,
                            None => {
                                let _ = term.sessions.close(id);
                                let _ = civis::reply(SESS_CREATE, !0, 0);
                                continue;
                            }
                        };
                        term.sessions.attach(msg.channel, id);
                        let _ = civis::reply(SESS_CREATE, id as u64, shm as u64);
                    }

                    SESS_WHOAMI => {
                        // Attestazione: nessun argomento, il kernel dice il pid.
                        let pid = civis::peer_pid(msg.channel).unwrap_or(-1);
                        match term.sessions.session_of_pid(pid) {
                            Some(id) => match term.ensure_mailbox(msg.channel, pid) {
                                Some(shm) => {
                                    term.sessions.attach(msg.channel, id);
                                    let _ = civis::reply(SESS_WHOAMI, id as u64, shm as u64);
                                }
                                None => {
                                    let _ = civis::reply(SESS_WHOAMI, !0, 0);
                                }
                            },
                            None => {
                                let _ = civis::reply(SESS_WHOAMI, !0, 0);
                            }
                        }
                    }

                    SESS_ATTACH_DEFAULT => {
                        // L'unico ingresso **nominato**: la sessione 0.
                        let pid = civis::peer_pid(msg.channel).unwrap_or(-1);
                        if term.sessions.get(0).is_none() {
                            let _ = civis::reply(SESS_ATTACH_DEFAULT, !0, 0);
                            continue;
                        }
                        match term.ensure_mailbox(msg.channel, pid) {
                            Some(shm) => {
                                term.sessions.attach(msg.channel, 0);
                                let _ = civis::reply(SESS_ATTACH_DEFAULT, 0, shm as u64);
                            }
                            None => {
                                let _ = civis::reply(SESS_ATTACH_DEFAULT, !0, 0);
                            }
                        }
                    }

                    SESS_BIND => {
                        // w0 = sessione, w1 = pid: il DE lega un figlio appena nato.
                        if !privileged(term.peer_class(msg.channel)) {
                            let _ = civis::reply(SESS_BIND, !0, 0);
                            continue;
                        }
                        let ok = term.sessions.bind(msg.w0 as usize, msg.w1 as i64);
                        let _ = civis::reply(SESS_BIND, if ok { 0 } else { !0 }, 0);
                    }

                    SESS_WRITE => {
                        // w0 = len (byte gia' nella meta' OUT della mailbox),
                        // w1 = 0 entra nel mirror seriale, 1 = solo schermo
                        // (eco, ridisegni, test).
                        let class = term.peer_class(msg.channel);
                        let sid = match term.sessions.attached(msg.channel) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_WRITE, !0, 0);
                                continue;
                            }
                        };
                        let len = msg.w0 as usize;
                        if !sess_proto::fits(len) || !term.sess_allowed(msg.channel, sid as u32, class) {
                            let _ = civis::reply(SESS_WRITE, !0, 0);
                            continue;
                        }
                        let Some(mb) = term.sess_mb(msg.channel) else {
                            let _ = civis::reply(SESS_WRITE, !0, 0);
                            continue;
                        };
                        let mut data = alloc::vec![0u8; len];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (mb as *const u8).add(sess_proto::OUT_OFF),
                                data.as_mut_ptr(),
                                len,
                            );
                        }
                        term.feed_session(sid, &data, msg.w1 == 0);
                        let _ = civis::reply(SESS_WRITE, len as u64, 0);
                    }

                    SESS_READ => {
                        let sid = match term.sessions.attached(msg.channel) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_READ, !0, 0);
                                continue;
                            }
                        };
                        let want = (msg.w0 as usize).min(sess_proto::MAX_CHUNK);
                        let mut buf = alloc::vec![0u8; want];
                        let n = match term.sessions.get_mut(sid) {
                            Some(s) => s.read_input(&mut buf),
                            None => {
                                let _ = civis::reply(SESS_READ, !0, 0);
                                continue;
                            }
                        };
                        if let Some(mb) = term.sess_mb(msg.channel) {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    buf.as_ptr(),
                                    (mb as *mut u8).add(sess_proto::IN_OFF),
                                    n,
                                );
                            }
                        }
                        let _ = civis::reply(SESS_READ, n as u64, 0);
                    }

                    SESS_RESIZE => {
                        // w0 = cols<<16|rows. In D8c1 la sessione in focus (lo
                        // schermo) non si ridimensiona da qui: la sua misura e'
                        // quella del display.
                        let sid = match term.sessions.attached(msg.channel) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_RESIZE, !0, 0);
                                continue;
                            }
                        };
                        if sid == term.sessions.focus {
                            let _ = civis::reply(SESS_RESIZE, !0, 0);
                            continue;
                        }
                        let g = msg.w0 as u32;
                        let (cols, rows) = (((g >> 16) as usize).max(1), ((g & 0xFFFF) as usize).max(1));
                        if let Some(s) = term.sessions.get_mut(sid) {
                            s.resize(cols, rows);
                        }
                        let _ = civis::reply(SESS_RESIZE, 0, 0);
                    }

                    SESS_ECHO => {
                        let sid = match term.sessions.attached(msg.channel) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_ECHO, !0, 0);
                                continue;
                            }
                        };
                        if let Some(s) = term.sessions.get_mut(sid) {
                            s.echo = msg.w0 != 0;
                        }
                        let _ = civis::reply(SESS_ECHO, 0, 0);
                    }

                    SESS_FOCUS => {
                        // w0 = sessione da disegnare. D8c1: solo la 0 (le tile
                        // multiple arrivano in D8e); serve l'attachment o il
                        // privilegio.
                        let class = term.peer_class(msg.channel);
                        let target = msg.w0 as usize;
                        let allowed = term.sessions.attached(msg.channel) == Some(target)
                            || privileged(class);
                        if target != 0 || !allowed || !term.sessions.set_focus(target) {
                            let _ = civis::reply(SESS_FOCUS, !0, 0);
                            continue;
                        }
                        // La sessione mostrata deve essere ridisegnata tutta.
                        if let Some(s) = term.sessions.focus_mut() {
                            s.grid.mark_all();
                        }
                        term.render();
                        term.flush_display_full();
                        let _ = civis::reply(SESS_FOCUS, 0, 0);
                    }

                    SESS_CLOSE => {
                        // w0 = sessione (mai la 0). Attaccato o privilegiato.
                        let class = term.peer_class(msg.channel);
                        let sid = msg.w0 as usize;
                        if sid == 0
                            || term.sessions.get(sid).is_none()
                            || !term.sess_allowed(msg.channel, sid as u32, class)
                        {
                            let _ = civis::reply(SESS_CLOSE, !0, 0);
                            continue;
                        }
                        // La mailbox e' del canale: si libera solo se il
                        // chiamante era attaccato a questa sessione (chi ne
                        // chiude un'altra da privilegiato non perde la sua).
                        if term.sessions.attached(msg.channel) == Some(sid) {
                            term.drop_mailbox(msg.channel);
                            term.sessions.detach(msg.channel);
                        }
                        let _ = term.sessions.close(sid);
                        let _ = civis::reply(SESS_CLOSE, 0, 0);
                    }

                    SESS_INFO => {
                        let class = term.peer_class(msg.channel);
                        let sid = msg.w0 as usize;
                        if !term.sess_allowed(msg.channel, sid as u32, class) {
                            let _ = civis::reply(SESS_INFO, !0, 0);
                            continue;
                        }
                        let (cols, rows, echo) = match term.sessions.get(sid) {
                            Some(s) => (s.grid.cols(), s.grid.rows(), s.echo),
                            None => {
                                let _ = civis::reply(SESS_INFO, !0, 0);
                                continue;
                            }
                        };
                        let focus = term.sessions.focus == sid;
                        let g = ((cols as u64) << 16) | rows as u64;
                        let flags = (echo as u64) | ((focus as u64) << 1);
                        let _ = civis::reply(SESS_INFO, g, flags);
                    }

                    SESS_STATE => {
                        // w0 = sessione, w1 = limite delle ultime celle.
                        let class = term.peer_class(msg.channel);
                        let sid = msg.w0 as usize;
                        if !term.sess_allowed(msg.channel, sid as u32, class) {
                            let _ = civis::reply(SESS_STATE, !0, 0);
                            continue;
                        }
                        match term.sessions.get(sid) {
                            Some(s) => {
                                let (non_empty, crc) = s.state(msg.w1 as usize);
                                let _ = civis::reply(SESS_STATE, non_empty as u64, crc as u64);
                            }
                            None => {
                                let _ = civis::reply(SESS_STATE, !0, 0);
                            }
                        }
                    }

                    SESS_DUMP => {
                        // w0 = sessione, w1 = max byte. Il testo va nella meta' IN.
                        let class = term.peer_class(msg.channel);
                        let sid = msg.w0 as usize;
                        if !term.sess_allowed(msg.channel, sid as u32, class) {
                            let _ = civis::reply(SESS_DUMP, !0, 0);
                            continue;
                        }
                        let want = (msg.w1 as usize).min(sess_proto::MAX_CHUNK);
                        let mut buf = alloc::vec![0u8; want];
                        let (n, total) = match term.sessions.get(sid) {
                            Some(s) => s.dump_text(&mut buf),
                            None => {
                                let _ = civis::reply(SESS_DUMP, !0, 0);
                                continue;
                            }
                        };
                        if let Some(mb) = term.sess_mb(msg.channel) {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    buf.as_ptr(),
                                    (mb as *mut u8).add(sess_proto::IN_OFF),
                                    n,
                                );
                            }
                        }
                        let _ = civis::reply(SESS_DUMP, n as u64, total as u64);
                    }

                    SESS_STATS => {
                        let _ = civis::reply(
                            SESS_STATS,
                            term.sessions.count() as u64,
                            term.sessions.focus as u64,
                        );
                    }

                    SESS_TEST_INPUT => {
                        // Solo test: inietta input nella sessione attaccata come
                        // farebbe la tastiera (D8d). Con l'eco acceso il testo
                        // passa dal feed normale (quindi scroll e render).
                        let sid = match term.sessions.attached(msg.channel) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_TEST_INPUT, !0, 0);
                                continue;
                            }
                        };
                        let len = msg.w0 as usize;
                        if !sess_proto::fits(len) {
                            let _ = civis::reply(SESS_TEST_INPUT, !0, 0);
                            continue;
                        }
                        let Some(mb) = term.sess_mb(msg.channel) else {
                            let _ = civis::reply(SESS_TEST_INPUT, !0, 0);
                            continue;
                        };
                        let mut data = alloc::vec![0u8; len];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (mb as *const u8).add(sess_proto::OUT_OFF),
                                data.as_mut_ptr(),
                                len,
                            );
                        }
                        let echo = match term.sessions.get_mut(sid) {
                            Some(s) => s.push_input(&data),
                            None => false,
                        };
                        if echo {
                            // Eco di test: solo schermo, mai nel mirror.
                            term.feed_session(sid, &data, false);
                        }
                        let _ = civis::reply(SESS_TEST_INPUT, 0, 0);
                    }

                    civis::EXIT_NOTIFY => {
                        // Chiude tutte le finestre del processo morto (w1 = pid):
                        // il leak shm e' impossibile per costruzione (l'owner e'
                        // il server, che le distrugge qui).
                        let dead = msg.w1 as i64;
                        if dead >= 0 {
                            term.close_windows_of(dead);
                            // Sessioni del morto: binding, attachment e le
                            // sessioni di cui era owner (mai la 0), con la
                            // mailbox smappata e distrutta.
                            term.purge_sessions_of(dead);
                        }
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
