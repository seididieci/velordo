//! impluvium — il **DE** di Velordo (D8e, ADR-0092).
//!
//! Possiede l'**albero delle tile** (split a sezione aurea), il **focus** e i
//! **tasti**: legge la tastiera da `porta` (`INPUT_*`, è il focus owner), i
//! tasti "globali" li consuma (modificatore **Super**), gli altri li inietta
//! nella sessione in focus (`SESS_INPUT`). Il layout lo pubblica al server con
//! `TILE_*`: la console disegna, il DE decide.
//!
//! Mappa tasti (sway/tmux): `Super+hjkl` = focus sinistra/sotto/sopra/destra,
//! `Super+%`/`Super+"` = split verticale/orizzontale, `Super+z` = zoom,
//! `Super+Shift+q` = chiudi la tile in focus.
//!
//! Con **una** tile sola il layout e' lo schermo intero e la barra non c'e'
//! (non c'e' niente da mostrare, e il boot resta identico a D8d). Da due tile
//! in su la barra prende la prima riga e mostra un blocco colorato per tile
//! (quello in focus evidenziato): è una finestra del DE in `Layer::Overlay`,
//! niente testo (i titoli sono D8f).
//!
//! Non registra un servizio (`Service::Impluvium` non esiste: il kernel resta
//! intatto): init lo spawana e lo supervisiona **per canale**.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use civis::println;
use impluvium_tree::{Dir, Move, Tree};
use musivum::Rect;

civis::entry!(real_main);

/// Altezza della barra in pixel: una riga di testo (cella 10x22).
const BAR_PX: u32 = 22;

/// Il DE.
struct De {
    /// L'albero delle tile.
    tree: Tree,
    /// L'area delle tile (schermo, o schermo meno la barra).
    area: Rect,
    /// Dimensione del display in pixel.
    screen: (u32, u32),
    /// La finestra della barra: `(win, va, shm)`.
    chrome: Option<(u32, usize, u32)>,
    /// Il canale di `porta` (0 = non ancora sottoscritto).
    kb_chan: u64,
    /// Byte accumulati dalla tastiera (un tasto può arrivare a pezzi).
    stream: Vec<u8>,
    /// La sessione in focus, per **iniettare** (mailbox su di lei): si
    /// riattacca quando il focus cambia (`SESS_ATTACH` sposta il canale).
    injector: Option<civis::sess::Session>,
}

impl De {
    fn new() -> De {
        De {
            tree: Tree::new(0),
            area: Rect::EMPTY,
            screen: (0, 0),
            chrome: None,
            kb_chan: 0,
            stream: Vec::new(),
            injector: None,
        }
    }

    /// Legge la dimensione del display e sistema l'area delle tile.
    fn measure(&mut self) -> bool {
        let Ok((w, h)) = civis::tile::size() else {
            return false;
        };
        if w == 0 || h == 0 {
            return false;
        }
        self.screen = (w, h);
        self.update_area();
        true
    }

    /// L'area delle tile: lo schermo intero con una tile sola, sotto la barra
    /// quando ce ne sono di piu'.
    fn update_area(&mut self) {
        let (w, h) = self.screen;
        self.area = if self.tree.len() > 1 && h > BAR_PX {
            Rect::new(0, BAR_PX, w, h - BAR_PX)
        } else {
            Rect::new(0, 0, w, h)
        };
    }

    /// Pubblica il layout al server: una `TILE_SET` per tile, le sessioni
    /// uscite dall'albero nascoste, poi un solo `TILE_COMMIT`.
    fn apply_layout(&mut self) -> bool {
        let tiles = self.tree.layout(self.area);
        let mut ok = true;
        for (sid, r) in tiles.iter().copied() {
            ok &= civis::tile::set(sid as u32, r.x, r.y, r.w, r.h).is_ok();
        }
        // Chi non e' piu' nell'albero si nasconde (la sessione vive: la chiude
        // chi la possiede, con SESS_CLOSE).
        let _ = ok;
        ok && civis::tile::commit().is_ok()
    }

    fn focus_session(&mut self) {
        let sid = self.tree.focus() as u32;
        let _ = civis::sess::focus(sid);
        // Il canale del DE si sposta sulla sessione in focus: da lì inietta.
        self.injector = None;
        self.injector = civis::sess::attach_to_session(sid).ok();
        self.draw_chrome();
    }

    /// La barra: un blocco per tile, quello in focus evidenziato. E' una
    /// finestra `Overlay` del DE (nessun testo: i titoli sono D8f).
    fn draw_chrome(&mut self) {
        let n = self.tree.len();
        if n < 2 {
            // Con una tile la barra non serve: la si chiude.
            if let Some((win, _, _)) = self.chrome.take() {
                let _ = civis::win::close(win);
            }
            return;
        }
        let (sw, _) = self.screen;
        if self.chrome.is_none() {
            let size = (sw << 16) | BAR_PX;
            let Ok(open) = civis::win::open(!0u32, size) else {
                return;
            };
            let va = match civis::shm_map(open.shm_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
                Ok(v) => v,
                Err(_) => {
                    let _ = civis::win::close(open.win);
                    return;
                }
            };
            self.chrome = Some((open.win, va, open.shm_id));
        }
        let Some((win, va, _)) = self.chrome else {
            return;
        };
        let leaves = self.tree.leaves();
        let focus = self.tree.focus();
        let n = leaves.len().max(1);
        let bw = sw / n as u32;
        // Sfondo della barra: grigio scuro; blocco della tile in focus:
        // accento; gli altri: grigio.
        const BG: u32 = 0xFF20_2024; // BGRA
        const IDLE: u32 = 0xFF50_5058;
        const HOT: u32 = 0xFF98_8A_5A; // BGR di un ambra
        let ptr = va as *mut u32;
        let row_words = sw as usize;
        for y in 0..BAR_PX as usize {
            for x in 0..row_words {
                let slot = (x as u32 / bw).min(n as u32 - 1) as usize;
                let hot = leaves.get(slot).copied() == Some(focus);
                let c = if x as u32 % bw < bw.saturating_sub(2) && (2..BAR_PX - 2).contains(&(y as u32)) {
                    if hot {
                        HOT
                    } else {
                        IDLE
                    }
                } else {
                    BG
                };
                unsafe { core::ptr::write(ptr.add(y * row_words + x) as *mut u32, c) };
            }
        }
        let _ = civis::win::commit(win, 0);
    }

    /// Consuma i byte della tastiera: `Super+<tasto>` sono i comandi del DE,
    /// tutto il resto va alla sessione in focus.
    fn on_keys(&mut self, bytes: &[u8]) {
        self.stream.extend_from_slice(bytes);
        let mut i = 0usize;
        while i < self.stream.len() {
            let b = self.stream[i];
            if b == civis::INPUT_SUPER {
                // Prefisso: il prossimo "tasto" e' del DE.
                let Some((text, used)) = next_key(&self.stream[i + 1..]) else {
                    break; // tasto incompleto: si aspetta il resto
                };
                i += 1 + used;
                self.command(text);
                continue;
            }
            // Tasto normale: alla sessione in focus.
            if let Some(sess) = &self.injector {
                let _ = sess.input(&[b]);
            }
            i += 1;
        }
        self.stream.drain(..i);
    }

    /// Un comando del DE (il "tasto" dopo Super, gia' decodificato in testo).
    fn command(&mut self, key: u8) {
        match key {
            b'h' | b'j' | b'k' | b'l' => {
                let mv = match key {
                    b'h' => Move::Left,
                    b'j' => Move::Down,
                    b'k' => Move::Up,
                    _ => Move::Right,
                };
                if let Some(sid) = self.tree.focus_dir(mv, self.area) {
                    self.focus_session();
                    println!("[impluvium] focus {} ({})", sid, key as char);
                }
            }
            b'%' => self.split(Dir::Col),
            b'"' => self.split(Dir::Row),
            b'z' => {
                self.tree.toggle_zoom();
                self.update_area();
                self.apply_layout();
                self.focus_session();
                println!(
                    "[impluvium] zoom {} (tile={})",
                    if self.tree.zoomed() { "on" } else { "off" },
                    self.tree.len()
                );
            }
            b'Q' => self.close_focused(),
            _ => {}
        }
    }

    /// Split della foglia in focus: una sessione nuova (nessun programma:
    /// lo spawn di una shell per foglia è il passo successivo) e una tile.
    fn split(&mut self, dir: Dir) {
        let Ok(sess) = civis::sess::create_unbound() else {
            println!("[impluvium] split: sessione non creata");
            return;
        };
        let sid = sess.id as usize;
        if !self.tree.split(sid, dir, self.area) {
            println!("[impluvium] split: rifiutato (spazio)");
            let _ = sess.close();
            return;
        }
        self.update_area();
        self.apply_layout();
        self.focus_session();
        println!(
            "[impluvium] split {:?} -> tile={} focus={}",
            dir,
            self.tree.len(),
            self.tree.focus()
        );
    }

    fn close_focused(&mut self) {
        let sid = self.tree.focus();
        if !self.tree.remove(sid) {
            return; // l'ultima tile resta
        }
        let _ = civis::tile::drop_tile(sid as u32);
        if let Ok(sess) = civis::sess::attach_to_session(sid as u32) {
            let _ = sess.close();
        }
        self.update_area();
        self.apply_layout();
        self.focus_session();
        println!(
            "[impluvium] close {} -> tile={} focus={}",
            sid,
            self.tree.len(),
            self.tree.focus()
        );
    }
}

/// Il "tasto" che segue il prefisso `INPUT_SUPER`: `(byte, consumati)`.
/// Le sequenze ESC (frecce ecc.) sono consumate intere e **non** mappate: il
/// DE lega caratteri, non posizioni fisiche.
fn next_key(rest: &[u8]) -> Option<(u8, usize)> {
    let b = *rest.first()?;
    if b != 0x1b {
        return Some((b, 1));
    }
    // ESC [ X (o ESC [ n ~): si consuma la sequenza, nessun comando.
    if rest.len() < 3 {
        return None;
    }
    let mut n = 2;
    while n < rest.len() && !(0x40..=0x7e).contains(&rest[n]) {
        n += 1;
    }
    if n >= rest.len() {
        return None;
    }
    Some((0, n + 1))
}

fn real_main(_sp: u64) -> ! {
    println!("[impluvium] partenza");
    let mut de = De::new();
    // 1. La console c'e' (init la spawna prima): misura e primo layout.
    for _ in 0..200 {
        if de.measure() {
            break;
        }
        civis::spin_ticks(1);
    }
    if de.screen == (0, 0) {
        println!("[impluvium] nessun display pixel: resto inerte (fallback VGA)");
    } else {
        if !de.apply_layout() {
            println!("[impluvium] layout iniziale rifiutato");
        }
        de.focus_session();
        println!(
            "[impluvium] display {}x{} area {}x{} tile={}",
            de.screen.0,
            de.screen.1,
            de.area.w,
            de.area.h,
            de.tree.len()
        );
    }
    // 2. La tastiera (D8e: il DE e' il focus owner; `porta` parte dopo la
    //    console, quindi si riprova).
    if civis::input::device::attach().is_ok() {
        de.kb_chan = 1;
        println!("[impluvium] tastiera sottoscritta (Super = modificatore)");
    } else {
        println!("[impluvium] tastiera non disponibile: la console inietta");
    }
    civis::signal_ready(1);

    // 3. Loop: hint di tastiera, TILE_SCREEN (il display si e' riadattato),
    //    morte di porta (riattacco), richieste (nessuna: il DE non serve
    //    nessuno).
    loop {
        match civis::recv() {
            Ok(m) => match m.tag {
                civis::INPUT_NOTIFY => {
                    let mut buf = [0u8; 64];
                    loop {
                        let n = civis::input::device::read(&mut buf);
                        if n == 0 {
                            break;
                        }
                        de.on_keys(&buf[..n]);
                    }
                }
                civis::TILE_SCREEN => {
                    if de.measure() {
                        de.apply_layout();
                        de.focus_session();
                        println!("[impluvium] riadattato a {}x{}", de.screen.0, de.screen.1);
                    }
                }
                civis::EXIT_NOTIFY => {
                    if civis::input::device::peer_pid() == m.w1 as i64 {
                        // porta ripartita: riattaccati (il botta e risposta e'
                        // gia' nel client, qui si riparte dal device).
                        civis::input::device::detach();
                        let _ = civis::input::device::attach();
                    }
                }
                _ => {}
            },
            Err(_) => {}
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[impluvium] panic\n");
    civis::exit(1)
}
