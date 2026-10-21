//! identistest — gate di regressione del servizio Identis (ADR-0055,
//! Fase 3.1).
//!
//! Verifica il protocollo IPC di autenticazione e creazione utenti:
//! sessione (token), verifica credenziali, creazione utente, duplicati,
//! nomi non conformi, autorizzazione (token non root), elenco utenti e
//! contenuto del bucket `identis:config`.
//!
//! Il volume ArcaFS viene ricreato a ogni run (`scripts/arca-tool.sh`), quindi
//! il bucket parte pulito: l'utente `alice` creato qui non collide con run
//! precedenti.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::format;
use alloc::string::String;

use blake2s::blake2s;
use civis;
use civis::println;
use identis_proto as proto;

civis::entry!(real_main);

/// Bucket con le credenziali (letto dal test per verifica del formato).
const BUCKET_PATH: &str = "identis:config";
/// Utente di default (seed del volume).
const ROOT_USER: &str = "root";
/// Password dell'utente di default.
const ROOT_PWD: &str = "root";
/// Utente creato dal test.
const NEW_USER: &str = "alice";
/// Password del nuovo utente.
const NEW_PWD: &str = "segreto";
/// Tentativi in caso di servizio occupato (`R_BUSY`).
const RETRIES: u32 = 20;

/// Entry: esegue i casi e segnala TEST_DONE a init.
fn real_main(_sp: u64) -> ! {
    println!("[identistest] starting, pid={}", civis::getpid());
    let mut all_ok = true;

    // t1: il servizio Identis e' risolvibile per nome.
    let chan = match civis::service_lookup(civis::Service::Identis) {
        Ok(c) => {
            println!("[identistest] t1: lookup Identis: PASS (chan={})", c);
            c as u64
        }
        Err(_) => {
            println!("[identistest] t1: lookup Identis: FAIL");
            finish(false);
        }
    };

    // t2: credenziali giuste -> SUCCESS + token di sessione.
    let root_hash = blake2s(ROOT_PWD.as_bytes());
    let payload = proto::build_verify(ROOT_USER, &root_hash);
    let token = match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) if r.ok() && r.w1 != 0 => {
            println!("[identistest] t2: verify root/root: PASS (sessione emessa)");
            r.w1
        }
        Ok(r) => {
            println!("[identistest] t2: verify root/root: FAIL ({})", r.describe());
            all_ok = false;
            0
        }
        Err(_) => {
            println!("[identistest] t2: verify root/root: FAIL (IPC)");
            all_ok = false;
            0
        }
    };

    // t3: password sbagliata -> FAIL.
    let bad_hash = blake2s(b"password-sbagliata");
    let payload = proto::build_verify(ROOT_USER, &bad_hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES);
    all_ok &= check(3, "verify con password errata", match r {
        Ok(r) => r.w0 == proto::R_FAIL,
        Err(_) => false,
    });

    // t4: creazione utente SENZA token -> rifiutata.
    let hash = blake2s(NEW_PWD.as_bytes());
    let payload = proto::build_add_user(0, NEW_USER, &hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        4,
        "add_user senza token",
        matches!(r, Ok(r) if r.w0 == proto::R_TOKEN_INVALID),
    );

    // t5: creazione utente con token valido -> OK.
    let payload = proto::build_add_user(token, NEW_USER, &hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(5, "add_user alice", matches!(r, Ok(r) if r.ok()));

    // t6: stesso nome di nuovo -> gia' esistente.
    let payload = proto::build_add_user(token, NEW_USER, &hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        6,
        "add_user duplicato",
        matches!(r, Ok(r) if r.w0 == proto::R_EXISTS),
    );

    // t7: nome non conforme (maiuscole e caratteri fuori charset).
    let payload = proto::build_add_user(token, "Bob!", &hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        7,
        "add_user nome non valido",
        matches!(r, Ok(r) if r.w0 == proto::R_NAME_INVALID),
    );

    // t8: nome vuoto -> non valido.
    let payload = proto::build_add_user(token, "", &hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        8,
        "add_user nome vuoto",
        matches!(r, Ok(r) if r.w0 == proto::R_NAME_INVALID),
    );

    // t9: il nuovo utente puo' autenticarsi con la password scelta.
    let payload = proto::build_verify(NEW_USER, &hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES);
    all_ok &= check(
        9,
        "verify alice/segreto",
        matches!(r, Ok(r) if r.ok()),
    );

    // t10: elenco utenti (LIST) coerente.
    let r = civis::ipc::send(chan, civis::IDENTIS_LIST, 0, token);
    all_ok &= check(
        10,
        "list utenti",
        matches!(r, Ok(r) if r.w0 == proto::R_OK && r.w1 >= 2),
    );

    // t11: il bucket contiene la riga dell'utente nuovo nel formato atteso.
    // NOTA: legge diretta del bucket — valida finche' il bucket non e'
    // protetto (debito dichiarato in ADR-0056, chiusura Fase 3.3: da li'
    // questo caso passa per LIST).
    let expected = format!("{}:{}", NEW_USER, hex_encode(&hash));
    all_ok &= check(
        11,
        "bucket contiene alice",
        bucket_contains(&expected),
    );

    if all_ok {
        println!("[identistest] PASS 11/11");
    } else {
        println!("[identistest] FAIL");
    }
    println!("[identistest] all tests done");
    finish(all_ok)
}

/// Chiude la sequenza: segnala TEST_DONE a init (SEMPRE, anche in caso di
/// fallimento: init aspetta quel messaggio e la suite si bloccherebbe) e
/// esce con il codice dell'esito.
fn finish(ok: bool) -> ! {
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(if ok { 0 } else { 1 })
}

/// Verifica un caso e stampa l'esito.
fn check(n: usize, name: &str, ok: bool) -> bool {
    if ok {
        println!("[identistest] t{}: {}: PASS", n, name);
    } else {
        println!("[identistest] t{}: {}: FAIL", n, name);
    }
    ok
}

/// Il bucket contiene la riga attesa?
fn bucket_contains(expected: &str) -> bool {
    let fd = match civis::open(BUCKET_PATH, 0) {
        Ok(f) => f,
        Err(_) => return false,
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
    out.lines().any(|l| l == expected)
}

/// Codifica esadecimale minuscola.
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
    println!("[identistest] panic");
    // Chiude la sequenza anche in panic: init aspetta TEST_DONE.
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
