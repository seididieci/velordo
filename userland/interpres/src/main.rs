//! Shell nativa Velordo (Fase 3.2, S3.2a + S3.2b): REPL minima post-login.
//!
//! Programma interattivo **nativo** (meccanismo, niente POSIX) che gira su
//! `/dev/input/keyboard` + `/dev/console`. Non usa `cd`, `stdin`, `stdout`,
//! `fork`, pipe Unix, `$VAR` come primitive speciali: e' un dispatcher di
//! comandi su risorse/capability.
//!
//! Comandi S3.2a:
//!   `help`           mostra i comandi disponibili
//!   `users`          elenca gli utenti registrati (via Identis)
//!   `create-user [nome]`  crea un utente (token VELORDO_SESSION richiesto)
//!   `logout` / `exit`   torna al prompt di login (init riavvia il login come getty)
//!
//! Comandi S3.2b — esecuzione esterna da bucket `binary`:
//!   `<programma>`  lancia un programma da un bucket eseguibile.
//!                  La shell scopre i bucket tramite BIN_LIST, cerca il nome
//!                  piatto `<programma>` (niente sottocartelle, ADR-0064),
//!                  verifica EXECUTE con cardo, e lo esegue con spawn_image.
//!
//! Se l'output di un comando non viene catturato in una variabile, il valore
//! (futuro, S3.2b+) viene stampato serializzato.
//!
//! Output su /dev/console (VGA + serial), input da /dev/input/keyboard.
//! La password non transita mai in chiaro: hash BLAKE2s-256 in locale.
//!
//! **Refresh del token**: quando un comando autenticato riceve `R_TOKEN_INVALID`
//! (scadenza TTL 300s), la shell richiede nuovamente nome+password e ottiene
//! un nuovo token da Identis — l'utente non perde mai il contesto.
//!
//! **Fase X (ADR-0065)**: interprete Rust-like con `let`, assegnamento, blocchi,
//! `if/else`, operatori binari, chiamate funzione e builtin (`dump`, `type`).

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use blake2s::blake2s;
use civis;
use civis::println;
use identis_proto as proto;

// Moduli interprete (ADR-0065).
mod lexer;
mod parser;
mod env;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";
const KEYBOARD_PATH: &str = "/dev/input/keyboard";
const IDENTIS_BUCKET: &str = "identis:config";

/// Variabile d'ambiente con il token di sessione (hex).
const ENV_SESSION: &str = "VELORDO_SESSION";

/// Variabile d'ambiente con il nome utente.
const ENV_USER: &str = "USER";

/// Hostname fisso (per ora).
const HOSTNAME: &str = "velordo";

/// Tentativi in caso di servizio occupato (`R_BUSY`).
const RETRIES: u32 = 20;

/// Tentativi di risoluzione del servizio Identis.
const IDENTIS_RESOLVE_RETRIES: u32 = 50;

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

/// Stack pointer iniziale (per leggere VELORDO_USER dall'env).
static mut SHELL_SP: u64 = 0;

fn console_write(data: &[u8]) {
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    civis::flush();
}

fn console_write_str(s: &str) {
    console_write(s.as_bytes());
}

/// Entry: apre console, legge il token di sessione, entra nel loop REPL.
fn real_main(_sp: u64) -> ! {
    let console_fd = match civis::open(CONSOLE_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[shell] ERRORE: impossibile aprire console");
            civis::exit(1);
        }
    };
    unsafe { CONSOLE_FD = console_fd; }

    // Salva lo stack pointer per la lettura dell'env (VELORDO_USER).
    unsafe { SHELL_SP = _sp; }

    // READY a init: il login ha gia' fatto il suo, noi segnaliamo di essere
    // vivi (utile se init ci supervisionasse in futuro).
    civis::signal_ready(1);

    // Token di sessione dall'ambiente (passato dal login).
    let mut session_token = read_session_token(_sp);

    console_write_str("\n[shell] Velordo shell nativa (S3.2a)\n");
    match session_token {
        Some(_) => console_write_str("[shell] Sessione attiva: `users` e `create-user` disponibili\n"),
        None => console_write_str("[shell] Nessuna sessione: `users` e `create-user` bloccati\n"),
    }
    console_write_str("[shell] Scrivi `help` per i comandi.\n");

    // Exit code dell'ultimo comando (0 = successo).
    let mut last_exit: u8 = 0;

    loop {
        // Costruisci il prompt con glifi Nerd Font.
        console_write(build_prompt(last_exit));
        let line = read_line_from_keyboard(false);
        let line = trim_str(&line);
        console_write_str("\n");
        if line.is_empty() {
            continue;
        }

        // ADR-0065 Fase 1+2: interprete Rust-like prima del dispatcher.
        // Se la riga è syntax-valida, viene valutata dall'interprete.
        // Se l'eval fallisce → fallback al dispatcher esistente (help, users, ecc.).
        let mut interpreted = false;
        match parser::parse(&line) {
            Ok(stmts) => {
                if !stmts.is_empty() {
                    // Valuta con l'interprete. Se fallisce, è probabilmente un
                    // comando shell (es. `help`) → fallback al dispatcher.
                    match env::evaluate(&stmts) {
                        Ok(_) => { interpreted = true; }
                        Err(e) => {
                            console_write_str("[ast] skip: ");
                            console_write_str(&format!("{}", e));
                            console_write_str("\n");
                        }
                    }
                }
            }
            Err(e) => {
                // Parsing fallito: non è syntax Rust-like, fallback dispatcher.
                console_write_str("[ast] skip: ");
                console_write_str(&format!("{}", e));
                console_write_str("\n");
            }
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts.first().copied().unwrap_or("");
        let args: Vec<&str> = parts[1..].to_vec();

        // Se un comando autenticato fallisce per token scaduto, il loop
        // chiede all'utente di reinserire le credenziali e poi ritenta.
        last_exit = 0; // reset su successo (default)
        if interpreted {
            // L'interprete ha gestito la riga — nessun comando built-in.
        } else {
            match execute_command(cmd, &args, session_token) {
                CmdResult::Ok => { last_exit = 0; }
                CmdResult::TokenExpired => {
                    console_write_str("Token scaduto, reinserire le credenziali\n");
                    match auth_and_get_token() {
                        Some(t) => { session_token = Some(t); }
                        None => { last_exit = 1; } // auth failed
                    }
                }
            }
        }
    }
}

/// Risultato di un comando: successo o token scaduto (retryable).
enum CmdResult {
    Ok,
    TokenExpired,
}

/// Esegue un comando con il token corrente. `TokenExpired` = Identis ha rifiutato
/// per scadenza sessione — il chiamante può aggiornare il token e ritentare.
fn execute_command(cmd: &str, args: &[&str], session_token: Option<u64>) -> CmdResult {
    match cmd {
        "help" => { cmd_help(); CmdResult::Ok }
        "users" => cmd_users(session_token),
        "create-user" => {
            let name = args.first().copied();
            cmd_create_user(session_token, name)
        }
        "logout" | "exit" => cmd_logout(),
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
    console_write_str("  users             - elenca gli utenti registrati\n");
    console_write_str("  create-user [nome]- crea un nuovo utente (richiede sessione root)\n");
    console_write_str("  logout / exit     - torna al prompt di login\n");
    console_write_str("  <programma>       - esegue un programma da un bucket eseguibile\n");
}

/// Elenca gli utenti registrati.
fn cmd_users(session_token: Option<u64>) -> CmdResult {
    let token = match session_token {
        Some(t) => t,
        None => {
            console_write_str("ERRORE: nessuna sessione attiva (login richiesto)\n");
            return CmdResult::Ok;
        }
    };

    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write_str("ERRORE: servizio Identis non disponibile\n");
            return CmdResult::Ok;
        }
    };

    // LIST: w0 = 0 (nessun payload), w1 = token. Reply: w0 = codice, w1 = conteggio.
    match civis::ipc::send(chan, civis::IDENTIS_LIST, 0, token) {
        Ok(r) if r.w0 == proto::R_OK => {
            let n = r.w1 as usize;
            console_write_str("Utenti registrati (");
            console_write_str(&format!("{}", n));
            console_write_str("):\n");
            // Legge i nomi direttamente dal bucket (Identis espone solo il conteggio).
            print_users_from_bucket();
        }
        Ok(r) if r.w0 == proto::R_TOKEN_INVALID => return CmdResult::TokenExpired,
        Ok(r) => {
            console_write_str("ERRORE LIST: ");
            match r.w0 {
                proto::R_BUSY => console_write_str("servizio occupato\n"),
                _ => console_write_str("errore di protocollo\n"),
            }
        }
        Err(_) => console_write_str("ERRORE IPC\n"),
    }
    CmdResult::Ok
}

/// Legge i nomi degli utenti dal bucket `identis:config` e li stampa.
fn print_users_from_bucket() {
    let fd = match civis::open(IDENTIS_BUCKET, 0) {
        Ok(f) => f,
        Err(_) => {
            console_write_str("ERRORE: impossibile leggere il bucket utenti\n");
            return;
        }
    };

    let mut buf = [0u8; 4096];
    let mut out = String::new();
    loop {
        let cap = buf.len();
        let n = match civis::read_fs(fd, &mut buf, cap) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        out.push_str(core::str::from_utf8(&buf[..n]).unwrap_or(""));
    }
    let _ = civis::close(fd);

    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ':');
        if let Some(name) = parts.next() {
            console_write_str("  ");
            console_write_str(name);
            console_write_str("\n");
        }
    }
}

/// Crea un nuovo utente.
fn cmd_create_user(session_token: Option<u64>, name_arg: Option<&str>) -> CmdResult {
    let token = match session_token {
        Some(t) => t,
        None => {
            console_write_str("ERRORE: nessuna sessione attiva (login richiesto)\n");
            return CmdResult::Ok;
        }
    };

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
    let payload = proto::build_add_user(token, &name, &hash);

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
        Ok(r) if r.w0 == proto::R_TOKEN_INVALID => return CmdResult::TokenExpired,
        Ok(r) => {
            console_write_str("ERRORE: ");
            console_write_str(r.describe());
            console_write_str("\n");
        }
        Err(_) => console_write_str("ERRORE IPC\n"),
    }
    CmdResult::Ok
}

/// Torna al prompt di login: chiede a init di riavviare il login (getty),
/// poi esce. La shell e' figlia di init (detached dal login al boot), quindi
/// `init_bounce` e' lecito.
fn cmd_logout() -> ! {
    console_write_str("logout: riavvio del login...\n");
    let _ = civis::init_bounce(civis::Service::Login);
    civis::exit(0);
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

        // 5. Esegui l'immagine con spawn_image, passando argv (argv[0] =
        // nome del programma + argomenti digitati). Il blocco packed deve
        // restare vivo fino a dopo la syscall (il kernel lo copia).
        let mut argv: Vec<&[u8]> = Vec::with_capacity(args.len() + 1);
        argv.push(key_bytes);
        for a in args {
            argv.push(a.as_bytes());
        }
        let packed = civis::pack_args(&argv, &[]);
        let mut meta = match civis::SpawnMeta::new(program, 16, &[]) {
            Some(m) => m,
            None => {
                console_write_str("ERRORE: nome programma non valido\n");
                return true;
            }
        };
        if let Some(ref p) = packed {
            meta = meta.argv(p);
        }
        match civis::spawn_image(&obj, &meta) {
            Ok(chan) => {
                console_write_str("[run] Processo avviato (canale ");
                console_write_str(&alloc::format!("{}", chan));
                console_write_str(")\n");
                // Attendi la terminazione del figlio leggendo dal canale di nascita.
                loop {
                    match civis::recv() {
                        Ok(msg) => {
                            if msg.channel as i64 == chan {
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
/// locale, invia IDENTIS_VERIFY a Identis. Ritorna il nuovo token (u64) o
/// `None` se l'autenticazione e' fallita.
fn auth_and_get_token() -> Option<u64> {
    console_write_str("Nome utente: ");
    let name = read_line_from_keyboard(false);
    console_write_str("\n");
    let name = trim_str(&name);
    if name.is_empty() {
        console_write_str("ERRORE: nome vuoto\n");
        return None;
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
            return None;
        }
    };

    match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) if r.ok() => {
            console_write_str("Nuova sessione ottenuta\n");
            Some(r.w1) // w1 = token emesso da Identis
        }
        Ok(r) => {
            console_write_str("Autenticazione fallita: ");
            console_write_str(r.describe());
            console_write_str("\n");
            None
        }
        Err(_) => {
            console_write_str("ERRORE IPC\n");
            None
        }
    }
}

/// Legge il token di sessione dall'ambiente (hex a 16 cifre).
fn read_session_token(sp: u64) -> Option<u64> {
    let env = civis::env_from_stack(sp)?;
    let raw = env.get(ENV_SESSION)?;
    parse_hex_u64(raw)
}

/// Parsa una stringa esadecimale in u64 (`None` se malformata).
fn parse_hex_u64(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() || raw.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &b in raw {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u64;
    }
    Some(v)
}

/// Legge il nome utente dall'ambiente (`USER`). Ritorna `""` se non
/// presente. Il nome viene copiato in un buffer statico (max 32 byte).
fn read_user() -> &'static str {
    // Buffer statico per il nome utente — sicuro: un solo processo shell alla volta.
    static mut USER: [u8; 33] = [0u8; 33];
    static mut LEN: usize = 0;

    if unsafe { LEN == 0 } {
        let sp = unsafe { SHELL_SP };
        if let Some(env) = civis::env_from_stack(sp) {
            if let Some(data) = env.get(ENV_USER) {
                let n = data.len().min(32);
                unsafe {
                    USER[..n].copy_from_slice(&data[..n]);
                    USER[n] = 0;
                    LEN = n;
                }
            }
        }
    }

    unsafe {
        if LEN == 0 { "" } else {
            core::str::from_utf8(&USER[..LEN]).unwrap_or("")
        }
    }
}

/// Restituisce l'ora corrente come `HH:MM:SS` (24h). Richiede il servizio Time.
fn get_time_str() -> &'static str {
    static mut BUF: [u8; 9] = [0u8; 9]; // "HH:MM:SS\0"

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
            BUF[8] = 0;
        }
    } else {
        return "";
    }

    // safe: BUF e' stato scritto in modo completo da questo stesso processo,
    // nessun altro lo modifica.
    unsafe { core::str::from_utf8(&*(&raw const BUF)).unwrap_or("") }
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
/// mascherato (password: niente in chiaro).
fn read_line_from_keyboard(mask: bool) -> String {
    let fd = match civis::open(KEYBOARD_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            console_write_str("[shell] ERRORE: impossibile aprire tastiera\n");
            civis::exit(1);
        }
    };

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];

    // Futex word locale per il wait: zero-initialized nel BSS, mai toccata da altri.
    static FUTEX_WORD: u32 = 0;

    loop {
        let n = civis::read_fs(fd, &mut byte, 1).unwrap_or(0);
        if n == 0 {
            // Niente di nuovo: prima controlla la coda IPC (non-blocking)
            // per eventuali eventi in arrivo, poi dormi 1 tick. Se c'e'
            // qualcos'altro nel ring buffer lo legge subito — zero latenza.
            let _ = civis::recv_poll();
            let _ = civis::futex_wait(&FUTEX_WORD as *const u32 as usize, 0, (civis::get_ticks() + 1) as u64);
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

    let _ = civis::close(fd);
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
