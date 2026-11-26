//! littera — la **sessione di terminale** del DE (D8c1, ADR-0089).
//!
//! Una sessione e' un terminale completo **senza display**: modello a celle
//! (`term::Grid`), decoder UTF-8 incrementale, parser ESC/CSI (colori
//! truecolor/256, movimento, cancellazione), input in attesa e eco. Non sa
//! nulla di pixel, tile, compositor o DE: quelle sono della console
//! (`musivum` + `term::surface`).
//!
//! La divisione e' deliberata:
//! - la **sessione** sa cosa e' stato scritto (modello, scrollback, stato del
//!   cursore nel modello);
//! - la **console** sa dove mostrarlo (surface, font, danno, shift dei pixel).
//!
//! Lo scroll e' l'unico punto che richiede entrambi: il modello deve scorrere
//! **prima** che il cursore si sposti, e chi disegna puo' accompagnare lo
//! scroll con uno shift di pixel (T4, ADR-0066). Per questo [`Session::put`]
//! non esegue lo scroll: ritorna [`Step::NeedScroll`], il chiamante fa il suo
//! scroll (modello + pixel) e richiama `put` con lo stesso codepoint: la
//! sessione ricorda quale azione stava eseguendo e la completa.
//!
//! `Sessions` e' la tabella (1:1 con le tile del DE, sessione 0 = schermo
//! intero), con il focus, le attestazioni per pid e gli attachment per canale.
//! Nessuna nozione di display: la usa il server `console`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use term::grid::{color, Grid, Segment};
use term::utf8::Decoder;

/// Il numero massimo di sessioni (sessione 0 compresa): bounded per
/// costruzione — ogni sessione costa una pagina di mailbox e una griglia.
pub const MAX_SESSIONS: usize = 8;

/// Righe di scrollback di una sessione **nuova** (D8c1): N tile non devono
/// costare 1000 righe l'una. La sessione 0 (lo schermo) tiene quelle del
/// terminale di sempre.
pub const SESSION_SCROLLBACK: usize = 256;

/// Byte di input che una sessione tiene in coda fra due `read`: la coda e'
/// un buffer di servizio, non un archivio — oltre si scarta il piu' vecchio.
pub const INPUT_MAX: usize = 1024;

/// Lunghezza massima del nome utente autenticato in una sessione.
pub const AUTH_USER_MAX: usize = 32;

/// Tetto dei byte di **toppe** per sessione (ADR-0099): si controlla
/// **all'ingresso** di una toppa nuova e si rifiuta (`false`), mai si evince
/// una toppa gia' presente. Il caso normale (un log con rami disegnati, una
/// barra, un'icona) sta in poche centinaia di KB; 8 MiB lasciano spazio senza
/// permettere a un `ThirdParty` di esaurire la memoria da un solo schermo.
pub const PATCH_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Tetto dell'alt-text di una toppa (byte UTF-8): entra nel `w1` di
/// `SESS_PATCH_BEGIN` (8 bit).
pub const PATCH_ALT_MAX: usize = 255;

/// Esito di un codepoint per chi disegna.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Codepoint consumato dal modello.
    Done,
    /// Il modello deve scorrere: il chiamante esegue lo scroll (modello +
    /// pixel) e richiama `put` con lo **stesso** codepoint.
    NeedScroll,
}

/// Azione da completare dopo lo scroll richiesto (macchina a stati minima:
/// `put` viene richiamato con lo stesso codepoint, ma l'azione e' gia'
/// decisa).
#[derive(Clone, Copy)]
enum Resume {
    None,
    /// `\n` all'ultima riga: dopo lo scroll, cursore a colonna 0.
    Newline,
    /// Glifo largo che non entra in fondo riga: dopo lo scroll va scritto a
    /// (0, riga corrente) e poi si applica il fine-riga.
    WideWrap { cp: u32, advance: usize },
    /// Fine riga con scroll: dopo lo scroll il cursore e' gia' a colonna 0.
    Home,
}

/// Una sessione di terminale: modello + parser + input. Nessun display.
/// La schermata **principale** messa da parte mentre e' attiva quella
/// alternativa (`ESC[?1049`): griglia, cursore e visibilita' del cursore.
struct SavedScreen {
    grid: Grid,
    cursor: (usize, usize),
    cursor_visible: bool,
}

pub struct Session {
    /// Il modello a celle (pubblico: chi disegna ne legge celle e cursore).
    pub grid: Grid,
    decoder: Decoder,
    pending: Vec<u32>,
    /// Stato del parser ESC/CSI: 0 normale, 1 ESC, 2 CSI entry, 3 parametri.
    esc: u8,
    /// Parametri CSI accumulati (es. `"38;2;255;0;0"`).
    csi_params: Vec<u8>,
    /// La sequenza CSI e' privata (`ESC[?…h/l`: modi del terminale).
    csi_private: bool,
    /// Schermata alternativa attiva (`ESC[?1049h`): la principale e' in
    /// `saved` con il suo cursore. L'alternativa **non** ha scrollback: e'
    /// una tela, non una cronologia (ADR-0097).
    alt: bool,
    saved: Option<SavedScreen>,
    /// Colore foreground corrente (RGBA packed).
    pub fg: u32,
    /// Colore background corrente (RGBA packed).
    pub bg: u32,
    /// Ultima posizione del cursore **disegnata** (stato del display, tenuto
    /// per sessione: cambiando il focus non si ridisegna a sproposito).
    pub cursor_drawn: (usize, usize),
    /// Ultima visibilita' del cursore disegnata.
    pub cursor_drawn_visible: bool,
    /// Il feed corrente e' "in blocco" (molti byte): lo scroll ricompone
    /// invece di spostare i pixel (T4).
    pub bulk_feed: bool,
    /// Write in blocco accumulate dall'ultimo render+flush (T4/bulk).
    pub bulk_writes: u32,
    /// Input dalla tastiera/sessione non ancora letto (byte).
    input: VecDeque<u8>,
    /// Eco dell'input sul modello (i campi password la spengono).
    pub echo: bool,
    /// pid che possiede la sessione (attestazione), `None` = non legata.
    pub owner: Option<i64>,
    /// Utente **autenticato** della sessione (S3.3c): l'identita' e' della
    /// sessione, non un valore che viaggia. Lo scrive il login
    /// (`SESS_AUTH_SET`) e lo risolve chi ne ha diritto (`SESS_AUTH`); la
    /// morte della sessione (o il logout) lo azzera.
    pub auth_user: [u8; AUTH_USER_MAX],
    /// Lunghezza di `auth_user` (0 = sessione anonima).
    auth_len: u8,
    /// pid che ha **stabilito** l'identita' (login sulla 0, DE sulle tile):
    /// alla sua morte l'identita' sparisce anche se la sessione resta viva.
    pub auth_by: i64,
    /// La geometria e' cambiata dall'ultima volta che il client l'ha chiesta
    /// (`SESS_INFO` la consuma). Serve alle app a schermo intero: il resize
    /// arriva dal display o dal DE, non da loro (ADR-0098).
    pub resized: bool,
    /// Staging di una **toppa** in arrivo (`SESS_PATCH_BEGIN`/`DATA`): una per
    /// sessione, come la transazione di Identis. Il commit e' implicito.
    patch_pending: Option<PendingPatch>,
    resume: Resume,
}

/// Una toppa in arrivo: geometria in celle, misura in pixel (la decide chi ha
/// il font) e i byte accumulati finora.
struct PendingPatch {
    col: usize,
    row: usize,
    cells: usize,
    rows: usize,
    px_w: usize,
    px_h: usize,
    alt: [u8; PATCH_ALT_MAX],
    alt_len: usize,
    bytes: Vec<u8>,
    total: usize,
}

/// L'ultima toppa che copre `col` (l'ultima disegnata vince).
fn covering_segment<'a>(segs: &'a [Segment], col: usize) -> Option<&'a Segment> {
    segs.iter()
        .rev()
        .find(|g| col >= g.col && col < g.col + g.cells)
}

/// Scrive l'alt-text di una toppa in `cells` celle: tronca al confine di
/// carattere, poi imbottisce di spazi (una toppa occupa sempre lo spazio che
/// copre, anche nel dump).
fn push_alt(out: &mut Vec<u8>, alt: &[u8], cells: usize) {
    let Ok(text) = core::str::from_utf8(alt) else {
        for _ in 0..cells {
            out.push(b' ');
        }
        return;
    };
    let mut used = 0usize;
    for ch in text.chars() {
        if used >= cells {
            break;
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        used += 1;
    }
    while used < cells {
        out.push(b' ');
        used += 1;
    }
}

impl Session {
    /// Nuova sessione `cols×rows` con scrollback `scrollback` righe.
    pub fn new(cols: usize, rows: usize, scrollback: usize, owner: Option<i64>) -> Self {
        Session {
            grid: Grid::new_with_scrollback(cols, rows, scrollback),
            decoder: Decoder::new(),
            pending: Vec::new(),
            esc: 0,
            csi_params: Vec::new(),
            csi_private: false,
            alt: false,
            saved: None,
            fg: color::DEFAULT_FG,
            bg: color::DEFAULT_BG,
            cursor_drawn: (0, 0),
            cursor_drawn_visible: false,
            bulk_feed: false,
            bulk_writes: 0,
            input: VecDeque::new(),
            echo: true,
            owner,
            auth_user: [0u8; AUTH_USER_MAX],
            auth_len: 0,
            auth_by: 0,
            resized: false,
            patch_pending: None,
            resume: Resume::None,
        }
    }

    /// L'utente autenticato della sessione (`b""` = anonima).
    pub fn auth(&self) -> &[u8] {
        &self.auth_user[..self.auth_len as usize]
    }

    /// `true` se la sessione ha un'identita' (login riuscito).
    pub fn is_authenticated(&self) -> bool {
        self.auth_len != 0
    }

    /// Scrive (o azzera, con `user` vuoto) l'identita' della sessione.
    /// `by` = pid che l'ha stabilita (login sulla 0, DE sulle tile): alla sua
    /// morte l'identita' sparisce. `false` = nome troppo lungo o non UTF-8
    /// (fail-loud: il server risponde `!0`, non tronca in silenzio).
    pub fn set_auth(&mut self, user: &[u8], by: i64) -> bool {
        if user.len() > AUTH_USER_MAX {
            return false;
        }
        if !user.is_empty() && core::str::from_utf8(user).is_err() {
            return false;
        }
        self.auth_user = [0u8; AUTH_USER_MAX];
        self.auth_user[..user.len()].copy_from_slice(user);
        self.auth_len = user.len() as u8;
        self.auth_by = if user.is_empty() { 0 } else { by };
        true
    }

    /// Toglie l'identita' se l'ha stabilita `pid` (morte del login o del DE).
    pub fn drop_auth_of(&mut self, pid: i64) -> bool {
        if self.auth_by != pid || pid == 0 {
            return false;
        }
        self.set_auth(b"", 0)
    }

    /// Decodifica i byte in codepoint. Il buffer e' **ceduto** al chiamante
    /// (che lo restituisce con [`Session::give_pending`]): evita di tenere un
    /// mut-borrow della sessione mentre il display lavora.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<u32> {
        self.decoder.push(bytes, &mut self.pending);
        core::mem::take(&mut self.pending)
    }

    /// Restituisce il buffer dei codepoint (riusandone la capacita').
    pub fn give_pending(&mut self, mut cps: Vec<u32>) {
        cps.clear();
        self.pending = cps;
    }

    /// Processa un codepoint. `advance` = celle occupate dal glifo (1 per un
    /// carattere normale, `font.glyph_width_cells` per le icone larghe).
    ///
    /// Se serve uno scroll ritorna [`Step::NeedScroll`] **senza** averlo
    /// fatto: il chiamante scrolla e richiama `put` con lo stesso codepoint
    /// (la sessione completa l'azione pendente).
    pub fn put(&mut self, cp: u32, advance: usize) -> Step {
        let advance = advance.max(1);
        if !matches!(self.resume, Resume::None) {
            return self.resume_after_scroll();
        }
        match self.esc {
            1 => {
                self.esc = if cp == b'[' as u32 { 2 } else { 0 };
                return Step::Done;
            }
            2 => {
                let c = cp as u8;
                if c.is_ascii_digit() || c == b';' {
                    self.csi_params.push(c);
                    return Step::Done;
                }
                // `?` = modo privato (`ESC[?25h`): i parametri si accumulano
                // lo stesso, il finale `h`/`l` decide.
                if c == b'?' {
                    self.csi_private = true;
                    return Step::Done;
                }
                self.esc = 0;
                let params = core::mem::take(&mut self.csi_params);
                if self.csi_private {
                    self.csi_private = false;
                    self.handle_mode(c, &params);
                    return Step::Done;
                }
                self.handle_csi(c, &params);
                return Step::Done;
            }
            _ => {}
        }
        match cp {
            0x1B => {
                self.esc = 1;
            }
            0x0A => {
                let row = self.grid.cursor.1;
                if row + 1 >= self.grid.rows() {
                    self.resume = Resume::Newline;
                    return Step::NeedScroll;
                }
                self.grid.cursor.1 = row + 1;
                self.grid.cursor.0 = 0;
            }
            0x0D => {
                self.grid.cursor.0 = 0;
            }
            0x08 => {
                let back = self.grid.span_back(self.grid.cursor.0, self.grid.cursor.1);
                self.grid.cursor.0 -= back;
            }
            0x0C => {
                self.grid.clear();
                self.grid.cursor = (0, 0);
            }
            _ => {
                let (mut col, mut row) = self.grid.cursor;
                if col + advance > self.grid.cols() {
                    if row + 1 >= self.grid.rows() {
                        self.resume = Resume::WideWrap { cp, advance };
                        return Step::NeedScroll;
                    }
                    col = 0;
                    row += 1;
                }
                self.grid.put_wide(col, row, cp, advance, self.fg, self.bg);
                if col + advance >= self.grid.cols() {
                    self.grid.cursor.0 = 0;
                    if row + 1 >= self.grid.rows() {
                        self.resume = Resume::Home;
                        return Step::NeedScroll;
                    }
                    self.grid.cursor.1 = row + 1;
                } else {
                    self.grid.cursor = (col + advance, row);
                }
            }
        }
        Step::Done
    }

    /// i-esimo parametro numerico (0 = il primo). Vuoto o assente =
    /// `default`. `ESC[5D` → `param(0,1) == 5`; `ESC[D` → `1`.
    fn param(&self, params: &[u8], i: usize, default: usize) -> usize {
        let mut n = 0usize;
        for part in params.split(|&b| b == b';') {
            if n == i {
                if part.is_empty() {
                    return default;
                }
                let mut v = 0usize;
                for &b in part {
                    if b.is_ascii_digit() {
                        v = v.saturating_mul(10).saturating_add((b - b'0') as usize);
                    }
                }
                return if v == 0 { default } else { v };
            }
            n += 1;
        }
        default
    }

    /// CSI non privata: movimento del cursore, cancellazione, SGR.
    fn handle_csi(&mut self, c: u8, params: &[u8]) {
        let n = self.param(params, 0, 1);
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        if cols == 0 || rows == 0 {
            return;
        }
        let (mut col, mut row) = self.grid.cursor;
        match c {
            b'A' => row = row.saturating_sub(n),
            b'B' => row = (row + n).min(rows - 1),
            b'C' => col = (col + n).min(cols - 1),
            // Indietro: di span in span (un glifo largo non si attraversa).
            b'D' => {
                for _ in 0..n {
                    col -= self.grid.span_back(col, row);
                }
            }
            b'E' => {
                col = 0;
                row = (row + n).min(rows - 1);
            }
            b'F' => {
                col = 0;
                row = row.saturating_sub(n);
            }
            // Colonna / riga assoluta (1-based, come il CUP).
            b'G' => col = (n - 1).min(cols - 1),
            b'd' => row = (n - 1).min(rows - 1),
            b'H' | b'f' => {
                row = (self.param(params, 0, 1) - 1).min(rows - 1);
                col = (self.param(params, 1, 1) - 1).min(cols - 1);
            }
            // `J`/`K` prendono un **modo**, non un conteggio: senza
            // parametro vale 0 (dal cursore in poi), non 1.
            b'J' => self.erase_display(self.param(params, 0, 0)),
            b'K' => self.erase_line(self.param(params, 0, 0)),
            b'm' => self.handle_sgr(params),
            _ => {}
        }
        self.grid.cursor = (col, row);
    }

    /// `ESC[<n>J`: 0 = dal cursore a fine schermo, 1 = dall'inizio al cursore,
    /// 2 = tutto (il cursore **non** si muove: e' cosi' in un terminale vero).
    fn erase_line_from(&mut self, row: usize, from: usize, to: usize) {
        let cols = self.grid.cols();
        if cols == 0 || from > to {
            return;
        }
        let mut c = from.min(cols - 1);
        let to = to.min(cols - 1);
        while c <= to {
            self.grid.put_wide(c, row, b' ' as u32, 1, self.fg, self.bg);
            c += 1;
        }
        self.grid.mark_rect(from.min(cols - 1), row, to, row);
    }

    fn erase_display(&mut self, mode: usize) {
        let (col, row) = self.grid.cursor;
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        if cols == 0 || rows == 0 {
            return;
        }
        match mode {
            2 => {
                // Tutto: la griglia si svuota ma il cursore resta dov'era.
                self.grid.clear();
                self.grid.cursor = (col, row);
            }
            1 => {
                for r in 0..row {
                    self.erase_line_from(r, 0, cols - 1);
                }
                self.erase_line_from(row, 0, col);
                self.grid.mark_rect(0, 0, cols - 1, row);
            }
            _ => {
                for r in row..rows {
                    self.erase_line_from(r, 0, cols - 1);
                }
                self.grid.mark_rect(0, row, cols - 1, rows - 1);
            }
        }
    }

    /// `ESC[<n>K`: 0 = dal cursore a fine riga, 1 = dall'inizio al cursore,
    /// 2 = tutta la riga.
    fn erase_line(&mut self, mode: usize) {
        let (col, row) = self.grid.cursor;
        let cols = self.grid.cols();
        match mode {
            2 => self.erase_line_from(row, 0, cols - 1),
            1 => self.erase_line_from(row, 0, col),
            _ => {
                let (c0, r) = (col, row);
                self.grid.erase_from(c0, r, self.fg, self.bg);
                self.grid.mark_rect(c0.min(cols.saturating_sub(1)), r, cols - 1, r);
            }
        }
    }

    /// `ESC[?<n>h/l`: modi del terminale. Solo quelli che servono a
    /// un'applicazione a schermo intero; gli altri si ignorano (un terminale
    /// che rifiuta un modo ignoto non e' un terminale).
    fn handle_mode(&mut self, c: u8, params: &[u8]) {
        let n = self.param(params, 0, 1);
        let on = c == b'h';
        match (n, c) {
            (25, b'h') => self.grid.cursor_visible = true,
            (25, b'l') => self.grid.cursor_visible = false,
            (1049, b'h') => self.enter_alt(),
            (1049, b'l') => self.exit_alt(),
            _ => {}
        }
        let _ = on;
    }

    /// Entra nella schermata **alternativa**: la principale (con il suo
    /// scrollback) va da parte, si parte da una tela vuota **senza**
    /// scrollback. Ripetere l'ingresso non azzera quella gia' attiva.
    fn enter_alt(&mut self) {
        if self.alt {
            return;
        }
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        let fresh = Grid::new_with_scrollback(cols, rows, 0);
        let old = core::mem::replace(&mut self.grid, fresh);
        self.saved = Some(SavedScreen {
            grid: old,
            cursor: self.grid.cursor,
            cursor_visible: true,
        });
        self.alt = true;
        self.grid.mark_all();
        self.cursor_drawn = (usize::MAX, usize::MAX);
    }

    /// Torna alla schermata principale: il testo che c'era e' ancora li'
    /// (scrollback compreso), il cursore alla sua posizione.
    fn exit_alt(&mut self) {
        if !self.alt {
            return;
        }
        if let Some(saved) = self.saved.take() {
            self.grid = saved.grid;
            self.grid.cursor = saved.cursor;
            self.grid.cursor_visible = saved.cursor_visible;
            self.grid.mark_all();
        }
        self.alt = false;
        self.cursor_drawn = (usize::MAX, usize::MAX);
    }

    /// `true` se la schermata alternativa e' attiva (per chi disegna e per i
    /// test).
    pub fn alt_screen(&self) -> bool {
        self.alt
    }

    /// Completa l'azione che aveva richiesto lo scroll (il modello ha gia'
    /// scorso: riga del cursore invariata).
    fn resume_after_scroll(&mut self) -> Step {
        let resume = core::mem::replace(&mut self.resume, Resume::None);
        match resume {
            Resume::None => Step::Done,
            Resume::Newline => {
                self.grid.cursor.0 = 0;
                Step::Done
            }
            Resume::Home => Step::Done,
            Resume::WideWrap { cp, advance } => {
                let row = self.grid.cursor.1;
                self.grid.put_wide(0, row, cp, advance, self.fg, self.bg);
                if advance >= self.grid.cols() {
                    self.grid.cursor.0 = 0;
                    if row + 1 >= self.grid.rows() {
                        self.resume = Resume::Home;
                        return Step::NeedScroll;
                    }
                    self.grid.cursor.1 = row + 1;
                } else {
                    self.grid.cursor = (advance, row);
                }
                Step::Done
            }
        }
    }

    /// Select Graphic Rendition (ESC[...m): truecolor (`38;2` / `48;2`) e
    /// 256 colori (`38;5` / `48;5`). I parametri ignoti sono ignorati.
    pub fn handle_sgr(&mut self, params: &[u8]) {
        let mut nums: Vec<u32> = Vec::new();
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
                    self.fg = color::DEFAULT_FG;
                    self.bg = color::DEFAULT_BG;
                }
                38 => {
                    if i + 1 < nums.len() {
                        match nums[i + 1] {
                            2 => {
                                if i + 4 < nums.len() {
                                    self.fg = term::surface::rgba(
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
                                    self.fg = term::surface::color256(nums[i + 2] as u8);
                                    i += 2;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                48 => {
                    if i + 1 < nums.len() {
                        match nums[i + 1] {
                            2 => {
                                if i + 4 < nums.len() {
                                    self.bg = term::surface::rgba(
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
                                    self.bg = term::surface::color256(nums[i + 2] as u8);
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

    /// Accoda input (tastiera o iniezione). Ritorna `true` se va
    /// **echeggiato** sul modello (`echo` acceso): l'eco e' un feed normale,
    /// quindi passa da scroll e render come qualunque altro testo.
    ///
    /// A coda piena si scarta il byte **nuovo**, non il piu' vecchio: il
    /// lettore sta per leggere la testa, e i tasti gia' arrivati sono quelli
    /// che l'utente ha battuto prima (D8d).
    pub fn push_input(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            if self.input.len() < INPUT_MAX {
                self.input.push_back(b);
            }
        }
        self.echo
    }

    /// Copia in `out` i byte di input in attesa (fino a `out.len()`).
    pub fn read_input(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.input.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.input.pop_front().unwrap_or(0);
        }
        n
    }

    /// Celle non vuote + CRC delle ultime `limit` (gemella di
    /// `CONSOLE_STATE`: la prova che il testo e' sopravvissuto).
    pub fn state(&self, limit: usize) -> (usize, u32) {
        (self.grid.non_empty(), self.grid.text_crc_last(limit))
    }

    /// Copia la **coda** del testo visibile (righe separate da `\n`, spazi
    /// finali tolti) in `out`, fino a `out.len()` byte. Ritorna
    /// `(scritti, totale)`.
    pub fn dump_text(&self, out: &mut [u8]) -> (usize, usize) {
        // Solo fino all'ultima riga che ha davvero contenuto (o al cursore):
        // le righe vuote in fondo gonfierebbero il totale e la "coda" del
        // dump sarebbe fatta di righe vuote, non di testo.
        let mut last = self.grid.cursor.1.min(self.grid.rows().saturating_sub(1));
        for row in 0..self.grid.rows() {
            // Una riga con una toppa ha contenuto anche se le celle sono
            // vuote: la toppa e' contenuto (ADR-0099).
            if !self.grid.segments_at(row).is_empty()
                || (0..self.grid.cols()).any(|col| {
                    let c = self.grid.cell(col, row);
                    !c.is_blank() || c.is_cont()
                })
            {
                last = last.max(row);
            }
        }
        let mut all: Vec<u8> = Vec::new();
        for row in 0..=last {
            let mut end = self.grid.cols();
            while end > 0 {
                let c = self.grid.cell(end - 1, row);
                if c.cp == b' ' as u32 && !c.is_cont() {
                    end -= 1;
                } else {
                    break;
                }
            }
            for seg in self.grid.segments_at(row) {
                end = end.max(seg.col + seg.cells);
            }
            // Le toppe: al posto delle celle coperte va il loro **alt-text**
            // (una toppa non ha forma testuale propria; ADR-0097 §7).
            let segs = self.grid.segments_at(row);
            let mut col = 0usize;
            while col < end {
                match covering_segment(segs, col) {
                    Some(seg) if seg.col == col => {
                        push_alt(&mut all, &seg.alt, seg.cells);
                        col += seg.cells;
                    }
                    Some(_) => col += 1, // dentro una toppa gia' resa
                    None => {
                        let c = self.grid.cell(col, row);
                        if c.is_cont() {
                            col += 1;
                            continue;
                        }
                        let ch = char::from_u32(if c.cp == 0 { b' ' as u32 } else { c.cp })
                            .unwrap_or('?');
                        let mut buf = [0u8; 4];
                        all.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        col += 1;
                    }
                }
            }
            all.push(b'\n');
        }
        let total = all.len();
        let max = out.len();
        if max == 0 {
            return (0, total);
        }
        let mut start = total.saturating_sub(max);
        // Non tagliare a meta' un codepoint UTF-8.
        while start < total && (all[start] & 0xC0) == 0x80 {
            start += 1;
        }
        let n = total - start;
        out[..n].copy_from_slice(&all[start..]);
        (n, total)
    }

    /// Ridimensiona il modello (reflow + scrollback, D4).
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.grid.cols() && rows == self.grid.rows() {
            return;
        }
        self.grid.resize(cols, rows);
        self.cursor_drawn = (usize::MAX, usize::MAX);
        self.resized = true;
        if let Some(saved) = self.saved.as_mut() {
            saved.grid.resize(cols, rows);
        }
    }

    /// Consuma il flag "geometria cambiata" (`SESS_INFO`): la prossima
    /// risposta non lo riporta finche' non cambia di nuovo.
    pub fn take_resized(&mut self) -> bool {
        core::mem::take(&mut self.resized)
    }

    // ── Toppe di pixel (ADR-0099) ────────────────────────────────────

    /// Apre lo staging di una toppa: `cells × rows` celle in `(col, row)`,
    /// `px_w × px_h` pixel (li calcola chi ha le metriche del font), con
    /// l'alt-text gia' presente.
    ///
    /// Rifiuta (`false`) se: c'e' gia' uno staging, la posizione esce dalla
    /// griglia, la toppa e' vuota, o il **tetto** `PATCH_MAX_BYTES` verrebbe
    /// superato. Il tetto si controlla qui e non tocca mai le toppe gia'
    /// presenti: una toppa rifiutata e' un errore per il client, non una
    /// perdita silenziosa per il log.
    pub fn patch_begin(
        &mut self,
        col: usize,
        row: usize,
        cells: usize,
        rows: usize,
        px_w: usize,
        px_h: usize,
        alt: &[u8],
    ) -> bool {
        if self.patch_pending.is_some() || cells == 0 || rows == 0 {
            return false;
        }
        if col + cells > self.grid.cols() || row + rows > self.grid.rows() {
            return false;
        }
        let total = px_w * px_h * 4;
        if total == 0 || px_h < rows {
            return false;
        }
        if self.grid.patch_bytes().saturating_add(total) > PATCH_MAX_BYTES {
            return false;
        }
        let n = alt.len().min(PATCH_ALT_MAX);
        let mut buf = [0u8; PATCH_ALT_MAX];
        buf[..n].copy_from_slice(&alt[..n]);
        self.patch_pending = Some(PendingPatch {
            col,
            row,
            cells,
            rows,
            px_w,
            px_h,
            alt: buf,
            alt_len: n,
            bytes: Vec::with_capacity(total),
            total,
        });
        true
    }

    /// Accumula un blocco della toppa. `offset` deve essere esattamente la
    /// lunghezza gia' ricevuta (il protocollo e' sequenziale: un buco e' un
    /// bug del client, non qualcosa da interpretare). Al completamento la
    /// toppa entra nella griglia (una fetta per riga) e lo staging si chiude.
    pub fn patch_data(&mut self, offset: usize, data: &[u8]) -> bool {
        let Some(p) = self.patch_pending.as_mut() else {
            return false;
        };
        if offset != p.bytes.len() || p.bytes.len() + data.len() > p.total {
            self.patch_pending = None;
            return false;
        }
        p.bytes.extend_from_slice(data);
        if p.bytes.len() < p.total {
            return true;
        }
        // Commit: una fetta per riga (la toppa scorre con la sua riga).
        let Some(p) = self.patch_pending.take() else {
            return false;
        };
        let row_h = p.px_h / p.rows;
        let row_bytes = p.px_w * row_h * 4;
        let alt = &p.alt[..p.alt_len];
        for r in 0..p.rows {
            let from = r * row_bytes;
            let to = from + row_bytes;
            if to > p.bytes.len() {
                break;
            }
            let seg = Segment {
                col: p.col,
                cells: p.cells,
                px_w: p.px_w,
                px_h: row_h,
                bitmap: p.bytes[from..to].to_vec(),
                // L'alt-text va sulla **prima** riga: le altre lo lascerebbero
                // ripetere nel dump.
                alt: if r == 0 { alt.to_vec() } else { Vec::new() },
            };
            if !self.grid.put_segment(p.row + r, seg) {
                return false;
            }
        }
        true
    }

    /// Abbandona lo staging (errore del client o disconnessione).
    pub fn patch_abort(&mut self) {
        self.patch_pending = None;
    }

    /// Byte di toppe della sessione (griglia + cronologia).
    pub fn patch_bytes(&self) -> usize {
        self.grid.patch_bytes()
    }

    /// Sessione "sporca" di cose da ridisegnare?
    pub fn has_dirty(&self) -> bool {
        self.grid.has_dirty()
    }
}

/// La tabella delle sessioni del servizio `console`.
///
/// Gli **id** sono stabili (indici di slot, mai riusati finche' il posto non
/// torna libero): un handle consegnato a un client resta valido fino alla
/// chiusura, e la sessione 0 e' sempre la stessa.
pub struct Sessions {
    slots: Vec<Option<Session>>,
    /// La sessione disegnata (D8c1: sempre 0).
    pub focus: usize,
    /// `pid -> sessione` (il DE lega un figlio appena spawnato, D8e).
    bindings: Vec<(i64, usize)>,
    /// `canale IPC -> sessione` (la capability e' la mailbox consegnata).
    attachments: Vec<(u64, usize)>,
    /// `sessione -> canale del **lettore dichiarato**`: chi ha letto per
    /// ultimo (`SESS_READ`). E' lui che riceve l'hint `SESS_INPUT_EVENT`
    /// quando arriva input — un hint perso non perde tasti (la coda e' la
    /// verita'), uno in piu' fa solo un drain a vuoto.
    readers: Vec<(usize, u64)>,
    /// Geometria con cui nascono le sessioni nuove (quella del display).
    pub cols: usize,
    pub rows: usize,
}

impl Sessions {
    /// Tabella con la **sessione 0** (lo schermo), scrollback 1000 come il
    /// terminale di sempre.
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut slots: Vec<Option<Session>> = Vec::with_capacity(MAX_SESSIONS);
        slots.push(Some(Session::new(
            cols,
            rows,
            term::grid::SCROLLBACK_MAX,
            None,
        )));
        Sessions {
            slots,
            focus: 0,
            bindings: Vec::new(),
            attachments: Vec::new(),
            readers: Vec::new(),
            cols,
            rows,
        }
    }

    /// Numero di sessioni vive.
    pub fn count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// La sessione (id stabile).
    pub fn get(&self, id: usize) -> Option<&Session> {
        self.slots.get(id).and_then(|s| s.as_ref())
    }

    /// La sessione mutabile.
    pub fn get_mut(&mut self, id: usize) -> Option<&mut Session> {
        self.slots.get_mut(id).and_then(|s| s.as_mut())
    }

    /// La sessione in focus.
    pub fn focus_session(&self) -> Option<&Session> {
        self.get(self.focus)
    }

    /// La sessione in focus, mutabile.
    pub fn focus_mut(&mut self) -> Option<&mut Session> {
        let f = self.focus;
        self.get_mut(f)
    }

    /// Crea una sessione: primo slot libero (mai lo 0), altrimenti in coda
    /// fino a [`MAX_SESSIONS`]. `owner` = pid che l'ha creata (attestazione).
    pub fn create(&mut self, owner: Option<i64>) -> Option<usize> {
        let id = match (1..self.slots.len()).find(|&i| self.slots[i].is_none()) {
            Some(i) => i,
            None if self.slots.len() < MAX_SESSIONS => {
                self.slots.push(None);
                self.slots.len() - 1
            }
            None => return None,
        };
        self.slots[id] = Some(Session::new(
            self.cols,
            self.rows,
            SESSION_SCROLLBACK,
            owner,
        ));
        Some(id)
    }

    /// Chiude la sessione `id` (mai la 0). Ritorna la sessione rimossa.
    pub fn close(&mut self, id: usize) -> Option<Session> {
        if id == 0 || id >= self.slots.len() {
            return None;
        }
        let s = self.slots[id].take()?;
        self.attachments.retain(|&(_, sid)| sid != id);
        self.bindings.retain(|&(_, sid)| sid != id);
        self.readers.retain(|&(sid, _)| sid != id);
        if self.focus == id {
            self.focus = 0;
        }
        Some(s)
    }

    /// Porta `id` in focus (solo una sessione viva).
    pub fn set_focus(&mut self, id: usize) -> bool {
        if self.get(id).is_some() {
            self.focus = id;
            true
        } else {
            false
        }
    }

    /// Lega `pid` alla sessione `id` (attestazione del DE).
    pub fn bind(&mut self, id: usize, pid: i64) -> bool {
        if self.get(id).is_none() {
            return false;
        }
        self.bindings.retain(|&(p, _)| p != pid);
        self.bindings.push((pid, id));
        true
    }

    /// La sessione di `pid`: binding esplicito, altrimenti la sessione di cui
    /// e' **owner** (il pid attestato alla creazione).
    pub fn session_of_pid(&self, pid: i64) -> Option<usize> {
        if let Some(&(_, sid)) = self.bindings.iter().find(|&&(p, _)| p == pid) {
            if self.get(sid).is_some() {
                return Some(sid);
            }
        }
        (0..self.slots.len()).find(|&i| {
            self.slots
                .get(i)
                .and_then(|s| s.as_ref())
                .map(|s| s.owner == Some(pid))
                .unwrap_or(false)
        })
    }

    /// Registra che il canale `chan` e' attaccato alla sessione `id`.
    pub fn attach(&mut self, chan: u64, id: usize) {
        self.attachments.retain(|&(c, _)| c != chan);
        self.attachments.push((chan, id));
    }

    /// La sessione a cui il canale `chan` e' attaccato.
    pub fn attached(&self, chan: u64) -> Option<usize> {
        self.attachments
            .iter()
            .find(|&&(c, _)| c == chan)
            .map(|&(_, sid)| sid)
            .filter(|&sid| self.get(sid).is_some())
    }

    /// Toglie l'attachment di un canale (chiusura esplicita o morte) e il suo
    /// ruolo di lettore.
    pub fn detach(&mut self, chan: u64) {
        self.attachments.retain(|&(c, _)| c != chan);
        self.readers.retain(|&(_, c)| c != chan);
    }

    /// Registra il **lettore dichiarato** di una sessione (`SESS_READ`): e'
    /// l'unico che riceve l'hint dell'input.
    pub fn set_reader(&mut self, chan: u64, id: usize) {
        self.readers.retain(|&(s, _)| s != id);
        self.readers.push((id, chan));
    }

    /// Il canale che legge la sessione `id`, se qualcuno l'ha dichiarato.
    pub fn reader_of(&self, id: usize) -> Option<u64> {
        self.readers
            .iter()
            .find(|&&(s, _)| s == id)
            .map(|&(_, c)| c)
    }

    /// Toglie il lettore di un canale (morte, hint fallito, detach).
    pub fn clear_reader(&mut self, chan: u64) {
        self.readers.retain(|&(_, c)| c != chan);
    }

    /// Purga le tracce di un processo morto: bindings, sessioni di cui era
    /// **owner** (mai la 0) e identita' che aveva **stabilito** (S3.3c: la
    /// morte del login/DE non lascia una sessione autenticata). Ritorna gli id
    /// delle sessioni da chiudere (il server ne libera la mailbox).
    pub fn purge_pid(&mut self, pid: i64) -> Vec<usize> {
        self.bindings.retain(|&(p, _)| p != pid);
        for slot in self.slots.iter_mut().flatten() {
            let _ = slot.drop_auth_of(pid);
        }
        let owned: Vec<usize> = (1..self.slots.len())
            .filter(|&i| {
                self.slots
                    .get(i)
                    .and_then(|s| s.as_ref())
                    .map(|s| s.owner == Some(pid))
                    .unwrap_or(false)
            })
            .collect();
        for &id in &owned {
            let _ = self.close(id);
        }
        owned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    /// Feed di una stringa (ASCII) nella sessione, scrollando il modello come
    /// farebbe la console (qui senza pixel).
    fn feed(s: &mut Session, text: &str) {
        let mut cps = s.decode(text.as_bytes());
        for cp in cps.drain(..) {
            loop {
                match s.put(cp, 1) {
                    Step::Done => break,
                    Step::NeedScroll => s.grid.scroll_up(),
                }
            }
        }
        s.give_pending(cps);
    }

    /// Testo di una riga (senza spazi finali).
    fn row_str(s: &Session, row: usize) -> String {
        let mut out = String::new();
        for col in 0..s.grid.cols() {
            let c = s.grid.cell(col, row);
            out.push(char::from_u32(if c.cp == 0 { ' ' as u32 } else { c.cp }).unwrap_or('?'));
        }
        out.trim_end().to_string()
    }

    fn dump(s: &Session) -> String {
        let mut buf = alloc::vec![0u8; 4096];
        let (n, _) = s.dump_text(&mut buf);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn testo_semplice_e_cursore() {
        let mut s = Session::new(10, 3, 16, None);
        feed(&mut s, "abc");
        assert_eq!(row_str(&s, 0), "abc");
        assert_eq!(s.grid.cursor, (3, 0));
    }

    #[test]
    fn a_capo_e_scroll() {
        let mut s = Session::new(4, 2, 16, None);
        feed(&mut s, "ab\ncd\nef");
        // "ab" e "cd" sono uscite nella scrollback, "ef" e' in riga 1.
        assert_eq!(row_str(&s, 0), "cd");
        assert_eq!(row_str(&s, 1), "ef");
    }

    #[test]
    fn wrap_in_fondo_riga() {
        let mut s = Session::new(4, 2, 16, None);
        // 4 caratteri riempiono la riga e il cursore va a capo.
        feed(&mut s, "abcd");
        assert_eq!(row_str(&s, 0), "abcd");
        assert_eq!(s.grid.cursor, (0, 1));
    }

    #[test]
    fn backspace_e_cancellazione() {
        let mut s = Session::new(10, 2, 16, None);
        feed(&mut s, "abc\x08X");
        assert_eq!(row_str(&s, 0), "abX");
        feed(&mut s, "\x1b[K");
        assert_eq!(row_str(&s, 0), "abX");
    }

    #[test]
    fn csi_conteggi_e_posizione() {
        let mut s = Session::new(20, 4, 8, None);
        feed(&mut s, "abcdef\x1b[3D");
        assert_eq!(s.grid.cursor.0, 3, "ESC[3D torna di 3 celle");
        feed(&mut s, "\x1b[2C");
        assert_eq!(s.grid.cursor.0, 5);
        feed(&mut s, "\x1b[3;10H");
        assert_eq!(s.grid.cursor, (9, 2));
        feed(&mut s, "X");
        assert_eq!(row_str(&s, 2), "         X", "CUP scrive dove dice (riga 3)");
        // `ESC[D` senza parametro = 1 cella (comportamento storico).
        feed(&mut s, "\x1b[D");
        assert_eq!(s.grid.cursor.0, 9);
        // Mai fuori griglia.
        feed(&mut s, "\x1b[999;999H");
        assert_eq!(s.grid.cursor, (19, 3)); // clip all'ultima riga (4 righe: 0..3)
    }

    #[test]
    fn ed_el_e_cursore_nascosto() {
        let mut s = Session::new(10, 3, 8, None);
        feed(&mut s, "aaaa\nbbbb\ncccc");
        // EL 0: dal cursore a fine riga (cursore a fine di "cccc").
        feed(&mut s, "\x1b[2;2H\x1b[K");
        assert_eq!(row_str(&s, 1), "b", "ESC[K cancella dal cursore in poi");
        // ED 2: tutto, cursore fermo dov'era (un terminale vero non lo muove).
        feed(&mut s, "\x1b[3;3H\x1b[2J");
        assert_eq!(s.grid.cursor, (2, 2));
        assert_eq!(dump(&s).trim(), "", "ED 2 svuota lo schermo");
        // Cursore nascosto/visibile: il `?` non deve finire a schermo.
        feed(&mut s, "\x1b[?25l");
        assert!(!s.grid.cursor_visible);
        assert!(!dump(&s).contains("25l"), "il modo privato non e' testo");
        feed(&mut s, "\x1b[?25h");
        assert!(s.grid.cursor_visible);
    }

    #[test]
    fn schermo_alternativo_conserva_la_principale() {
        let mut s = Session::new(12, 3, 8, None);
        feed(&mut s, "principale");
        let before = dump(&s);
        feed(&mut s, "\x1b[?1049h");
        assert!(s.alt_screen());
        assert_eq!(dump(&s).trim(), "", "la tela alternativa parte vuota");
        feed(&mut s, "tela\x1b[2;3H*");
        assert!(dump(&s).contains("tela"));
        feed(&mut s, "\x1b[?1049l");
        assert!(!s.alt_screen());
        assert_eq!(dump(&s), before, "la principale torna com'era");
        // Entrare due volte non azzera la tela gia' attiva; uscire due volte
        // non scala la principale.
        feed(&mut s, "\x1b[?1049h\x1b[?1049h");
        feed(&mut s, "x");
        feed(&mut s, "\x1b[?1049l\x1b[?1049l");
        assert_eq!(dump(&s), before);
    }

    #[test]
    fn resize_segnala_il_cambiamento_una_volta() {
        let mut s = Session::new(20, 4, 8, None);
        assert!(!s.take_resized(), "nessun resize all'inizio");
        // Stessa misura: nessun segnale (il display che si riadatta uguale
        // non deve svegliare nessuno).
        s.resize(20, 4);
        assert!(!s.take_resized());
        s.resize(30, 8);
        assert!(s.take_resized(), "la misura e' cambiata");
        assert!(!s.take_resized(), "il flag si consuma");
        assert_eq!((s.grid.cols(), s.grid.rows()), (30, 8));
        // Anche la schermata principale messa da parte si riadatta: uscendo
        // dalla tela si trova la misura giusta, non quella di prima.
        feed(&mut s, "principale\x1b[?1049h");
        s.resize(24, 6);
        assert!(s.take_resized());
        feed(&mut s, "\x1b[?1049l");
        assert_eq!((s.grid.cols(), s.grid.rows()), (24, 6));
        assert!(dump(&s).contains("principale"));
    }

    #[test]
    fn toppa_staging_commit_e_tetto() {
        let mut s = Session::new(12, 3, 8, None);
        // 2 celle x 1 riga, 10x22 px per cella: 20x22 BGRA.
        let (px_w, px_h) = (20, 22);
        let total = px_w * px_h * 4;
        assert!(s.patch_begin(1, 1, 2, 1, px_w, px_h, b"<>"));
        // Uno staging alla volta.
        assert!(!s.patch_begin(0, 0, 1, 1, 10, 22, b""));
        // Offset fuori ordine: scarta lo staging, niente pixel a meta'.
        assert!(!s.patch_data(4, &[0u8; 8]));
        assert_eq!(s.patch_bytes(), 0);
        // Commit a blocchi: la toppa entra solo quando e' completa.
        assert!(s.patch_begin(1, 1, 2, 1, px_w, px_h, b"<>"));
        let bitmap = alloc::vec![0x40u8; total];
        let half = total / 2;
        assert!(s.patch_data(0, &bitmap[..half]));
        assert_eq!(s.patch_bytes(), 0, "incompleta: non e' ancora nella griglia");
        assert!(s.patch_data(half, &bitmap[half..]));
        assert_eq!(s.patch_bytes(), total);
        // Il dump rende l'alt-text al posto delle celle.
        let d = dump(&s);
        assert!(d.contains("<>"), "dump senza alt-text: {:?}", d);
        // Oltre griglia: rifiutata.
        assert!(!s.patch_begin(11, 0, 2, 1, px_w, px_h, b""));
        assert!(!s.patch_begin(0, 2, 1, 2, 10, 22, b""));
    }

    #[test]
    fn toppa_tetto_solo_rifiuto() {
        let mut s = Session::new(8, 2, 4, None);
        // Due toppe che stanno: la prima grande (meta' del tetto), la seconda
        // piccola. Il tetto **non** toglie quella gia' presente.
        let big = PATCH_MAX_BYTES / 2;
        // 1 cella, altezza 1 riga: px_w*px_h*4 = big  →  px misura inventata
        // purche' coerente (il modello non conosce il font).
        let px_w = 1024;
        let px_h = big / (px_w * 4);
        assert!(s.patch_begin(0, 0, 1, 1, px_w, px_h, b"A"));
        assert!(s.patch_data(0, &alloc::vec![1u8; px_w * px_h * 4]));
        let after_first = s.patch_bytes();
        assert_eq!(after_first, px_w * px_h * 4);
        // Una seconda toppa che sfonderebbe il tetto: rifiutata all'ingresso.
        assert!(!s.patch_begin(1, 0, 1, 1, px_w, px_h * 2, b"B"));
        // ...e la prima e' ancora li'.
        assert_eq!(s.patch_bytes(), after_first);
        assert!(dump(&s).contains('A'));
    }

    #[test]
    fn toppa_scorre_e_muore_con_la_riga() {
        let mut s = Session::new(10, 2, 4, None);
        let (px_w, px_h) = (10, 22);
        let total = px_w * px_h * 4;
        assert!(s.patch_begin(0, 0, 1, 1, px_w, px_h, b"X"));
        assert!(s.patch_data(0, &alloc::vec![9u8; total]));
        assert_eq!(s.patch_bytes(), total);
        // Lo schermo scorre: la toppa va nello scrollback (c'e' ancora).
        s.grid.scroll_up();
        assert_eq!(s.patch_bytes(), total);
        // `ESC[2J` libera tutto.
        feed(&mut s, "\x1b[2J");
        assert_eq!(s.patch_bytes(), 0);
    }

    #[test]
    fn sgr_truecolor() {
        let mut s = Session::new(10, 2, 16, None);
        feed(&mut s, "\x1b[38;2;1;2;3mZ");
        let c = s.grid.cell(0, 0);
        assert_eq!(c.fg, term::surface::rgba(1, 2, 3, 0xFF));
        // Reset
        feed(&mut s, "\x1b[0mY");
        assert_eq!(s.grid.cell(1, 0).fg, color::DEFAULT_FG);
    }

    #[test]
    fn sgr_256() {
        let mut s = Session::new(4, 2, 16, None);
        feed(&mut s, "\x1b[38;5;42mQ");
        assert_eq!(s.grid.cell(0, 0).fg, term::surface::color256(42));
    }

    #[test]
    fn input_eco_e_read() {
        let mut s = Session::new(8, 2, 16, None);
        assert!(s.push_input(b"pw"), "eco acceso di default");
        let mut buf = [0u8; 8];
        assert_eq!(s.read_input(&mut buf), 2);
        assert_eq!(&buf[..2], b"pw");
        // Coda vuota: niente da leggere.
        assert_eq!(s.read_input(&mut buf), 0);
        // Eco spento: push non chiede il feed.
        s.echo = false;
        assert!(!s.push_input(b"x"));
    }

    #[test]
    fn stato_e_dump() {
        let mut s = Session::new(10, 3, 16, None);
        feed(&mut s, "littera");
        let (non_empty, crc) = s.state(0);
        assert!(non_empty >= 7);
        assert_eq!(crc, s.grid.text_crc());
        assert!(dump(&s).contains("littera"));
    }

    #[test]
    fn dump_tiene_la_coda() {
        let mut s = Session::new(8, 2, 16, None);
        feed(&mut s, "vecchio\nnuovo");
        let mut buf = [0u8; 6];
        let (n, total) = s.dump_text(&mut buf);
        assert!(total > n);
        assert_eq!(&buf[..n], b"nuovo\n");
    }

    #[test]
    fn resize_conserva_il_testo() {
        let mut s = Session::new(20, 4, 16, None);
        feed(&mut s, "littera");
        s.resize(10, 3);
        assert_eq!(s.grid.cols(), 10);
        assert!(dump(&s).contains("littera"));
    }

    #[test]
    fn tabella_crea_chiude_e_focus() {
        let mut t = Sessions::new(20, 4);
        assert_eq!(t.count(), 1);
        let id = t.create(Some(7)).expect("sessione creata");
        assert_eq!(id, 1);
        assert_eq!(t.count(), 2);
        assert_eq!(t.session_of_pid(7), Some(id));
        assert!(t.set_focus(id));
        assert_eq!(t.focus, id);
        assert!(t.close(0).is_none(), "la sessione 0 non si chiude");
        assert!(t.close(id).is_some());
        assert_eq!(t.count(), 1);
        assert_eq!(t.focus, 0, "chiudere la sessione in focus riporta alla 0");
        assert_eq!(t.session_of_pid(7), None);
    }

    #[test]
    fn tabella_attachment_e_binding() {
        let mut t = Sessions::new(20, 4);
        let id = t.create(None).unwrap();
        t.attach(3, id);
        assert_eq!(t.attached(3), Some(id));
        assert!(t.bind(id, 42));
        assert_eq!(t.session_of_pid(42), Some(id));
        t.detach(3);
        assert_eq!(t.attached(3), None);
    }

    #[test]
    fn purge_chiude_le_sessioni_del_morto() {
        let mut t = Sessions::new(20, 4);
        let a = t.create(Some(9)).unwrap();
        let b = t.create(Some(9)).unwrap();
        let c = t.create(Some(10)).unwrap();
        let closed = t.purge_pid(9);
        assert_eq!(closed.len(), 2);
        assert!(closed.contains(&a) && closed.contains(&b));
        assert!(t.get(a).is_none() && t.get(b).is_none());
        assert!(t.get(c).is_some(), "le sessioni degli altri restano");
        assert!(t.get(0).is_some(), "la sessione 0 non si tocca mai");
    }

    #[test]
    fn identita_di_sessione() {
        let mut t = Sessions::new(20, 4);
        let s = t.get_mut(0).unwrap();
        assert!(!s.is_authenticated());
        assert!(s.set_auth(b"root", 7));
        assert_eq!(t.get(0).unwrap().auth(), b"root");
        assert!(t.get(0).unwrap().is_authenticated());
        // Un nome non UTF-8 o troppo lungo e' rifiutato (mai troncato).
        assert!(!t.get_mut(0).unwrap().set_auth(&[0xFF], 7));
        assert!(!t.get_mut(0).unwrap().set_auth(&[b'a'; AUTH_USER_MAX + 1], 7));
        // La morte di chi l'ha stabilita la toglie; un altro pid no.
        assert!(!t.get_mut(0).unwrap().drop_auth_of(8));
        t.purge_pid(7);
        assert_eq!(t.get(0).unwrap().auth(), b"");
        // Deautenticazione esplicita.
        assert!(t.get_mut(0).unwrap().set_auth(b"alice", 9));
        assert!(t.get_mut(0).unwrap().set_auth(b"", 0));
        assert!(!t.get(0).unwrap().is_authenticated());
        assert_eq!(t.get(0).unwrap().auth_by, 0);
    }

    #[test]
    fn lettore_dichiarato() {
        let mut t = Sessions::new(20, 4);
        let id = t.create(None).unwrap();
        assert_eq!(t.reader_of(id), None, "nessuno legge ancora");
        t.set_reader(5, id);
        assert_eq!(t.reader_of(id), Some(5));
        // Un secondo lettore sostituisce il primo (una sessione, un lettore).
        t.set_reader(6, id);
        assert_eq!(t.reader_of(id), Some(6));
        // Il detach del canale toglie anche il ruolo.
        t.attach(6, id);
        t.detach(6);
        assert_eq!(t.reader_of(id), None);
        // Chiusura della sessione: via il lettore.
        t.set_reader(7, id);
        let _ = t.close(id);
        assert_eq!(t.reader_of(id), None);
    }

    #[test]
    fn input_pieno_scarta_il_nuovo() {
        let mut s = Session::new(20, 3, 8, None);
        let mut bytes = alloc::vec::Vec::new();
        for i in 0..(INPUT_MAX + 5) {
            bytes.push(b'a' + (i % 26) as u8);
        }
        s.push_input(&bytes);
        let mut out = alloc::vec![0u8; INPUT_MAX + 8];
        let n = s.read_input(&mut out);
        assert_eq!(n, INPUT_MAX, "la coda si ferma al tetto");
        assert_eq!(out[0], b'a', "la testa e' il primo tasto battuto");
        assert_eq!(out[n - 1], bytes[INPUT_MAX - 1], "la coda e' il tetto");
    }

    #[test]
    fn limite_sessioni() {
        let mut t = Sessions::new(20, 4);
        for _ in 1..MAX_SESSIONS {
            assert!(t.create(None).is_some());
        }
        assert_eq!(t.count(), MAX_SESSIONS);
        assert!(t.create(None).is_none(), "oltre il tetto si nega");
    }
}
