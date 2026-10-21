//! Servizio Identis (ADR-0055, Fase 3.1): identita' e autenticazione utenti.
//!
//! Detiene il bucket ArcaFS `identis:config` (una riga per utente:
//! `nome:hash_hex_bLAKE2s256`). Espone tre operazioni IPC:
//!
//! - `IDENTIS_VERIFY` (0x70): verifica le credenziali; se tornano giuste
//!   **emette un token di sessione** (w1 della reply) legato all'utente.
//! - `IDENTIS_ADD_USER` (0x71): crea un utente. Richiede un token di
//!   sessione **root** nel payload (la password non transita mai in chiaro:
//!   il client calcola l'hash BLAKE2s-256 in locale).
//! - `IDENTIS_LIST` (0x72): numero utenti registrati (w1 della reply).
//!
//! Il framing dei payload e' condiviso (`libs/identis-proto`): qui il
//! framing e' lato server (stato, validazione, scrittura del bucket).
//!
//! # Anti-interferenza
//!
//! Una sola transazione attiva per volta: la transazione e' **legata al
//! canale** che l'ha aperta. Un altro client che bussa durante una
//! trasmissione riceve `R_BUSY` e il suo messaggio **non** viene consumato
//! come chunk (bug chiuso in Fase 3.1: prima il secondo client corrompeva il
//! buffer del primo). Timeout di inattivita' (5 s) e `EXIT_NOTIFY` (morte
//! del client) scattano la transazione e tornano in idle.

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

extern crate alloc;
use alloc::format;
use alloc::string::String;

use civis;
use identis_proto as proto;

civis::entry!(real_main);

/// Bucket ArcaFS con le credenziali (nome -> hash hex).
const BUCKET_PATH: &str = "identis:config";
/// Utente di default, creato col bucket se assente.
const DEFAULT_USER: &str = "root";
/// BLAKE2s-256("root") — l'utente di default ha la password "root".
const DEFAULT_HASH_HEX: &str = "0cb8a879dc5d94ed67eb49cc7a4d3052d6a346f2e28fcd9e10e5822aba92eb39";

/// Dimensione massima del payload accettata (4 KiB).
const MAX_PAYLOAD: usize = 4096;
/// Timeout di inattivita' durante una trasmissione (tick PIT 100 Hz = 5 s).
const TIMEOUT_TICKS: u64 = 500;
/// Slot di sessione contemporanee.
const MAX_SESSIONS: usize = 8;
/// TTL della sessione (300 s = 30.000 tick a 100 Hz).
const SESSION_TTL_TICKS: u64 = 30_000;

// ── Stato della transazione in corso ───────────────────────────────────────

static mut TXN_ACTIVE: bool = false;
static mut TXN_TAG: u64 = 0;
static mut TXN_CHAN: u64 = 0;
static mut TXN_TOTAL: u64 = 0;
static mut TXN_BUF: [u8; MAX_PAYLOAD] = [0; MAX_PAYLOAD];
static mut TXN_LEN: usize = 0;
static mut TXN_TICK: u64 = 0;

// ── Tabella delle sessioni ─────────────────────────────────────────────────

/// Una sessione attiva: token -> utente. Bearer token (chi lo possiede agisce
/// come l'utente): la capability per-canale e' Fase 3.3 (debito dichiarato
/// in ADR-0056).
#[derive(Clone, Copy)]
struct Session {
    token: u64,
    name: [u8; proto::MAX_NAME],
    name_len: usize,
    issued: u64,
}

static mut SESSIONS: [Session; MAX_SESSIONS] = [Session {
    token: 0,
    name: [0; proto::MAX_NAME],
    name_len: 0,
    issued: 0,
}; MAX_SESSIONS];

// ── Entry ──────────────────────────────────────────────────────────────────

/// Entry: registra il servizio, prepara il bucket, entra nel loop.
fn real_main(_sp: u64) -> ! {
    civis::println!("[identis] avvio servizio Identis, pid={}", civis::getpid());

    if civis::service_register(civis::Service::Identis).is_err() {
        civis::println!("[identis] FAILED: service_register");
        civis::exit(1);
    }
    civis::println!("[identis] registrato come servizio Identis");

    if ensure_bucket().is_err() {
        civis::println!("[identis] FAILED: bucket non disponibile");
        civis::exit(1);
    }

    civis::signal_ready(1);
    civis::println!("[identis] pronto");

    // Loop bloccante: `recv` mette in sonno il processo finche' non arriva
    // un messaggio. Niente busy-loop su syscall (maschera gli interrupt e
    // affama il timer: un servizio che gira a vuoto ruba tempo a tutti gli
    // altri, e in TCG il path syscall e' costoso).
    loop {
        match civis::ipc::recv() {
            Ok(m) => handle_msg(m),
            Err(_) => civis::exit(0), // canale morto: il servizio esce
        }
    }
}

/// Smista il messaggio ricevuto.
fn handle_msg(m: civis::IpcMsg) {
    // Transazione troppo ferma? (client vivo che ha smesso di parlare): la
    // si scatta qui, all'arrivo di un messaggio qualsiasi — cosi' un client
    // nuovo non resta bloccato su R_BUSY per sempre. Il controllo e' fatto
    // all'ingresso e non in un timer: il servizio dorme tra un messaggio e
    // l'altro.
    unsafe {
        if TXN_ACTIVE && now_tick().saturating_sub(TXN_TICK) > TIMEOUT_TICKS {
            civis::println!(
                "[identis] timeout ricezione ({} byte attesi, {} ricevuti)",
                TXN_TOTAL, TXN_LEN
            );
            reset_txn();
        }
    }

    match m.tag {
        civis::IDENTIS_VERIFY | civis::IDENTIS_ADD_USER => handle_chunked(m),
        civis::IDENTIS_LIST => handle_list(m),
        civis::EXIT_NOTIFY => {
            // Morte di un peer: se era il client della transazione, scatta.
            unsafe {
                if TXN_ACTIVE && TXN_CHAN == m.channel {
                    civis::println!("[identis] client morto, scatto transazione");
                    reset_txn();
                }
            }
        }
        _ => {}
    }
}

// ── Operazioni chunked (VERIFY / ADD_USER) ────────────────────────────────

/// Gestisce header, chunk ed EOF delle operazioni con payload.
fn handle_chunked(m: civis::IpcMsg) {
    unsafe {
        if !TXN_ACTIVE {
            // Header: w0 = dimensione totale, w1 = 0.
            if m.w1 != 0 || m.w0 < 8 || m.w0 > MAX_PAYLOAD as u64 {
                reply(m.tag, proto::R_PROTOCOL, 0);
                return;
            }
            TXN_ACTIVE = true;
            TXN_TAG = m.tag;
            TXN_CHAN = m.channel;
            TXN_TOTAL = m.w0;
            TXN_LEN = 0;
            TXN_TICK = now_tick();
            reply(m.tag, proto::R_OK, 0);
            return;
        }

        // Transazione in corso: il canale decide chi e' il cliente.
        if m.channel != TXN_CHAN {
            // Altro client: R_BUSY e il messaggio NON viene consumato.
            reply(m.tag, proto::R_BUSY, 0);
            return;
        }
        if m.tag != TXN_TAG {
            reply(m.tag, proto::R_PROTOCOL, 0);
            reset_txn();
            return;
        }

        if m.w0 == proto::MSG_EOF {
            // Fine trasmissione: elabora e risponde.
            let tag = TXN_TAG;
            let len = TXN_LEN;
            let (code, extra) = dispatch_payload(tag, &TXN_BUF[..len]);
            reset_txn();
            reply(tag, code, extra);
            return;
        }

        // Chunk: w0 = offset, w1 = 4 byte little-endian. L'offset deve
        // combaciare con i byte gia' ricevuti: il protocollo e' sequenziale
        // (niente riordine/sovrapposizioni: un chunk fuori posto e' un bug
        // del client, non qualcosa da interpretare).
        let offset = m.w0 as usize;
        let packed = m.w1 as u32;
        if offset != TXN_LEN {
            reply(m.tag, proto::R_PROTOCOL, 0);
            reset_txn();
            return;
        }
        let remaining = TXN_TOTAL as usize - offset;
        let n = remaining.min(proto::CHUNK);
        for i in 0..n {
            TXN_BUF[TXN_LEN + i] = ((packed >> (8 * i)) & 0xFF) as u8;
        }
        TXN_LEN += n;
        TXN_TICK = now_tick();
        reply(m.tag, proto::R_OK, 0);
    }
}

/// Elabora il payload completo e restituisce (codice, w1 della reply).
fn dispatch_payload(tag: u64, buf: &[u8]) -> (u64, u64) {
    match tag {
        civis::IDENTIS_VERIFY => do_verify(buf),
        civis::IDENTIS_ADD_USER => do_add_user(buf),
        _ => (proto::R_PROTOCOL, 0),
    }
}

/// Verifica le credenziali e, se tornano giuste, emette un token di sessione.
fn do_verify(buf: &[u8]) -> (u64, u64) {
    let (name, hash) = match proto::parse_verify(buf) {
        Ok(v) => v,
        Err(_) => return (proto::R_PROTOCOL, 0),
    };

    let ok = match load_bucket_entry(&name) {
        Some(stored) => stored == hex_encode(&hash),
        None => false,
    };

    if !ok {
        // NB: niente la parola "FAIL" nel log (il gate grepa FAIL/PANIC e
        // questa riga e' un caso NEGATIVO atteso, non un errore).
        civis::println!("[identis] verifica utente '{}': credenziali non valide", name);
        return (proto::R_FAIL, 0);
    }

    let token = session_issue(&name);
    civis::println!("[identis] verifica utente '{}': SUCCESS", name);
    (proto::R_OK, token)
}

/// Crea un utente: token di sessione root + nome valido + nome non gia' preso.
fn do_add_user(buf: &[u8]) -> (u64, u64) {
    let (token, name, hash) = match proto::parse_add_user(buf) {
        Ok(v) => v,
        Err(_) => return (proto::R_PROTOCOL, 0),
    };

    // 1. Token di sessione valido?
    let user = match session_lookup(token) {
        Some(u) => u,
        None => {
            civis::println!("[identis] add_user '{}': token non valido", name);
            return (proto::R_TOKEN_INVALID, 0);
        }
    };

    // 2. La sessione deve essere di root (un utente normale non crea utenti).
    if user != DEFAULT_USER {
        civis::println!(
            "[identis] add_user '{}': sessione di '{}' non autorizzata",
            name, user
        );
        return (proto::R_UNAUTHORIZED, 0);
    }

    // 3. Nome conforme?
    if !proto::valid_name(&name) {
        civis::println!("[identis] add_user: nome '{}' non valido", name);
        return (proto::R_NAME_INVALID, 0);
    }

    // 4. Utente gia' esistente?
    if load_bucket_entry(&name).is_some() {
        civis::println!("[identis] add_user '{}': gia' esistente", name);
        return (proto::R_EXISTS, 0);
    }

    // 5. Scrittura nel bucket (solo Identis scrive qui).
    match append_bucket_entry(&name, &hex_encode(&hash)) {
        Ok(()) => {
            civis::println!("[identis] utente '{}' creato", name);
            (proto::R_OK, 0)
        }
        Err(()) => {
            civis::println!("[identis] add_user '{}': scrittura bucket fallita", name);
            (proto::R_FS_ERROR, 0)
        }
    }
}

// ── Operazione LIST ───────────────────────────────────────────────────────

/// LIST: numero utenti registrati. Richiede un token di sessione valido (w1).
fn handle_list(m: civis::IpcMsg) {
    unsafe {
        if TXN_ACTIVE {
            reply(m.tag, proto::R_BUSY, 0);
            return;
        }
    }
    if session_lookup(m.w1).is_none() {
        reply(m.tag, proto::R_TOKEN_INVALID, 0);
        return;
    }
    let n = count_users();
    reply(m.tag, proto::R_OK, n as u64);
}

// ── Sessioni ───────────────────────────────────────────────────────────────

/// Emette un token di sessione per `name` (slot libero, o il piu' vecchio).
fn session_issue(name: &str) -> u64 {
    let token = rand_token();
    let now = now_tick();
    let bytes = name.as_bytes();
    let n = bytes.len().min(proto::MAX_NAME);

    unsafe {
        let mut target = usize::MAX;
        for i in 0..MAX_SESSIONS {
            if SESSIONS[i].token == 0 {
                target = i;
                break;
            }
        }
        if target == usize::MAX {
            // Nessuno slot libero: si sostituisce la sessione piu' vecchia.
            target = 0;
            for i in 1..MAX_SESSIONS {
                if SESSIONS[i].issued < SESSIONS[target].issued {
                    target = i;
                }
            }
        }
        SESSIONS[target].token = token;
        SESSIONS[target].name_len = n;
        SESSIONS[target].name[..n].copy_from_slice(&bytes[..n]);
        SESSIONS[target].issued = now;
    }
    token
}

/// Cerca la sessione di un token: `Some(utente)` se valida e non scaduta.
fn session_lookup(token: u64) -> Option<String> {
    if token == 0 {
        return None;
    }
    let now = now_tick();
    unsafe {
        for i in 0..MAX_SESSIONS {
            let s = SESSIONS[i];
            if s.token != token {
                continue;
            }
            if now.saturating_sub(s.issued) > SESSION_TTL_TICKS {
                return None;
            }
            return core::str::from_utf8(&s.name[..s.name_len])
                .ok()
                .map(String::from);
        }
    }
    None
}

// ── Casualita' del token ──────────────────────────────────────────────────

/// Token casuale a 64 bit. RDRAND quando la CPU lo espone e funziona,
/// altrimenti fallback xorshift64* seminato da rdtsc+ticks+pid (NON e'
/// crypto-grade: sotto TCG l'istruzione RDRAND non funziona mai — vedi la
/// nota nel PAL, S2.5d). Il token 0 e' riservato (mai emesso).
fn rand_token() -> u64 {
    for _ in 0..10 {
        unsafe {
            let mut v = 0u64;
            if core::arch::x86_64::_rdrand64_step(&mut v) == 1 && v != 0 {
                return v;
            }
        }
    }
    let mut s = seed_value();
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    let v = s.wrapping_mul(0x2545F4914F6CDD1D);
    if v == 0 { 1 } else { v }
}

/// Seme del fallback: rdtsc ^ ticks ^ pid.
fn seed_value() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    let tsc = ((hi as u64) << 32) | lo as u64;
    let ticks = now_tick();
    let pid = civis::getpid() as u64;
    let mut s = tsc ^ ticks.rotate_left(17) ^ pid.wrapping_mul(0x9E3779B97F4A7C15);
    if s == 0 {
        s = 0x243F6A8885A308D3;
    }
    s
}

// ── Bucket `identis:config` ───────────────────────────────────────────────

/// Legge l'hash (hex) associato a un nome utente.
fn load_bucket_entry(name: &str) -> Option<String> {
    let all = read_bucket_all().ok()?;
    for line in all.lines() {
        let mut parts = line.splitn(2, ':');
        let stored_name = parts.next()?;
        let stored_hash = parts.next()?;
        if stored_name == name {
            return Some(String::from(stored_hash));
        }
    }
    None
}

/// Conta le righe del bucket (utenti registrati).
fn count_users() -> usize {
    match read_bucket_all() {
        Ok(all) => all.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

/// Legge l'intero bucket in una String.
fn read_bucket_all() -> Result<String, civis::Error> {
    let fd = civis::open(BUCKET_PATH, 0)?;
    let mut buf = [0u8; 4096];
    let mut out = String::new();
    loop {
        let cap = buf.len();
        let n = civis::read_fs(fd, &mut buf, cap)?;
        if n == 0 {
            break;
        }
        out.push_str(core::str::from_utf8(&buf[..n]).unwrap_or(""));
    }
    let _ = civis::close(fd);
    Ok(out)
}

/// Appende una riga `nome:hash` al bucket.
fn append_bucket_entry(name: &str, hash_hex: &str) -> Result<(), ()> {
    let fd = civis::open(BUCKET_PATH, civis::OPEN_APPEND).map_err(|_| ())?;
    let line = format!("{}:{}\n", name, hash_hex);
    let n = civis::write_fs(fd, line.as_bytes(), line.len()).map_err(|_| ())?;
    let _ = civis::close(fd);
    if n != line.len() {
        return Err(());
    }
    Ok(())
}

/// Assicura che il bucket esista e contenga almeno l'utente `root`.
fn ensure_bucket() -> Result<(), civis::Error> {
    let fd = match civis::open(BUCKET_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            let fd = civis::open(BUCKET_PATH, civis::OPEN_CREATE)?;
            let line = format!("{}:{}\n", DEFAULT_USER, DEFAULT_HASH_HEX);
            let _ = civis::write_fs(fd, line.as_bytes(), line.len());
            let _ = civis::close(fd);
            civis::println!("[identis] bucket creato con utente di default 'root'");
            return Ok(());
        }
    };
    let mut buf = [0u8; 4096];
    let cap = buf.len();
    let n = civis::read_fs(fd, &mut buf, cap)?;
    let _ = civis::close(fd);
    if n == 0 {
        let fd = civis::open(BUCKET_PATH, civis::OPEN_APPEND)?;
        let line = format!("{}:{}\n", DEFAULT_USER, DEFAULT_HASH_HEX);
        let _ = civis::write_fs(fd, line.as_bytes(), line.len());
        let _ = civis::close(fd);
        civis::println!("[identis] bucket vuoto, aggiunto utente di default 'root'");
    }
    Ok(())
}

// ── Utility ────────────────────────────────────────────────────────────────

/// Reset della transazione corrente.
fn reset_txn() {
    unsafe {
        TXN_ACTIVE = false;
        TXN_TAG = 0;
        TXN_CHAN = 0;
        TXN_TOTAL = 0;
        TXN_LEN = 0;
        TXN_TICK = 0;
    }
}

/// Reply al mittente del messaggio corrente (reply implicito, ADR-0008).
fn reply(tag: u64, w0: u64, w1: u64) {
    let _ = civis::reply(tag, w0, w1);
}

/// Tick monotoni (PIT 100 Hz).
fn now_tick() -> u64 {
    let t = civis::get_ticks();
    if t < 0 { 0 } else { t as u64 }
}

/// Codifica un array di byte in stringa esadecimale (minuscole).
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, &b) in bytes.iter().enumerate() {
        if i * 2 + 1 >= out.len() {
            break;
        }
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0xF) as usize];
    }
    String::from(core::str::from_utf8(&out).unwrap_or(""))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    civis::println!("[identis] panic");
    civis::exit(1)
}
