//! porta — Terminale in userspace (Fase 15, raw in 43b, R9: ex-tty).
//!
//! Legge scancode raw (Set 1) da `/dev/kbd` (driver `kbd`), li decodifica
//! con `pc_keyboard` (layout US) e serve i byte ai client sul device
//! `/dev/input/keyboard` SENZA echo (Fase 43b: l'echo lo fa il lettore, la
//! shell con la sua readline — tty resta un trasporto raw). Mappa tasti:
//! char-by-char, `Enter→\n`, `Backspace→0x08`, frecce→`ESC[A/B/C/D`,
//! `Home/End→ESC[H/F`, `Delete→ESC[3~`, `Esc→ESC`, resto filtrato.
//!
//! REGOLA ANTI-DEADLOCK (Fase 15, ciclo cardo<->tty): un driver che SERVE
//! richieste sincrone non deve MAI emettere IPC FS sincrone. cardo gli
//! inoltra relay DEV e resta bloccato finche' non risponde; se nel mentre il
//! driver resta bloccato su cardo (pump read, echo write), nessuno dei due
//! avanza piu' (osservato: wedge t30/t31). Per questo tty e' un client FS
//! PURAMENTE async (`read_async`/`write_async`/`open_async`/
//! `fs_register_async` + collect via poll): non si blocca mai, risponde alle
//! relay all'istante (i DEV_WRITE vengono accodati e recapitati in background:
//! le scritte su console/VGA non possono fallire).
//!
//! EVENT-DRIVEN (anti-dilution): tty dorme in `recv()` bloccante e si sveglia
//! solo su reply async / EXIT_NOTIFY / KBD_NOTIFY (kbd lo avvisa quando ci
//! sono scancode) / relay DEV. Niente pump in polling: un server che gira a
//! vuoto ruba quanta a tutti (osservato: flooder t30 rallentato 25x da UN
//! solo spinner a pari priorita'). L'unico codice sincrono e' pre-
//! registrazione (nessuno puo' instradargli relay: il mount non esiste ancora).

#![no_std]
#![no_main]

extern crate alloc;
use alloc::collections::VecDeque;
use pc_keyboard::{DecodedKey, HandleControl, KeyCode, Keyboard, KeyboardLayout, ScancodeSet1, layouts};

use civis::println;

// ── IPC tags (DocsD: single source in `syscall-numbers`, via `civis`) ──
use civis::{DEV_CLOSE, DEV_KEYBOARD, DEV_OPEN, DEV_READ, DEV_WRITE};

/// Notify da kbd (fire-and-forget, NESSUNA reply): scancode in attesa.
/// tty dorme in recv() e si sveglia solo qui (o su relay DEV / reply async).
/// Senza notify servirebbe pump in polling (sempre Ready → dilution scheduler).
/// Single source in `syscall-numbers` (DocsB), via `civis`.
use civis::KBD_NOTIFY;
/// Eventi di input (E0, ADR-0082): attach/detach dei lettori e notify in
/// uscita verso chi ha il focus.
use civis::{INPUT_ATTACH, INPUT_DETACH, INPUT_NOTIFY};

/// Device type per DEV_OPEN (stesso di prima: il path non cambia; valore in
/// `syscall-numbers`, importato sopra).

// ── Ring I/O (Fase 10.2, pattern console/devfs: servire i client) ───

const CLI_REQ: u64 = civis::CLI_REQ_VA;
const CLI_RESP: u64 = civis::CLI_RESP_VA;
// Geometria ring + errore IPC (A1) + frame helpers (A2): single source in `civis`.
use civis::ERR;
use civis::{req_frame_read, resp_frame_write};

// ── Stato ───────────────────────────────────────────────────────────

/// Layout US-ANSI corretto (Fase 41 + 43b): `pc-keyboard 0.7` mappa lo
/// scancode `0x2B` (backslash ANSI, quello che QEMU `sendkey backslash`
/// invia) su `KeyCode::Oem7`, ma `Us104Key` non gestisce `Oem7` (cade in
/// `RawKey`, che `decode_bytes` scarta) — solo `0x56` (tasto ISO, assente
/// sulle ANSI) dava `\`/`|`. Risultato: `\` e `|` non arrivavano mai al guest
/// (nomi QEMU validi ma byte mai consegnati). Stesso buco per Delete (43b):
/// `Us104Key` lo mappa a `Unicode(0x7f)`, mai a `RawKey`, quindi l'arm
/// `KeyCode::Delete→ESC[3~` non scatterebbe mai (Delete muto mid-line,
/// osservato). Si delega tutto a `Us104Key` tranne `Oem7` (posizione ANSI US:
/// `\` / `|` con shift) e `Delete` (RawKey, editing della shell).
struct Us104Fix;

impl KeyboardLayout for Us104Fix {
    fn map_keycode(
        &self,
        keycode: KeyCode,
        modifiers: &pc_keyboard::Modifiers,
        handle_ctrl: HandleControl,
    ) -> DecodedKey {
        if keycode == KeyCode::Oem7 {
            if modifiers.is_shifted() {
                DecodedKey::Unicode('|')
            } else {
                DecodedKey::Unicode('\\')
            }
        } else if keycode == KeyCode::Delete {
            DecodedKey::RawKey(KeyCode::Delete)
        } else {
            layouts::Us104Key.map_keycode(keycode, modifiers, handle_ctrl)
        }
    }
}
const INPUT_CAPACITY: usize = 256;
/// Coda output verso /dev/console: echo + DEV_WRITE inoltrati, in ordine.
/// Le scritte VGA non falliscono mai: le relay DEV_WRITE rispondono OK subito.
const OUT_CAPACITY: usize = 16384;
#[derive(Clone, Copy, PartialEq)]
enum OpKind {
    BufReg,
    OpenKbd,
    OpenCon,
    Register,
    PumpRead,
    ConWrite,
}

#[derive(Clone, Copy)]
struct Pending {
    req: i64,
    kind: OpKind,
}

/// Sottoscrittore degli eventi di input (E0, ADR-0082). `chan` e' il canale
/// diretto client→porta (`service_lookup`), `pid` l'identita' del client per
/// riconoscerne la morte. Il focus e' LIFO: l'ultimo attaccato riceve i tasti.
#[derive(Clone, Copy)]
struct Sub {
    chan: u64,
    pid: i64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    LookupFs,
    BufReg,
    OpenKbd,
    OpenCon,
    Register,
    Steady,
}

struct Tty {
    phase: Phase,
    pending: Option<Pending>,
    kbd_fd: i64,
    con_fd: i64,
    input: VecDeque<u8>,
    out: alloc::vec::Vec<u8>,
    decoder: Keyboard<Us104Fix, ScancodeSet1>,
    err_streak: u32,
    ready_sent: bool,
    flush_wait_until: i64,
    /// Pump richiesta (notify kbd o ingresso in Steady): il prossimo giro
    /// avvia una read async (level-triggered: resta finche' non parte).
    pump_now: bool,
    /// Backoff dopo un pump fallito (vedi retry in collect PumpRead).
    pump_wait_until: i64,
    /// Lettori sottoscritti agli eventi di input, in ordine di attach: il
    /// **ultimo** ha il focus (LIFO). Potati su `INPUT_DETACH` e alla morte
    /// del client (`EXIT_NOTIFY`), cosi' il focus torna al precedente.
    subs: alloc::vec::Vec<Sub>,
}

impl Tty {
    fn new() -> Self {
        Self {
            phase: Phase::LookupFs,
            pending: None,
            kbd_fd: -1,
            con_fd: -1,
            input: VecDeque::new(),
            out: alloc::vec::Vec::new(),
            decoder: Keyboard::new(
                ScancodeSet1::new(),
                Us104Fix,
                HandleControl::MapLettersToUnicode,
            ),
            err_streak: 0,
            ready_sent: false,
            flush_wait_until: 0,
            pump_now: false,
            pump_wait_until: 0,
            subs: alloc::vec::Vec::new(),
        }
    }

    /// Torna allo stato pre-boot (morte cardo o streak di errori): scarta
    /// l'op in volo e i fd; il loop riparte dal lookup Fs. Idempotente.
    /// Stampa SEMPRE: un reset inatteso deve essere visibile (lo stallo
    /// silenzioso di Fase 15 e' costato un giorno di diagnosi).
    fn reset_to_lookup(&mut self) {
        println!("[porta] reset boot SM (phase={:?})", self.phase as u8);        civis::fs_abort_pending();
        self.pending = None;
        self.phase = Phase::LookupFs;
        self.kbd_fd = -1;
        self.con_fd = -1;
        self.input.clear();
        self.out.clear();
        self.err_streak = 0;
    }

    fn note_error(&mut self) {
        self.err_streak += 1;
        // Il peer driver (kbd/console) potrebbe essere stato riavviato senza
        // che ce ne accorgessimo (nessun EXIT a noi: non siamo suoi peer):
        // dopo 50 errori consecutivi riparti dal lookup (riapre i peer).
        if self.err_streak >= 50 {
            println!("[porta] streak errori, riapro i peer");
            self.reset_to_lookup();
        }
    }

    /// Avanza il boot async: una transizione per chiamata, mai bloccante.
    /// INVARIANTE FONDAMENTALE: l'invio avviene NELLA STESSA chiamata che
    /// entra nella fase (mai "imposto fase ora, invio al prossimo giro"):
    /// prima della registrazione nessuno puo' svegliare tty, quindi un giro
    /// che si chiude con recv() bloccante senza aver inviato nulla dorme per
    /// sempre (osservato: stallo silenzioso al boot con recv bloccante).
    fn boot_step(&mut self) {
        // Una sola op in volo (formato frame senza lunghezza): se c'e' gia'
        // una pending, la collect la chiude al giro dopo.
        if self.pending.is_some() {
            return;
        }
        loop {
            match self.phase {
                Phase::LookupFs => {
                    if civis::service_lookup(civis::Service::Cardo).is_ok() {
                        self.phase = Phase::BufReg;
                        continue;
                    } else {
                        for _ in 0..100_000 {
                            core::hint::spin_loop();
                        }
                        return;
                    }
                }
            // SEMPRE handshake prima di aprire: i ring di cardo sono
            // indicizzati per canale — sotto un nuovo canale (restart cardo,
            // re-lookup dopo stale) serve un nuovo BUF_REG o ogni op prende
            // NOHANDSHAKE per sempre (osservato Fase 15). Idempotente.
            Phase::BufReg => {
                if let Ok(req) = civis::fs_buf_reg_async() {
                    self.pending = Some(Pending { req, kind: OpKind::BufReg });
                } else {
                    for _ in 0..100_000 {
                        core::hint::spin_loop();
                    }
                }
                return;
            }
            Phase::OpenKbd => {
                // NOTA: il FILE device ("/dev/kbd/kbd"), mai la radice del
                // mount ("/dev/kbd" ha rel="" → dev_type fallisce, EISDIR).
                if let Ok(req) = civis::open_async("/dev/kbd/kbd", 0) {
                    self.pending = Some(Pending { req, kind: OpKind::OpenKbd });
                } else {
                    // Fallimento (backpressure o peer in restart): backoff,
                    // non martellare lookup/send a vuoto (igiene Livello 1).
                    for _ in 0..100_000 {
                        core::hint::spin_loop();
                    }
                }
                return;
            }
            Phase::OpenCon => {
                if let Ok(req) = civis::open_async("/dev/console/console", 0) {
                    self.pending = Some(Pending { req, kind: OpKind::OpenCon });
                } else {
                    for _ in 0..100_000 {
                        core::hint::spin_loop();
                    }
                }
                return;
            }
            Phase::Register => {
                if let Ok(req) = civis::fs_register_async(b"/dev/input") {
                    self.pending = Some(Pending { req, kind: OpKind::Register });
                } else {
                    for _ in 0..100_000 {
                        core::hint::spin_loop();
                    }
                }
                return;
            }
            Phase::Steady => {
                // Peer mai aperti (boot sfortunato): riparti dal lookup.
                if self.kbd_fd < 0 || self.con_fd < 0 {
                    self.reset_to_lookup();
                }
                return;
            }
        }
        }
    }

    /// Raccoglie una reply async che matcha `pending`. Ritorna true se era
    /// nostra (consumata), false altrimenti.
    fn collect_if_mine(&mut self, m: &civis::IpcMsg) -> bool {
        let p = match self.pending {
            Some(p) if m.req_id > 0 && m.req_id == p.req => p,
            _ => return false,
        };
        // Niente remap: i relay usano le finestre dedicate CLI_*, i ring
        // propri non vengono mai rimappati da nessuno.
        let mut tmp = [0u8; 64];
        match p.kind {
            OpKind::BufReg => {
                // Niente frame nel ring per BUF_REG: basta w0==0. MA il guard
                // 1-in-volo va resettato comunque (fs_collect_msg lo fa per le
                // altre op): senza, ogni op successiva viene rifiutata per
                // sempre (osservato: stallo silenzioso al boot).
                civis::fs_abort_pending();
                if m.w0 == 0 {
                    self.err_streak = 0;
                    self.phase = Phase::OpenKbd;
                } else {
                    self.note_error();
                    self.reset_to_lookup();
                    return true;
                }
            }
            OpKind::OpenKbd => {
                match civis::fs_collect_msg(m, &mut tmp, 64, false) {
                    Ok(fd) => {
                        self.kbd_fd = fd;
                        self.err_streak = 0;
                        self.phase = Phase::OpenCon;
                    }
                    Err(_) => {
                        self.note_error();
                        self.reset_to_lookup();
                        return true;
                    }
                }
            }
            OpKind::OpenCon => {
                match civis::fs_collect_msg(m, &mut tmp, 64, false) {
                    Ok(fd) => {
                        self.con_fd = fd;
                        self.err_streak = 0;
                        self.phase = Phase::Register;
                    }
                    Err(_) => {
                        self.note_error();
                        self.reset_to_lookup();
                        return true;
                    }
                }
            }
            OpKind::Register => {
                match civis::fs_collect_msg(m, &mut tmp, 64, false) {
                    Ok(0) => {
                        self.err_streak = 0;
                        self.phase = Phase::Steady;
                        println!("[porta] registered /dev/input with cardo");
                    }
                    _ => {
                        self.note_error();
                        self.reset_to_lookup();
                    }
                }
            }
            OpKind::PumpRead => {
                match civis::fs_collect_msg(m, &mut tmp, 64, true) {
                    Ok(n) if n > 0 => {
                        self.err_streak = 0;
                        self.decode_bytes(&tmp[..n as usize]);
                    }
                    Err(_) => {
                    // Errore (es. resync cardo che ha scartato il frame):
                    // riprova al prossimo giro invece di aspettare una nuova
                    // notify (che potrebbe non arrivare mai: la notify e' andata
                    // persa col frame scartato e kbd dorme). Il relay DEV_READ
                    // della riprova sveglia kbd da solo: se ha dati li consegna,
                    // se e' vuoto torna 0 e ci si ferma. Solo su ERR, mai su 0
                    // (0 = vuoto legittimo, nessun retry). Throttle via
                    // pump_wait_until (come flush); dopo 50 fallimenti
                    // consecutivi reset_to_lookup riapre i peer.
                    self.note_error();
                    self.pump_now = true;
                    self.pump_wait_until = civis::get_ticks().wrapping_add(2);
                    }
                    // Ok(0) = vuoto legittimo: niente da fare, nessun errore.
                    Ok(_) => {}
                }
            }
            OpKind::ConWrite => {
                match civis::fs_collect_msg(m, &mut tmp, 64, false) {
                    Ok(r) => {
                        self.err_streak = 0;
                        let adv = (r as usize).min(self.out.len());
                        self.out.drain(..adv);
                    }
                    Err(_) => {
                        self.note_error();
                    }
                }
            }
        }
        self.pending = None;
        true
    }

    /// Decodifica scancode in byte per il client (Fase 43b, raw): SOLO coda
    /// input, mai eco (lo fa il lettore, la shell). Char-by-char immediato.
    /// Se il lotto ha prodotto byte, sveglia il sottoscrittore con focus: il
    /// notify e' un hint, i byte restano qui e si leggono col DEV_READ.
    fn decode_bytes(&mut self, scancodes: &[u8]) {
        let before = self.input.len();
        for &sc in scancodes {
            if let Ok(Some(event)) = self.decoder.add_byte(sc) {
                if let Some(decoded) = self.decoder.process_keyevent(event) {
                    match decoded {
                        DecodedKey::Unicode(c) => {
                            let mut tmp = [0u8; 4];
                            let s = c.encode_utf8(&mut tmp);
                            self.emit(s.as_bytes());
                        }
                        DecodedKey::RawKey(code) => match code {
                            KeyCode::Return | KeyCode::NumpadEnter => self.emit(b"\n"),
                            KeyCode::Backspace => self.emit(b"\x08"),
                            KeyCode::ArrowUp => self.emit(b"\x1b[A"),
                            KeyCode::ArrowDown => self.emit(b"\x1b[B"),
                            KeyCode::ArrowRight => self.emit(b"\x1b[C"),
                            KeyCode::ArrowLeft => self.emit(b"\x1b[D"),
                            KeyCode::Home => self.emit(b"\x1b[H"),
                            KeyCode::End => self.emit(b"\x1b[F"),
                            KeyCode::Delete => self.emit(b"\x1b[3~"),
                            KeyCode::Escape => self.emit(b"\x1b"),
                            _ => {}
                        },
                    }
                }
            }
        }
        if self.input.len() != before {
            self.notify_input();
        }
    }

    /// Sveglia il sottoscrittore con focus (LIFO). Fire-and-forget: non
    /// aspettiamo reply e non blocchiamo il path del relay DEV (regola
    /// anti-deadlock). Un canale morto (client uscito senza EXIT recapitato)
    /// si pota qui, e il prossimo evento va al sottoscrittore precedente.
    fn notify_input(&mut self) {
        let Some(sub) = self.subs.last().copied() else {
            return;
        };
        if civis::send_async(sub.chan, INPUT_NOTIFY, self.input.len() as u64, 0).is_err() {
            self.subs.pop();
        }
    }

    /// Accoda byte in input (client). L'eco su console NON passa di qui
    /// (43b): lo fa il lettore con le sue write (la coda `out` resta per il
    /// DEV_WRITE dei client, recapitata dal flush come prima).
    fn emit(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.input.len() < INPUT_CAPACITY {
                self.input.push_back(b);
            }
        }
    }

    /// Pump tastiera event-driven: parte SOLO se richiesta (notify kbd o
    /// ingresso in Steady), una read async alla volta. Mai polling periodico:
    /// tty dorme in recv() quando idle (zero dilution scheduler).
    fn pump_maybe(&mut self) {
        if self.phase != Phase::Steady || self.pending.is_some() || self.kbd_fd < 0 {
            return;
        }
        if !self.pump_now {
            return;
        }
        // Backoff dopo un invio fallito (come flush): non riprovare a vuoto
        // ogni giro (igiene Livello 1).
        let now = civis::get_ticks();
        if now.wrapping_sub(self.pump_wait_until) < 0 {
            return;
        }
        self.pump_now = false;
        if let Ok(req) = civis::read_async(self.kbd_fd, 64) {
            self.pending = Some(Pending { req, kind: OpKind::PumpRead });
        } else {
            // Invio fallito (backpressure): riprova con backoff, come flush.
            // Senza, un pump perso resta perso fino alla prossima notify.
            self.pump_now = true;
            self.pump_wait_until = civis::get_ticks().wrapping_add(2);
        }
    }

    /// Scarica la coda output verso /dev/console (un chunk async alla volta).
    fn flush_maybe(&mut self) {
        if self.phase != Phase::Steady || self.pending.is_some() {
            return;
        }
        if self.out.is_empty() || self.con_fd < 0 {
            return;
        }
        // Backoff dopo un invio fallito (backpressure/restart): non riprovare
        // a vuoto ogni giro (igiene Livello 1).
        let now = civis::get_ticks();
        if now.wrapping_sub(self.flush_wait_until) < 0 {
            return;
        }
        // Bound operativo del frame (single source in `civis`, audit CAP P4):
        // `write_async` rifiuta oltre `RING_MAX_PAYLOAD`, mai magic number.
        let n = self.out.len().min(civis::RING_MAX_PAYLOAD);
        if let Ok(req) = civis::write_async(self.con_fd, &self.out[..n]) {
            self.pending = Some(Pending { req, kind: OpKind::ConWrite });
        } else {
            self.flush_wait_until = now.wrapping_add(2);
        }
    }
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    println!("[porta] starting, pid={}", civis::getpid());

    // Registra il servizio Tty per nome (supervisione init-restart; i client
    // usano il FS, nessuno risolve questo nome per parlare).
    if civis::service_register(civis::Service::Porta).is_ok() {
        println!("[porta] registered as service Porta");
    }

    let mut tty = Tty::new();

    loop {
        // 1. Boot async (no-op in Steady).
        //    (Niente remap dance: i relay usano le finestre dedicate CLI_*,
        //    i ring propri non vengono mai rimappati da nessuno.)
        tty.boot_step();
        if tty.phase == Phase::Steady && !tty.ready_sent {
            tty.ready_sent = true;
            // Prima pump: svuota eventuali tasti arrivati prima di noi (kbd li
            // accoda anche senza tty registrato).
            tty.pump_now = true;
            // SVC_READY fire-and-forget in `civis` (A3, init aspetta a boot).
            civis::signal_ready(1);
        }

        // 2. Pump + flush (solo Steady, mai bloccanti: tutto async).
        tty.pump_maybe();
        tty.flush_maybe();

        // 3. Attesa messaggi. REGOLA FONDAMENTALE (osservato: sleep-forever):
        //    si dorme in recv() bloccante SOLO se c'e' un wake garantito
        //    (pending settata → arrivera' reply o EXIT; Steady → relay DEV /
        //    notify / reply reali, o giusto idle). In boot SENZA pending
        //    (LookupFs, retry dopo send fallita) NESSUNO puo' svegliarci
        //    (mount assente, reply inesistente): li' si POLLA con spin
        //    (come gli ensure loop degli altri driver), mai block.
        let can_sleep = tty.pending.is_some() || tty.phase == Phase::Steady;
        if !can_sleep {
            match civis::recv_poll() {
                Some(m) => tty.handle_msg(m),
                None => {
                    for _ in 0..10_000 {
                        core::hint::spin_loop();
                    }
                    continue;
                }
            };
            continue;
        }
        // 3b. BLOCCANTE: tty dorme qui quando idle o in attesa di reply
        //    (zero dilution scheduler).
        match civis::recv() {
            Ok(m) => tty.handle_msg(m),
            Err(_) => {
                // Peer morto senza EXIT recapitato, o wake spurio: ricontrolla
                // il giro dopo (EXIT_NOTIFY arrivera' e resettera').
            }
        }
    }
}

/// Gestione di un singolo messaggio ricevuto (poll o blocking): reply async
/// (collect/stale), EXIT_NOTIFY (reset), KBD_NOTIFY (pump, mai reply),
/// relay DEV (serve e rispondi subito). Estratta perche' usata sia dal ramo
/// poll (boot senza pending) che da quello bloccante.
impl Tty {
    fn handle_msg(&mut self, m: civis::IpcMsg) {
        if m.req_id > 0 {
            // Risposta async: nostra (collect) o stale (scarta: niente frame
            // nostro nel ring, nessun consumo da fare).
            self.collect_if_mine(&m);
            return;
        }
        if m.tag == civis::EXIT_NOTIFY {
            // Se muore un lettore sottoscritto il focus torna al precedente
            // (LIFO): non e' la morte di cardo, il boot SM resta dov'e'.
            if self.subs.iter().any(|s| s.pid == m.w1 as i64) {
                self.subs.retain(|s| s.pid != m.w1 as i64);
                return;
            }
            // cardo morto e rinato (o altro peer): riparte il boot async
            // (riapre i peer, ri-registra). Mai reply.
            println!("[porta] peer morto, riparto dal lookup");
            self.reset_to_lookup();
            return;
        }
        if m.tag == INPUT_ATTACH {
            // Focus LIFO: la stessa connessione si riposiziona in cima (un
            // processo che ri-attacca non accumula voci ne' perde il focus).
            self.subs.retain(|s| s.chan != m.channel);
            let pid = civis::peer_pid(m.channel).unwrap_or(-1);
            self.subs.push(Sub { chan: m.channel, pid });
            let _ = civis::reply(0, 0, 0);
            return;
        }
        if m.tag == INPUT_DETACH {
            self.subs.retain(|s| s.chan != m.channel);
            let _ = civis::reply(0, 0, 0);
            return;
        }
                if m.tag == KBD_NOTIFY {
                    // Fire-and-forget da kbd: NESSUNA reply (il mittente async
                    // non aspetta; rispondergli accoderebbe spazzatura in kbd).
                    self.pump_now = true;
                    return;
                }
        let result: Option<u64> = match m.tag {
            DEV_OPEN => {
                if m.w0 == DEV_KEYBOARD {
                    Some(0)
                } else {
                    None
                }
            }
            DEV_READ => {
                let count = (m.w1 as usize).min(256);
                let mut buf = [0u8; 256];
                let mut i = 0;
                while i < count {
                    match self.input.pop_front() {
                        Some(c) => {
                            buf[i] = c;
                            i += 1;
                        }
                        None => break,
                    }
                }
                // Frame SEMPRE (anche vuoto con i==0, come kbd/devfs): il
                // client distingue "0 byte" da "ring vuoto" solo dal frame.
                // Senza, un async-reader confonde vuoto e risposta persa.
                unsafe { resp_frame_write(CLI_RESP, &buf[..i]); }
                Some(i as u64)
            }
            DEV_WRITE => {
                // Accoda per il flush async e rispondi OK SUBITO: aspettare il
                // completamento qui ricreerebbe il ciclo (cardo aspetta noi,
                // noi cardo).
                let count = m.w1 as usize;
                if count > 0 {
                    let mut data = alloc::vec::Vec::with_capacity(count);
                    data.resize(count, 0);
                    unsafe { req_frame_read(CLI_REQ, &mut data, count); }
                    if self.out.len() + count <= OUT_CAPACITY {
                        self.out.extend_from_slice(&data);
                    }
                }
                Some(count as u64)
            }
            DEV_CLOSE => Some(0),
            _ => None,
        };
        let _ = civis::reply(0, result.unwrap_or(ERR), 0);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[porta] panic");
    civis::exit(1)
}
