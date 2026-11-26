//! Servizio Identis (ADR-0055, Fase 3.1): identita' e autenticazione utenti.
//!
//! Detiene il bucket ArcaFS `identis:config` (una riga per utente:
//! `nome:hash_hex_bLAKE2s256:ruolo[,ruolo...]`). Espone quattro operazioni:
//!
//! - `IDENTIS_VERIFY` (0x70): verifica le credenziali; se tornano giuste,
//!   **lega il pid chiamante** all'utente (credenziale locale, mai trasmessa,
//!   purgata su `EXIT_NOTIFY`). La password non transita mai in chiaro: il
//!   client calcola l'hash BLAKE2s-256 in locale.
//! - `IDENTIS_ADD_USER` (0x71): crea un utente con i suoi ruoli. L'autorita'
//!   e' quella dell'identita' attestata del chiamante (il pid che ha fatto
//!   un `VERIFY` riuscito, oppure il login/DE via sessione autenticata).
//! - `IDENTIS_LIST` (0x72): numero utenti registrati.
//! - `IDENTIS_WHOAMI` (0x73): ruoli **correnti** dell'utente del chiamante.
//!   I ruoli si leggono **live** da `identis:config`: cambiarli ha effetto
//!   senza rifare il login (ADR-0080, fase 5).
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
use civis::hex;
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
/// Credenziali per pid contemporanee (S3.3c: identita' attestata, non bearer).
const MAX_CREDENTIALS: usize = 8;

// ── Stato della transazione in corso ───────────────────────────────────────

static mut TXN_ACTIVE: bool = false;
static mut TXN_TAG: u64 = 0;
static mut TXN_CHAN: u64 = 0;
static mut TXN_TOTAL: u64 = 0;
static mut TXN_BUF: [u8; MAX_PAYLOAD] = [0; MAX_PAYLOAD];
static mut TXN_LEN: usize = 0;
static mut TXN_TICK: u64 = 0;

// ── Credenziali per pid (S3.3c: identita' attestata, mai trasmessa) ──────────────────────

/// Credenziale di un processo: il pid che ha fatto `VERIFY` riuscito è legato
/// all'utente autenticato. Mai trasmessa: il kernel dà il pid, Identis lo
/// risolve. Purgata su `EXIT_NOTIFY` (niente buco di riuso di pid).
#[derive(Clone, Copy)]
struct Credential {
    pid: i64,
    name: [u8; proto::MAX_NAME],
    name_len: usize,
}

static mut CREDENTIALS: [Credential; MAX_CREDENTIALS] = [Credential {
    pid: 0,
    name: [0; proto::MAX_NAME],
    name_len: 0,
}; MAX_CREDENTIALS];

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
        civis::IDENTIS_WHOAMI => handle_whoami(m),
        civis::EXIT_NOTIFY => {
            // Morte di un peer: se era il client della transazione, scatta.
            // Sempre: purga la sua credenziale (mai un pid riusato con
            // l'identita' di un altro).
            unsafe {
                if TXN_ACTIVE && TXN_CHAN == m.channel {
                    civis::println!("[identis] client morto, scatto transazione");
                    reset_txn();
                }
                purge_credential(m.w1 as i64);
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
        civis::IDENTIS_VERIFY => {
            let pid = unsafe { civis::peer_pid(TXN_CHAN).unwrap_or(-1) };
            do_verify(buf, pid)
        }
        civis::IDENTIS_ADD_USER => do_add_user(buf),
        _ => (proto::R_PROTOCOL, 0),
    }
}

/// Verifica le credenziali e, se tornano giuste, **lega il pid chiamante**
/// all'utente (credenziale locale, mai trasmessa). Nessun token: la prova e'
/// l'attestazione del kernel sul pid.
fn do_verify(buf: &[u8], pid: i64) -> (u64, u64) {
    let (name, hash) = match proto::parse_verify(buf) {
        Ok(v) => v,
        Err(_) => return (proto::R_PROTOCOL, 0),
    };

    let ok = match load_bucket_entry(&name) {
        Some(stored) => stored == hex::encode(&hash),
        None => false,
    };

    if !ok {
        // NB: niente la parola "FAIL" nel log (il gate grepa FAIL/PANIC e
        // questa riga e' un caso NEGATIVO atteso, non un errore).
        civis::println!("[identis] verifica utente '{}': credenziali non valide", name);
        return (proto::R_FAIL, 0);
    }

    if pid < 0 {
        return (proto::R_UNAUTHORIZED, 0);
    }
    bind_credential(pid, &name);
    civis::println!("[identis] verifica utente '{}': SUCCESS (pid={})", name, pid);
    (proto::R_OK, 0)
}

/// Crea un utente: identita' del chiamante + nome valido + nome non gia' preso.
fn do_add_user(buf: &[u8]) -> (u64, u64) {
    let (name, hash, roles) = match proto::parse_add_user(buf) {
        Ok(v) => v,
        Err(_) => return (proto::R_PROTOCOL, 0),
    };

    // 1. Identita' del chiamante (nessun token: il pid lo da' il kernel).
    let pid = unsafe { civis::peer_pid(TXN_CHAN).unwrap_or(-1) };
    let Some(user) = caller_user(pid) else {
        civis::println!("[identis] add_user '{}': chiamante senza identita' (pid={})", name, pid);
        return (proto::R_NO_IDENTITY, 0);
    };

    // 2. Autorita' AL MOMENTO della chiamata: i ruoli del chiamante si
    //    leggono live dal bucket. `min(richiesta, ruoli del chiamante)`:
    //    un `user` non crea nessuno, un `net-admin` non crea `root`
    //    (ADR-0080, fase 5).
    let requester = roles_of(&user).unwrap_or(proto::ROLE_USER);
    if !proto::can_grant_roles(requester, roles) {
        civis::println!(
            "[identis] add_user '{}': identita' di '{}' (pid={}) senza autorita' per i ruoli richiesti",
            name, user, pid
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
    match append_bucket_entry(&name, &hex::encode(&hash), roles) {
        Ok(()) => {
            civis::println!(
                "[identis] utente '{}' creato da '{}' (pid={}, ruoli: {})",
                name,
                user,
                pid,
                proto::format_roles(roles)
            );
            (proto::R_OK, 0)
        }
        Err(()) => {
            civis::println!("[identis] add_user '{}': scrittura bucket fallita", name);
            (proto::R_FS_ERROR, 0)
        }
    }
}

/// WHOAMI: ruoli correnti dell'identita' del chiamante (letti live dal bucket).
///
/// `w1` = pid (0 = il chiamante; un altro pid solo `System`).
fn handle_whoami(m: civis::IpcMsg) {
    unsafe {
        if TXN_ACTIVE {
            reply(m.tag, proto::R_BUSY, 0);
            return;
        }
    }
    let pid = if m.w1 == 0 {
        civis::peer_pid(m.channel).unwrap_or(-1)
    } else {
        // Un altro pid e' lecito solo a `System` (servizi di controllo).
        let class = civis::peer_info_class(m.channel)
            .map(|(_, c)| c)
            .unwrap_or(civis::ImageClass::Untrusted);
        if class != civis::ImageClass::System {
            reply(m.tag, proto::R_UNAUTHORIZED, 0);
            return;
        }
        m.w1 as i64
    };
    let Some(user) = caller_user(pid) else {
        reply(m.tag, proto::R_NO_IDENTITY, 0);
        return;
    };
    let roles = roles_of(&user).unwrap_or(proto::ROLE_USER);
    reply(m.tag, proto::R_OK, roles);
}

/// LIST: numero utenti registrati. Richiede un'identita' attestata del
/// chiamante (nessun token: il pid lo da' il kernel).
fn handle_list(m: civis::IpcMsg) {
    unsafe {
        if TXN_ACTIVE {
            reply(m.tag, proto::R_BUSY, 0);
            return;
        }
    }
    let pid = civis::peer_pid(m.channel).unwrap_or(-1);
    if caller_user(pid).is_none() {
        reply(m.tag, proto::R_NO_IDENTITY, 0);
        return;
    }
    let n = count_users();
    reply(m.tag, proto::R_OK, n as u64);
}

// ── Credenziali per pid (S3.3c: identita' attestata, mai trasmessa) ───────────────────────

/// Lega `pid` all'utente `name` (credenziale stabilita da `VERIFY` riuscito).
fn bind_credential(pid: i64, name: &str) {
    unsafe {
        // Sostituisci un'eventuale credenziale precedente dello stesso pid.
        let mut target: Option<usize> = None;
        for i in 0..MAX_CREDENTIALS {
            if CREDENTIALS[i].pid == pid {
                target = Some(i);
                break;
            }
            if CREDENTIALS[i].pid == 0 {
                target = Some(i);
                break;
            }
        }
        let i = target.unwrap_or(0);
        let bytes = name.as_bytes();
        let n = bytes.len().min(proto::MAX_NAME);
        CREDENTIALS[i].pid = pid;
        CREDENTIALS[i].name_len = n;
        CREDENTIALS[i].name[..n].copy_from_slice(&bytes[..n]);
    }
}

/// Cerca l'utente di `pid`: `Some(nome)` se ha un'identita' attestata.
fn lookup_credential(pid: i64) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    unsafe {
        for i in 0..MAX_CREDENTIALS {
            if CREDENTIALS[i].pid == pid {
                return core::str::from_utf8(&CREDENTIALS[i].name[..CREDENTIALS[i].name_len])
                    .ok()
                    .map(String::from);
            }
        }
    }
    None
}

/// Toglie la credenziale di `pid` (morte del processo: niente buco di riuso).
fn purge_credential(pid: i64) {
    if pid <= 0 {
        return;
    }
    unsafe {
        for i in 0..MAX_CREDENTIALS {
            if CREDENTIALS[i].pid == pid {
                CREDENTIALS[i].pid = 0;
                CREDENTIALS[i].name_len = 0;
            }
        }
    }
}

/// L'utente del processo `pid`, da due fonti in ordine:
///
/// 1. la **credenziale locale** (un `VERIFY` riuscito di quel processo: e' la
///    via dei client che si autenticano da soli e dei test);
/// 2. la **sessione autenticata** (`SESS_AUTH` al server `console`, S3.3c):
///    e' la via normale, perche' il login lega il nome alla sessione e ogni
///    processo che ci gira dentro la eredita per **attestazione del pid**
///    (la catena dei padri la da' il kernel). Identis e' `System`, quindi
///    puo' chiedere l'identita' di un pid.
///
/// Nessuna delle due e' un valore che il chiamante puo' dichiarare: il pid lo
/// da' il kernel, il nome lo dice il server o un `VERIFY` validato.
fn caller_user(pid: i64) -> Option<String> {
    if let Some(u) = lookup_credential(pid) {
        return Some(u);
    }
    match civis::sess::auth_of(pid) {
        Ok(a) if a.is_authenticated() => Some(String::from(a.as_str())),
        _ => None,
    }
}

// ── Bucket `identis:config` ───────────────────────────────────────────────

/// Legge l'hash (hex) associato a un nome utente.
fn load_bucket_entry(name: &str) -> Option<String> {
    let all = read_bucket_all().ok()?;
    for line in all.lines() {
        let mut parts = line.splitn(3, ':');
        let stored_name = parts.next()?;
        let stored_hash = parts.next()?;
        if stored_name == name {
            return Some(String::from(stored_hash));
        }
    }
    None
}

/// Ruoli correnti dell'utente `name` (`None` se non registrato).
///
/// Terza colonna `ruolo[,ruolo...]`; una riga a due campi (formato storico)
/// vale `ROLE_USER`. Un ruolo sconosciuto e' fail-closed sull'utente semplice.
fn roles_of(name: &str) -> Option<u64> {
    let all = read_bucket_all().ok()?;
    for line in all.lines() {
        let mut parts = line.splitn(3, ':');
        let stored_name = parts.next()?;
        let _hash = parts.next()?;
        if stored_name != name {
            continue;
        }
        return Some(match parts.next() {
            Some(spec) => proto::parse_roles(spec).unwrap_or(proto::ROLE_USER),
            None => proto::ROLE_USER,
        });
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

/// Appende una riga `nome:hash:ruoli` al bucket.
fn append_bucket_entry(name: &str, hash_hex: &str, roles: u64) -> Result<(), ()> {
    let fd = civis::open(BUCKET_PATH, civis::OPEN_APPEND).map_err(|_| ())?;
    let line = format!("{}:{}:{}\n", name, hash_hex, proto::format_roles(roles));
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
            let line = format!("{}:{}:root\n", DEFAULT_USER, DEFAULT_HASH_HEX);
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
        let line = format!("{}:{}:root\n", DEFAULT_USER, DEFAULT_HASH_HEX);
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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    civis::println!("[identis] panic");
    civis::exit(1)
}
