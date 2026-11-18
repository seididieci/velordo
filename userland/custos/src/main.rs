//! Custos — server di policy per identita' (ADR-0080, fase 5).
//!
//! Decide **cosa puo' fare** un'immagine: detiene le capability per `ImageId`
//! (bind di rete, uso hardware) e le concede/revoca su richiesta di una
//! sessione autorizzata. E' l'**unico writer** del dato `sys:caps`.
//!
//! # Perche' un dato separato da `sys:policy`
//!
//! `sys:policy` resta la tabella ops FS di cardo (build-seeded, writer init):
//! cardo la legge **direttamente** nel suo loop, senza IPC. Se Custos
//! scrivesse quel file, cardo dovrebbe rileggerlo a ogni handshake e un
//! restart di Custos potrebbe incrociarsi con un handshake in corso
//! (deadlock circolare cardo↔Custos). `sys:caps` tiene separate le due
//! domande: "quali op FS" (cardo) e "quali capability" (Custos), un dato per
//! writer.
//!
//! # Fail-closed
//!
//! `sys:caps` assente/illeggibile = tabella vuota = ogni query nega. Un
//! errore di persistenza non muta la RAM (prima si scrive, poi si applica).
//! Il servizio dorme in `recv` tra un messaggio e l'altro: nessun busy-loop.

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

extern crate alloc;
use alloc::format;
use alloc::string::String;

use civis;
use custos_proto as proto;
use identis_proto as identis;

civis::entry!(real_main);

/// Bucket e chiave del dato delle capability (scritto SOLO da Custos).
const CAPS_BUCKET: &[u8] = b"sys";
const CAPS_KEY: &[u8] = b"caps";

/// Voci massime della tabella (una per identita'+capability).
const CAPS_MAX: usize = 64;
/// Dimensione massima del payload accettata (4 KiB).
const MAX_PAYLOAD: usize = 4096;
/// Timeout di inattivita' durante una trasmissione (tick PIT 100 Hz = 5 s).
const TIMEOUT_TICKS: u64 = 500;

// ── Tabella delle capability ───────────────────────────────────────────────

/// Una concessione: soggetto (`ImageId`), capability `(kind, arg)`, decisione.
#[derive(Clone, Copy)]
struct Cap {
    id_lo: u64,
    id_hi: u64,
    kind: u64,
    arg: u64,
    allowed: bool,
}

/// Tabella in RAM (fonte di verita' a runtime; `sys:caps` e' la persistenza).
static mut CAPS: [Cap; CAPS_MAX] = [Cap {
    id_lo: 0,
    id_hi: 0,
    kind: 0,
    arg: 0,
    allowed: false,
}; CAPS_MAX];
static mut CAPS_N: usize = 0;

// ── Stato della transazione in corso ───────────────────────────────────────

static mut TXN_ACTIVE: bool = false;
static mut TXN_TAG: u64 = 0;
static mut TXN_CHAN: u64 = 0;
static mut TXN_TOTAL: u64 = 0;
static mut TXN_BUF: [u8; MAX_PAYLOAD] = [0; MAX_PAYLOAD];
static mut TXN_LEN: usize = 0;
static mut TXN_TICK: u64 = 0;

// ── Entry ──────────────────────────────────────────────────────────────────

fn real_main(_sp: u64) -> ! {
    civis::println!("[custos] avvio servizio Custos, pid={}", civis::getpid());

    if civis::service_register(civis::Service::Custos).is_err() {
        civis::println!("[custos] FAILED: service_register");
        civis::exit(1);
    }
    civis::println!("[custos] registrato come servizio Custos");

    load_caps();

    civis::signal_ready(1);
    println_caps();
    civis::println!("[custos] pronto");

    loop {
        match civis::ipc::recv() {
            Ok(m) => handle_msg(m),
            Err(_) => civis::exit(0),
        }
    }
}

/// Carica `sys:caps` in RAM. Assente/illeggibile = tabella vuota (fail-closed).
fn load_caps() {
    let data = match civis::obj_get(CAPS_BUCKET, CAPS_KEY) {
        Ok(d) => d,
        Err(_) => {
            civis::println!("[custos] sys:caps assente: nessuna capability (fail-closed)");
            return;
        }
    };
    let text = core::str::from_utf8(&data).unwrap_or("");
    let mut n = 0usize;
    let mut bad = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let parsed = (|| {
            let hi = parse_hex(it.next()?)?;
            let lo = parse_hex(it.next()?)?;
            let kind = parse_dec(it.next()?)?;
            let arg = parse_dec(it.next()?)?;
            let decision = parse_dec(it.next()?)?;
            if !proto::valid_kind(kind) || decision > 1 {
                return None;
            }
            Some((hi, lo, kind, arg, decision == 1))
        })();
        let Some((hi, lo, kind, arg, allowed)) = parsed else {
            bad += 1;
            continue;
        };
        if n >= CAPS_MAX {
            civis::println!("[custos] sys:caps: oltre {} voci, resto ignorato", CAPS_MAX);
            break;
        }
        unsafe {
            CAPS[n] = Cap { id_lo: lo, id_hi: hi, kind, arg, allowed };
        }
        n += 1;
    }
    unsafe { CAPS_N = n };
    civis::println!("[custos] sys:caps: {} voci ({} righe scartate)", n, bad);
}

fn println_caps() {
    unsafe {
        for i in 0..CAPS_N {
            let c = CAPS[i];
            civis::println!(
                "[custos] cap {:016x}{:016x} kind={} arg={} -> {}",
                c.id_hi,
                c.id_lo,
                c.kind,
                c.arg,
                if c.allowed { "granted" } else { "denied" }
            );
        }
    }
}

// ── Loop dei messaggi ──────────────────────────────────────────────────────

fn handle_msg(m: civis::IpcMsg) {
    unsafe {
        if TXN_ACTIVE && now_tick().saturating_sub(TXN_TICK) > TIMEOUT_TICKS {
            civis::println!("[custos] timeout ricezione, transazione scartata");
            reset_txn();
        }
    }

    match m.tag {
        civis::CUSTOS_QUERY | civis::CUSTOS_GRANT => handle_chunked(m),
        civis::CUSTOS_STATUS => handle_status(m),
        civis::EXIT_NOTIFY => unsafe {
            if TXN_ACTIVE && TXN_CHAN == m.channel {
                civis::println!("[custos] client morto, transazione scartata");
                reset_txn();
            }
        },
        _ => {}
    }
}

fn handle_chunked(m: civis::IpcMsg) {
    unsafe {
        if !TXN_ACTIVE {
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

        if m.channel != TXN_CHAN {
            reply(m.tag, proto::R_BUSY, 0);
            return;
        }
        if m.tag != TXN_TAG {
            reply(m.tag, proto::R_PROTOCOL, 0);
            reset_txn();
            return;
        }

        if m.w0 == proto::MSG_EOF {
            let tag = TXN_TAG;
            let len = TXN_LEN;
            let (code, extra) = dispatch_payload(tag, &TXN_BUF[..len]);
            reset_txn();
            reply(tag, code, extra);
            return;
        }

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

fn dispatch_payload(tag: u64, buf: &[u8]) -> (u64, u64) {
    match tag {
        civis::CUSTOS_QUERY => do_query(buf),
        civis::CUSTOS_GRANT => do_grant(buf),
        _ => (proto::R_PROTOCOL, 0),
    }
}

// ── Operazioni ─────────────────────────────────────────────────────────────

/// Query: lookup pura in RAM. Reply `(R_OK, 1|0)`; identita' senza riga = 0.
fn do_query(buf: &[u8]) -> (u64, u64) {
    let (id_lo, id_hi, kind, arg) = match proto::parse_query(buf) {
        Ok(v) => v,
        Err(_) => return (proto::R_PROTOCOL, 0),
    };
    let granted = lookup(id_lo, id_hi, kind, arg).unwrap_or(false);
    (proto::R_OK, granted as u64)
}

/// Grant/revoca: sessione con autorita' per la capability, poi persistenza.
fn do_grant(buf: &[u8]) -> (u64, u64) {
    let (token, id_lo, id_hi, kind, arg, decision) = match proto::parse_grant(buf) {
        Ok(v) => v,
        Err(_) => return (proto::R_PROTOCOL, 0),
    };

    let roles = match whoami(token) {
        Ok(r) => r,
        Err(code) => return (code, 0),
    };

    // Anti-confused-deputy: la sessione deve coprire la capability. Root
    // copre tutto; gli amministratori coprono il proprio dominio.
    let authorized = roles & identis::ROLE_ROOT != 0
        || match kind {
            proto::CAP_NET_BIND | proto::CAP_NET_USE => roles & identis::ROLE_NET_ADMIN != 0,
            proto::CAP_HW_MMIO | proto::CAP_HW_DMA => roles & identis::ROLE_HW_ADMIN != 0,
            _ => false,
        };
    if !authorized {
        civis::println!(
            "[custos] grant kind={} arg={}: sessione senza autorita' (ruoli {})",
            kind,
            arg,
            identis::format_roles(roles)
        );
        return (proto::R_UNAUTHORIZED, 0);
    }

    // Prima la persistenza (tabella nuova senza l'entry vecchia + questa),
    // poi la RAM: un errore FS non lascia stato divergente.
    if persist_with(id_lo, id_hi, kind, arg, decision).is_err() {
        civis::println!("[custos] grant: scrittura sys:caps fallita");
        return (proto::R_FS_ERROR, 0);
    }
    if !apply(id_lo, id_hi, kind, arg, decision) {
        civis::println!("[custos] grant: tabella piena");
        return (proto::R_FS_ERROR, 0);
    }
    civis::println!(
        "[custos] cap {:016x}{:016x} kind={} arg={} -> {}",
        id_hi,
        id_lo,
        kind,
        arg,
        if decision { "granted" } else { "denied" }
    );
    (proto::R_OK, 0)
}

/// STATUS: numero di voci della tabella (diagnostica).
fn handle_status(m: civis::IpcMsg) {
    unsafe {
        if TXN_ACTIVE {
            reply(m.tag, proto::R_BUSY, 0);
            return;
        }
        reply(m.tag, proto::R_OK, CAPS_N as u64);
    }
}

// ── Tabella ────────────────────────────────────────────────────────────────

/// Lookup dell'ultima voce che combacia (l'ultima scritta vince).
fn lookup(id_lo: u64, id_hi: u64, kind: u64, arg: u64) -> Option<bool> {
    unsafe {
        let n = CAPS_N;
        for i in (0..n).rev() {
            let c = CAPS[i];
            if c.id_lo == id_lo && c.id_hi == id_hi && c.kind == kind && c.arg == arg {
                return Some(c.allowed);
            }
        }
    }
    None
}

/// Aggiorna o appende una voce. `false` se la tabella e' piena.
fn apply(id_lo: u64, id_hi: u64, kind: u64, arg: u64, allowed: bool) -> bool {
    unsafe {
        let n = CAPS_N;
        for i in 0..n {
            let c = &mut CAPS[i];
            if c.id_lo == id_lo && c.id_hi == id_hi && c.kind == kind && c.arg == arg {
                c.allowed = allowed;
                return true;
            }
        }
        if n >= CAPS_MAX {
            return false;
        }
        CAPS[n] = Cap { id_lo, id_hi, kind, arg, allowed };
        CAPS_N = n + 1;
    }
    true
}

/// Scrive `sys:caps` come se la voce `(id, kind, arg)` valesse `allowed`:
/// le voci esistenti vengono riemesse, quella combaciante sostituita, la
/// nuova accodata. Nessuna mutazione della RAM qui.
fn persist_with(id_lo: u64, id_hi: u64, kind: u64, arg: u64, allowed: bool) -> Result<(), ()> {
    let mut text = String::new();
    unsafe {
        for i in 0..CAPS_N {
            let c = CAPS[i];
            let same = c.id_lo == id_lo && c.id_hi == id_hi && c.kind == kind && c.arg == arg;
            if same {
                continue;
            }
            text.push_str(&format!(
                "{:016x} {:016x} {} {} {}\n",
                c.id_hi, c.id_lo, c.kind, c.arg, c.allowed as u64
            ));
        }
    }
    text.push_str(&format!(
        "{:016x} {:016x} {} {} {}\n",
        id_hi, id_lo, kind, arg, allowed as u64
    ));
    civis::obj_put(CAPS_BUCKET, CAPS_KEY, text.as_bytes()).map_err(|_| ())?;
    Ok(())
}

// ── Identis: ruoli live di una sessione ───────────────────────────────────

/// Interroga Identis: `Ok(maschera ruoli)` o `Err(codice da rispondere)`.
///
/// Fail-closed: se Identis non risponde, la richiesta non e' autorizzata
/// (`R_UNAUTHORIZED`); un token invalido resta `R_TOKEN_INVALID`.
fn whoami(token: u64) -> Result<u64, u64> {
    for _ in 0..50 {
        if let Ok(chan) = civis::service_lookup(civis::Service::Identis) {
            if chan > 0 {
                return match civis::ipc::send(chan as u64, civis::IDENTIS_WHOAMI, 0, token) {
                    Ok(r) if r.w0 == identis::R_OK => Ok(r.w1),
                    Ok(r) if r.w0 == identis::R_TOKEN_INVALID => Err(proto::R_TOKEN_INVALID),
                    Ok(_) => Err(proto::R_UNAUTHORIZED),
                    Err(_) => Err(proto::R_UNAUTHORIZED),
                };
            }
        }
        let _ = civis::futex_wait(
            &IDENTIS_WAIT as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    Err(proto::R_UNAUTHORIZED)
}

static IDENTIS_WAIT: u32 = 0;

// ── Utility ────────────────────────────────────────────────────────────────

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

fn reply(tag: u64, w0: u64, w1: u64) {
    let _ = civis::reply(tag, w0, w1);
}

fn now_tick() -> u64 {
    let t = civis::get_ticks();
    if t < 0 { 0 } else { t as u64 }
}

fn parse_hex(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for b in s.bytes() {
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

fn parse_dec(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0u64;
    for b in s.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(v)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    civis::println!("[custos] panic");
    civis::exit(1)
}
