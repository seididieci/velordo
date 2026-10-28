//! Shell nativa Velordo (Fase 3.2, S3.2a): REPL minima post-login.
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
//! Se l'output di un comando non viene catturato in una variabile, il valore
//! (futuro, S3.2b+) viene stampato serializzato.
//!
//! Output su /dev/console (VGA + serial), input da /dev/input/keyboard.
//! La password non transita mai in chiaro: hash BLAKE2s-256 in locale.
//!
//! **Refresh del token**: quando un comando autenticato riceve `R_TOKEN_INVALID`
//! (scadenza TTL 300s), la shell richiede nuovamente nome+password e ottiene
//! un nuovo token da Identis — l'utente non perde mai il contesto.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use core::sync::atomic::AtomicU32;

use blake2s::blake2s;
use civis;
use civis::println;
use identis_proto as proto;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";
const KEYBOARD_PATH: &str = "/dev/input/keyboard";
const IDENTIS_BUCKET: &str = "identis:config";

/// Futex per attendere senza busy-wait quando la tastiera non ha input.
static KEYBOARD_FUTEX: AtomicU32 = AtomicU32::new(0);

/// Futex per attendere senza busy-wait durante la risoluzione di Identis.
static IDENTIS_FUTEX: AtomicU32 = AtomicU32::new(0);

/// Variabile d'ambiente con il token di sessione (hex).
const ENV_SESSION: &str = "VELORDO_SESSION";

/// Tentativi in caso di servizio occupato (`R_BUSY`).
const RETRIES: u32 = 20;

/// Tentativi di risoluzione del servizio Identis.
const IDENTIS_RESOLVE_RETRIES: u32 = 50;

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

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

    loop {
        console_write_str("os> ");
        let line = read_line_from_keyboard(false);
        let line = trim_str(&line);
        console_write_str("\n");
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts.first().copied().unwrap_or("");
        let args: Vec<&str> = parts[1..].to_vec();

        // Se un comando autenticato fallisce per token scaduto, il loop
        // chiede all'utente di reinserire le credenziali e poi ritenta.
        loop {
            match execute_command(cmd, &args, session_token) {
                CmdResult::Ok => break,
                CmdResult::TokenExpired => {
                    console_write_str("Token scaduto, reinserire le credenziali\n");
                    match auth_and_get_token() {
                        Some(t) => { session_token = Some(t); continue; } // retry with new token
                        None => break, // auth failed, stop retrying
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
            console_write_str("Comando sconosciuto: ");
            console_write_str(cmd);
            console_write_str(" (scrivi `help`)\n");
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

    loop {
        let n = civis::read_fs(fd, &mut byte, 1).unwrap_or(0);
        if n == 0 {
            // Attendi senza busy-wait: dormi fino al prossimo tick o wake.
            // ~100 ms di deadline; il processo va in Blocked nel kernel scheduler.
            let deadline = civis::get_ticks() as u64 + 10;
            let _ = civis::futex_wait(
                &KEYBOARD_FUTEX as *const AtomicU32 as usize,
                0,
                deadline,
            );
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
                // Attendi senza busy-wait: dormi fino al prossimo tick o wake.
                let deadline = civis::get_ticks() as u64 + 10;
                let _ = civis::futex_wait(
                    &IDENTIS_FUTEX as *const AtomicU32 as usize,
                    0,
                    deadline,
                );
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
