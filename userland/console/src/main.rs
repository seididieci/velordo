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

use littera::{Sessions, Step, AUTH_USER_MAX};
use musivum;

// Tag delle finestre del DE (D8b, ADR-0088): il servizio console le gestisce
// direttamente; i clienti le usano tramite `civis::win`.
use civis::{WIN_CLOSE, WIN_COMMIT, WIN_OPEN, WIN_RAISE, WIN_SET_RECT, WIN_SET_SURFACE, WIN_STATS};
// Tag delle sessioni di terminale (D8c1, ADR-0089).
use civis::{
    SESS_ATTACH, SESS_ATTACH_DEFAULT, SESS_AUTH, SESS_AUTH_SET, SESS_BIND, SESS_CLOSE, SESS_CREATE,
    SESS_DUMP, SESS_ECHO, SESS_FOCUS, SESS_INFO, SESS_INPUT, SESS_READ, SESS_RESIZE, SESS_STATE,
    SESS_STATS, SESS_TEST_INPUT, SESS_WHOAMI, SESS_WRITE,
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

/// D8e3 — **bordo di una tile** (px), dentro il rect e fuori dalla griglia:
/// due tile adiacenti fanno una linea di `2*TILE_BORDER`. Con una tile sola
/// (o zoomata) il bordo non c'e' e il boot resta identico a D8d.
const TILE_BORDER: u32 = 2;
/// Bordo della tile in **focus**: l'accento del terminale (lo stesso blu-viola
/// del prompt), tmux-style. Cosi' la barra non serve.
const BORDER_FOCUS: u32 = 0xFF5B_7CFF; // BGRA: A=FF, R=5B, G=7C, B=FF
/// Bordo delle altre tile: il colore del fondo, appena piu' chiaro.
const BORDER_IDLE: u32 = 0xFF24_2020; // BGRA

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

/// D8f — profondita' massima della risalita dei padri in `SESS_WHOAMI`: un
/// figlio lanciato da una shell non e' legato a nessuno, e deve parlare con la
/// sessione della tile in cui e' stato lanciato. 16 livelli coprono
/// login → DE → shell → comando → figlio; un albero piu' profondo si ferma
/// (e il chiamante ripiega sulla 0, come prima).
const ANCESTRY_MAX: usize = 16;

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

/// Una **tile** del DE (D8e): un rettangolo che mostra una sessione.
///
/// La tile non ha stato proprio: e' `rect + sessione + finestra` nel
/// compositor. Il layout lo decide il DE (`TILE_*`, D8e1 passo 2); finche' ce
/// n'e' **una** e' lo schermo intero con la sessione 0, e il server si adatta
/// da solo al modeset (D4/D8d).
#[derive(Clone, Copy)]
struct Tile {
    /// Indice **1-based**: handle di `WIN_OPEN` e dei `TILE_*` (`w0 = 0` resta
    /// "la tile della mia sessione").
    id: u32,
    /// La sessione mostrata.
    sid: usize,
    /// Il rettangolo sullo schermo (pixel).
    rect: musivum::Rect,
    /// La griglia nella tile, in coordinate **schermo** (danno, `cells_rect`).
    geom: Geom,
    /// La stessa griglia in coordinate **tile** (per disegnare nella vista).
    geom_local: Geom,
    /// La finestra `Direct` in `Layer::Tiles`.
    win: u32,
    /// `false` = nascosta (zoom di un'altra tile): la sessione resta viva.
    visible: bool,
    /// Spessore del bordo (px) **dentro** il rect: la griglia ci sta dentro.
    /// 0 = nessun bordo (una tile sola / zoomata).
    border: u32,
}

impl Tile {
    fn build(
        id: u32,
        sid: usize,
        rect: musivum::Rect,
        cell_w: u32,
        cell_h: u32,
        ascent_px: i32,
        border: u32,
    ) -> Tile {
        // La griglia sta **dentro** il bordo: il rect resta intero (il clip per
        // tile resta quello), cambia solo dove cade il testo.
        let b = border.min(rect.w / 2).min(rect.h / 2);
        let geom = Geom::for_rect(
            rect.x + b,
            rect.y + b,
            rect.w - 2 * b,
            rect.h - 2 * b,
            cell_w,
            cell_h,
            ascent_px,
        );
        Tile {
            id,
            sid,
            rect,
            geom,
            geom_local: geom.local(rect.x, rect.y),
            win: 0,
            visible: true,
            border: b,
        }
    }

    fn cols(&self) -> usize {
        self.geom.cols
    }

    fn rows(&self) -> usize {
        self.geom.rows
    }
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
    /// D8d — tick della prossima **riattaccata** della tastiera dopo la morte
    /// di `porta` (0 = attaccata: il loop dorme senza scadenza).
    kb_retry_at: i64,
    /// D8e — **staging** del layout: `TILE_SET` scrive qui, `TILE_COMMIT`
    /// applica tutto in un render solo (niente stati intermedi a schermo).
    tile_pending: alloc::vec::Vec<(usize, musivum::Rect)>,
    /// D8e — il canale del **DE** che ha disposto il layout (0 = nessuno): a
    /// lui si manda `TILE_SCREEN` quando il display si riadatta, invece di
    /// adattare la tile da soli.
    tile_owner: u64,
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
    /// Le **tile** (D8e): ognuna mostra una sessione nel suo rettangolo. In
    /// D8e1 ce n'e' una sola (lo schermo, sessione 0); con `TILE_*` il DE ne
    /// dispone N.
    tiles: alloc::vec::Vec<Tile>,
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

    // ── Tile (D8e) ───────────────────────────────────────────────────

    /// Il bordo che spetta a un rect: il bordo c'e' sempre (anche con una
    /// singola tile); la griglia sta gia' dentro il bordo, quindi non copre
    /// il testo. Zero solo se la superficie non e' ancora pronta.
    fn border_for(&self, _rect: musivum::Rect) -> u32 {
        let (sw, sh) = self.surface.pixel_size();
        if sw == 0 || sh == 0 {
            0
        } else {
            TILE_BORDER
        }
    }

    /// I **bordi** delle tile (D8e3): accento sulla tile in focus, scuro sulle
    /// altre. Si ridisegnano dopo ogni cambio di layout o di focus; il danno
    /// e' il rect della tile (poche migliaia di pixel).
    fn draw_borders(&mut self) {
        let focus = self.sessions.focus;
        for t in 0..self.tiles.len() {
            let tile = self.tiles[t];
            if tile.border == 0 || !tile.visible {
                continue;
            }
            let color = if tile.sid == focus {
                BORDER_FOCUS
            } else {
                BORDER_IDLE
            };
            let b = tile.border;
            let r = tile.rect;
            if let Surface::Pixel(p) = &mut self.surface {
                p.fill_rect(r.x, r.y, r.w, b, color);
                p.fill_rect(r.x, r.y + r.h - b, r.w, b, color);
                p.fill_rect(r.x, r.y, b, r.h, color);
                p.fill_rect(r.x + r.w - b, r.y, b, r.h, color);
            }
            self.comp.add_damage(r);
        }
    }

    /// L'indice della tile che mostra la sessione `sid`.
    fn tile_of(&self, sid: usize) -> Option<usize> {
        self.tiles.iter().position(|t| t.sid == sid)
    }

    /// L'indice della tile in **focus** (quella che riceve i tasti e dove sta
    /// il cursore del terminale).
    fn focus_tile(&self) -> Option<usize> {
        self.tile_of(self.sessions.focus)
    }

    /// La geometria della tile `t` in coordinate **schermo** (danno e celle).
    fn tile_geom(&self, t: usize) -> Geom {
        self.tiles
            .get(t)
            .map(|t| t.geom)
            .unwrap_or_else(|| self.surface.geom())
    }

    /// La sessione della tile `t`.
    fn tile_sid(&self, t: usize) -> usize {
        self.tiles
            .get(t)
            .map(|t| t.sid)
            .unwrap_or(self.sessions.focus)
    }

    /// La vista della superficie limitata alla tile `t`: e' cosi' che il
    /// clipping per tile non richiede un check in piu' — `Pixel::view` porta
    /// `width`/`height` del rettangolo e il codice di disegno che c'e' gia' ci
    /// si adatta. `None` = fallback VGA (una sola griglia testuale).
    fn tile_view(&self, t: usize) -> Option<Pixel> {
        let tile = self.tiles.get(t)?;
        match &self.surface {
            Surface::Pixel(p) => Some(p.view(
                tile.rect.x,
                tile.rect.y,
                tile.rect.w,
                tile.rect.h,
                tile.geom_local,
            )),
            Surface::VgaText(_) => None,
        }
    }

    /// D8e1 — una sola tile: la **1**, grande quanto lo schermo, che mostra la
    /// sessione 0. Ricalcolata al modeset (il server si adatta da solo finche'
    /// c'e' una tile: D4/D8d); con N tile il layout arrivera' da `TILE_*`.
    fn ensure_screen_tile(&mut self) {
        let geom = self.surface.geom();
        let (w, h) = match &self.surface {
            Surface::Pixel(p) => (p.width, p.height),
            Surface::VgaText(_) => (0, 0),
        };
        let rect = musivum::Rect::new(0, 0, w, h);
        // La griglia della sessione 0 e' quella del display.
        if let Some(s) = self.sessions.get_mut(0) {
            s.resize(geom.cols, geom.rows);
        }
        self.sessions.cols = geom.cols;
        self.sessions.rows = geom.rows;
        let mut tile = Tile::build(1, 0, rect, geom.cell_w, geom.cell_h, geom.ascent_px, 0);
        // In VGA la geometria e' quella del buffer testuale, non un rect pixel.
        if matches!(self.surface, Surface::VgaText(_)) {
            tile.geom = geom;
            tile.geom_local = geom.local(0, 0);
        }
        if let Some(existing) = self.tiles.get(0).copied() {
            tile.win = existing.win;
            self.tiles[0] = tile;
            if tile.win != 0 {
                self.comp.set_rect(tile.win, rect);
                self.comp.set_clip(tile.win, rect);
                self.comp.set_visible(tile.win, true);
            }
        } else {
            if matches!(self.surface, Surface::Pixel(_)) {
                tile.win = self.comp.open(musivum::Window {
                    id: 0,
                    owner: 0,
                    rect,
                    clip: rect,
                    backing: musivum::Backing::Direct,
                    layer: musivum::Layer::Tiles,
                    z: 0,
                    visible: true,
                });
            }
            self.tiles.push(tile);
        }
    }

    /// Applica una tile (D8e): crea o aggiorna rettangolo, geometria,
    /// sessione e finestra del compositor. `rect` vuoto = tile **nascosta**:
    /// la sessione non si tocca (al ritorno il testo e' dove stava), si toglie
    /// solo dal compositor.
    fn apply_tile(&mut self, sid: usize, rect: musivum::Rect) {
        let hidden = rect.w == 0 || rect.h == 0;
        let existing = self.tiles.iter().position(|t| t.sid == sid);
        if hidden {
            if let Some(i) = existing {
                let (win, old) = (self.tiles[i].win, self.tiles[i].rect);
                self.tiles[i].visible = false;
                if win != 0 {
                    self.comp.set_visible(win, false);
                }
                self.comp.add_damage(old);
            }
            return;
        }
        let m = match self.font.as_ref() {
            Some(f) => f.metrics(),
            None => return,
        };
        let border = self.border_for(rect);
        let mut tile = Tile::build(
            (sid as u32) + 1,
            sid,
            rect,
            m.cell_w as u32,
            m.cell_h as u32,
            m.ascent_px,
            border,
        );
        // La sessione prende la misura della tile e si ridisegna tutta.
        if let Some(s) = self.sessions.get_mut(sid) {
            s.resize(tile.geom.cols, tile.geom.rows);
            s.grid.mark_all();
        }
        match existing {
            Some(i) => {
                tile.win = self.tiles[i].win;
                let old = self.tiles[i].rect;
                self.tiles[i] = tile;
                if tile.win != 0 {
                    self.comp.set_rect(tile.win, rect);
                    self.comp.set_clip(tile.win, rect);
                    self.comp.set_visible(tile.win, true);
                }
                // Il vecchio rect torna al fondo (o a un'altra tile).
                self.comp.add_damage(old);
            }
            None => {
                tile.win = self.comp.open(musivum::Window {
                    id: 0,
                    owner: 0,
                    rect,
                    clip: rect,
                    backing: musivum::Backing::Direct,
                    layer: musivum::Layer::Tiles,
                    z: sid as i32,
                    visible: true,
                });
                self.tiles.push(tile);
            }
        }
        self.comp.add_damage(rect);
    }

    /// Toglie la tile della sessione `sid` (il rect torna al fondo). La
    /// sessione resta: la chiude chi la possiede, con `SESS_CLOSE`.
    fn drop_tile_of(&mut self, sid: usize) {
        if let Some(i) = self.tiles.iter().position(|t| t.sid == sid) {
            let (win, rect) = (self.tiles[i].win, self.tiles[i].rect);
            self.tiles.remove(i);
            if win != 0 {
                let _ = self.comp.close(win);
                self.comp.add_damage(rect);
            }
        }
    }

    /// CRC32 del **rect** della tile: `(byte, crc)`. Prova che un'operazione
    /// ha toccato una tile e non l'altra.
    fn tile_crc(&self, sid: usize) -> Option<(usize, u32)> {
        let rect = self.tiles.iter().find(|t| t.sid == sid)?.rect;
        let Surface::Pixel(p) = &self.surface else {
            return None;
        };
        let row_bytes = rect.w as usize * 4;
        let mut crc = 0xFFFF_FFFFu32;
        let mut hashed = 0usize;
        for y in 0..rect.h as usize {
            let sy = rect.y as usize + y;
            if sy >= p.height as usize {
                break;
            }
            let row = unsafe {
                core::slice::from_raw_parts(
                    (p.ptr as *const u8)
                        .add(sy * p.stride as usize + rect.x as usize * 4),
                    row_bytes,
                )
            };
            for &b in row {
                crc ^= b as u32;
                crc = crc.wrapping_mul(0x0100_0193);
            }
            hashed += row_bytes;
        }
        Some((hashed, crc))
    }

    /// D8f — il **DE è morto**: il layout torna alla console (una tile 0 a
    /// tutto schermo, **senza bordo**) e i pixel dei bordi spariscono, insieme
    /// al testo della sessione 0 (lo schermo del login riparte pulito). Il DE
    /// è riconosciuto dal suo canale di layout (`tile_owner`).
    fn reset_layout_after_de(&mut self) {
        self.tile_owner = 0;
        self.tile_pending.clear();
        for t in &self.tiles {
            if t.win != 0 {
                let _ = self.comp.close(t.win);
                self.comp.add_damage(t.rect);
            }
        }
        self.tiles.clear();
        self.sessions.focus = 0;
        if let Some(s) = self.sessions.get_mut(0) {
            s.grid.clear();
        }
        self.ensure_screen_tile();
        // Copre i bordi (pixel fuori dalla griglia) col wallpaper e ridisegna
        // le celle: con una tile il bordo non c'e' (login a tutto schermo).
        let (w, h) = self.surface.pixel_size();
        self.repaint_tile_area(musivum::Rect::new(0, 0, w, h));
        self.flush_display_full();
    }

    /// Applica lo staging del layout (`TILE_COMMIT`): tutte le tile, poi un
    /// solo render+flush. I **margini** (bordo + centraggio delle celle)
    /// vanno ricoperti col wallpaper: il render copre solo le celle, e senza
    /// questo la striscia tra bordo e griglia tiene i pixel del layout
    /// precedente (i residui del login dopo il primo `TILE_SET`).
    fn commit_tiles(&mut self) {
        let pending = core::mem::take(&mut self.tile_pending);
        for (sid, rect) in pending {
            self.apply_tile(sid, rect);
        }
        let (sw, sh) = self.surface.pixel_size();
        if sw > 0 && sh > 0 {
            self.repaint_tile_area(musivum::Rect::new(0, 0, sw, sh));
        } else {
            self.render();
        }
        self.draw_borders();
        self.flush_display_full();
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
        // D8e: hanno pixel da aggiornare **tutte** le sessioni con una tile
        // visibile, non solo quella in focus (una tile non in focus si vede lo
        // stesso: e' il suo rettangolo sullo schermo). Una sessione senza tile
        // — o nascosta dallo zoom — vive solo nel modello.
        if !self.tiles.iter().any(|t| t.sid == idx && t.visible) {
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

    // ── Input: i tasti sono della sessione (D8d) ─────────────────────

    /// **Inietta i tasti nella sessione in focus** (D8d): il focus owner —
    /// qui la console, in D8e il DE — drena la tastiera e li mette nella coda
    /// della sessione, poi sveglia il **lettore dichiarato** con un hint
    /// coalescente (la coda e' la verita': un hint perso non perde tasti).
    ///
    /// L'eco lo fa il modello **solo se il flag e' acceso**: i lettori reali
    /// (login, shell) hanno il loro editor di linea e lo spengono; un
    /// consumatore senza editor (D8e) lo lascia acceso e vede i caratteri.
    /// L'eco non entra mai nel mirror seriale.
    fn inject_input(&mut self) {
        if !civis::input::device::attached() {
            return;
        }
        const DRAIN_MAX: usize = 4096;
        let mut buf = [0u8; 64];
        let mut total = 0usize;
        while total < DRAIN_MAX {
            let n = civis::input::device::read(&mut buf);
            if n == 0 {
                break;
            }
            if !self.queue_input(self.sessions.focus, &buf[..n]) {
                break;
            }
            total += n;
        }
        if total > 0 {
            self.hint_reader(self.sessions.focus, total);
        }
    }

    /// Accoda `bytes` nella coda di input della sessione `sid` (tasti veri o
    /// iniezione di test) e, se l'eco e' accesa, li mostra sul modello: un feed
    /// normale (scroll e render) che **non** entra nel mirror seriale.
    ///
    /// `false` = la sessione non c'e' piu'.
    fn queue_input(&mut self, sid: usize, bytes: &[u8]) -> bool {
        let echo = match self.sessions.get_mut(sid) {
            Some(s) => s.push_input(bytes),
            None => return false,
        };
        // Il modello ecoa **per il suo lettore**, non nel vuoto: una tile dove
        // nessuno legge ancora (la sessione creata da uno split, prima che il
        // programma si attacchi) non deve far comparire i tasti digitati.
        if echo && self.sessions.reader_of(sid).is_some() {
            self.feed_session(sid, bytes, false);
        }
        true
    }

    /// Sveglia il **lettore dichiarato** della sessione: hint coalescente, la
    /// coda e' la verita' (un hint perso non perde un tasto). Fire-and-forget;
    /// se il canale e' morto il ruolo si libera e il prossimo che legge si
    /// ridichiara.
    fn hint_reader(&mut self, sid: usize, queued: usize) {
        if let Some(chan) = self.sessions.reader_of(sid) {
            if civis::send_async(chan, civis::SESS_INPUT_EVENT, queued as u64, 0).is_err() {
                self.sessions.clear_reader(chan);
            }
        }
    }

    /// Quando svegliarsi per riattaccare la tastiera (0 = mai: e' attaccata).
    fn kb_deadline(&self) -> u64 {
        if self.kb_retry_at <= 0 {
            0
        } else {
            self.kb_retry_at as u64
        }
    }

    /// Un tentativo di riattacco per volta, **mai bloccante**: il server ha
    /// clienti (una scrittura della shell non deve aspettare `porta`).
    fn kb_retry(&mut self) {
        if self.kb_retry_at <= 0 || civis::get_ticks() < self.kb_retry_at {
            return;
        }
        match civis::input::device::attach_once() {
            Ok(()) => {
                self.kb_retry_at = 0;
                let _ = civis::print_string(b"[console] tastiera riattaccata\n");
            }
            Err(_) => self.kb_retry_at = civis::get_ticks() + 10,
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
        // D8e: la misura delle sessioni e' quella delle loro **tile**
        // (`ensure_screen_tile` qui sotto): con una tile e' lo schermo, come in
        // D8c1; con N tile decide il layout del DE.
        // 5) il fondo del buffer nuovo (colore di pagina + immagine
        //    ricentrata: gli offset sono relativi alla superficie) e
        //    **trasferisci i pixel all'host**: il PRESENT ha agganciato il
        //    buffer nuovo ma il device ha visto solo gli zeri dell'allocazione
        //    (il primo FLUSH, senza questo, arriverebbe al primo tasto:
        //    schermo nero fino a li').
        self.build_bg_layer();
        if self.tile_owner != 0 {
            // D8e: il DE (se c'è) decide il layout — gli si dice che il
            // display e' cambiato e lui rilancia `TILE_SET`/`TILE_COMMIT`.
            // Con una sola tile il DE esiste comunque (D8f), quindi va
            // avvisato anche lui; altrimenti il resize non si propaga.
            let _ = civis::send_async(
                self.tile_owner,
                civis::TILE_SCREEN,
                ((w as u64) << 16) | (h as u64),
                0,
            );
        } else {
            // Nessun DE: una tile sola e' lo schermo, si adatta da sola (D4/D8c1).
            self.ensure_screen_tile();
        }
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

        // 2. le celle sotto il rect (inchiostro + bg espliciti), per **ogni**
        //    tile che il rect tocca (D8e: un ospite puo' stare in una tile non
        //    in focus).
        for t in 0..self.tiles.len() {
            let (geom, sid) = (self.tiles[t].geom, self.tiles[t].sid);
            let (cw, ch) = (geom.cell_w as u32, geom.cell_h as u32);
            if cw == 0 || ch == 0 {
                continue;
            }
            let (ox, oy) = (geom.origin_x, geom.origin_y);
            let x1 = r.right().saturating_sub(1);
            let y1 = r.bottom().saturating_sub(1);
            let c0 = r.x.saturating_sub(ox) / cw;
            let c1 = x1.saturating_sub(ox) / cw;
            let r0 = r.y.saturating_sub(oy) / ch;
            let r1 = y1.saturating_sub(oy) / ch;
            if let Some(s) = self.sessions.get_mut(sid) {
                s.grid.mark_rect(c0 as usize, r0 as usize, c1 as usize, r1 as usize);
            }
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

    /// La sessione del processo `pid`, **risalendo i padri** (D8f): il binding
    /// esplicito (o l'owner) vale per chi e' legato, ma un comando lanciato
    /// dentro una tile non lo e' — il padre (la shell della tile) si'. La
    /// catena viene dal kernel (`ps_info.parent`), quindi resta attestazione:
    /// un processo non puo' fingersi figlio di un altro.
    fn session_of_process(&self, pid: i64) -> Option<usize> {
        let mut cur = pid;
        for _ in 0..ANCESTRY_MAX {
            if cur < 0 {
                return None;
            }
            if let Some(sid) = self.sessions.session_of_pid(cur) {
                return Some(sid);
            }
            // Padre morto o senza padre: la risalita si ferma (fail-loud: il
            // chiamante risponde "non attaccato", non una sessione a caso).
            cur = civis::ps_info(cur as u32)?.parent? as i64;
        }
        None
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

    /// Area della griglia della tile `t` in pixel: `(x, y, w, h)` clippata
    /// alla sua superficie (in coordinate schermo).
    fn grid_px_rect(&self, t: usize) -> (u32, u32, u32, u32) {
        let g = self.tile_geom(t);
        let (right, bottom) = match self.tiles.get(t) {
            Some(tile) => (tile.rect.right(), tile.rect.bottom()),
            None => self.surface.pixel_size(),
        };
        let x = g.origin_x;
        let y = g.origin_y;
        let w = (g.cols as u32 * g.cell_w).min(right.saturating_sub(x));
        let h = (g.rows as u32 * g.cell_h).min(bottom.saturating_sub(y));
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
        // D8e: lo shift e' **della tile** che mostra la sessione (non "della
        // sessione in focus"): le altre tile non hanno pixel da spostare.
        let tile = self.tile_of(idx);
        let visible = tile.map(|t| self.tiles[t].visible).unwrap_or(false);
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
        let (gx, gy, gw, gh) = match tile {
            Some(t) => self.grid_px_rect(t),
            None => return,
        };
        let cell_h = self.tile_geom(tile.unwrap_or(0)).cell_h;
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

    // ── Letture della griglia di una **sessione** (una tile) ────────────
    fn grid_span_at(&self, sid: usize, col: usize, row: usize) -> (usize, usize) {
        self.sessions
            .get(sid)
            .map(|s| s.grid.span_at(col, row))
            .unwrap_or((col, 1))
    }

    fn grid_cell(&self, sid: usize, col: usize, row: usize) -> Cell {
        self.sessions
            .get(sid)
            .map(|s| s.grid.cell(col, row))
            .unwrap_or_default()
    }

    fn grid_cell_width(&self, sid: usize, col: usize, row: usize) -> u8 {
        self.grid_cell(sid, col, row).width
    }

    fn grid_cursor(&self, sid: usize) -> (usize, usize) {
        self.sessions
            .get(sid)
            .map(|s| s.grid.cursor_clamped())
            .unwrap_or((0, 0))
    }

    fn grid_cursor_visible(&self, sid: usize) -> bool {
        self.sessions
            .get(sid)
            .map(|s| s.grid.cursor_visible)
            .unwrap_or(false)
    }

    /// Ridisegna le **tile visibili** e il cursore di quella in focus (D8e).
    ///
    /// Ottimizzazione: solo le celle dirty di ogni sessione vengono ritoccate.
    /// Per le celle vuote (cp == 0) o con background trasparente, prima si
    /// ripristina lo sfondo dal layer 0; se manca il layer 0, si riempie col
    /// colore di background della cella (cosi' i pixel del vecchio carattere
    /// non restano impressi). Poi si disegna il glifo (se presente).
    fn render(&mut self) {
        // T0 (ADR-0066): misura i cicli per render.
        let t0 = civis::rdtsc();
        // T4 (solo test): con la bg uniforme il percorso legacy non deve
        // comporre il wallpaper, o shift e repaint darebbero pixel diversi.
        let l0_data = if self.test_uniform_bg {
            None
        } else {
            self.layer0.as_ref().map(|v| (v.as_ptr() as *const u8, v.len()))
        };
        // T3 (ADR-0066): banda di background dalla bg layer (memcpy) invece di
        // fill+blend per cella. Puntatore+len+stride estratti prima del
        // mut-borrow della surface.
        let bg_data: Option<(*const u8, usize, u32)> = match (self.comp.wallpaper(), &self.surface) {
            (Some(wp), Surface::Pixel(_)) => {
                Some((wp.data().as_ptr(), wp.data().len(), wp.stride()))
            }
            _ => None,
        };
        for t in 0..self.tiles.len() {
            if !self.tiles[t].visible {
                continue;
            }
            self.render_tile(t, l0_data, bg_data);
        }
        self.render_cursor(l0_data);
        // Nel fallback VGA il cursore e' quello hardware (CRTC): uno solo, la
        // griglia e' una.
        if matches!(self.surface, Surface::VgaText(_)) {
            let sid = self.sessions.focus;
            let (cc, cr) = self.grid_cursor(sid);
            unsafe { move_hw_cursor(cr, cc) };
        }
        self.stat_renders += 1; // T0 (ADR-0066)
        self.stat_cyc_render += civis::rdtsc() - t0;
    }

    /// La zona sporca della **tile `t`**: una banda di background per riga
    /// (memcpy dalla bg layer, in coordinate schermo) e poi l'inchiostro.
    fn render_tile(
        &mut self,
        t: usize,
        l0_data: Option<(*const u8, usize)>,
        bg_data: Option<(*const u8, usize, u32)>,
    ) {
        let sid = self.tile_sid(t);
        let (cols, rows, geom) = match self.tiles.get(t) {
            Some(tile) => (tile.cols(), tile.rows(), tile.geom),
            None => return,
        };
        if cols == 0 || rows == 0 {
            return;
        }
        let dirty = match self.sessions.get_mut(sid) {
            Some(s) => s.grid.take_dirty(),
            None => return,
        };
        if !dirty.any {
            return;
        }
        let r0 = dirty.min_row.min(rows.saturating_sub(1));
        let r1 = dirty.max_row.min(rows.saturating_sub(1));
        let c0 = dirty.min_col.min(cols.saturating_sub(1));
        let c1 = dirty.max_col.min(cols.saturating_sub(1));
        // Il cursore del terminale c'e' **solo** nella tile in focus.
        let (cc, cr) = self.grid_cursor(sid);
        let cursor_here = self.focus_tile() == Some(t) && self.grid_cursor_visible(sid);
        if let Some((bg_ptr, bg_len, bg_stride)) = bg_data {
            // Allarga il bbox agli span interi: una banda che parte in mezzo a
            // un glifo largo cancellerebbe inchiostro che nessuno ridisegna.
            let mut bc0 = c0;
            let mut bc1 = c1;
            for row in r0..=r1 {
                let (s0, _) = self.grid_span_at(sid, c0, row);
                bc0 = bc0.min(s0);
                let (s1, l1) = self.grid_span_at(sid, c1, row);
                bc1 = bc1.max((s1 + l1).saturating_sub(1));
            }
            let bg = unsafe { core::slice::from_raw_parts(bg_ptr, bg_len) };
            for row in r0..=r1 {
                let x0 = geom.origin_x + (bc0 as u32) * geom.cell_w;
                let y0 = geom.origin_y + (row as u32) * geom.cell_h;
                let bw = ((bc1 - bc0 + 1) as u32) * geom.cell_w;
                // La banda sta nella griglia, che sta nella tile: nessun
                // sbordo (il clip per tile lo fa la **vista** per le celle).
                if let Surface::Pixel(p) = &mut self.surface {
                    p.copy_from(bg, bg_stride, x0, y0, bw, geom.cell_h);
                }
                self.comp.add_damage(musivum::Rect::new(x0, y0, bw, geom.cell_h));
                for col in bc0..=bc1 {
                    // Le continuazioni le disegna la testa dello span.
                    if self.grid_cell_width(sid, col, row) == 0 {
                        continue;
                    }
                    let is_cursor = cursor_here && (col, row) == (cc, cr);
                    self.draw_cell_at(t, col, row, is_cursor, l0_data, true);
                    self.stat_cells += 1; // T0 (ADR-0066)
                }
            }
        } else {
            // Fallback (VGA o bg layer non pronta): percorso per-cella.
            for row in r0..=r1 {
                for col in c0..=c1 {
                    // Le continuazioni le disegna la **testa** dello span: qui
                    // non si ridipinge il background sopra il glifo largo (era
                    // il bug: l'icona restava di una cella).
                    if self.grid_cell_width(sid, col, row) == 0 {
                        continue;
                    }
                    let is_cursor = cursor_here && (col, row) == (cc, cr);
                    self.draw_cell_at(t, col, row, is_cursor, l0_data, false);
                    self.stat_cells += 1; // T0 (ADR-0066)
                }
            }
        }
    }

    /// Il cursore del terminale della tile in focus: si ridisegna solo se si e'
    /// spostato o ha cambiato visibilita'.
    fn render_cursor(&mut self, l0_data: Option<(*const u8, usize)>) {
        let Some(t) = self.focus_tile() else {
            return;
        };
        let sid = self.tile_sid(t);
        let (cols, rows) = match self.tiles.get(t) {
            Some(tile) => (tile.cols(), tile.rows()),
            None => return,
        };
        let cur = self.grid_cursor(sid);
        let cur_visible = self.grid_cursor_visible(sid);
        let (drawn, drawn_visible) = match self.sessions.get(sid) {
            Some(s) => (s.cursor_drawn, s.cursor_drawn_visible),
            None => return,
        };
        if cur != drawn || cur_visible != drawn_visible {
            // Cancella la vecchia posizione del cursore (se diversa).
            if cur != drawn {
                let (dc, dr) = drawn;
                if dc < cols && dr < rows {
                    self.draw_cell_at(t, dc, dr, false, l0_data, false);
                }
            }
            let (cc, cr) = cur;
            if cc < cols && cr < rows {
                self.draw_cell_at(t, cc, cr, cur_visible, l0_data, false);
            }
            if let Some(s) = self.sessions.get_mut(sid) {
                s.cursor_drawn = cur;
                s.cursor_drawn_visible = cur_visible;
            }
        }
    }

    /// Riempe una cella (o uno span di celle) col suo background (nessun
    /// glifo). Usato per le celle vuote quando non c'e' un layer 0 da
    /// ripristinare. Si disegna nella **vista** della tile: il clip e' il suo
    /// rettangolo, niente pixel nel vicino.
    fn paint_cell_bg(&mut self, t: usize, col: usize, row: usize, span: usize, bg: u32) {
        let Some(mut view) = self.tile_view(t) else {
            return;
        };
        let cw = view.geom.cell_w as u32;
        let ch = view.geom.cell_h as u32;
        let x = view.geom.origin_x + (col as u32) * cw;
        let y = view.geom.origin_y + (row as u32) * ch;
        let w = cw * span.max(1) as u32;
        let color = surface::rgba_to_bgra(bg);
        view.fill_rect(x, y, w, ch, color);
    }

    /// Disegna una cella nella **tile `t`** (tutto lo span del glifo).
    fn draw_cell(
        &mut self,
        t: usize,
        col: usize,
        row: usize,
        span: usize,
        cp: u32,
        fg: u32,
        bg: u32,
        cursor: bool,
    ) {
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
        // La vista si prende **prima** del prestito del font (sono due campi
        // diversi di `self`, ma il compilatore lo deve sapere).
        let view = self.tile_view(t);
        // Nel path VGA il font non serve (il BIOS disegna il carattere).
        let font = self.font.as_mut();
        match view {
            Some(view) => {
                // Path pixel: la vista porta il clip della tile, e il layer 0
                // (che e' dello schermo) si trasla nell'origine della tile.
                let (rx, ry) = self
                    .tiles
                    .get(t)
                    .map(|tile| (tile.rect.x, tile.rect.y))
                    .unwrap_or((0, 0));
                let l0 = l0.map(|(d, w, h, ox, oy)| {
                    (d, w, h, ox.saturating_sub(rx), oy.saturating_sub(ry))
                });
                let mut surf = Surface::Pixel(view);
                surf.put_char(col, row, span, cp, fg, bg, cursor, font, l0);
            }
            // Fallback VGA: una sola griglia testuale, nessuna tile pixel.
            None => self
                .surface
                .put_char(col, row, span, cp, fg, bg, cursor, font, l0),
        }
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
        t: usize,
        col: usize,
        row: usize,
        cursor: bool,
        l0_data: Option<(*const u8, usize)>,
        bg_ready: bool,
    ) {
        let sid = self.tile_sid(t);
        let (start, _len) = self.grid_span_at(sid, col, row);
        let cell = self.grid_cell(sid, start, row);
        let span = cell.width.max(1) as usize;
        // D7 (ADR-0066): dichiara il danno subito dopo aver calcolato lo span
        // (una cella toccata e' una cella cambiata), con lo sbordo di 1 cella
        // e il clip alla superficie. La geom e' quella della tile, in
        // coordinate **schermo**: il danno resta assoluto.
        let geom = self.tile_geom(t);
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
            match self.tile_view(t) {
                Some(mut view) => {
                    view.draw_ink(start, row, span, cell.cp, cell.fg, cursor, self.font.as_mut())
                }
                None => {
                    if let Surface::Pixel(p) = &mut self.surface {
                        p.draw_ink(start, row, span, cell.cp, cell.fg, cursor, self.font.as_mut());
                    }
                }
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
                self.restore_layer0_cell(t, start, row, span, l0_slice);
            } else {
                self.paint_cell_bg(t, start, row, span, cell.bg);
            }
            return;
        }
        // Celle con carattere: put_char gestisce layer 0 (BMP come sfondo) +
        // glifo in un colpo solo.
        self.draw_cell(t, start, row, span, cell.cp, cell.fg, cell.bg, cursor);
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
        // D8a/D8e: la tile (o le tile) le registra `ensure_screen_tile` /
        // `TILE_*`, con `Backing::Direct`: il compositor non copia nulla,
        // sottrae la loro area al fondo. Il wallpaper resta la sorgente che le
        // tile leggono per comporre le proprie bande (T3).
        self.comp.set_screen(musivum::Rect::new(0, 0, w_fb, h_fb));
    }

    /// Ripristina una cella (o uno span di celle) dal layer 0: prima il colore
    /// di pagina su tutta la zona (cancella il carattere precedente e i
    /// margini fuori dall'immagine), poi l'overlap del BMP composto con il
    /// suo alpha.
    fn restore_layer0_cell(
        &mut self,
        t: usize,
        col: usize,
        row: usize,
        span: usize,
        layer0: &[u8],
    ) {
        let bg = surface::rgba_to_bgra(color::BLACK);
        let w_img = self.layer0_width;
        let ox = self.layer0_offset_x;
        let oy = self.layer0_offset_y;
        let h_img = if w_img == 0 {
            0
        } else {
            (layer0.len() / (w_img as usize * 4)) as u32
        };
        let Some(mut view) = self.tile_view(t) else {
            return;
        };
        // Nella vista si lavora in coordinate **tile**: la geometria e' quella
        // locale e il layer 0 (che e' dello schermo) si trasla dell'origine.
        let cw = view.geom.cell_w as u32;
        let ch = view.geom.cell_h as u32;
        let x = view.geom.origin_x + (col as u32) * cw;
        let y = view.geom.origin_y + (row as u32) * ch;
        let span_px = cw * span.max(1) as u32;
        let (rx, ry) = self
            .tiles
            .get(t)
            .map(|tile| (tile.rect.x, tile.rect.y))
            .unwrap_or((0, 0));
        let ox = ox.saturating_sub(rx);
        let oy = oy.saturating_sub(ry);

        // Fondo di pagina (clippato alla tile dalla vista).
        let w = span_px.min(view.width.saturating_sub(x));
        let h = ch.min(view.height.saturating_sub(y));
        view.fill_rect(x, y, w, h, bg);

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
                if sy >= view.height {
                    break;
                }
                let sy0 = (sy - oy) as usize;
                let src_off = sy0 * w_img as usize * 4 + ((overlap_x_start - ox) as usize) * 4;
                let bytes = n as usize * 4;
                if src_off + bytes > layer0.len() {
                    break;
                }
                view.blend_bgra_span(
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
        kb_retry_at: 0,
        tile_pending: alloc::vec::Vec::new(),
        tile_owner: 0,
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
        tiles: alloc::vec::Vec::new(),
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
    // D8e: una tile (la 1) = lo schermo, che mostra la sessione 0.
    term.ensure_screen_tile();
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
    //
    //    La **tastiera** invece si sottoscrive (D8d): `porta` parte dopo di
    //    noi (init), quindi l'attach e' bounded; se fallisce si riprova nel
    //    loop senza fermare i clienti.
    if civis::input::device::attach().is_err() {
        let _ = civis::print_string(b"[console] tastiera non ancora pronta, riprovo\n");
        term.kb_retry_at = civis::get_ticks() + 10;
    } else {
        let _ = civis::print_string(b"[console] tastiera sottoscritta (INPUT_ATTACH)\n");
    }

    // 8. Loop IPC: sessioni di terminale, finestre, display, tasti.
    loop {
        // Riattacco della tastiera (morte di `porta`): un tentativo quando
        // scade, mai una attesa bloccante.
        term.kb_retry();
        match civis::recv_timeout(term.kb_deadline()) {
            Ok(msg) => {
                match msg.tag {
                    civis::INPUT_NOTIFY => {
                        // Hint da `porta` (E0): ci sono tasti. Fire-and-forget
                        // (nessuna reply) e si **drena**: la coda e' la verita',
                        // il notify e' coalescente.
                        term.inject_input();
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
                        // D8e: le tile sono indicizzate 1..N (la finestra di un
                        // ospite e' clippata al rect della **sua** tile).
                        let tile_id = if msg.w0 == 0 {
                            match term.sessions.attached(msg.channel) {
                                Some(sid) if sid == term.sessions.focus => match term.focus_tile()
                                {
                                    Some(t) => term.tiles[t].id,
                                    None => {
                                        let _ = civis::reply(WIN_OPEN, !0, 0);
                                        continue;
                                    }
                                },
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
                        // Il rect della tile (clip dell'ospite) e la superficie.
                        // D8e: `w0 = !0` = finestra di **chrome** del DE —
                        // `Layer::Overlay`, clip allo schermo, solo >=
                        // `ThirdParty` (il piano §10.1: "il layer e' un
                        // privilegio"; primo mattone della policy di D8g).
                        let chrome = tile_id == !0u32;
                        let (tile_rect, layer) = if chrome {
                            if !privileged(term.peer_class(msg.channel)) {
                                let _ = civis::reply(WIN_OPEN, !0, 0);
                                continue;
                            }
                            let (sw, sh) = term.surface.pixel_size();
                            (musivum::Rect::new(0, 0, sw, sh), musivum::Layer::Overlay)
                        } else {
                            match term.tiles.iter().find(|t| t.id == tile_id) {
                                Some(t) => (t.rect, musivum::Layer::TileGuests),
                                None => {
                                    let _ = civis::reply(WIN_OPEN, !0, 0);
                                    continue;
                                }
                            }
                        };
                        let (sw, sh) = term.surface.pixel_size();
                        let ok = sw > 0
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
                        // D8e: l'ospite nasce **dentro** la sua tile — il rect
                        // parte dall'origine della tile e il clip e' il suo
                        // rettangolo (la torsione 1: un ospite non disegna fuori).
                        let rect = musivum::Rect::new(tile_rect.x, tile_rect.y, w, h);
                        let clip = tile_rect;
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
                            layer,
                            z: if chrome { 100 } else { 0 },
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

                    // ── Tile del DE (D8e, ADR-0092) ────────────────────
                    civis::TILE_SET => {
                        // w0 = sessione, w1 = rect impacchettato. Solo il DE
                        // (>= ThirdParty) dispone il layout. Staging: si
                        // applica con TILE_COMMIT.
                        if !privileged(term.peer_class(msg.channel)) {
                            let _ = civis::reply(civis::TILE_SET, !0, 0);
                            continue;
                        }
                        let sid = msg.w0 as usize;
                        let (x, y, w, h) = civis::syscall_numbers::unpack_tile_rect(msg.w1);
                        if term.sessions.get(sid).is_none() {
                            let _ = civis::reply(civis::TILE_SET, !0, 0);
                            continue;
                        }
                        let rect = musivum::Rect::new(x, y, w, h);
                        term.tile_pending.retain(|&(s, _)| s != sid);
                        term.tile_pending.push((sid, rect));
                        let _ = civis::reply(civis::TILE_SET, 0, 0);
                    }

                    civis::TILE_COMMIT => {
                        if !privileged(term.peer_class(msg.channel)) {
                            let _ = civis::reply(civis::TILE_COMMIT, !0, 0);
                            continue;
                        }
                        if term.tile_pending.is_empty() {
                            let _ = civis::reply(civis::TILE_COMMIT, !0, 0);
                            continue;
                        }
                        // Da qui il DE e' il **padrone del layout**: al
                        // modeset si manda TILE_SCREEN a lui, non si adatta.
                        term.tile_owner = msg.channel;
                        term.commit_tiles();
                        let _ = civis::reply(civis::TILE_COMMIT, 0, 0);
                    }

                    civis::TILE_INFO => {
                        let visible = term.tiles.iter().filter(|t| t.visible).count();
                        let focus = (term.sessions.focus as u64) + 1;
                        let _ = civis::reply(civis::TILE_INFO, visible as u64, focus);
                    }

                    civis::TILE_DROP => {
                        if !privileged(term.peer_class(msg.channel)) {
                            let _ = civis::reply(civis::TILE_DROP, !0, 0);
                            continue;
                        }
                        term.drop_tile_of(msg.w0 as usize);
                        term.render();
                        term.draw_borders();
                        term.flush_display();
                        let _ = civis::reply(civis::TILE_DROP, 0, 0);
                    }

                    civis::TILE_SIZE => {
                        let (w, h) = term.surface.pixel_size();
                        let _ = civis::reply(civis::TILE_SIZE, ((w as u64) << 16) | h as u64, 0);
                    }

                    civis::TILE_CRC => {
                        match term.tile_crc(msg.w0 as usize) {
                            Some((bytes, crc)) => {
                                let _ = civis::reply(civis::TILE_CRC, bytes as u64, crc as u64);
                            }
                            None => {
                                let _ = civis::reply(civis::TILE_CRC, !0, 0);
                            }
                        }
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
                        // Attestazione: nessun argomento, il kernel dice il pid;
                        // se il processo non e' legato (un comando lanciato da
                        // una shell) risaliamo i padri fino alla tile (D8f).
                        let pid = civis::peer_pid(msg.channel).unwrap_or(-1);
                        match term.session_of_process(pid) {
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
                        // Chi legge si **dichiara**: e' lui che riceve l'hint
                        // quando arriva input (una sessione, un lettore).
                        term.sessions.set_reader(msg.channel, sid);
                        let _ = civis::reply(SESS_READ, n as u64, 0);
                    }

                    SESS_ATTACH => {
                        // w0 = sessione: mailbox per un **iniettore** nominato
                        // (il DE, D8e). Non dichiara il lettore.
                        let class = term.peer_class(msg.channel);
                        if !privileged(class) || term.sessions.get(msg.w0 as usize).is_none() {
                            let _ = civis::reply(SESS_ATTACH, !0, 0);
                            continue;
                        }
                        let pid = civis::peer_pid(msg.channel).unwrap_or(-1);
                        match term.ensure_mailbox(msg.channel, pid) {
                            Some(shm) => {
                                // La capability e' la mailbox; l'attachment
                                // dice su quale sessione scrive (`SESS_INPUT`).
                                term.sessions.attach(msg.channel, msg.w0 as usize);
                                let _ = civis::reply(SESS_ATTACH, 0, shm as u64);
                            }
                            None => {
                                let _ = civis::reply(SESS_ATTACH, !0, 0);
                            }
                        }
                    }

                    SESS_INPUT => {
                        // w0 = len (byte in OUT): accoda nel **buffer di input**
                        // della sessione attaccata (non e' testo) e sveglia il
                        // lettore dichiarato.
                        let class = term.peer_class(msg.channel);
                        let sid = match term.sessions.attached(msg.channel) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_INPUT, !0, 0);
                                continue;
                            }
                        };
                        let len = msg.w0 as usize;
                        if !sess_proto::fits(len) || !term.sess_allowed(msg.channel, sid as u32, class)
                        {
                            let _ = civis::reply(SESS_INPUT, !0, 0);
                            continue;
                        }
                        let Some(mb) = term.sess_mb(msg.channel) else {
                            let _ = civis::reply(SESS_INPUT, !0, 0);
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
                        let queued = term.queue_input(sid, &data);
                        let _ = civis::reply(SESS_INPUT, 0, 0);
                        if queued {
                            term.hint_reader(sid, data.len());
                        }
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

                    SESS_AUTH_SET => {
                        // w0 = len del nome nella meta' OUT (S3.3c): scrive
                        // l'**utente autenticato** sulla sessione **attaccata**
                        // al canale (implicita, come `SESS_RESIZE`). Solo
                        // `System` (il login sulla 0, il DE sulle tile nuove);
                        // `len = 0` = deautentica (fine sessione).
                        let sid = match term.sessions.attached(msg.channel) {
                            Some(id) => id,
                            None => {
                                let _ = civis::reply(SESS_AUTH_SET, !0, 0);
                                continue;
                            }
                        };
                        if !matches!(term.peer_class(msg.channel), civis::ImageClass::System) {
                            let _ = civis::reply(SESS_AUTH_SET, !0, 0);
                            continue;
                        }
                        let len = msg.w0 as usize;
                        let Some(mb) = term.sess_mb(msg.channel) else {
                            let _ = civis::reply(SESS_AUTH_SET, !0, 0);
                            continue;
                        };
                        if len > AUTH_USER_MAX || !sess_proto::fits(len) {
                            let _ = civis::reply(SESS_AUTH_SET, !0, 0);
                            continue;
                        }
                        let mut name = [0u8; AUTH_USER_MAX];
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (mb as *const u8).add(sess_proto::OUT_OFF),
                                name.as_mut_ptr(),
                                len,
                            );
                        }
                        let by = civis::peer_pid(msg.channel).unwrap_or(-1);
                        let ok = match term.sessions.get_mut(sid) {
                            Some(s) => s.set_auth(&name[..len], by),
                            None => false,
                        };
                        let _ = civis::reply(SESS_AUTH_SET, if ok { 0 } else { !0 }, 0);
                    }

                    SESS_AUTH => {
                        // w0 = pid (0 = la sessione attaccata al canale).
                        // `w0` della reply = 1/0 (autenticata), `w1` = len del
                        // nome nella meta' IN. Solo `System`: il kernel da' il
                        // pid, il server dice la sessione e l'utente — un
                        // programma non legge l'identita' di un'altra tile.
                        if !matches!(term.peer_class(msg.channel), civis::ImageClass::System) {
                            let _ = civis::reply(SESS_AUTH, !0, 0);
                            continue;
                        }
                        let sid = if msg.w0 == 0 {
                            term.sessions.attached(msg.channel)
                        } else {
                            term.session_of_process(msg.w0 as i64)
                        };
                        let pid = civis::peer_pid(msg.channel).unwrap_or(-1);
                        // La mailbox del canale: si crea al primo uso, poi si
                        // scrive la meta' IN attraverso il **puntatore**
                        // mappato (`sess_mb`), mai con l'id della shm.
                        if term.sess_mb(msg.channel).is_none() && pid >= 0 {
                            let _ = term.ensure_mailbox(msg.channel, pid);
                        }
                        let Some(mb) = term.sess_mb(msg.channel) else {
                            let _ = civis::reply(SESS_AUTH, !0, 0);
                            continue;
                        };
                        let (authenticated, len, name) = match sid.and_then(|id| term.sessions.get(id)) {
                            Some(s) => {
                                let mut buf = [0u8; AUTH_USER_MAX];
                                let len = s.auth().len();
                                buf[..len].copy_from_slice(s.auth());
                                (!s.auth().is_empty(), len, buf)
                            }
                            None => (false, 0, [0u8; AUTH_USER_MAX]),
                        };
                        if len > 0 {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    name.as_ptr(),
                                    (mb as *mut u8).add(sess_proto::IN_OFF),
                                    len,
                                );
                            }
                        }
                        let _ = civis::reply(SESS_AUTH, authenticated as u64, len as u64);
                    }

                    SESS_FOCUS => {
                        // w0 = sessione che riceve i **tasti** (D8e: qualunque
                        // sessione con una tile). Serve l'attachment o il
                        // privilegio.
                        let class = term.peer_class(msg.channel);
                        let target = msg.w0 as usize;
                        let allowed = term.sessions.attached(msg.channel) == Some(target)
                            || privileged(class);
                        if !allowed
                            || term.sessions.get(target).is_none()
                            || term.tile_of(target).is_none()
                        {
                            let _ = civis::reply(SESS_FOCUS, !0, 0);
                            continue;
                        }
                        let old = term.sessions.focus;
                        if old != target {
                            // Il cursore del terminale e' **solo** della tile in
                            // focus: la vecchia lo ridisegna spento, la nuova
                            // acceso (`render_cursor` fa il resto).
                            for sid in [old, target] {
                                if let Some(s) = term.sessions.get_mut(sid) {
                                    let (c, r) = s.cursor_drawn;
                                    let c = c.min(s.grid.cols().saturating_sub(1));
                                    let r = r.min(s.grid.rows().saturating_sub(1));
                                    s.grid.mark_rect(c, r, c, r);
                                }
                            }
                            let _ = term.sessions.set_focus(target);
                        }
                        term.render();
                        // L'accento del bordo segue il focus (D8e3).
                        term.draw_borders();
                        term.flush_display();
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
                        let queued = term.queue_input(sid, &data);
                        // La reply **prima** dell'hint: chi inietta e' spesso
                        // anche il lettore, e un hint in coda davanti alla
                        // reply farebbe fallire la sua `send` sincrona.
                        let _ = civis::reply(SESS_TEST_INPUT, 0, 0);
                        if queued {
                            term.hint_reader(sid, data.len());
                        }
                    }

                    civis::EXIT_NOTIFY => {
                        // Chiude tutte le finestre del processo morto (w1 = pid):
                        // il leak shm e' impossibile per costruzione (l'owner e'
                        // il server, che le distrugge qui).
                        let dead = msg.w1 as i64;
                        if dead >= 0 {
                            // Morte di `porta`: la tastiera si riattacca dal
                            // loop (mai un'attesa qui dentro). Il riattacco
                            // azzera anche il peer memorizzato.
                            if civis::input::device::peer_pid() == dead {
                                civis::input::device::detach();
                                term.kb_retry_at = civis::get_ticks() + 10;
                            }
                            term.close_windows_of(dead);
                            // Sessioni del morto: binding, attachment e le
                            // sessioni di cui era owner (mai la 0), con la
                            // mailbox smappata e distrutta.
                            term.purge_sessions_of(dead);
                            // D8f: se è morto il **DE** (il padrone del
                            // layout), il server torna a una tile 0 a tutto
                            // schermo senza bordo: i bordi e il testo della
                            // sessione spariscono, il login riparte pulito.
                            if term.tile_owner != 0 && msg.channel == term.tile_owner {
                                term.reset_layout_after_de();
                            }
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
