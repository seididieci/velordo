//! identistest — gate di regressione del servizio Identis (ADR-0055,
//! Fase 3.1).
//!
//! Verifica il protocollo IPC di autenticazione e creazione utenti (S3.3c:
//! nessun token, l'identita' e' del pid attestato dal kernel): WHOAMI senza
//! identita', verifica credenziali, creazione utente, duplicati, nomi non
//! conformi, autorizzazione (identita' non root), elenco utenti e contenuto
//! del bucket `identis:config`.
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
use civis::hex;
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

    // t2: WHOAMI senza identita' -> R_NO_IDENTITY. Il test non ha fatto
    // nessun VERIFY e non e' in una sessione autenticata (gira prima del
    // login): la risposta e' "nessuna identita'", non un errore opaco.
    let r = civis::ipc::send(chan, civis::IDENTIS_WHOAMI, 0, 0);
    all_ok &= check(
        2,
        "whoami senza identita'",
        matches!(r, Ok(r) if r.w0 == proto::R_NO_IDENTITY),
    );

    // t3: credenziali giuste -> SUCCESS (l'identita' e' legata al pid).
    let root_hash = blake2s(ROOT_PWD.as_bytes());
    let payload = proto::build_verify(ROOT_USER, &root_hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES);
    all_ok &= check(3, "verify root/root", matches!(r, Ok(r) if r.ok()));

    // t4: password sbagliata -> FAIL, **senza** cambiare l'identita' gia'
    // stabilita: un tentativo fallito non deautentica ne' eleva.
    let bad_hash = blake2s(b"password-sbagliata");
    let payload = proto::build_verify(ROOT_USER, &bad_hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES);
    let failed_ok = matches!(r, Ok(r) if r.w0 == proto::R_FAIL);
    let still_root = matches!(
        civis::ipc::send(chan, civis::IDENTIS_WHOAMI, 0, 0),
        Ok(r) if r.w0 == proto::R_OK && r.w1 & proto::ROLE_ROOT != 0
    );
    all_ok &= check(4, "verify errata non cambia l'identita'", failed_ok && still_root);

    // t5: creazione utente con identita' root -> OK.
    let hash = blake2s(NEW_PWD.as_bytes());
    let payload = proto::build_add_user(NEW_USER, &hash, proto::ROLE_USER);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(5, "add_user alice", matches!(r, Ok(r) if r.ok()));

    // t6: stesso nome di nuovo -> gia' esistente.
    let payload = proto::build_add_user(NEW_USER, &hash, proto::ROLE_USER);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        6,
        "add_user duplicato",
        matches!(r, Ok(r) if r.w0 == proto::R_EXISTS),
    );

    // t7: nome non conforme (maiuscole e caratteri fuori charset).
    let payload = proto::build_add_user("Bob!", &hash, proto::ROLE_USER);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        7,
        "add_user nome non valido",
        matches!(r, Ok(r) if r.w0 == proto::R_NAME_INVALID),
    );

    // t8: nome vuoto -> non valido.
    let payload = proto::build_add_user("", &hash, proto::ROLE_USER);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        8,
        "add_user nome vuoto",
        matches!(r, Ok(r) if r.w0 == proto::R_NAME_INVALID),
    );

    // t9: il nuovo utente puo' autenticarsi e diventa l'identita' del pid.
    let payload = proto::build_verify(NEW_USER, &hash);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES);
    all_ok &= check(
        9,
        "verify alice/segreto",
        matches!(r, Ok(r) if r.ok()),
    );

    // t10: un'identita' non root non crea utenti (l'autorita' e' live).
    let payload = proto::build_add_user("t10boss", &hash, proto::ROLE_USER);
    let r = proto::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES);
    all_ok &= check(
        10,
        "add_user da identita' non root",
        matches!(r, Ok(r) if r.w0 == proto::R_UNAUTHORIZED),
    );

    // t11: elenco utenti (LIST) coerente: l'identita' e' attestata.
    let r = civis::ipc::send(chan, civis::IDENTIS_LIST, 0, 0);
    all_ok &= check(
        11,
        "list utenti",
        matches!(r, Ok(r) if r.w0 == proto::R_OK && r.w1 >= 2),
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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[identistest] panic");
    // Chiude la sequenza anche in panic: init aspetta TEST_DONE.
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
