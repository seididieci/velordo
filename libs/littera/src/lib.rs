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

use term::grid::{color, Grid};
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
pub struct Session {
    /// Il modello a celle (pubblico: chi disegna ne legge celle e cursore).
    pub grid: Grid,
    decoder: Decoder,
    pending: Vec<u32>,
    /// Stato del parser ESC/CSI: 0 normale, 1 ESC, 2 CSI entry, 3 parametri.
    esc: u8,
    /// Parametri CSI accumulati (es. `"38;2;255;0;0"`).
    csi_params: Vec<u8>,
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
    /// Token di sessione (D8f): l'**identita'** della sessione vive qui, non
    /// in una variabile d'ambiente. `0` = sessione anonima (nessun login).
    /// Lo scrive il login (`SESS_TOKEN_SET`) e lo legge chi e' attaccato
    /// (`SESS_TOKEN`); muore con la sessione.
    pub token: u64,
    resume: Resume,
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
            fg: color::DEFAULT_FG,
            bg: color::DEFAULT_BG,
            cursor_drawn: (0, 0),
            cursor_drawn_visible: false,
            bulk_feed: false,
            bulk_writes: 0,
            input: VecDeque::new(),
            echo: true,
            owner,
            token: 0,
            resume: Resume::None,
        }
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
                self.esc = 0;
                let params = core::mem::take(&mut self.csi_params);
                match c {
                    b'D' => {
                        let back = self.grid.span_back(self.grid.cursor.0, self.grid.cursor.1);
                        self.grid.cursor.0 -= back;
                    }
                    b'C' => {
                        if self.grid.cursor.0 + 1 < self.grid.cols() {
                            self.grid.cursor.0 += 1;
                        }
                    }
                    b'K' => {
                        let (col, row) = self.grid.cursor;
                        self.grid.erase_from(col, row, self.fg, self.bg);
                    }
                    b'm' => self.handle_sgr(&params),
                    _ => {}
                }
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
            if (0..self.grid.cols()).any(|col| {
                let c = self.grid.cell(col, row);
                !c.is_blank() || c.is_cont()
            }) {
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
            for col in 0..end {
                let c = self.grid.cell(col, row);
                if c.is_cont() {
                    continue;
                }
                let ch = char::from_u32(if c.cp == 0 { b' ' as u32 } else { c.cp })
                    .unwrap_or('?');
                let mut buf = [0u8; 4];
                all.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
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
        self.grid.resize(cols, rows);
        self.cursor_drawn = (usize::MAX, usize::MAX);
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

    /// Purga le tracce di un processo morto: bindings e le sessioni di cui
    /// era **owner** (mai la 0). Ritorna gli id delle sessioni da chiudere
    /// (il server ne libera la mailbox).
    pub fn purge_pid(&mut self, pid: i64) -> Vec<usize> {
        self.bindings.retain(|&(p, _)| p != pid);
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
