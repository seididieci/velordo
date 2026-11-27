//! monitor — system monitor (btop-like TUI, ADR-0097/0099).
//!
//! Prima app grafica che reagisce al **tempo** (1 Hz) e non solo ai tasti.
//! Mostra valori veri senza toccare il kernel:
//!   - CPU: delta `ticks_used` / delta parete (un core; idle escluso)
//!   - RAM: `meminfo()` (frame da 4 KiB)
//!   - Processi: `ps_info(0..PS_SCAN_MAX)` con sort e selezione
//!   - Uptime: `get_ticks()`
//!
//! Costruito sopra `formae` (componenti condivisi con la futura GUI) +
//! `tabula` (celle) + `pictura` (pixel nelle toppe). Zero `civis` dentro
//! `formae`; il monitor e' l'unico consumatore.
//!
//! Loop: un solo `input::wait_until(deadline)` per tasto-o-tick (E0/E1),
//! niente spin, niente polling.
//!
//! Tasti: `q` esci, `p` pausa, `j/k` selezione, `s` sort (cpu/time/pid/name),
//! `r` hook repaint (incrementale == repaint).
//!
//! Marker seriale: `start`, `tick=… cpu=… mem=… procs=… sel=… sort=… patches=… bytes=…`.
//! Seedato in `usr/bin/monitor.bin` (uso manuale dalla shell nativa).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use civis;
use civis::input::{self, Wait};
use civis::sess::Screen;
use civis::sys;

use formae::prelude::*;

civis::entry!(real_main);

const TICKS_HZ: u64 = 100;
const SAMPLE_TICKS: u64 = 100; // 1 Hz
const HISTORY: usize = 60; // 60 secondi di storia

const PS_MAX: u32 = civis::syscall_numbers::PS_SCAN_MAX;
/// Slot della tabella `ps` (indice = pid).
const PS_SLOTS: usize = PS_MAX as usize;

/// Il tema della dashboard (una decisione sola).
const TH: Theme = Theme::NIGHT;

/// `12%` da permille (0 decimale, troncato).
fn pct(permille: u32) -> String {
    let mut b = [0u8; 8];
    let n = fmt::percent(permille, 0, &mut b);
    core::str::from_utf8(&b[..n]).unwrap_or("0%").to_string()
}

/// `12.3%` da permille (1 decimale): la CPU per processo ha bisogno del
/// decimale, o i valori sotto il 1% spariscono e la somma della lista non
/// torna col totale.
fn pct1(permille: u32) -> String {
    let mut b = [0u8; 8];
    let n = fmt::percent(permille, 1, &mut b);
    core::str::from_utf8(&b[..n]).unwrap_or("0.0%").to_string()
}

/// `800 MiB` / `1.4 GiB` da frame di 4 KiB.
fn mem_str(frames: u64) -> String {
    let (whole, tenths, unit) = fmt::mem(frames);
    if whole >= 1024 {
        format!("{}.{} {}", whole / 1024, tenths, unit)
    } else {
        format!("{} {}", whole, unit)
    }
}

/// Ordinamento della tabella processi.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Cpu,
    Time,
    Pid,
    Name,
}

impl Sort {
    fn next(self) -> Sort {
        match self {
            Sort::Cpu => Sort::Time,
            Sort::Time => Sort::Pid,
            Sort::Pid => Sort::Name,
            Sort::Name => Sort::Cpu,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Time => "time",
            Sort::Pid => "pid",
            Sort::Name => "name",
        }
    }
}

/// Una riga della tabella processi.
struct Proc {
    pid: u32,
    name: String,
    cpu_permille: u32,
    ticks: u64,
    state: char,
}

/// Il modello del monitor: tutto lo stato che i widget disegnano.
struct Model {
    cpu_hist: History<HISTORY>,
    mem_hist: History<HISTORY>,
    procs: Vec<Proc>,
    sort: Sort,
    sel: usize,
    /// Prima riga visibile della tabella (finestra scorrevole): `j`/`k` muovono
    /// `sel` e `draw` aggiusta `offset` per tenerla visibile.
    offset: usize,
    paused: bool,
    /// Tick precedenti **per pid**: il delta di CPU di un processo è
    /// `e.ticks - prev_ticks[pid]`. (Prima si usava il totale globale, quindi
    /// ogni processo risultava sempre 0%.)
    prev_ticks: [u64; PS_SLOTS],
    /// Il pid era presente al campione precedente: un pid riusato non eredita
    /// i tick del processo morto.
    seen: [bool; PS_SLOTS],
    /// Almeno un campione fatto (il primo è la baseline: niente delta).
    primed: bool,
    last_wall: u64,
}

impl Model {
    fn new() -> Model {
        let now = sys::get_ticks() as u64;
        let mut cpu_hist = History::<HISTORY>::new();
        let mut mem_hist = History::<HISTORY>::new();
        cpu_hist.fill(0);
        mem_hist.fill(0);
        Model {
            cpu_hist,
            mem_hist,
            procs: Vec::new(),
            sort: Sort::Cpu,
            sel: 0,
            offset: 0,
            paused: false,
            prev_ticks: [0; PS_SLOTS],
            seen: [false; PS_SLOTS],
            primed: false,
            last_wall: now,
        }
    }

    /// Campiona CPU/RAM/processi.
    fn sample(&mut self) -> (u32, u32, usize) {
        let wall = sys::get_ticks() as u64;
        let dt = wall.saturating_sub(self.last_wall).max(1);
        let mut nprocs: usize = 0;
        // Somma dei permille **per processo**: la CPU globale e' esattamente la
        // somma di cio' che la lista mostra (un fit per costruzione, invece di
        // `delta_totali * 1000 / dt` che troncava il decimale di ognuno).
        let mut sum_permille: u32 = 0;
        let mut present = [false; PS_SLOTS];
        let mut rows: Vec<Proc> = Vec::new();

        for pid in 1..PS_MAX {
            // pid 0 = idle: escluso (la CPU dell'idle non conta).
            if let Some(e) = sys::ps_info(pid) {
                let idx = pid as usize;
                present[idx] = true;
                let delta = if self.primed && self.seen[idx] {
                    e.ticks.saturating_sub(self.prev_ticks[idx])
                } else {
                    0 // baseline o pid nuovo: nessun delta
                };
                self.prev_ticks[idx] = e.ticks;
                let cpu_permille = ((delta.saturating_mul(1000)) / dt).min(1000) as u32;
                sum_permille = sum_permille.saturating_add(cpu_permille);
                let state = match e.state {
                    0 => 'R',
                    1 => 'S',
                    2 => 'T',
                    3 => 'Z',
                    _ => '?',
                };
                rows.push(Proc {
                    pid: e.pid,
                    name: e.name_str().to_string(),
                    cpu_permille,
                    ticks: e.ticks,
                    state,
                });
                nprocs += 1;
            }
        }
        // I pid spariti non devono lasciare `seen` acceso: se il pid viene
        // riusato da un processo nuovo, riparte da zero.
        self.seen[1..].copy_from_slice(&present[1..]);
        self.primed = true;

        // CPU% globale = **somma dei permille per-processo** (1 core): il
        // totale e la somma della lista coincidono per costruzione.
        let cpu_permille = sum_permille.min(1000);

        // RAM: used / total.
        let (_free, total, used) = sys::meminfo();
        let mem_permille = if total > 0 {
            ((used.saturating_mul(1000) / total).min(1000)) as u32
        } else {
            0
        };

        self.cpu_hist.push(cpu_permille as u64);
        self.mem_hist.push(mem_permille as u64);

        // Ordina.
        match self.sort {
            Sort::Cpu => rows.sort_by(|a, b| b.cpu_permille.cmp(&a.cpu_permille)),
            Sort::Time => rows.sort_by(|a, b| b.ticks.cmp(&a.ticks)),
            Sort::Pid => rows.sort_by(|a, b| a.pid.cmp(&b.pid)),
            Sort::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        }
        // Tieni la selezione coerente (clamp).
        if self.sel >= rows.len() {
            self.sel = rows.len().saturating_sub(1);
        }
        self.procs = rows;
        self.last_wall = wall;
        (cpu_permille, mem_permille, nprocs)
    }

    fn uptime_str(&self) -> String {
        // Uptime di **boot** (get_ticks conta dal PIT): non il tempo dell'app.
        let secs = (sys::get_ticks().max(0) as u64) / TICKS_HZ;
        let mut b = [0u8; 16];
        let n = fmt::clock(secs, 1, &mut b);
        core::str::from_utf8(&b[..n]).unwrap_or("0:00:00").to_string()
    }
}

/// Disegna il frame nel `formae::Frame`.
fn draw(model: &mut Model, frame: &mut Frame) {
    let m = frame.metrics();
    let (cols, rows) = (m.cols, m.rows);
    if cols < 20 || rows < 10 {
        return; // troppo piccolo: niente
    }

    frame.begin();

    // Sfondo **unico traslucido** di tutto il monitor: ogni cella ha il bg
    // della card (blu scuro, ~84% di opacita'). Il compositor lo fonde sul
    // wallpaper (la banda del fondo e' gia' li'), quindi il monitor e' un
    // overlay: il wallpaper traspare attenuato, non un pannello opaco.
    // Le card sono toppe **trasparenti** con lo stesso colore; le loro celle
    // vuote vengono saltate (`blank_over_patch`) e mostrano la toppa.
    frame.cells().fill_styled(
        0,
        0,
        cols as usize,
        rows as usize,
        ' ',
        TH.pen_on(TH.text, TH.card),
    );

    // ── Titolo ──
    let title = format!(
        " monitor  uptime {}  procs {}  sort:{}",
        model.uptime_str(),
        model.procs.len(),
        model.sort.label()
    );
    // Scritto **sopra** il fondo traslucido (bg della card, non di pagina):
    // cosi' anche il titolo e' sopra il pannello, non sul wallpaper pieno.
    frame.cells().text_styled(0, 0, &title, TH.pen_on(TH.text, TH.card));

    // ── Layout delle card ──
    // Due colonne a >= 100 colonne (CPU a sinistra, MEM a destra), una
    // colonna altrimenti (CPU sopra, MEM sotto).
    let card_h: u16 = if rows >= 28 { 8 } else { 6 };
    let two_col = cols >= 100;
    let y0: u16 = 2;

    if two_col {
        let w = cols / 2;
        let cpu_rect = CellRect::new(0, y0, w, card_h);
        let mem_rect = CellRect::new(w, y0, cols - w, card_h);
        draw_cpu_card(model, frame, cpu_rect);
        draw_mem_card(frame, mem_rect);
        let py = y0 + card_h + 1;
        if rows > py + 2 {
            draw_proc_card(model, frame, CellRect::new(0, py, cols, rows - py - 1));
        }
    } else {
        let cpu_rect = CellRect::new(0, y0, cols, card_h);
        draw_cpu_card(model, frame, cpu_rect);
        let my = y0 + card_h + 1;
        draw_mem_card(frame, CellRect::new(0, my, cols, card_h));
        let py = my + card_h + 1;
        if rows > py + 2 {
            draw_proc_card(model, frame, CellRect::new(0, py, cols, rows - py - 1));
        }
    }
}

fn draw_cpu_card(model: &Model, frame: &mut Frame, rect: CellRect) {
    let m = frame.metrics();
    let px = rect.px(&m);
    let last = model.cpu_hist.last().unwrap_or(0);
    let mut samples = [0u64; HISTORY];
    let nn = model.cpu_hist.read(&mut samples);

    frame.patch(rect, |t| {
        t.round(px, m.cell_h / 4, TH.card);
        t.border(px, 1, TH.accent);
        t.text(px.x + 4, px.y + 2, " CPU", TH.pen(TH.text));
        let val = pct1(last as u32);
        let vx = px.right() - 6 - (val.len() as u32) * m.cell_w;
        t.text(vx.max(px.x + 4), px.y + 2, &val, TH.pen(TH.dim));
        // Grafico: dalla seconda riga alla penultima.
        let top = px.y + m.cell_h;
        let bottom = px.bottom() - m.cell_h;
        let chart_r = PxRect::new(px.x + 2, top, px.w.saturating_sub(4), bottom.saturating_sub(top));
        // Auto-scala alla finestra: con la storia **piena** (campionamento a
        // 1 Hz regolare) la linea e l'area riempiono il grafico anche a CPU
        // bassa; una scala fissa 0..100 schiaccerebbe tutto sul fondo.
        Chart::new(&samples[..nn.min(samples.len())], TH.accent).draw(t, chart_r);
        let bar_r = PxRect::new(px.x + 2, px.bottom() - m.cell_h + 4, px.w.saturating_sub(4), 2);
        let mut bar = Bar::new(TH.track, TH.level(last as u32));
        bar.set(last, 1000);
        bar.draw(t, bar_r);
    });
}

fn draw_mem_card(frame: &mut Frame, rect: CellRect) {
    let m = frame.metrics();
    let px = rect.px(&m);
    let (_free, total, used) = sys::meminfo();
    let permille = if total > 0 {
        ((used * 1000 / total).min(1000)) as u32
    } else {
        0
    };
    let val = format!("{} / {}", mem_str(used), mem_str(total));

    frame.patch(rect, |t| {
        t.round(px, m.cell_h / 4, TH.card);
        t.border(px, 1, TH.accent);
        t.text(px.x + 4, px.y + 2, " MEM", TH.pen(TH.text));
        let pv = pct(permille);
        let vx = px.right() - 6 - (pv.len() as u32) * m.cell_w;
        t.text(vx.max(px.x + 4), px.y + 2, &pv, TH.pen(TH.dim));
        // Barra: seconda riga. Valore: terza riga.
        let bar_r = PxRect::new(px.x + 2, px.y + m.cell_h + 6, px.w.saturating_sub(4), 6);
        let mut bar = Bar::new(TH.track, TH.level(permille));
        bar.set(permille as u64, 1000);
        bar.draw(t, bar_r);
        t.text(px.x + 4, px.y + 2 * m.cell_h, &val, TH.pen(TH.dim));
    });
}

fn draw_proc_card(model: &mut Model, frame: &mut Frame, rect: CellRect) {
    // Tabella **stile btop**: bordo box-drawing, selezione a **tutta riga**,
    // scrollbar a celle sul bordo destro, scroll con `j`/`k`. Tutto testo
    // sopra il pannello traslucido unico (gia' riempito da `draw`).
    if rect.cols < 10 || rect.rows < 4 {
        return;
    }
    let col0 = rect.col as usize;
    let row0 = rect.row as usize;
    let w = rect.cols as usize;
    let h = rect.rows as usize;
    let card = TH.pen_on(TH.text, TH.card);
    let dim = TH.pen_on(TH.dim, TH.card);

    // ── Bordo (box-drawing) ──
    let right = col0 + w - 1;
    let bottom = row0 + h - 1;
    frame.cells().set_styled(col0, row0, '┌', dim);
    frame.cells().set_styled(right, row0, '┐', dim);
    frame.cells().set_styled(col0, bottom, '└', dim);
    frame.cells().set_styled(right, bottom, '┘', dim);
    let mut c = col0 + 1;
    while c < right {
        frame.cells().set_styled(c, row0, '─', dim);
        frame.cells().set_styled(c, bottom, '─', dim);
        c += 1;
    }
    let mut r = row0 + 1;
    while r < bottom {
        frame.cells().set_styled(col0, r, '│', dim);
        frame.cells().set_styled(right, r, '│', dim);
        r += 1;
    }

    // ── Area contenuto (dentro il bordo) ──
    let cc0 = col0 + 1;
    let cr0 = row0 + 1;
    let cw = w - 2;
    let chh = h - 2;
    if cw < 8 || chh < 2 {
        return;
    }
    let sb_col = cc0 + cw - 1; // scrollbar: ultima colonna del contenuto
    let data_w = cw - 1; // larghezza del testo (la scrollbar ha la sua)
    let visible = chh - 1; // righe dati (l'header ne occupa una)

    // Finestra scorrevole: `sel` sempre visibile, `offset` entro i limiti.
    let n = model.procs.len();
    if n == 0 {
        model.sel = 0;
        model.offset = 0;
    } else {
        model.sel = model.sel.min(n - 1);
        if model.sel < model.offset {
            model.offset = model.sel;
        }
        if model.sel >= model.offset + visible {
            model.offset = model.sel + 1 - visible;
        }
        model.offset = model.offset.min(n.saturating_sub(visible));
    }

    // Colonne: PID fisso, NAME **elastica**, poi CPU%/TIME/STATE.
    let name_w = data_w.saturating_sub(29).max(4);
    let hdr: [(&str, usize); 5] = [
        ("PID", 6),
        ("NAME", name_w),
        ("CPU%", 6),
        ("TIME", 7),
        ("STATE", 6),
    ];
    let mut c = cc0;
    for (name, wd) in hdr {
        put_field(frame, c, cr0, name, wd, dim);
        c += wd + 1;
    }

    // ── Righe dati ──
    for i in 0..visible {
        let idx = model.offset + i;
        if idx >= n {
            break;
        }
        let row = cr0 + 1 + i;
        let p = &model.procs[idx];
        let st = if idx == model.sel {
            TH.pen_on(TH.text, TH.accent)
        } else {
            card
        };
        // Fascia piena su tutta la riga: la selezione evidenzia **tutta** la
        // riga, non solo le celle con testo.
        frame.cells().fill_styled(cc0, row, data_w, 1, ' ', st);
        let mut c = cc0;
        put_field(frame, c, row, &format!("{}", p.pid), 6, st);
        c += 7;
        put_field(frame, c, row, &p.name, name_w, st);
        c += name_w + 1;
        put_field(frame, c, row, &pct1(p.cpu_permille), 6, st);
        c += 7;
        let secs = p.ticks / TICKS_HZ;
        put_field(frame, c, row, &format!("{}:{:02}", secs / 60, secs % 60), 7, st);
        c += 8;
        put_field(frame, c, row, &p.state.to_string(), 6, st);
    }

    // ── Scrollbar (celle, 1 colonna): track + thumb proporzionale ──
    let track_top = cr0 + 1;
    let track_h = visible;
    if n > visible && track_h > 0 {
        let thumb_h = (track_h * visible / n).max(1).min(track_h);
        let max_off = n - visible;
        let thumb_top = model.offset * (track_h - thumb_h) / max_off.max(1);
        for i in 0..track_h {
            let on = i >= thumb_top && i < thumb_top + thumb_h;
            let st = if on {
                TH.pen_on(TH.accent, TH.accent)
            } else {
                TH.pen_on(TH.track, TH.track)
            };
            frame.cells().set_styled(sb_col, track_top + i, ' ', st);
        }
    }
}

/// Scrive `s` a partire da `col`, troncandolo a `width` caratteri (monospace:
/// 1 carattere = 1 cella). Non tocca cio' che sta oltre il campo.
fn put_field(frame: &mut Frame, col: usize, row: usize, s: &str, width: usize, st: Style) {
    let mut c = col;
    for ch in s.chars().take(width) {
        frame.cells().set_styled(c, row, ch, st);
        c += 1;
    }
}

fn input_ticks_now() -> u64 {
    sys::get_ticks() as u64
}

/// Gestisce un tasto; ritorna `true` se l'app deve uscire.
fn on_key(b: u8, model: &mut Model) -> bool {
    match b {
        b'q' => return true,
        b'p' => model.paused = !model.paused,
        b's' => model.sort = model.sort.next(),
        b'j' => {
            if model.sel + 1 < model.procs.len() {
                model.sel += 1;
            }
        }
        b'k' => {
            if model.sel > 0 {
                model.sel -= 1;
            }
        }
        b'r' => {} // hook repaint: il ridisegno pieno avviene gia' a ogni giro
        _ => {}
    }
    false
}

fn main_loop() -> ! {
    // Il lettore della tastiera **si dichiara**: senza, gli hint di `porta`
    // vanno al focus owner (il DE/shell) e i tasti non arrivano qui (D8d).
    if input::attach().is_err() {
        civis::exit(1);
    }
    let mut screen = match Screen::open() {
        Ok(s) => s,
        Err(_) => civis::exit(1),
    };

    // Le metriche vere della cella (dal font della console): niente 10x22
    // cablato, o il server rifiuterebbe le toppe (px_w derivato dalle metriche).
    let (cw, ch) = civis::sess::metrics().unwrap_or((10, 22));

    let mut model = Model::new();
    model.sample(); // baseline (prima riga: cpu 0)

    let (mut cols, mut rows) = screen.size();
    let mut frame = Frame::new(Metrics::new(cols, rows, cw, ch));

    let mut next_sample = input_ticks_now() + SAMPLE_TICKS;
    // Ultimo invio di ogni toppa (stesso indice del frame): una toppa identica
    // non si rispedisce (la card MEM cambia raramente, la CPU ogni tick).
    let mut sent: Vec<Vec<u8>> = Vec::new();
    let mut dirty = true; // primo frame da disegnare

    let _ = civis::print_string(format!("[monitor] start {}x{}\n", cols, rows).as_bytes());

    loop {
        // Riadattamento: il frame **riusa** la tela (differenziale), un resize
        // la scarta e si ridisegna da zero.
        if screen.poll_resize() {
            let (c, r) = screen.size();
            cols = c;
            rows = r;
            frame.resize(cols, rows);
            sent.clear();
            dirty = true;
            let _ = civis::print_string(
                format!("[monitor] resize {}x{}\n", cols, rows).as_bytes(),
            );
        }

        if dirty {
            // Repaint **pieno** ogni frame: il monitor e' a 1 Hz, il costo e'
            // trascurabile, e cosi' non dipendiamo dal differenziale (che con
            // la tabella che si riordina a ogni tick lasciava righe "fantasma").
            frame.cells().invalidate();
            draw(&mut model, &mut frame);

            // Manda prima le toppe (pixel) **solo se cambiate**, poi il
            // differenziale delle celle.
            let patches = frame.patches().len();
            if sent.len() != patches {
                sent = (0..patches).map(|_| Vec::new()).collect();
            }
            let mut sent_bytes: usize = 0;
            let sess = civis::sess::current();
            for (i, p) in frame.patches().iter().enumerate() {
                let rect = p.rect;
                let bytes = p.canvas.bytes();
                if let Some(s) = sess {
                    if sent[i].as_slice() != bytes {
                        let _ = s.patch(
                            rect.col,
                            rect.row,
                            rect.cols,
                            rect.rows,
                            bytes,
                            b"monitor",
                        );
                        sent[i].clear();
                        sent[i].extend_from_slice(bytes);
                        sent_bytes += bytes.len();
                    }
                }
            }
            let diff = frame.diff();
            if !diff.is_empty() {
                let _ = screen.write(diff);
            }
            let _ = screen.present();

            // Marker seriale per i test.
            let maxproc = model.procs.iter().map(|p| p.cpu_permille).max().unwrap_or(0);
            // Somma dei permille per-processo: deve coincidere col globale
            // (la lista e il totale sono la stessa cosa per costruzione).
            let sumproc: u64 = model.procs.iter().map(|p| p.cpu_permille as u64).sum();
            let _ = civis::print_string(
                format!(
                    "[monitor] tick cpu={} sumproc={} maxproc={} mem={} procs={} sel={} sort={} patches={} bytes={}\n",
                    model.cpu_hist.last().unwrap_or(0),
                    sumproc,
                    maxproc,
                    model.mem_hist.last().unwrap_or(0),
                    model.procs.len(),
                    model.sel,
                    model.sort.label(),
                    patches,
                    sent_bytes,
                )
                .as_bytes(),
            );
            dirty = false;
        }

        // Attesa: tasto o **prossima scadenza fissa** (0 = mai, in pausa). Non
        // si sposta la scadenza su un `Event`: un messaggio spurio non rimanda
        // il campione (prima la storia si riempiva a singhiozzo e il grafico
        // sembrava fermo).
        match input::wait_until(if model.paused { 0 } else { next_sample }) {
            Wait::Event | Wait::Timeout => {}
            Wait::Dead => break,
        }

        // **Drena** i tasti: e' `input::read` che dichiara il lettore
        // (`SESS_READ`) e consuma quelli gia' arrivati.
        let mut quit = false;
        let mut keyed = false;
        loop {
            let mut buf = [0u8; 16];
            let n = input::read(&mut buf);
            if n == 0 {
                break;
            }
            for &b in &buf[..n] {
                if on_key(b, &mut model) {
                    quit = true;
                }
                keyed = true;
            }
        }
        if quit {
            break;
        }

        // Campiona **sull'orologio**, a prescindere dall'esito dell'attesa: una
        // volta per secondo, con la scadenza riallineata a `now` (niente burst,
        // niente deriva).
        let now = input_ticks_now();
        let mut sampled = false;
        if !model.paused && now >= next_sample {
            model.sample();
            next_sample = now + SAMPLE_TICKS;
            sampled = true;
        }

        if keyed || sampled {
            dirty = true;
        }
    }

    let _ = screen.close();
    let _ = civis::print_string(b"[monitor] exit\n");
    civis::exit(0);
}

fn real_main(_sp: u64) -> ! {
    main_loop()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    civis::print_string(b"[monitor] panic\n");
    civis::exit(1)
}
