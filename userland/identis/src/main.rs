//! Servizio Identis (ADR-0055): identita' e autenticazione utenti.
//!
//! Detiene il bucket ArcaFS `identis:config` (nome utente -> hash
//! BLAKE2s-256 della password). Espone tag IPC per uso futuro:
//!   IDENTIS_VERIFY   (w0=0) -> w0=1 (non implementato via IPC, usa il bucket direttamente)
//!   IDENTIS_ADD_USER (w0=0) -> w0=1 (non implementato via IPC)
//!   IDENTIS_LIST     (w0=0) -> w1=numero utenti
//!
//! Al primo avvio, se il bucket e' assente o vuoto, lo crea con l'utente
//! `root` e l'hash di `password` (seed host-side, vedi scripts/arca-tool.sh).
//!
//! NOTA: nella prima implementazione il programma login legge il bucket
//! direttamente; il servizio Identis sara' usato via IPC in una fase
//! successiva, quando il meccanismo di passaggio dati (ring SPSC) sara'
//! integrato.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::format;
use alloc::string::String;

use civis;

civis::entry!(real_main);

const BUCKET_PATH: &str = "identis:config";
const DEFAULT_USER: &str = "root";
const DEFAULT_HASH_HEX: &str = "0cb8a879dc5d94ed67eb49cc7a4d3052d6a346f2e28fcd9e10e5822aba92eb39";

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
        match civis::recv() {
            Ok(m) if m.tag == civis::IDENTIS_VERIFY => {
                // Non implementato via IPC nella prima versione: il client
                // legge il bucket direttamente. Risposta "non disponibile".
                let _ = civis::reply(civis::IDENTIS_VERIFY, 1, 0);
            }
            Ok(m) if m.tag == civis::IDENTIS_ADD_USER => {
                let _ = civis::reply(civis::IDENTIS_ADD_USER, 1, 0);
            }
            Ok(m) if m.tag == civis::IDENTIS_LIST => {
                let n = handle_list();
                let _ = civis::reply(civis::IDENTIS_LIST, n, 0);
            }
            Ok(m) if m.tag == civis::EXIT_NOTIFY => {
                // Morte di un peer: nessun stato per-client da pulire.
            }
            Ok(_) => {
                let _ = civis::reply(civis::IDENTIS_VERIFY, u64::MAX, 0);
            }
            Err(_) => {}
        }
    }
}

/// Ritorna il numero di utenti nel bucket.
fn handle_list() -> u64 {
    match read_bucket_all() {
        Ok(entries) => entries.len() as u64,
        Err(_) => 0,
    }
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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    civis::println!("[identis] panic");
    civis::exit(1)
}
