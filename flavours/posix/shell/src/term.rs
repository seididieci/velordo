use super::*;

// ── Terminale (sessione di terminale) ───────────────────────────────

// D8d: nessun fd. L'input si legge dalla **sessione** del processo
// (`civis::input`), il testo vi si scrive: la tastiera non e' piu' un file e
// non c'e' un device da aprire.

/// Budget del **mirror seriale** per comando (T4/bulk): un output enorme
/// (es. `hexdump` di un file da MB) non deve passare tutto sul filo seriale
/// byte-per-byte (la UART costa ~µs/byte: minuti). Oltre il budget il mirror
/// si ferma e lo si segnala una volta; il terminale riceve comunque tutto.
/// I byte li mirrora il server (D8c2): qui resta la **politica**, che e' per
/// comando — l'unico che sa cosa e' un comando e' la shell.
const MIRROR_BUDGET: usize = 256 * 1024; // 256 KiB
static mut MIRROR_LEFT: usize = MIRROR_BUDGET;
static mut MIRROR_TRUNCATED: bool = false;

/// Azzera il budget del mirror: chiamata dal REPL prima di ogni comando.
pub(crate) fn reset_mirror_budget() {
    unsafe {
        MIRROR_LEFT = MIRROR_BUDGET;
        MIRROR_TRUNCATED = false;
    }
}

/// Attese bounded in **solo spin** (nessuna syscall): servono alle
/// continuazioni ESC, dove i byte sono gia' in coda e bastano microsecondi.
/// Mai per l'attesa dell'utente (quella dorme su `civis::input::wait`: uno
/// spin illimitato tiene la CPU al 100%, ADR-0067).

/// Attesa brevissima per le continuazioni ESC (Fase 43b-fix): i byte della
/// sequenza sono gia' in coda (il tty li spinge insieme per scancode), quindi
/// bastano microsecondi. `spin_brief` (1M spin × 50 iterazioni ≈ decimi di
/// secondo) allargava la finestra fino al tasto successivo — un ESC solitario
/// mangiava il primo carattere digitato dopo (flaky su host veloci, dove la
/// digitazione arriva entro la finestra). Bound totale ~pochi ms: >> gap del
/// burst (µs), << digitazione umana/test (≥ 60 ms).
fn spin_esc() {
    for _ in 0..1_000 {
        core::hint::spin_loop();
    }
}

pub(crate) fn term_init() -> bool {
    // D8d: mi dichiaro **lettore** della mia sessione (`civis::input::attach`
    // fa il retry bounded di boot e spegne l'eco del modello: l'editor di
    // linea e' nostro).
    civis::input::attach().is_ok()
}
/// Scrive byte sul terminale: la **sessione** del processo (`civis::sess`,
/// D8c2). Il mirror seriale lo fa il server: qui resta solo la politica del
/// budget per comando (sopra) e il redirect.
/// Hook B1 (Fase 40.4b): con stdout redirectato (`set_stdio`) i builtin
/// scrivono sul file invece che sul terminale — e' l'unico punto d'aggancio
/// (i builtin usano tutti `term_print`, non `println!`).
pub(crate) fn term_write_bytes(data: &[u8]) {
    let out = libr::stdout_fd();
    if out >= 0 {
        let _ = civis::write_fs(out, data, data.len());
        return;
    }
    // Budget del mirror per comando: la testa entra nel mirror (la scrive il
    // server), la coda e' **solo schermo**. Il budget e' della shell perche'
    // il confine e' il comando, non la sessione.
    unsafe {
        if MIRROR_LEFT > 0 {
            let n = data.len().min(MIRROR_LEFT);
            let _ = civis::sess::out(&data[..n]);
            MIRROR_LEFT -= n;
            if n == data.len() {
                return;
            }
            if !MIRROR_TRUNCATED {
                MIRROR_TRUNCATED = true;
                let _ = civis::print_string(
                    b"\n[mirror seriale troncato: output troppo grande]\n",
                );
            }
            let _ = civis::sess::out_screen(&data[n..]);
            return;
        }
        if !MIRROR_TRUNCATED {
            MIRROR_TRUNCATED = true;
            let _ = civis::print_string(b"\n[mirror seriale troncato: output troppo grande]\n");
        }
    }
    let _ = civis::sess::out_screen(data);
}

pub(crate) fn term_print(s: &str) {
    term_write_bytes(s.as_bytes());
}

/// Scrive un messaggio d'errore: stderr redirectato (`2>`, Fase 40.4d) o
/// terminale — MAI lo stdout redirectato (gli errori non devono inquinare il
/// file di `>`). Va sulla sessione (quindi anche nel mirror del server).
pub(crate) fn term_err_bytes(data: &[u8]) {
    let err = libr::stderr_fd();
    if err >= 0 {
        let _ = civis::write_fs(err, data, data.len());
    } else {
        let _ = civis::sess::out(data);
    }
}

pub(crate) fn term_err(s: &str) {
    term_err_bytes(s.as_bytes());
}

/// Legge TUTTO lo stdin redirectato (`<`, Fase 40.4d) fino a EOF. Vuoto =
/// file vuoto; `None` interno (errore/byte mancante) chiude comunque come EOF
/// (file, mai device-a-caratteri qui: `<` su device puo' troncare — limite
/// documentato, come `cat /dev/zero` da file che non termina mai).
/// Chiamare solo con stdin redirectato (`stdin_fd() >= 0`).
pub(crate) fn term_read_stdin() -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        match libr::stdin_byte() {
            Some(b) => out.push(b),
            None => break,
        }
    }
    out
}

/// Legge un byte di input dalla sessione. Quando la coda e' vuota dorme
/// sull'evento (`civis::input::wait`, recv): `None` = sorgente persa (la
/// console e' morta e non si e' riattaccata) — il chiamante esce loud.
fn kbd_read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    loop {
        if civis::input::read(&mut buf) > 0 {
            return Some(buf[0]);
        }
        if !civis::input::wait() {
            return None;
        }
    }
}

/// Legge un byte SENZA attesa (Fase 44a, fg wait): `Some(b)` se presente,
/// `None` se il buffer e' vuoto. Mai spin qui: lo schedule il chiamante
/// (`wait_fg` alterna poll e budget di spin puri).
pub(crate) fn kbd_try_read() -> Option<u8> {
    let mut buf = [0u8; 1];
    if civis::input::read(&mut buf) > 0 {
        Some(buf[0])
    } else {
        None
    }
}
// ── Line editing (Fase 43b, readline nella shell) ────────────────────
// L'input arriva gia' decodificato dalla sessione (frecce→ESC[D/C/A/B,
// Home/End→ESC[H/F, Delete→ESC[3~, Esc→ESC) e NON e' ecoato dal modello:
// l'editor qui possiede buffer, cursore ed echo (console-only, mai seriale:
// il log seriale dei test non deve vedere i digitati). History a livello
// shell (comandi, non righe terminale). Solo ASCII (0x20..=0x7e) come prima;
// righe oltre 80 colonne (wrap VGA) non editabili: documentato in
// 12-utilities.

/// History comandi (persistente per sessione shell, mai su disco).
static mut HISTORY: Vec<String> = Vec::new();

fn history() -> &'static mut Vec<String> {
    // Come cwd.rs/VARS: raw pointer, single-threaded.
    unsafe { &mut *core::ptr::addr_of_mut!(HISTORY) }
}

/// Scrive byte sul terminale SENZA mirror seriale (eco dei digitati): vanno
/// solo sulla sessione, che il server non mirrora (`write_screen`, D8c2).
/// Separato da `term_write_bytes` (che entra nel mirror) apposta.
fn term_echo(data: &[u8]) {
    let _ = civis::sess::out_screen(data);
}

/// Legge un byte con attesa bounded (continuazioni ESC): i byte della
/// sequenza sono gia' in coda (il tty li spinge insieme), quindi basta un
/// bound corto in wall-time (`spin_esc`, mai `spin_brief`: vedi sopra).
/// Spin puri IF=1 (mai `get_ticks` in loop). `None` = Esc solitario
/// (ignorato, mai hang, mai mangiato il tasto successivo).
fn read_byte_bounded() -> Option<u8> {
    let mut buf = [0u8; 1];
    for _ in 0..50 {
        if civis::input::read(&mut buf) > 0 {
            return Some(buf[0]);
        }
        spin_esc();
    }
    None
}

/// Stato editor di una riga: `screen` = offset fisico del cursore dopo il
/// prompt (dove la VGA lo mostra davvero).
struct Editor {
    buf: Vec<u8>,
    cur: usize,
    screen: usize,
    hist_pos: usize,
    stash: Option<Vec<u8>>,
    record: bool,
}

impl Editor {
    /// Ridisegna la riga senza conoscere la lunghezza del prompt: torna a
    /// inizio riga (dopo il prompt) di `screen` celle, cancella fino a EOL,
    /// riscrive il buffer, riposiziona a `cur`.
    fn redraw(&mut self) {
        let mut out = Vec::new();
        for _ in 0..self.screen {
            out.extend_from_slice(b"\x1b[D");
        }
        out.extend_from_slice(b"\x1b[K");
        out.extend_from_slice(&self.buf);
        for _ in 0..self.buf.len() - self.cur {
            out.extend_from_slice(b"\x1b[D");
        }
        term_echo(&out);
        self.screen = self.cur;
    }

    /// Registra la riga in history (Enter): non vuota, no duplicato
    /// consecutivo. Solo se `record` (gli heredoc non sporcano la history).
    fn commit(&mut self) {
        if !self.record || self.buf.is_empty() {
            return;
        }
        let h = history();
        let line = String::from_utf8(self.buf.clone()).unwrap_or_default();
        if h.last().map(|s| s.as_str()) != Some(line.as_str()) {
            h.push(line);
        }
    }

    /// Tasto normale: `Some(riga)` = Enter (finito), `None` = continua.
    fn key(&mut self, b: u8) -> Option<String> {
        match b {
            b'\n' | b'\r' => {
                term_echo(b"\n");
                self.commit();
                return Some(String::from_utf8(core::mem::take(&mut self.buf)).unwrap_or_default());
            }
            0x08 => {
                if self.cur > 0 {
                    self.buf.remove(self.cur - 1);
                    self.cur -= 1;
                    self.redraw();
                }
            }
            0x1b => self.esc_seq(),
            0x20..=0x7e => {
                self.buf.insert(self.cur, b);
                self.cur += 1;
                self.redraw();
            }
            _ => {}
        }
        None
    }

    /// Sequenza ESC (il primo 0x1b e' gia' consumato): frecce, Home/End,
    /// Delete (`ESC[3~`). Esc solitario o sequenza ignota = ignorati.
    fn esc_seq(&mut self) {
        if read_byte_bounded() != Some(b'[') {
            return;
        }
        match read_byte_bounded() {
            Some(b'A') => self.hist_prev(),
            Some(b'B') => self.hist_next(),
            Some(b'C') => {
                if self.cur < self.buf.len() {
                    self.cur += 1;
                    self.redraw();
                }
            }
            Some(b'D') => {
                if self.cur > 0 {
                    self.cur -= 1;
                    self.redraw();
                }
            }
            Some(b'H') => {
                self.cur = 0;
                self.redraw();
            }
            Some(b'F') => {
                self.cur = self.buf.len();
                self.redraw();
            }
            Some(b'3') => {
                if read_byte_bounded() == Some(b'~') && self.cur < self.buf.len() {
                    self.buf.remove(self.cur);
                    self.redraw();
                }
            }
            _ => {}
        }
    }

    /// History Up: salva la riga in corso allo `stash` alla prima salita,
    /// poi richiama a ritroso (cursore a fine).
    fn hist_prev(&mut self) {
        let h = history();
        if h.is_empty() || self.hist_pos == 0 {
            return;
        }
        if self.hist_pos == h.len() {
            self.stash = Some(self.buf.clone());
        }
        self.hist_pos -= 1;
        self.buf = h[self.hist_pos].clone().into_bytes();
        self.cur = self.buf.len();
        self.redraw();
    }

    /// History Down: avanza fino a oltre la piu' recente (ripristina `stash`).
    fn hist_next(&mut self) {
        let h = history();
        if self.hist_pos >= h.len() {
            return;
        }
        self.hist_pos += 1;
        self.buf = if self.hist_pos == h.len() {
            self.stash.take().unwrap_or_default()
        } else {
            h[self.hist_pos].clone().into_bytes()
        };
        self.cur = self.buf.len();
        self.redraw();
    }
}

/// Legge una riga con editing (prompt via `term_print`: resta su seriale per
/// `wait_prompt`; l'eco dei digitati e' console-only). Registra in history.
pub(crate) fn read_line(prompt: &str) -> String {
    read_line_rec(prompt, true)
}

/// Come `read_line` ma con registrazione history opzionale (gli heredoc,
/// prompt `> `, non sporcano la history dei comandi).
pub(crate) fn read_line_rec(prompt: &str, record: bool) -> String {
    term_print(prompt);
    let mut ed = Editor {
        buf: Vec::new(),
        cur: 0,
        screen: 0,
        hist_pos: history().len(),
        stash: None,
        record,
    };
    // L'eco del PRIMO carattere digitato parte da riga vuota: niente redraw
    // preventivo (il prompt e' gia' a video, il cursore dopo di lui).
    loop {
        let Some(b) = kbd_read_byte() else { continue; };
        if let Some(line) = ed.key(b) {
            return line;
        }
    }
}
