//! Servizio Identis (ADR-0055): identita' e autenticazione utenti.
//!
//! Detiene il bucket ArcaFS `identis:config` (nome utente -> hash
//! BLAKE2s-256 della password). Espone il tag IPC `IDENTIS_VERIFY`
//! (0x70) per la verifica delle credenziali.
//!
//! Protocollo IPC (messaggi sincroni su canale di nascita):
//!   1° msg: IDENTIS_VERIFY, w0=total_len, w1=0          (annuncia dimensione)
//!   2°..N°: IDENTIS_VERIFY, w0=offset,  w1=chunk_4B     (payload a pezzi)
//!   Ultimo: IDENTIS_VERIFY, w0=0xFFFFFFFF (EOF), w1=0   (fine trasmissione)
//!   Reply:  IDENTIS_VERIFY, w0=0 (SUCCESS) o 1 (FAIL)
//!
//! Anti-wait-infinito: Identis non fa recv bloccante durante la ricezione.
//! Usa recv_poll + get_ticks con timeout (5 s di inattivita'). Al timeout
//! o alla morte del client (EXIT_NOTIFY) scatta la transazione e torna in IDLE.

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

extern crate alloc;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use civis;
use civis::ipc;

civis::entry!(real_main);

const BUCKET_PATH: &str = "identis:config";
const DEFAULT_USER: &str = "root";
const DEFAULT_HASH_HEX: &str = "0cb8a879dc5d94ed67eb49cc7a4d3052d6a346f2e28fcd9e10e5822aba92eb39";

/// Tag IPC per Identis.
const IDENTIS_VERIFY: u64 = 0x70;
/// Marker EOF per fine trasmissione.
const MSG_EOF: u64 = 0xFFFFFFFF;
/// Timeout inattivita' (tick PIT, 100 Hz = 5 s).
const TIMEOUT_TICKS: u64 = 500;
/// Dimensione massima payload accettata.
const MAX_PAYLOAD: usize = 4096;

/// Stati della macchina a stati di ricezione.
const STATE_IDLE: u8 = 0;
const STATE_RECEIVING: u8 = 1;
const STATE_COMPLETE: u8 = 2;
const STATE_RESPONDING: u8 = 3;

/// Stato globale della transazione (campi statici mutabili).
static mut TXN_STATE: u8 = STATE_IDLE;
static mut TXN_TOTAL_LEN: u64 = 0;
static mut TXN_RECEIVED: u64 = 0;
static mut TXN_BUF: [u8; MAX_PAYLOAD] = [0; MAX_PAYLOAD];
static mut TXN_BUF_LEN: usize = 0;
static mut TXN_LAST_TICK: u64 = 0;
static mut TXN_CLIENT_CHAN: u64 = 0;
static mut TXN_ACTIVE: bool = false;

/// Resetta lo stato della transazione corrente.
fn reset_txn() {
    unsafe {
        TXN_STATE = STATE_IDLE;
        TXN_TOTAL_LEN = 0;
        TXN_RECEIVED = 0;
        TXN_BUF_LEN = 0;
        TXN_LAST_TICK = 0;
        TXN_CLIENT_CHAN = 0;
        TXN_ACTIVE = false;
    }
}

/// Entry: registra il servizio Identis, carica/crea il bucket, entra nel loop
/// di ricevimento richieste IPC.
fn real_main(_sp: u64) -> ! {
    civis::println!("[identis] avvio servizio Identis, pid={}", civis::getpid());

    if civis::service_register(civis::Service::Identis).is_err() {
        civis::println!("[identis] FAILED: service_register");
        civis::exit(1);
    }
    civis::println!("[identis] registrato come servizio Identis");

    if let Err(e) = ensure_bucket() {
        civis::println!("[identis] FAILED: ensure_bucket err={:?}", e);
        civis::exit(1);
    }

    civis::signal_ready(1);
    civis::println!("[identis] pronto");

    loop {
        tick();
    }
}

/// Un tick del loop principale: prova a ricevere un messaggio (non bloccante)
/// ed eventualmente avanza la transazione.
fn tick() {
    // Prova a ricevere un messaggio (non blocca).
    match civis::ipc::recv_poll() {
        Some(m) => handle_msg(m),
        None => {} // nessun messaggio in attesa
    }

    // Gestione timeout se siamo in Receiving.
    unsafe {
        if TXN_ACTIVE && TXN_STATE == STATE_RECEIVING {
            let now = civis::get_ticks() as u64;
            if now.saturating_sub(TXN_LAST_TICK) > TIMEOUT_TICKS {
                civis::println!(
                    "[identis] timeout ricezione da chan {} ({} byte attesi, {} ricevuti)",
                    TXN_CLIENT_CHAN, TXN_TOTAL_LEN, TXN_RECEIVED
                );
                reset_txn();
            }
        }
    }
}

/// Gestisce un messaggio IPC ricevuto.
fn handle_msg(m: civis::IpcMsg) {
    match m.tag {
        IDENTIS_VERIFY => handle_verify_msg(m),
        civis::EXIT_NOTIFY => {
            // Morte di un peer: scatta la transazione se era il nostro client.
            unsafe {
                if TXN_ACTIVE && TXN_CLIENT_CHAN == m.channel {
                    civis::println!(
                        "[identis] client {} morto, scatto transazione",
                        TXN_CLIENT_CHAN
                    );
                    reset_txn();
                }
            }
        }
        _ => {
            // Tag ignoto: ignora.
        }
    }
}

/// Gestisce un messaggio IDENTIS_VERIFY.
fn handle_verify_msg(m: civis::IpcMsg) {
    unsafe {
        if !TXN_ACTIVE {
            // Nessuna transazione in corso: questo e' il primo messaggio.
            if m.w1 != 0 {
                reply_error(m.channel, 1);
                return;
            }
            if m.w0 > MAX_PAYLOAD as u64 {
                civis::println!(
                    "[identis] payload troppo grande: {} byte",
                    m.w0
                );
                reply_error(m.channel, 1);
                return;
            }
            TXN_STATE = STATE_RECEIVING;
            TXN_TOTAL_LEN = m.w0;
            TXN_RECEIVED = 0;
            TXN_BUF_LEN = 0;
            TXN_LAST_TICK = civis::get_ticks() as u64;
            TXN_CLIENT_CHAN = m.channel;
            TXN_ACTIVE = true;
            reply_ok(m.channel);
            return;
        }

        // Transazione in corso.
        if TXN_STATE == STATE_RECEIVING {
            if m.w0 == MSG_EOF {
                // Fine trasmissione: elabora.
                complete_transaction();
            } else {
                // Chunk di 4 byte in w1 (little-endian).
                let offset = m.w0 as usize;
                let chunk = m.w1 as u32;
                if offset + 4 > TXN_TOTAL_LEN as usize {
                    // Ultimo chunk potrebbe essere < 4 byte.
                    let remaining = TXN_TOTAL_LEN as usize - offset;
                    if remaining == 0 || remaining > 4 {
                        reply_error(m.channel, 1);
                        return;
                    }
                    // Accetta solo i byte effettivi (gli altri sono 0).
                    for i in 0..remaining {
                        TXN_BUF[TXN_BUF_LEN + i] = ((chunk >> (8 * i)) & 0xFF) as u8;
                    }
                    TXN_BUF_LEN += remaining;
                } else {
                    for i in 0..4 {
                        TXN_BUF[TXN_BUF_LEN + i] = ((chunk >> (8 * i)) & 0xFF) as u8;
                    }
                    TXN_BUF_LEN += 4;
                }
                TXN_RECEIVED = TXN_BUF_LEN as u64;
                TXN_LAST_TICK = civis::get_ticks() as u64;
                reply_ok(m.channel);
            }
            // Stati COMPLETE, RESPONDING, IDLE: ignora.
            // Qualsiasi altro valore di stato: reset.
            let st = TXN_STATE;
            if st != STATE_IDLE && st != STATE_RECEIVING && st != STATE_COMPLETE && st != STATE_RESPONDING {
                reset_txn();
            }
        }
    }
}

/// Completa la transazione: parsa il payload e confronta con il bucket.
fn complete_transaction() {
    unsafe {
        // Verifica magic "ACLI" + versione.
        if TXN_BUF_LEN < 8 {
            reply_error(TXN_CLIENT_CHAN, 1);
            reset_txn();
            return;
        }
        if TXN_BUF[0] != b'A' || TXN_BUF[1] != b'C' || TXN_BUF[2] != b'L' || TXN_BUF[3] != b'I' {
            reply_error(TXN_CLIENT_CHAN, 1);
            reset_txn();
            return;
        }
        let _version = u32::from_le_bytes([TXN_BUF[4], TXN_BUF[5], TXN_BUF[6], TXN_BUF[7]]);

        let mut offset = 8;
        if TXN_BUF_LEN < offset + 4 {
            reply_error(TXN_CLIENT_CHAN, 1);
            reset_txn();
            return;
        }
        let name_len = u32::from_le_bytes([
            TXN_BUF[offset], TXN_BUF[offset + 1], TXN_BUF[offset + 2], TXN_BUF[offset + 3],
        ]) as usize;
        offset += 4;

        if TXN_BUF_LEN < offset + name_len {
            reply_error(TXN_CLIENT_CHAN, 1);
            reset_txn();
            return;
        }
        let name = core::str::from_utf8(&TXN_BUF[offset..offset + name_len]).unwrap_or("");
        offset += name_len;

        if TXN_BUF_LEN < offset + 4 {
            reply_error(TXN_CLIENT_CHAN, 1);
            reset_txn();
            return;
        }
        let hash_len = u32::from_le_bytes([
            TXN_BUF[offset], TXN_BUF[offset + 1], TXN_BUF[offset + 2], TXN_BUF[offset + 3],
        ]) as usize;
        offset += 4;

        if hash_len != 32 || TXN_BUF_LEN < offset + 32 {
            reply_error(TXN_CLIENT_CHAN, 1);
            reset_txn();
            return;
        }
        let mut provided_hash = [0u8; 32];
        for i in 0..32 {
            provided_hash[i] = TXN_BUF[offset + i];
        }

        // Carica il bucket e confronta.
        let ok = match load_bucket_entry(&name) {
            Some(stored_hash) => hex_encode(&provided_hash) == stored_hash,
            None => false,
        };

        civis::println!(
            "[identis] verifica utente '{}': {}",
            name,
            if ok { "SUCCESS" } else { "FAIL" }
        );

        // Risponde.
        let reply_w0 = if ok { 0 } else { 1 };
        let _ = civis::reply(IDENTIS_VERIFY, reply_w0, 0);

        // Pronto per la prossima transazione.
        TXN_STATE = STATE_RESPONDING;
        reset_txn();
    }
}

/// Risponde OK (w0=0) al client.
fn reply_ok(chan: u64) {
    let _ = civis::reply(IDENTIS_VERIFY, 0, 0);
}

/// Risponde errore (w0=1) al client.
fn reply_error(chan: u64, code: u64) {
    let _ = civis::reply(IDENTIS_VERIFY, code, 0);
}

/// Carica l'hash associato a un nome utente dal bucket.
fn load_bucket_entry(name: &str) -> Option<String> {
    let all = read_bucket_all().ok()?;
    for line in all.lines() {
        let mut parts = line.splitn(2, ':');
        let stored_name = parts.next()?;
        let stored_hash = parts.next()?;
        if stored_name == name {
            return Some(stored_hash.to_string());
        }
    }
    None
}

/// Legge l'intero bucket `identis:config` in una String.
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

/// Assicura che il bucket esista e contenga almeno l'utente root.
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

/// Codifica un array di byte in stringa esadecimale (minuscole).
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, &b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0xF) as usize];
    }
    String::from(core::str::from_utf8(&out).unwrap())
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    civis::println!("[identis] panic");
    civis::exit(1)
}
