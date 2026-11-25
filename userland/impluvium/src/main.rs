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
//! intatto): lo spawana il **login** (D8f) dentro la sessione autenticata, ed
//! e' init a sorvegliare il login. Le shell delle tile nascono da qui.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use civis::println;
use impluvium_tree::{Dir, Move, Tree};
use musivum::Rect;

civis::entry!(real_main);

/// Il DE.
struct De {
    /// L'albero delle tile.
    tree: Tree,
    /// L'area delle tile (schermo, o schermo meno la barra).
    area: Rect,
    /// Dimensione del display in pixel.
    screen: (u32, u32),
    /// Il canale di `porta` (0 = non ancora sottoscritto).
    kb_chan: u64,
    /// Byte accumulati dalla tastiera (un tasto può arrivare a pezzi).
    stream: Vec<u8>,
    /// La sessione in focus, per **iniettare** (mailbox su di lei): si
    /// riattacca quando il focus cambia (`SESS_ATTACH` sposta il canale).
    injector: Option<civis::sess::Session>,
    /// I figli avviati per le foglie: `(pid, sessione)`. Serve a togliere la
    /// tile quando la shell esce.
    children: Vec<(i64, usize)>,
    /// Nome utente del login (D8f): passato alle shell come `USER` (un nome,
    /// non un segreto). Vuoto = nessun nome (prompt senza utente).
    user: String,
}

impl De {
    fn new() -> De {
        De {
            tree: Tree::new(0),
            area: Rect::EMPTY,
            screen: (0, 0),
            kb_chan: 0,
            stream: Vec::new(),
            injector: None,
            children: Vec::new(),
            user: String::new(),
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

    /// L'area delle tile: **tutto lo schermo**. La barra non c'e' (D8e3): il
    /// focus lo dice il bordo della tile, colorato dal server — cosi' non si
    /// perde una riga di testo e il chrome non e' un rettangolo in piu'.
    fn update_area(&mut self) {
        let (w, h) = self.screen;
        self.area = Rect::new(0, 0, w, h);
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
        // La sessione nuova **non** deve ecoare nel vuoto: il lettore sara' la
        // shell che stiamo per avviare (che si spegne l'eco da sola, in
        // `input::attach`). Fino ad allora niente tasti a schermo.
        let _ = sess.echo(false);
        // D8f: copia il token dalla sessione 0 (dove il login lo ha messo con
        // SESS_TOKEN_SET) sulla sessione della nuova tile, prima di spawnare.
        // Sessione 0: il token c'e' gia'. `Session::set_token` scrive sulla
        // sessione a cui e' attaccato il canale, quindi si attacca prima a 0
        // (per leggere) e poi a `sid` (per scrivere).
        if sid != 0 {
            let token = civis::sess::attach_to_session(0)
                .and_then(|s| s.token())
                .unwrap_or(0);
            if let Ok(s) = civis::sess::attach_to_session(sid as u32) {
                let _ = s.set_token(token);
            }
            // `s` viene drop qui (smunmap della sua mailbox); il canale resta
            // attaccato a `sid`, serve per SESS_INPUT del DE.
        }
        // Prima il layout e il focus, poi la shell: cosi' la tile esiste (e la
        // sessione e' quella in focus) gia' quando il programma scrive il suo
        // banner — altrimenti quelle righe le perde il mirror seriale.
        self.update_area();
        self.apply_layout();
        self.focus_session();
        let spawned = self.spawn_shell(sid);
        drop(sess);
        println!(
            "[impluvium] split {:?} -> tile={} focus={} shell={}",
            dir,
            self.tree.len(),
            self.tree.focus(),
            if spawned { "ok" } else { "NO" }
        );
    }

    /// Avvia una **shell nella sessione `sid`** (una tile = una sessione = un
    /// programma).
    ///
    /// Il token e' gia' stato copiato sulla sessione `sid` dal chiamante
    /// (`split` lo copia dalla sessione 0; la sessione 0 lo ha già dal login).
    ///
    /// Ordine che chiude la razza: `spawn_image` (come fa `login`, fail-closed
    /// sulla firma) -> **`suspend`** del figlio -> `SESS_BIND(sid, pid)` ->
    /// **`resume`**. Senza il `suspend` il figlio puo' chiamare
    /// `sess::current()` durante la bind (che e' una IPC sincrona: il padre si
    /// blocca e il figlio gira) e risolversi sulla sessione 0 — il suo output
    /// finirebbe nella tile sbagliata.
    fn spawn_shell(&mut self, sid: usize) -> bool {
        match civis::fs::rights::rights_execute_verdict(b"sys", b"bin/interpres.bin") {
            Ok((true, _)) => {}
            Ok((false, _)) => {
                println!("[impluvium] shell non eseguibile (firma assente o negata)");
                return false;
            }
            Err(e) => {
                println!("[impluvium] verifica shell fallita: {:?}", e);
                return false;
            }
        }
        let Ok(img) = civis::obj_get(b"sys", b"bin/interpres.bin") else {
            println!("[impluvium] shell non trovata su ArcaFS");
            return false;
        };
        if img.is_empty() {
            return false;
        }
        let Some(meta) = civis::SpawnMeta::new("interpres", 16, &[]) else {
            return false;
        };
        // D8f — `USER` alla shell (il token invece e' gia' nella sessione, e
        // la shell lo legge dal server per attestazione). L'env block deve
        // restare vivo fino a dopo lo spawn (il kernel lo copia nella
        // syscall).
        let user_env = format!("USER={}", self.user);
        let meta = match civis::pack_args(&[], &[&user_env.as_bytes()]) {
            Some(packed) => meta.argv(&packed),
            None => meta,
        };
        let Ok(chan) = civis::spawn_image(&img, &meta.detached()) else {
            println!("[impluvium] spawn della shell fallito");
            return false;
        };
        let pid = civis::peer_pid(chan as u64).unwrap_or(-1);
        if pid <= 0 {
            return false;
        }
        let _ = civis::suspend(pid);
        let bound = civis::sess::bind(sid as u32, pid).is_ok();
        let _ = civis::resume(pid);
        if !bound {
            // Senza binding la shell scriverebbe sulla sessione 0: si chiude.
            println!("[impluvium] bind fallito: chiudo la shell");
            let _ = civis::kill(pid, 0);
            return false;
        }
        self.children.push((pid, sid));
        true
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

/// Il nome utente del login, dall'ambiente del DE (`USER`, D8f). Un nome —
/// non un segreto: il DE lo copia nell'ambiente delle shell per il prompt.
fn read_user(sp: u64) -> String {
    let Some(env) = civis::env_from_stack(sp) else {
        return String::new();
    };
    match env.get("USER") {
        Some(v) => core::str::from_utf8(v).unwrap_or("").into(),
        None => String::new(),
    }
}

fn real_main(sp: u64) -> ! {
    println!("[impluvium] partenza");
    let mut de = De::new();
    de.user = read_user(sp);
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
        println!(
            "[impluvium] display {}x{} area {}x{} tile={}",
            de.screen.0,
            de.screen.1,
            de.area.w,
            de.area.h,
            de.tree.len()
        );
    }
    // La sessione 0 e' il terminale di boot: il DE ci inietta comunque (anche
    // nel fallback VGA, dove e' lui il focus owner).
    de.focus_session();
    // 2. La tastiera (D8e: il DE e' il focus owner; `porta` parte dopo la
    //    console, quindi si riprova).
    if civis::input::device::attach().is_ok() {
        de.kb_chan = 1;
        println!("[impluvium] tastiera sottoscritta (Super = modificatore)");
    } else {
        println!("[impluvium] tastiera non disponibile: la console inietta");
    }
    // 3. La shell della **tile 0** (D8f): prima la avviava il login; ora la
    //    sessione e' del DE, che la riempie come fa per ogni tile nuova.
    //    Pulisce la sessione 0: i messaggi del login e di impluvium non devono
    //    restare sullo schermo quando la shell disegna il prompt. `0x0C` e' il
    //    clear che `littera` implementa (CSI 2J no).
    let _ = civis::sess::out_screen(b"\x0C");
    if !de.spawn_shell(0) {
        println!("[impluvium] shell iniziale NON avviata");
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
                    // Una **shell di foglia** e' morta (`exit`): la sua tile si
                    // chiude e il fratello prende lo spazio (mai l'ultima).
                    let dead = m.w1 as i64;
                    if let Some(pos) = de.children.iter().position(|&(p, _)| p == dead) {
                        let (_, sid) = de.children.remove(pos);
                        let was_last = de.children.is_empty();
                        if de.tree.remove(sid) {
                            let _ = civis::tile::drop_tile(sid as u32);
                            if let Ok(sess) = civis::sess::attach_to_session(sid as u32) {
                                let _ = sess.close();
                            }
                            de.update_area();
                            de.apply_layout();
                            de.focus_session();
                            println!(
                                "[impluvium] shell uscita (pid {}) -> tile={} focus={}",
                                dead,
                                de.tree.len(),
                                de.tree.focus()
                            );
                        }
                        // D8f passo 5: l'ultima shell che esce chiude la
                        // sessione (non lascia una tile vuota). Il DE esce con
                        // 0; il login, che e' in attesa della sua morte,
                        // riapre il prompt (OnDeath::LoginGate con codice 0).
                        if was_last {
                            println!("[impluvium] ultima shell uscita: fine sessione");
                            civis::exit(0);
                        }
                    }
                    if civis::input::device::peer_pid() == dead {
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
