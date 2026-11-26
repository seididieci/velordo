//! gtui — la prima **Graphic TUI**: un'applicazione testuale che disegna
//! **pixel dentro le celle** (ADR-0097, ADR-0099).
//!
//! Non e' un editor e non e' una GUI: e' un programma a schermo normale che
//! scrive testo come sempre e, nelle colonne dove il testo non c'e', mette
//! **toppe** di pixel — il grafo dei rami di un log, una barra, uno
//! sparkline. Una cella vuota mostra la toppa; un glifo la copre.
//!
//! Le toppe **vivono quanto il loro testo**: scorrendo finiscono nello
//! scrollback con la riga, e quando la cronologia scarta la riga i pixel se ne
//! vanno con lei (nessuna eviction separata). Il programma lo verifica da solo
//! e lo scrive sul seriale: `script/run-tui-tests.sh` lo controlla.
//!
//! Comandi: `q` esce (l'app resta a video finche' non la si chiude).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use civis;
use pictura::{rgb, Canvas};

civis::entry!(real_main);

// Colori (BGR A impacchettato, convenzione del framebuffer).
const BG: u32 = rgb(24, 24, 26);
const FRAME: u32 = rgb(70, 74, 86);
const BLUE: u32 = rgb(91, 124, 255);
const SLATE: u32 = rgb(141, 150, 168);
const GREEN: u32 = rgb(80, 200, 140);
const YELLOW: u32 = rgb(230, 180, 60);
const PURPLE: u32 = rgb(180, 120, 220);

/// Marker sul **seriale** (il log che legge lo script).
fn mark(text: &str) {
    let line = format!("[gtui] {}\n", text);
    let _ = civis::print_string(line.as_bytes());
}

fn write(bytes: &[u8]) {
    let _ = civis::sess::out(bytes);
}

fn write_str(s: &str) {
    write(s.as_bytes());
}

/// Metriche della cella: il programma non le indovina, le chiede (ADR-0099).
fn metrics() -> (usize, usize) {
    match civis::sess::metrics() {
        Ok((w, h)) if w > 0 && h > 0 => (w as usize, h as usize),
        _ => (10, 22),
    }
}

/// Consegna una toppa di `cells × rows` celle in `(col, row)`.
fn patch(col: u16, row: u16, cells: u16, rows: u16, canvas: &Canvas, alt: &[u8]) -> bool {
    let Some(s) = civis::sess::current() else {
        return false;
    };
    s.patch(col, row, cells, rows, canvas.bytes(), alt).is_ok()
}

/// CRC della riga `row` (tag di test della console): prova che i pixel sono
/// arrivati al framebuffer, non solo nella griglia.
fn row_crc(row: u16) -> u32 {
    let Ok(console) = civis::service_lookup(civis::Service::Console) else {
        return 0;
    };
    match civis::send(
        console as u64,
        civis::syscall_numbers::CONSOLE_ROW_CRC,
        row as u64,
        0,
    ) {
        Ok(r) if r.w1 != 0 => r.w0 as u32,
        _ => 0,
    }
}

/// Il grafo dei rami di una riga di log: `lanes` corsie, il nodo del commit
/// nella sua corsia, le linee delle corsie attive. Disegnato in pixel (non con
/// i caratteri `|`/`├`): e' il punto della toppa.
fn graph_row(cw: usize, ch: usize, lanes: &[bool], node: usize, color: u32) -> Canvas {
    let mut c = Canvas::new(cw, ch, BG);
    let lane_x = |i: usize| -> usize { (cw * (2 * i + 1)) / (2 * lanes.len().max(1)) };
    let mid = ch / 2;
    // Le corsie attive: linea verticale.
    for (i, &on) in lanes.iter().enumerate() {
        if on {
            let x = lane_x(i);
            c.vline(x, 0, ch, FRAME);
        }
    }
    // Il nodo: un quadratino pieno (a 10×22 px dev'essere visibile: ~1/3
    // dell'altezza della cella).
    let nx = lane_x(node);
    let r = (ch / 3).max(3);
    c.fill(nx.saturating_sub(r / 2), mid.saturating_sub(r / 2), r.max(2), r.max(2), color);
    c
}

/// Una barra di capacita' (CPU) in una toppa alta una cella.
fn gauge_row(cw: usize, ch: usize, permille: u32, color: u32) -> Canvas {
    let mut c = Canvas::new(cw, ch, BG);
    c.rect(0, 0, cw, ch, 1, FRAME);
    c.bar(1, 1, cw.saturating_sub(2), ch.saturating_sub(2), permille, BG, color);
    c
}

/// Uno sparkline in una toppa alta una cella.
fn spark_row(cw: usize, ch: usize, values: &[u64], color: u32) -> Canvas {
    let mut c = Canvas::new(cw, ch, BG);
    c.sparkline(0, 0, cw.min(values.len()), ch, values, color);
    c
}

/// Il cursore e' a inizio riga: la riga `row` e' quella in cui scriveremo.
/// La toppa va messa **prima** del testo (sta sotto i glifi) e le celle che
/// copre restano spazi, cosi' si vede.
fn draw_log_line(cols: usize, cw: usize, ch: usize, row: u16, lanes: &[bool], node: usize, color: u32, text: &str) -> bool {
    let lanes_w = 5u16.min(cols as u16);
    let canvas = graph_row(lanes_w as usize * cw, ch, lanes, node, color);
    let ok = patch(0, row, lanes_w, 1, &canvas, b"graph");
    // Il testo comincia dopo la toppa: celle vuote (il glifo ' ' non copre).
    let mut line = Vec::with_capacity(lanes_w as usize + text.len());
    for _ in 0..lanes_w {
        line.push(b' ');
    }
    line.extend_from_slice(text.as_bytes());
    line.push(b'\n');
    write(&line);
    ok
}

/// Disegna la schermata "bella" a partire dal cursore in alto a sinistra e
/// ritorna `(righe di log disegnate, crc dell'ultima riga, byte di toppe)`.
fn draw_pretty(cw: usize, ch: usize, cols: usize, rows: usize) -> (usize, u32, usize) {
    // ── Intestazione: una toppa a tutta larghezza con la barra dentro ──
    let mut head = Canvas::new(cols.min(64) * cw, ch, BG);
    head.bar(0, 0, head.w, ch, 780, BG, BLUE);
    head.rect(0, 0, head.w, head.h, 1, FRAME);
    let head_cells = (head.w / cw) as u16;
    let _ = patch(0, 0, head_cells, 1, &head, b"gtui header");
    write_str("  GTUI - Graphic TUI: pixel dentro le celle\n");
    let mut row = 1u16;

    // ── Il log con i rami disegnati ──
    // (messaggi finti: nessun git, solo la forma)
    let commits = [
        ("a1f4c2e", "unisce il ramo della rete nella main", &[true, true, false][..], 0, GREEN),
        ("9b02d71", "net: TCP a eventi (N8)", &[true, true, false][..], 1, PURPLE),
        ("4c81e35", "dns: DoT una connessione per query", &[true, false, false][..], 0, BLUE),
        ("77aa013", "de: tile e impluvium", &[true, false, true][..], 2, YELLOW),
        ("20be9f6", "sess: SESS_PATCH, toppe di pixel", &[true, false, true][..], 0, GREEN),
        ("e5510b8", "term: shift di pixel (T4)", &[true, false, false][..], 0, SLATE),
        ("d0993ac", "fs: sys:identis riservato", &[true, false, false][..], 0, BLUE),
        ("3f2e17d", "login: capo-sessione (D8f)", &[true, false, false][..], 0, SLATE),
    ];
    let mut drawn = 0usize;
    for (sha, msg, lanes, node, color) in commits.iter() {
        let text = format!("{}  {}", sha, msg);
        if draw_log_line(cols, cw, ch, row, lanes, *node, *color, &text) {
            drawn += 1;
        }
        row += 1;
        if (row as usize) >= rows.saturating_sub(4) {
            break;
        }
    }

    // ── Barra e sparkline: due toppe, una per riga ──
    let gauge_cells = 20u16.min(cols as u16);
    let g = gauge_row(gauge_cells as usize * cw, ch, 640, BLUE);
    let _ = patch(0, row, gauge_cells, 1, &g, b"cpu 64%");
    write_str("  cpu 64%   ");
    write_str("[=========---------]\n");
    row += 1;
    let spark_cells = 40u16.min(cols as u16);
    let samples = [3u64, 7, 5, 9, 12, 8, 15, 11, 18, 14, 9, 6, 11, 16, 21, 17, 13, 19, 22, 18, 12, 8, 14, 20, 24, 19, 15, 11, 7, 13, 17, 12, 9, 5, 10, 14, 18, 13, 8, 4];
    let sp = spark_row(spark_cells as usize * cw, ch, &samples, GREEN);
    let _ = patch(0, row, spark_cells, 1, &sp, b"spark");
    write_str("  carico delle ultime 40 misure\n");
    row += 1;

    (drawn, row_crc(row.saturating_sub(1)), civis::sess::patch_bytes().unwrap_or(0))
}

fn real_main(_sp: u64) -> ! {
    if civis::input::attach().is_err() {
        mark("ERRORE: sessione non raggiungibile");
        civis::exit(1);
    }
    let (cw, ch) = metrics();
    let (cols, rows) = civis::sess::current()
        .and_then(|s| s.info().ok())
        .map(|i| (i.cols as usize, i.rows as usize))
        .unwrap_or((80, 24));
    mark(&format!("metriche cella {}x{}, schermo {}x{}", cw, ch, cols, rows));

    // ── La schermata (ridisegnata anche alla fine: i test di scroll la
    //    spazzano via e chi guarda vuole vederla). Si parte da schermo pulito:
    //    il cursore e' dove lo ha lasciato la shell.
    write(b"\x1b[2J\x1b[H");
    let (drawn, crc, p0) = draw_pretty(cw, ch, cols, rows);
    let row = 0u16;
    // ── Prova autonoma: le toppe vivono nel testo ──
    let _ = row;
    mark(&format!("log {} commit, patches={} crc={:x}", drawn, p0, crc));

    // Scroll *dentro* la cronologia (40 righe): le toppe restano.
    let mut pad = Vec::new();
    for _ in 0..40 {
        pad.extend_from_slice(b"riga di riempimento\n");
    }
    write(&pad);
    let p1 = civis::sess::patch_bytes().unwrap_or(0);
    mark(&format!("dopo 40 righe patches={}", p1));

    // Scroll *oltre* la cronologia (1100 righe su 1000): le righe con le
    // toppe escono e i pixel se ne vanno con loro.
    let mut pad = Vec::new();
    for _ in 0..1100 {
        pad.extend_from_slice(b".\n");
    }
    write(&pad);
    let p2 = civis::sess::patch_bytes().unwrap_or(0);
    mark(&format!("oltre la cronologia patches={}", p2));
    // Ridisegna la schermata: chi guarda deve vedere la GTUI, non i puntini
    // dei test di scroll.
    write(b"\x1b[2J\x1b[H");
    let _ = draw_pretty(cw, ch, cols, rows);
    write_str("\n");
    write_str("[gtui] le toppe sono vive finche' il loro testo e' in cronologia\n");
    write_str("[gtui] premi q per uscire\n");

    // Resta a video: `q` esce.
    loop {
        let mut buf = [0u8; 16];
        let n = civis::input::read(&mut buf);
        if n == 0 {
            if !civis::input::wait() {
                break;
            }
            continue;
        }
        if buf[..n].iter().any(|&b| b == b'q' || b == b'Q') {
            break;
        }
    }
    mark("exit");
    civis::exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[gtui] panic\n");
    civis::exit(1)
}
