//! Shell nativa Velordo (Fase 3.2, S3.2a + S3.2b): REPL minima post-login.
//!
//! Programma interattivo **nativo** (meccanismo, niente POSIX) che gira su
//! `civis::input` (input, dalla sessione — D8d) e la **sessione di terminale** (output,
//! `civis::sess`, D8c2). Non usa `cd`, `stdin`, `stdout`,
//! `fork`, pipe Unix, `$VAR` come primitive speciali: e' un dispatcher di
//! comandi su risorse/capability.
//!
//! Comandi S3.2a:
//!   `help`           mostra i comandi disponibili
//!   `create-user [nome] [ruoli]`  crea un utente (identita' attestata richiesta)
//!   `exit`           chiude la shell
//!
//! Comandi S3.2b — esecuzione esterna da bucket `binary`:
//!   `<programma>`  lancia un programma da un bucket eseguibile.
//!                  La shell scopre i bucket tramite BIN_LIST, cerca il nome
//!                  piatto `<programma>` (niente sottocartelle, ADR-0064),
//!                  verifica EXECUTE con cardo, e lo esegue con spawn_image.
//!
//! `users`, `logout` e `dns-resolve` sono programmi esterni (ADR-0075), non
//! built-in: la shell non conosce ne' `identis:config` ne' `init_bounce`.
//!
//! Se l'output di un comando non viene catturato in una variabile, il valore
//! (futuro, S3.2b+) viene stampato serializzato.
//!
//! Output sulla sessione di terminale (`civis::sess::out`/`out_screen`,
//! D8c2: il mirror seriale lo fa il server), input dalla sessione (D8d).
//! La password non transita mai in chiaro: hash BLAKE2s-256 in locale.
//!
//! **Identita' (S3.3c)**: nessun token. Un comando che richiede identita'
//! (`create-user`, `consent`) la ottiene dal **pid** attestato dal kernel:
//! Identis/Custos risolvono il chiamante. Se manca, la shell chiede di
//! (ri)autenticarsi (`IDENTIS_VERIFY` lega il pid all'utente).
//!
//! **Fase X (ADR-0065)**: interprete Rust-like con `let`, assegnamento, blocchi,
//! `if/else`, operatori binari, chiamate funzione e builtin (`dump`, `type`).

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

// ------------------------------------------------------------------
// History & line editor implementation for REPL
// ------------------------------------------------------------------

/// Numero massimo di comandi tenuti in history (i piu' vecchi escono).
const HISTORY_MAX: usize = 100;

/// Simple history buffer for the REPL.
/// Stores command strings and provides navigation via up/down.
struct History {
    entries: Vec<String>,
    index: usize, // current navigation position (0 = top of history)
}

impl History {
    fn new() -> Self {
        Self { entries: Vec::new(), index: 0 }
    }

    /// Push a new entry and reset navigation index. I duplicati consecutivi
    /// non entrano (come nella shell POSIX): premere Invio due volte non
    /// riempie la history di righe identiche.
    fn push(&mut self, entry: String) {
        if self.entries.last().is_some_and(|last| last == &entry) {
            self.index = self.entries.len();
            return;
        }
        if self.entries.len() >= HISTORY_MAX {
            self.entries.remove(0);
        }
        self.entries.push(entry);
        self.index = self.entries.len();
    }

    /// Move one step up in history (previous command).
    fn prev(&mut self) -> Option<String> {
        if self.index > 0 {
            self.index -= 1;
            Some(self.entries[self.index].clone())
        } else {
            None
        }
    }

    /// Move one step down in history (next command). Returns None when at end.
    fn next(&mut self) -> Option<String> {
        if self.index < self.entries.len() {
            self.index += 1;
            if self.index == self.entries.len() {
                None
            } else {
                Some(self.entries[self.index].clone())
            }
        } else {
            None
        }
    }

    /// Return a suggestion that starts with the given prefix, if any.
    fn suggest(&self, prefix: &str) -> Option<&str> {
        for entry in self.entries.iter().rev() {
            if entry.starts_with(prefix) && entry.len() > prefix.len() {
                return Some(&entry[prefix.len()..]);
            }
        }
        None
    }
}

/// Colore del suggerimento (Slate #8D96A8): il console server ignora SGR
/// `2`/`22` (dim), quindi si usa un truecolor supportato.
const HINT_COLOR: &[u8] = b"\x1b[38;2;141;150;168m";
const HINT_RESET: &[u8] = b"\x1b[0m";
/// Cifre dell'ora nel prompt: `"HH:MM:SS"`.
const CLOCK_CELLS: usize = 8;
/// Celle fra la fine dell'ora e il cursore: `" " + ✓/✗ + " " + ">" + " "`.
/// Servono al repaint in-place dell'orologio (E1, ADR-0083): il prompt non
/// si ridisegna (il gate conta i `" > "`), si muove solo il cursore fin
/// sull'ora. Il console server muove di UNA cella per `ESC[D`/`ESC[C`
/// (salta l'intero glifo se largo; qui tutto il suffisso e' da 1 cella).
const CLOCK_SUFFIX_CELLS: usize = 5;
/// Celle da tornare indietro per mettersi sulla prima cifra: ora + suffisso
/// (l'ora **resta** le sue 8 celle: il suffisso non va sovrascritto).
const CLOCK_BACK_CELLS: usize = CLOCK_CELLS + CLOCK_SUFFIX_CELLS;
const CLOCK_COLOR: &[u8] = HINT_COLOR;

/// Line editor that supports arrow keys, backspace, and history navigation.
struct LineEditor<'a> {
    buffer: String,
    history: &'a mut History,
    last_exit: u8,
    /// `true` when continuing after a `{` (multi-line block).
    continuation: bool,
    /// Celle occupate dal suggerimento attualmente disegnato (0 = nessuno).
    hint_len: usize,
    /// Ultima ora disegnata nel prompt (`HH:MM:SS`) e validita' (E1): il
    /// repaint in-place scrive solo se le cifre sono cambiate.
    clock: [u8; 8],
    clock_valid: bool,
}

impl<'a> LineEditor<'a> {
    fn new(history: &'a mut History, last_exit: u8) -> Self {
        Self {
            buffer: String::new(),
            history,
            last_exit,
            continuation: false,
            hint_len: 0,
            clock: [0u8; 8],
            clock_valid: false,
        }
    }

    /// Disegna il prompt una volta sola all'inizio della riga (o la sola
    /// indentazione in continuazione). Prima veniva ridisegnato a ogni tasto.
    fn draw_prompt(&mut self) {
        if self.continuation {
            console_write(b"  ");
            self.clock_valid = false;
        } else {
            console_write(build_prompt(self.last_exit));
            // Ricorda l'ora appena disegnata: il repaint in-place la
            // confronta e non riscrive se il secondo non e' cambiato. Se il
            // prompt e' stato disegnato senza ora (servizio Time assente)
            // `clock_valid = false` e il repaint non tocca nulla.
            let t = get_time_str();
            self.clock_valid = t.len() == CLOCK_CELLS;
            if self.clock_valid {
                self.clock.copy_from_slice(t.as_bytes());
            }
        }
        self.hint_len = 0;
    }

    /// Ripulisce in-place le cifre dell'orologio nel prompt (E1, ADR-0083).
    ///
    /// Solo a buffer vuoto (nessun tasto digitato) e fuori continuazione:
    /// il cursore e' subito dopo il prompt, quindi si torna indietro di
    /// **ora + suffisso** (13 celle), si riscrive `HH:MM:SS` con lo stesso
    /// colore e si torna avanti delle sole celle del suffisso (5): il
    /// cursore finisce esattamente dove era, le cifre scritte restano 8 e
    /// il suffisso `✓ > ` non viene toccato. Il saldo del repaint e' **zero**
    /// (con un saldo positivo la riga crescerebbe di 8 celle al secondo).
    /// Niente prompt ridisegnato: la riga e il suo `" > "` restano gli
    /// stessi (il gate della shell conta i `" > "`).
    fn refresh_clock(&mut self) {
        if self.continuation || !self.buffer.is_empty() || self.hint_len != 0 {
            return;
        }
        // Prompt senza orologio disegnato (servizio Time assente): non c'e'
        // nulla da aggiornare, e muoversi indietro corromperebbe il prompt.
        if !self.clock_valid {
            return;
        }
        let t = get_time_str();
        if t.len() != CLOCK_CELLS || &self.clock[..] == t.as_bytes() {
            return;
        }
        let mut out = Vec::with_capacity(64);
        for _ in 0..CLOCK_BACK_CELLS {
            out.extend_from_slice(b"\x1b[D");
        }
        out.extend_from_slice(CLOCK_COLOR);
        out.extend_from_slice(t.as_bytes());
        out.extend_from_slice(HINT_RESET);
        for _ in 0..CLOCK_SUFFIX_CELLS {
            out.extend_from_slice(b"\x1b[C");
        }
        console_write(&out);
        self.clock.copy_from_slice(t.as_bytes());
        self.clock_valid = true;
    }

    /// Suffisso suggerito dalla history. Mai in continuazione ne' a buffer
    /// vuoto (altrimenti al prompt comparirebbe l'intero ultimo comando).
    fn hint(&self) -> Option<&str> {
        if self.continuation || self.buffer.is_empty() {
            None
        } else {
            self.history.suggest(&self.buffer)
        }
    }

    /// Scrive il suggerimento dopo il cursore e riporta il cursore a fine
    /// buffer (`ESC[D` per glifo: il console server salta l'intero glifo
    /// largo, quindi un carattere = un movimento).
    fn push_hint(&mut self, out: &mut Vec<u8>) {
        let sug = self.hint().map(String::from);
        let Some(sug) = sug else {
            self.hint_len = 0;
            return;
        };
        out.extend_from_slice(HINT_COLOR);
        out.extend_from_slice(sug.as_bytes());
        out.extend_from_slice(HINT_RESET);
        let cells = sug.chars().count();
        for _ in 0..cells {
            out.extend_from_slice(b"\x1b[D");
        }
        self.hint_len = cells;
    }

    /// Cancella il suggerimento eventualmente a schermo (cursore a fine
    /// buffer: `ESC[K` pulisce da qui a fine riga).
    fn erase_hint(&mut self, out: &mut Vec<u8>) {
        if self.hint_len > 0 {
            out.extend_from_slice(b"\x1b[K");
            self.hint_len = 0;
        }
    }

    /// Eco incrementale: una sola `console_write` per evento (la console
    /// renderizza e flushatta una volta per scrittura, non una per tasto).
    fn echo(&mut self, bytes: &[u8]) {
        let mut out = Vec::new();
        self.erase_hint(&mut out);
        out.extend_from_slice(bytes);
        self.push_hint(&mut out);
        if !out.is_empty() {
            console_write(&out);
        }
    }

    /// Ridisegno completo: serve solo alla navigazione della history
    /// (Up/Down), dove il contenuto della riga cambia del tutto.
    fn redraw(&mut self) {
        let mut out = Vec::new();
        out.extend_from_slice(b"\r\x1b[K");
        if self.continuation {
            out.extend_from_slice(b"  ");
        } else {
            out.extend_from_slice(build_prompt(self.last_exit));
        }
        out.extend_from_slice(self.buffer.as_bytes());
        self.hint_len = 0;
        self.push_hint(&mut out);
        console_write(&out);
    }

    /// Chiude la riga: toglie il suggerimento e va a capo, senza ridisegnarne
    /// uno nuovo (il cursore e' gia' sulla riga successiva).
    fn finish_line(&mut self) {
        let mut out = Vec::new();
        self.erase_hint(&mut out);
        out.push(b'\n');
        console_write(&out);
    }

    /// Clear the current buffer (called between lines of a multi‑line block).
    fn clear_buffer(&mut self) {
        self.buffer.clear();
        self.continuation = true;
    }

    /// Read user input with editing support.
    fn read(&mut self) -> String {
        self.draw_prompt();
        let mut byte = [0u8; 1];
        loop {
            let n = civis::input::read(&mut byte);
            if n == 0 {
                // Nessun byte: si dorme su `recv` finche' non arriva un
                // evento di `porta` (E0, ADR-0082) oppure scade il prossimo
                // secondo (E1, ADR-0083) e l'orologio del prompt si aggiorna
                // in-place. Mai un risveglio a tick; la scadenza si arma solo
                // col prompt visibile e il buffer vuoto (e' li' che l'ora si
                // vede). Se la sorgente e' persa, uscita loud.
                let deadline = if !self.continuation && self.buffer.is_empty() {
                    next_clock_deadline().unwrap_or(0)
                } else {
                    0
                };
                match civis::input::wait_until(deadline) {
                    civis::input::Wait::Event => {}
                    civis::input::Wait::Timeout => self.refresh_clock(),
                    civis::input::Wait::Dead => {
                        console_write_str("[shell] ERRORE: input della tastiera perso\n");
                        civis::exit(1);
                    }
                }
                continue;
            }
            let c = byte[0];
            match c {
                0x1b => {
                    // Escape sequence: i 3 byte arrivano nello stesso lotto
                    // decodificato da porta (read non bloccante: se mancano,
                    // si scarta la sequenza, come prima).
                    let _ = civis::input::read(&mut byte);
                    let b1 = byte[0];
                    let _ = civis::input::read(&mut byte);
                    let b2 = byte[0];
                    match (b1 as char, b2 as char) {
                        ('[', 'A') => { // Up
                            if let Some(cmd) = self.history.prev() {
                                self.buffer = cmd;
                                self.redraw();
                            }
                        }
                        ('[', 'B') => { // Down
                            if let Some(cmd) = self.history.next() {
                                self.buffer = cmd;
                                self.redraw();
                            } else {
                                self.buffer.clear();
                                self.redraw();
                            }
                        }
                        _ => {}
                    }
                }
                0x08 | 0x7F => {
                    if self.buffer.pop().is_some() {
                        self.echo(b"\x08 \x08");
                    }
                }
                0x0a | 0x0d => {
                    self.finish_line();
                    break;
                }
                _ => {
                    let ch = c as char;
                    self.buffer.push(ch);
                    self.echo(&[c]);
                }
            }
        }
        let result = core::mem::take(&mut self.buffer);
        result
    }
}


use blake2s::blake2s;
use civis;
use identis_proto as proto;

// Moduli interprete (ADR-0065).
mod lexer;
mod parser;
mod env;

civis::entry!(real_main);

/// Tentativi in caso di servizio occupato (`R_BUSY`).
const RETRIES: u32 = 20;

/// Tentativi di risoluzione del servizio Identis.
const IDENTIS_RESOLVE_RETRIES: u32 = 50;

fn console_write(data: &[u8]) {
    if !civis::sess::out(data) {
        let _ = civis::print_string(data);
    }
}

fn console_write_str(s: &str) {
    console_write(s.as_bytes());
}

/// Entry: apre console, risolve l'identita' della sessione, entra nel REPL.
fn real_main(_sp: u64) -> ! {
    // READY a init: il login ha gia' fatto il suo, noi segnaliamo di essere
    // vivi (utile se init ci supervisionasse in futuro).
    civis::signal_ready(1);

    // S3.3c — l'identita' e' della **sessione** (il server la custodisce, il
    // login l'ha scritta con `SESS_AUTH_SET`), non dell'ambiente: `SESS_AUTH`
    // risolve per attestazione del pid (whoami + ancestry). `false` = sessione
    // anonima (nessun login): i comandi che ne hanno bisogno lo dicono.
    let mut session_auth = civis::sess::auth()
        .map(|a| a.is_authenticated())
        .unwrap_or(false);

    console_write_str("\n[shell] Velordo shell nativa (S3.2a)\n");
    match session_auth {
        true => console_write_str("[shell] Sessione attiva: `users` e `create-user` disponibili\n"),
        false => console_write_str("[shell] Nessuna sessione: `users` e `create-user` bloccati\n"),
    }
    console_write_str("[shell] Scrivi `help` per i comandi.\n");

    // Exit code dell'ultimo comando (0 = successo).
    let mut last_exit: u8 = 0;

    // Ambiente persistente per l'interprete: le variabili sopravvivono tra
    // una riga e l'altra del REPL.
    let mut interp_env = env::Env::new();
    let mut history = History::new();

    // Sottoscrive l'input come evento (E0, ADR-0082): il device si apre una
    // volta per tutta la sessione REPL, il focus e' LIFO in `porta`.
    if civis::input::attach().is_err() {
        console_write_str("[shell] ERRORE: impossibile aprire tastiera\n");
        civis::exit(1);
    }

    loop {
        // Read a line, supporting multi‑line blocks. L'editor e' in uno scope
        // proprio: tiene `&mut history` e va rilasciato prima del `push`.
        let mut accumulated = String::new();
        let mut brace_depth: i32 = 0;
        {
            let mut editor = LineEditor::new(&mut history, last_exit);
            loop {
                let raw_line = editor.read();
                editor.clear_buffer();
                // Append to buffer.
                accumulated.push_str(&raw_line);
                accumulated.push('\n');

                // Count braces for very simple matching.
                brace_depth += raw_line.matches('{').count() as i32 - raw_line.matches('}').count() as i32;
                if brace_depth <= 0 {
                    break;
                }
            }
        }
        let line = trim_str(&accumulated);
        if line.is_empty() {
            continue;
        }

        // History: solo comandi mono-riga (i blocchi multi-riga non sono
        // riproponibili con Up/Down ne' suggeribili).
        if !line.contains('\n') {
            history.push(String::from(line));
        }

        // ADR-0065 Fase 1+2: interprete Rust-like prima del dispatcher.
        // Se la riga è syntax-valida, viene valutata dall'interprete.
        // Se l'eval fallisce → fallback al dispatcher esistente (help, users, ecc.).
        // I diagnostici `[ast]` si stampano solo per le righe che sembrano
        // espressioni: per un comando (`dns-resolve example.com`) il fallback
        // al dispatcher e' il comportamento normale, non un errore da mostrare.
        let expr_like = looks_like_expression(line);
        let mut interpreted = false;
        match parser::parse(&line) {
            Ok(stmts) => {
                if !stmts.is_empty() {
                    // Valuta con l'interprete. Se fallisce, è probabilmente un
                    // comando shell (es. `help`) → fallback al dispatcher.
                    match env::evaluate(&mut interp_env, &stmts) {
                        Ok(Some(val)) => {
                            console_write_str(&format!("{}", val));
                            console_write_str("\n");
                            interpreted = true;
                        }
                        Ok(None) => {
                            // Statement senza valore (es. `let a = 1`).
                            interpreted = true;
                        }
                        Err(e) => {
                            if expr_like {
                                console_write_str("[ast] skip: ");
                                console_write_str(&format!("{}", e));
                                console_write_str("\n");
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // Parsing fallito: non è syntax Rust-like, fallback dispatcher.
                if expr_like {
                    console_write_str("[ast] skip: ");
                    console_write_str(&format!("{}", e));
                    console_write_str("\n");
                }
            }
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts.first().copied().unwrap_or("");
        let args: Vec<&str> = parts[1..].to_vec();

        // Un comando che richiede identita' (create-user, consent) e'
        // rifiutato da Identis/Custos se il chiamante non e' autenticato
        // (nessun token: il pid lo da' il kernel). Il loop puo' chiedere di
        // (ri)autenticarsi.
        last_exit = 0; // reset su successo (default)
        if interpreted {
            // L'interprete ha gestito la riga — nessun comando built-in.
        } else {
            match execute_command(cmd, &args, session_auth) {
                CmdResult::Ok => { last_exit = 0; }
                CmdResult::NeedAuth => {
                    console_write_str("Identita' richiesta: (ri)autenticati\n");
                    match authenticate() {
                        Ok(()) => {
                            session_auth = true;
                            // L'identita' e' ora del pid (VERIFY l'ha legata
                            // al chiamante): nessun valore viaggia.
                        }
                        Err(()) => { last_exit = 1; } // auth failed
                    }
                }
            }
        }
    }
}

/// True se la riga ha la forma di un'espressione/stato dell'interprete
/// (ADR-0065) e non di un comando della shell. Serve solo a decidere se i
/// diagnostici `[ast]` vanno mostrati: il fallback al dispatcher e' normale.
fn looks_like_expression(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("let ")
        || t.starts_with("if ")
        || t.starts_with("while ")
        || t.starts_with("dump ")
        || t.starts_with("type ")
    {
        return true;
    }
    // Assegnamento o chiamata: `x = ...`, `f(...)`.
    line.contains('=') || line.contains('(')
}

/// Risultato di un comando: successo o identita' richiesta (retryable).
enum CmdResult {
    Ok,
    NeedAuth,
}

/// Esegue un comando con l'identita' del chiamante (attestata dal kernel).
/// `NeedAuth` = il chiamante non ha un'identita' attestata (nessun VERIFY
/// riuscito): il chiamante puo' (ri)autenticarsi e ritentare.
fn execute_command(cmd: &str, args: &[&str], session_auth: bool) -> CmdResult {
    match cmd {
        "help" => { cmd_help(); CmdResult::Ok }
        "exit" => {
            console_write_str("logout\n");
            civis::exit(0);
        }
        "create-user" => {
            if !session_auth {
                return CmdResult::NeedAuth;
            }
            let name = args.first().copied();
            let roles = args.get(1).copied();
            cmd_create_user(name, roles)
        }
        "consent" => {
            if !session_auth {
                return CmdResult::NeedAuth;
            }
            cmd_consent(&args)
        }
        "logout" => {
            // Il programma esterno stampa il messaggio; il bounce lo fa la
            // shell, che e' figlia di init (`INIT_BOUNCE` va sul canale di
            // nascita: per un nipote sarebbe la shell, non init). Dopo il
            // bounce la shell esce: altrimenti resterebbe un secondo lettore
            // della tastiera accanto al login riavviato.
            try_execute_external("logout", args);
            let _ = civis::init_bounce(civis::Service::Login);
            civis::exit(0);
        }
        _ => {
            // Non e' un comando built-in: prova a eseguirlo come programma esterno.
            if !try_execute_external(cmd, args) {
                console_write_str("Comando non trovato: ");
                console_write_str(cmd);
                console_write_str("\n");
            }
            CmdResult::Ok
        }
    }
}

/// Mostra i comandi disponibili.
fn cmd_help() {
    console_write_str("Comandi disponibili:\n");
    console_write_str("  help              - mostra questa guida\n");
    console_write_str("  create-user [nome] [ruoli] - crea un utente (ruoli: root|net-admin|hw-admin|user)\n");
    console_write_str("  consent <prog> on|off - consenso rete per un programma (ADR-0080)\n");
    console_write_str("  users             - elenca gli utenti registrati\n");
    console_write_str("  exit              - chiudi la shell\n");
    console_write_str("  logout            - torna al prompt di login\n");
    console_write_str("  <programma>       - esegue un programma da un bucket eseguibile\n");
}

/// Crea un nuovo utente (ruoli opzionali: `create-user alice net-admin,user`).
/// L'identita' e' quella del chiamante (attestata dal kernel): nessun token
/// viaggia, Identis risolve il pid.
fn cmd_create_user(
    name_arg: Option<&str>,
    roles_arg: Option<&str>,
) -> CmdResult {
    // Nome utente: da arg o chiesto alla tastiera.
    let name = match name_arg {
        Some(n) if !n.is_empty() => String::from(n),
        _ => {
            console_write_str("Nome utente: ");
            let n = read_line_from_keyboard(false);
            console_write_str("\n");
            let n = trim_str(&n);
            if n.is_empty() {
                console_write_str("ERRORE: nome vuoto\n");
                return CmdResult::Ok;
            }
            String::from(n)
        }
    };

    // Validazione anticipata (fail-fast lato client).
    if !proto::valid_name(&name) {
        console_write_str("ERRORE: nome non valido (solo [a-z0-9_-], max 32 caratteri)\n");
        return CmdResult::Ok;
    }

    // Ruoli (ADR-0080, fase 5): default `user`.
    let roles = match roles_arg {
        Some(spec) => match proto::parse_roles(spec) {
            Some(r) => r,
            None => {
                console_write_str("ERRORE: ruoli non validi (root|net-admin|hw-admin|user, virgola)\n");
                return CmdResult::Ok;
            }
        },
        None => proto::ROLE_USER,
    };

    // Password (con conferma).
    console_write_str("Password: ");
    let pwd = read_line_from_keyboard(true);
    console_write_str("\n");
    console_write_str("Conferma: ");
    let pwd2 = read_line_from_keyboard(true);
    console_write_str("\n");
    if pwd != pwd2 {
        console_write_str("ERRORE: le password non coincidono\n");
        return CmdResult::Ok;
    }
    if pwd.is_empty() {
        console_write_str("ERRORE: password vuota\n");
        return CmdResult::Ok;
    }

    // L'hash si calcola qui: la password non lascia mai il processo.
    let hash = blake2s(pwd.as_bytes());
    let payload = proto::build_add_user(&name, &hash, roles);

    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write_str("ERRORE: servizio Identis non disponibile\n");
            return CmdResult::Ok;
        }
    };

    match proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES) {
        Ok(r) if r.ok() => {
            console_write_str("utente '");
            console_write_str(&name);
            console_write_str("' creato\n");
        }
        Ok(r) if r.w0 == proto::R_NO_IDENTITY => return CmdResult::NeedAuth,
        Ok(r) => {
            console_write_str("ERRORE: ");
            console_write_str(r.describe());
            console_write_str("\n");
        }
        Err(_) => console_write_str("ERRORE IPC\n"),
    }
    CmdResult::Ok
}

/// `consent <prog> on|off` (ADR-0080, fase 6): registra il consenso di rete
/// dell'utente corrente per un'**immagine** (il contenuto del binario, non il
/// nome). Il consenso si somma al grant admin (`sys:caps`): senza uno dei due
/// il broker nega le destinazioni globali. L'identita' e' quella del
/// chiamante (attestata dal kernel via `SESS_AUTH`): nessun token viaggia.
fn cmd_consent(args: &[&str]) -> CmdResult {
    let Some(prog) = args.first().copied() else {
        console_write_str("uso: consent <prog> on|off\n");
        return CmdResult::Ok;
    };
    let decision = match args.get(1).copied() {
        Some("on") => true,
        Some("off") => false,
        _ => {
            console_write_str("uso: consent <prog> on|off\n");
            return CmdResult::Ok;
        }
    };
    // L'identita' e' quella dei byte: si legge l'oggetto e si misura.
    let Some(img) = load_program(prog) else {
        console_write_str("ERRORE: programma non trovato: ");
        console_write_str(prog);
        console_write_str("\n");
        return CmdResult::Ok;
    };
    let id = civis::image_id(&img);
    let chan = match resolve_custos() {
        Ok(c) => c,
        Err(_) => {
            console_write_str("ERRORE: servizio Custos non disponibile\n");
            return CmdResult::Ok;
        }
    };
    let user = read_user();
    if custos_proto::consent_write(chan, user, id.lo, id.hi, decision) {
        console_write_str("consenso ");
        console_write_str(if decision { "ON " } else { "OFF " });
        console_write_str(prog);
        console_write_str(" (");
        console_write_str(&alloc::format!("{:016x}{:016x}", id.hi, id.lo));
        console_write_str(")\n");
    } else {
        console_write_str("ERRORE: consenso rifiutato (identita' non valida?)\n");
        return CmdResult::NeedAuth;
    }
    CmdResult::Ok
}

/// `true` se l'immagine `id` non ha una riga di consenso allow
/// (`usr:consent`). Fail-closed: se Custos non risponde, si avvisa (il broker
/// neghera' comunque; l'avviso e' un servizio all'utente, non una decisione).
fn consent_missing(id: civis::ImageId) -> bool {
    let Ok(chan) = resolve_custos() else {
        return true;
    };
    match custos_proto::consent_read(chan, read_user(), id.lo, id.hi) {
        Some(true) => false,
        _ => true,
    }
}

/// Carica i byte del programma `prog` dai bucket eseguibili (stessa ricerca
/// piatta di `try_execute_external`).
fn load_program(prog: &str) -> Option<Vec<u8>> {
    if prog.is_empty() || prog.contains('/') {
        return None;
    }
    let buckets = civis::ipc::bin_list().ok()?;
    for b in &buckets {
        if let Ok(data) = civis::fs::obj::obj_get(b.as_slice(), prog.as_bytes()) {
            return Some(data);
        }
    }
    None
}

/// Risolve il servizio Custos per nome (con retry breve).
fn resolve_custos() -> Result<u64, &'static str> {
    for _ in 0..IDENTIS_RESOLVE_RETRIES {
        match civis::service_lookup(civis::Service::Custos) {
            Ok(c) if c > 0 => return Ok(c as u64),
            _ => core::hint::spin_loop(),
        }
    }
    Err("Custos non trovato")
}
/// S3.2b — Tenta di eseguire un programma esterno da un bucket `binary`.
/// La shell scopre i bucket tramite BIN_LIST, cerca il nome PIATTO
/// `<programma>` (i bucket binary non hanno sottocartelle, ADR-0064),
/// verifica EXECUTE con cardo (R_RIGHTS_EXECUTE) e lo esegue con spawn_image.
/// Ritorna `true` se il programma e' stato trovato (eseguito o rifiutato con
/// un errore specifico), `false` se non esiste in nessun bucket binary.
fn try_execute_external(program: &str, args: &[&str]) -> bool {
    // 1. Richiedi la lista dei bucket eseguibili a cardo (BIN_LIST).
    let buckets = match civis::ipc::bin_list() {
        Ok(b) => b,
        Err(_) => {
            console_write_str("ERRORE: impossibile ottenere la lista dei bucket\n");
            return false;
        }
    };

    if buckets.is_empty() {
        console_write_str("Nessun bucket eseguibile disponibile\n");
        return false;
    }

    // I bucket binary sono piatti: la chiave e' il nome del programma, senza
    // path ne' suffisso `.bin`. Un nome con '/' non e' un comando binary.
    if program.is_empty() || program.contains('/') {
        return false;
    }
    let key_bytes = program.as_bytes();

    // 2. Itera i bucket binary e cerca la chiave piatta.
    for bucket in &buckets {
        let bucket_str = core::str::from_utf8(bucket).unwrap_or("");

        // 3. Prova a leggere l'oggetto dal bucket (obj_get).
        let obj = match civis::fs::obj::obj_get(bucket.as_slice(), key_bytes) {
            Ok(data) => data,
            Err(_) => continue, // Non trovato in questo bucket, prova il prossimo.
        };

        console_write_str("[run] Esecuzione di '");
        console_write_str(program);
        console_write_str("' dal bucket '");
        console_write_str(bucket_str);
        console_write_str("' (");
        console_write_str(&alloc::format!("{}", obj.len()));
        console_write_str(" byte)\n");

        // 4. Verifica EXECUTE con cardo (R_RIGHTS_EXECUTE) tramite FS protocol.
        match civis::fs::rights::rights_execute(bucket.as_slice(), key_bytes) {
            Ok(true) => {} // Autorizzato, procedi
            Ok(false) => {
                console_write_str("ERRORE: EXECUTE negato per '");
                console_write_str(program);
                console_write_str("'\n");
                return true;
            }
            Err(e) => {
                console_write_str("ERRORE rights_execute: ");
                console_write_str(&alloc::format!("{:?}", e));
                console_write_str("\n");
                return true;
            }
        }

        // 5. Consenso di rete (ADR-0080, fase 6): se l'immagine non ha una
        // riga in `usr:consent`, il programma parte lo stesso ma il broker
        // neghera' le destinazioni globali. Il messaggio e' **non
        // bloccante**: la shell non puo' sapere se il programma usa la rete,
        // e un prompt su ogni lancio (anche `users`/`logout`) romperebbe le
        // esecuzioni non interattive. Il dialogo s/n all'avvio arriva con il
        // DE (fase 7).
        let id = civis::image_id(&obj);
        if consent_missing(id) {
            console_write_str("[shell] avviso: nessun consenso di rete per '");
            console_write_str(program);
            console_write_str("' (usa `consent ");
            console_write_str(program);
            console_write_str(" on`)\n");
        }

        // 6. Esegui l'immagine con spawn_image, passando argv. Niente env: il
        // token non viaggia piu' qui (D8f) — i figli lo leggono dalla
        // **sessione** (`SESS_AUTH` + ancestry), che il kernel attesta.
        let mut argv: Vec<&[u8]> = Vec::with_capacity(args.len() + 1);
        argv.push(key_bytes);
        for a in args {
            argv.push(a.as_bytes());
        }
        let env: Vec<&[u8]> = Vec::new();
        let packed = civis::pack_args(&argv, &env);
        let mut meta = match civis::SpawnMeta::new(program, 16, &[]) {
            Some(m) => m,
            None => {
                console_write_str("ERRORE: nome programma non valido\n");
                return true;
            }
        };
        // S3.4b — i figli della shell non possono essere piu' privilegiati dei
        // programmi utente: floor `ThirdParty`. Un'immagine non firmata viene
        // rifiutata dal **kernel** (non da un check facoltativo qui), e un
        // `exec` verso `System` non eleva.
        meta = meta.floor(civis::ImageClass::ThirdParty);
        if let Some(ref p) = packed {
            meta = meta.argv(p);
        }
        match civis::spawn_image(&obj, &meta) {
            Ok(chan) => {
                console_write_str("[run] Processo avviato (canale ");
                console_write_str(&alloc::format!("{}", chan));
                console_write_str(")\n");
                // Attendi la terminazione del figlio: esci solo su
                // EXIT_NOTIFY (un messaggio dati del figlio non e' una
                // terminazione).
                loop {
                    match civis::recv() {
                        Ok(msg) => {
                            if msg.channel as i64 == chan
                                && msg.tag == civis::EXIT_NOTIFY
                            {
                                console_write_str("[run] Processo terminato\n");
                                return true;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(e) => {
                console_write_str("ERRORE spawn_image: ");
                console_write_str(&alloc::format!("{:?}", e));
                console_write_str("\n");
            }
        }

        // Solo il primo match valido viene eseguito.
        return true;
    }

    // Non trovato in nessun bucket.
    false
}

/// Autenticazione completa: chiede nome+password, calcola hash BLAKE2s in
/// locale, invia IDENTIS_VERIFY a Identis. Se riuscito, Identis lega il pid
/// chiamante all'utente (credenziale locale): nessun token restituito, nessun
/// valore che viaggi. Ritorna `Ok(())` se l'autenticazione e' riuscita,
/// `Err(())` altrimenti.
fn authenticate() -> Result<(), ()> {
    console_write_str("Nome utente: ");
    let name = read_line_from_keyboard(false);
    console_write_str("\n");
    let name = trim_str(&name);
    if name.is_empty() {
        console_write_str("ERRORE: nome vuoto\n");
        return Err(());
    }

    console_write_str("Password: ");
    let pwd = read_line_from_keyboard(true);
    console_write_str("\n");

    // Calcolo hash BLAKE2s in locale (la password non transita mai in chiaro).
    let hash = blake2s(pwd.as_bytes());
    let payload = proto::build_verify(name, &hash);

    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write_str("ERRORE: servizio Identis non disponibile\n");
            return Err(());
        }
    };

    match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) if r.ok() => {
            console_write_str("Autenticazione riuscita\n");
            Ok(())
        }
        Ok(r) => {
            console_write_str("Autenticazione fallita: ");
            console_write_str(r.describe());
            console_write_str("\n");
            Err(())
        }
        Err(_) => {
            console_write_str("ERRORE IPC\n");
            Err(())
        }
    }
}

/// L'utente del terminale: l'**identità della sessione**, chiesta al server
/// (S3.3d, ADR-0095). Non c'è nessuna variabile d'ambiente da leggere né da
/// fidarsi: il login ha scritto il nome sulla sessione (`SESS_AUTH_SET`) e il
/// kernel dice di che pid siamo. `""` = sessione anonima (nessun login) o
/// server non raggiungibile: il prompt resta senza nome, l'output non si perde.
///
/// Si chiede **a ogni prompt** (una IPC in più per riga): il nome è un fatto
/// del server, non una copia da tenere aggiornata.
fn read_user() -> &'static str {
    static mut USER: [u8; 33] = [0u8; 33];
    static mut LEN: usize = 0;

    let mut auth = civis::sess::auth();
    if auth.is_err() {
        // Cache stantia (la console è ripartita): una risoluzione, un tentativo.
        civis::sess::forget_current();
        auth = civis::sess::auth();
    }
    let name = auth.ok().filter(|a| a.is_authenticated());

    unsafe {
        match name {
            Some(a) => {
                let bytes = a.as_bytes();
                let n = bytes.len().min(32);
                USER[..n].copy_from_slice(&bytes[..n]);
                USER[n] = 0;
                LEN = n;
            }
            None => LEN = 0,
        }
        if LEN == 0 {
            ""
        } else {
            core::str::from_utf8(&USER[..LEN]).unwrap_or("")
        }
    }
}

/// Restituisce l'ora corrente come `HH:MM:SS` (24h), esattamente 8 byte: il
/// prompt e il repaint in-place (E1) si basano su questa lunghezza (il vecchio
/// `\0` finale finiva nella cella dopo l'ora). Richiede il servizio Time.
fn get_time_str() -> &'static str {
    static mut BUF: [u8; 8] = [0u8; 8]; // "HH:MM:SS"

    let chan = match civis::service_lookup(civis::Service::Time) {
        Ok(c) => c as u64,
        Err(_) => return "",
    };
    let r = civis::ipc::send(chan, civis::TIME_NOW, 0, 0);
    if let Ok(rep) = r {
        let sec = rep.w0 % 86400;
        let hour = (sec / 3600) as u8;
        let min = ((sec % 3600) / 60) as u8;
        let s = (sec % 60) as u8;
        unsafe {
            BUF[0] = b'0' + hour / 10;
            BUF[1] = b'0' + hour % 10;
            BUF[2] = b':';
            BUF[3] = b'0' + min / 10;
            BUF[4] = b'0' + min % 10;
            BUF[5] = b':';
            BUF[6] = b'0' + s / 10;
            BUF[7] = b'0' + s % 10;
        }
    } else {
        return "";
    }

    // safe: BUF e' stato scritto in modo completo da questo stesso processo,
    // nessun altro lo modifica.
    unsafe { core::str::from_utf8(&*(&raw const BUF)).unwrap_or("") }
}

/// Deadline (tick assoluto) del prossimo confine di secondo, per aggiornare
/// l'orologio del prompt mentre la shell dorme (E1, ADR-0083). `None` se il
/// servizio Time non risponde: la shell ricade sull'attesa senza scadenza
/// (un orologio fermo non deve fermare la shell).
fn next_clock_deadline() -> Option<u64> {
    let (_, cents) = civis::time::time_now().ok()?;
    Some((civis::get_ticks() + (100 - cents.min(99) as i64)).max(0) as u64)
}

/// Costruisce il prompt con glifi Nerd Font e colori truecolor.
/// Format: `⚡ root@velordo ⏰ 14:32:07 ✓ > `
///   ⚡ + user@hostname → Electric Blue (#5B7CFF)
///   ⏰ + time          → Slate (#8D96A8)
///   ✓/✗ + >            → Ice (#E8EBF2, default)
fn build_prompt(last_exit: u8) -> &'static [u8] {
    static mut BUF: [u8; 256] = [0u8; 256];
    static mut LEN: usize = 0;

    let user = read_user();
    let time_str = get_time_str();

    // Status glifo: ✓ se ultimo comando OK, ✗ altrimenti.
    let status_gliph = if last_exit == 0 { "✓" } else { "✗" };

    // ANSI truecolor escape codes (RGB 24-bit).
    // Electric Blue #5B7CFF → RGB(91, 124, 255)
    const BLUE: &[u8] = b"\x1b[38;2;91;124;255m";
    // Slate #8D96A8 → RGB(141, 150, 168)
    const SLATE: &[u8] = b"\x1b[38;2;141;150;168m";
    // Reset
    const RESET: &[u8] = b"\x1b[0m";

    unsafe {
        let mut off = 0usize;

        // Colore Electric Blue per ⚡ + user@hostname
        BUF[off..off+BLUE.len()].copy_from_slice(BLUE);
        off += BLUE.len();

        // ⚡ (HIGH VOLTAGE)
        let bolt: &[u8] = b"\xe2\x9a\xa1";
        BUF[off..off+bolt.len()].copy_from_slice(bolt);
        off += bolt.len();

        // spazio + user
        BUF[off] = b' ';
        off += 1;
        let user_bytes = user.as_bytes();
        BUF[off..off+user_bytes.len()].copy_from_slice(user_bytes);
        off += user_bytes.len();

        // @hostname
        let host = b"@velordo";
        BUF[off..off+host.len()].copy_from_slice(host);
        off += host.len();

        // Colore Slate per ⏰ + time
        BUF[off..off+SLATE.len()].copy_from_slice(SLATE);
        off += SLATE.len();

        // spazio + ⏰ (U+23F0 BLACK CLOCK FACE)
        let space = b" ";
        BUF[off..off+space.len()].copy_from_slice(space);
        off += space.len();

        // ⏰ (U+F520 CLOCK — Nerd Font icon)
        let clock: &[u8] = b"\xef\x94\xA0";
        BUF[off..off+clock.len()].copy_from_slice(clock);
        off += clock.len();

        // spazio + time
        BUF[off..off+space.len()].copy_from_slice(space);
        off += space.len();
        BUF[off..off+time_str.len()].copy_from_slice(time_str.as_bytes());
        off += time_str.len();

        // Reset al colore Ice (default)
        BUF[off..off+RESET.len()].copy_from_slice(RESET);
        off += RESET.len();

        // spazio + status glifo
        let sp = b" ";
        BUF[off..off+sp.len()].copy_from_slice(sp);
        off += sp.len();
        let status_bytes = status_gliph.as_bytes();
        BUF[off..off+status_bytes.len()].copy_from_slice(status_bytes);
        off += status_bytes.len();

        // spazio + >
        BUF[off] = b' ';
        off += 1;
        BUF[off] = b'>';
        off += 1;
        BUF[off] = b' ';
        off += 1;

        LEN = off;
    }

    unsafe { &BUF[..LEN] }
}

/// Rimuove spazi bianchi iniziali e finali (trim minimale, no alloc).
fn trim_str(s: &str) -> &str {
    let start = s.bytes().position(|b| !b.is_ascii_whitespace()).unwrap_or(0);
    let end = s
        .bytes()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|p| p + 1)
        .unwrap_or(0);
    if end <= start { "" } else { &s[start..end] }
}

/// Legge una riga terminata da newline dalla tastiera. Con `mask` l'eco e'
/// mascherato (password: niente in chiaro). La sottoscrizione e' quella del
/// processo (`civis::input::attach` in `real_main`, idempotente): qui si
/// legge/drena soltanto, senza aprire un secondo fd ne' pollare a tick.
fn read_line_from_keyboard(mask: bool) -> String {
    if civis::input::attach().is_err() {
        console_write_str("[shell] ERRORE: impossibile aprire tastiera\n");
        civis::exit(1);
    }

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let n = civis::input::read(&mut byte);
        if n == 0 {
            // Niente di nuovo: dorme su un evento di porta (mai spin/tick).
            if !civis::input::wait() {
                console_write_str("[shell] ERRORE: input della tastiera perso\n");
                civis::exit(1);
            }
            continue;
        }
        let b = byte[0];
        if b == b'\n' || b == b'\r' {
            break;
        }
        if b == 0x7F || b == 0x08 {
            if !buf.is_empty() {
                buf.pop();
                console_write(b"\x08 \x08");
            }
            continue;
        }
        buf.push(b);
        if mask {
            console_write(b"*");
        } else {
            console_write(&[b]);
        }
    }

    String::from_utf8(buf).unwrap_or_default()
}

/// Risolve il servizio Identis per nome (con retry).
fn resolve_identis() -> Result<u64, &'static str> {
    for _ in 0..IDENTIS_RESOLVE_RETRIES {
        match civis::service_lookup(civis::Service::Identis) {
            Ok(c) => return Ok(c as u64),
            Err(_) => {
                core::hint::spin_loop();
            }
        }
    }
    Err("Identis non trovato")
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[shell] panic\n");
    civis::exit(1)
}
