use super::*;

// ── Terminale (device console) ──────────────────────────────────────

const KEYBOARD_PATH: &str = "/dev/input/keyboard";

static mut TERM_FD: i64 = -1;

/// Budget del **mirror seriale** per comando (T4/bulk): un output enorme
/// (es. `hexdump` di un file da MB) non deve passare tutto sul filo seriale
/// byte-per-byte (la UART costa ~µs/byte: minuti). Oltre il budget il mirror
/// si ferma e lo si segnala una volta; il terminale riceve comunque tutto.
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

/// Breve attesa in SOLO spin (nessuna syscall): serve ai bound di boot (retry
/// dell'`open`) e non all'attesa dell'utente. Per il prompt si usa
/// `wait_input_tick` (futex): uno spin illimitato tiene la CPU al 100%.
fn spin_brief() {
    for _ in 0..1_000_000 {
        core::hint::spin_loop();
    }
}

/// Word futex dell'attesa input: mai scritta (resta 0), l'indirizzo e' solo la
/// chiave del waiter per `(mm, addr)`. Unica per processo (la shell e'
/// single-threaded, i due campi non si attendono mai insieme).
static INPUT_FUTEX: u32 = 0;

/// Dorme fino al prossimo tick quando la tastiera e' vuota. Il processo va in
/// `Blocked`: il PIT continua a girare e il futex lo risveglia alla deadline
/// (stesso meccanismo di `useruptime`/login/native shell). 1 tick = latenza
/// massima 10 ms; nessuna diluizione per gli altri processi, niente CPU
/// bruciata al prompt (lo spin qui la teneva al 100%).
fn wait_input_tick() {
    let deadline = civis::get_ticks() + 1;
    let _ = civis::futex_wait(core::ptr::addr_of!(INPUT_FUTEX) as usize, 0, deadline as u64);
}

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
    // Il mount /dev/input viene registrato da porta al suo avvio:
    // ritenta se l'open iniziale fallisce (race di boot).
    for _ in 0..100 {
        if let Ok(fd) = civis::open(KEYBOARD_PATH, 0) {
            unsafe {
                TERM_FD = fd;
            }
            return true;
        }
        spin_brief();
    }
    false
}
/// Scrive byte sul terminale: cardo li inoltra al console server che li
/// disegna sulla VGA (DEV_WRITE). Echo dei tasti gestito dal console.
/// Mirror su seriale per debugging e per test automatici.
/// Hook B1 (Fase 40.4b): con stdout redirectato (`set_stdio`) i builtin
/// scrivono sul file invece che sul terminale — e' l'unico punto d'aggancio
/// (i builtin usano tutti `term_print`, non `println!`). Il mirror seriale
/// resta sempre (l'output debug non si perde mai).
pub(crate) fn term_write_bytes(data: &[u8]) {
    // Mirror seriale con budget (T4/bulk): oltre 1 MiB per comando il
    // seriale non e' piu' un mirror utile (e costa minuti sulla UART).
    unsafe {
        if MIRROR_LEFT > 0 {
            let n = data.len().min(MIRROR_LEFT);
            let _ = civis::print_string(&data[..n]);
            MIRROR_LEFT -= n;
            if n < data.len() && !MIRROR_TRUNCATED {
                MIRROR_TRUNCATED = true;
                let _ = civis::print_string(
                    b"\n[mirror seriale troncato: output troppo grande]\n",
                );
            }
        } else if !MIRROR_TRUNCATED {
            MIRROR_TRUNCATED = true;
            let _ = civis::print_string(
                b"\n[mirror seriale troncato: output troppo grande]\n",
            );
        }
    }
    let out = libr::stdout_fd();
    if out >= 0 {
        let _ = civis::write_fs(out, data, data.len());
    } else {
        unsafe {
            let _ = civis::write_fs(TERM_FD, data, data.len());
        }
    }
}

pub(crate) fn term_print(s: &str) {
    term_write_bytes(s.as_bytes());
}

/// Scrive un messaggio d'errore: stderr redirectato (`2>`, Fase 40.4d) o
/// terminale — MAI lo stdout redirectato (gli errori non devono inquinare il
/// file di `>`). Mirror su seriale come l'output normale.
pub(crate) fn term_err_bytes(data: &[u8]) {
    let _ = civis::print_string(data);
    let err = libr::stderr_fd();
    if err >= 0 {
        let _ = civis::write_fs(err, data, data.len());
    } else {
        unsafe {
            let _ = civis::write_fs(TERM_FD, data, data.len());
        }
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

/// Legge un byte di input dal terminale. Quando il buffer e' vuoto dorme un
/// tick (`wait_input_tick`): il futex blocca il processo, non la CPU.
fn kbd_read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    loop {
        let n = unsafe { civis::read_fs(TERM_FD, &mut buf, 1).unwrap_or(0) };
        if n > 0 {
            return Some(buf[0]);
        }
        wait_input_tick();
    }
}

/// Legge un byte SENZA attesa (Fase 44a, fg wait): `Some(b)` se presente,
/// `None` se il buffer e' vuoto. Mai spin qui: lo schedule il chiamante
/// (`wait_fg` alterna poll e budget di spin puri).
pub(crate) fn kbd_try_read() -> Option<u8> {
    let mut buf = [0u8; 1];
    let n = unsafe { civis::read_fs(TERM_FD, &mut buf, 1).unwrap_or(0) };
    if n > 0 {
        Some(buf[0])
    } else {
        None
    }
}
// ── Line editing (Fase 43b, readline nella shell) ────────────────────
// Il tty e' raw: decodifica i tasti (frecce→ESC[D/C/A/B, Home/End→ESC[H/F,
// Delete→ESC[3~, Esc→ESC) e NON fa echo. L'editor qui possiede buffer,
// cursore ed echo (console-only, mai seriale: il log seriale dei test non
// deve vedere i digitati). History a livello shell (comandi, non righe
// terminale). Solo ASCII (0x20..=0x7e) come prima; righe oltre 80 colonne
// (wrap VGA) non editabili: documentato in 12-utilities.

/// History comandi (persistente per sessione shell, mai su disco).
static mut HISTORY: Vec<String> = Vec::new();

fn history() -> &'static mut Vec<String> {
    // Come cwd.rs/VARS: raw pointer, single-threaded.
    unsafe { &mut *core::ptr::addr_of_mut!(HISTORY) }
}

/// Scrive byte sul terminale SENZA mirror seriale (eco dei digitati):
/// va solo sulla VGA via tty→console. Separato da `term_write_bytes`
/// (che specchia su seriale per debug/test) apposta.
fn term_echo(data: &[u8]) {
    unsafe {
        let _ = civis::write_fs(TERM_FD, data, data.len());
    }
}

/// Legge un byte con attesa bounded (continuazioni ESC): i byte della
/// sequenza sono gia' in coda (il tty li spinge insieme), quindi basta un
/// bound corto in wall-time (`spin_esc`, mai `spin_brief`: vedi sopra).
/// Spin puri IF=1 (mai `get_ticks` in loop). `None` = Esc solitario
/// (ignorato, mai hang, mai mangiato il tasto successivo).
fn read_byte_bounded() -> Option<u8> {
    let mut buf = [0u8; 1];
    for _ in 0..50 {
        let n = unsafe { civis::read_fs(TERM_FD, &mut buf, 1).unwrap_or(0) };
        if n > 0 {
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
