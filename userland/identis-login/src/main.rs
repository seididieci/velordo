//! Programma login (ADR-0055, Fase 3.1): chiede utente e password, calcola
//! l'hash BLAKE2s-256 **in locale** e lo verifica tramite Identis via IPC.
//!
//! La password non transita MAI in chiaro: nel payload IPC c'e' solo
//! l'hash. La password digitata non viene neanche ecoata a schermo (era un
//! buco: finiva in chiaro nel log seriale).
//!
//! Output sulla **sessione di terminale** (`civis::sess::out`, D8c2: il
//! mirror seriale lo fa il server), input dalla sessione (D8d).
//!
//! **D8f — il login e' il capo-sessione**: dopo l'autenticazione il nome va
//! nel **server** (`SESS_AUTH_SET`, sulla sessione 0) e il login spawma il
//! DE (`impluvium`) dentro la sessione, con il solo `USER` in ambiente. Resta
//! vivo in `recv` finche' il DE non muore (= fine sessione), poi azzera
//! l'identita' della 0 ed esce 0: `init` (politica `LoginGate`) riapre subito
//! il prompt. Non e' piu' un lanciatore one-shot e il token non viaggia in env.
//!
//! Il framing del protocollo e' condiviso (`libs/identis-proto`): qui c'e'
//! solo il lato client.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use blake2s::blake2s;
use civis;
use identis_proto as proto;

civis::entry!(real_main);

/// Nome utente dell'ultimo login riuscito. Passato al DE come `USER` (un
/// nome, non un segreto: il prompt con glifi Nerd Font lo mostra).
#[allow(dead_code)]
static mut LOGIN_USER: [u8; 32] = [0u8; 32];

/// Lunghezza del nome utente salvato (0 = non impostato).
#[allow(dead_code)]
static mut LOGIN_USER_LEN: u8 = 0;

/// Scrive sul terminale: la **sessione** del processo (`civis::sess::out`,
/// D8c2). Il mirror seriale lo fa il server; se la sessione non e'
/// raggiungibile (console non ancora su) si cade sul seriale: l'output non si
/// perde mai.
fn console_write(data: &[u8]) {
    if !civis::sess::out(data) {
        let _ = civis::print_string(data);
    }
}

/// Entry: fa il self-test, poi chiede credenziali.
fn real_main(_sp: u64) -> ! {
    // Pulisce la sessione 0: il login non deve lasciare i messaggi di una
    // sessione precedente (es. dopo un logout) sullo schermo quando riparte.
    // `0x0C` (form feed) e' il clear supportato dal modello (`littera`:
    // `grid.clear()` + cursore a 0,0 e tutte le celle sporche); `CSI 2J` non
    // e' implementato. out_screen non entra nel mirror seriale.
    let _ = civis::sess::out_screen(b"\x0C");

    // Registra il servizio: serve a **init** per supervisionarlo (e'
    // l'unico modo per entrare nel sistema: se il login fallisce, init lo
    // riavvia come un getty). Nessuno lo risolve per nome.
    if civis::service_register(civis::Service::Login).is_ok() {
        let _ = civis::print_string(b"[login] registered as service Login\n");
    } else {
        let _ = civis::print_string(b"[login] FAILED to register service Login\n");
    }
    // READY a init: da qui init ricava il pid (dal servizio registrato) e lo
    // supervisiona.
    civis::signal_ready(1);

    console_write(b"[login] Velordo login\n");

    let chan = match resolve_identis() {
        Ok(c) => c,
        Err(_) => {
            console_write(b"[login] ERRORE: servizio Identis non disponibile\n");
            civis::exit(1);
        }
    };

    // ---- Self-test automatico: root/root via IPC ----
    console_write(b"[login] Self-test: root/root\n");
    let test_hash = blake2s(b"root");
    let payload = proto::build_verify(DEFAULT_USER, &test_hash);
    match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) if r.ok() => console_write(b"[login] Self-test: SUCCESS (root/root valido)\n"),
        Ok(r) => {
            console_write(b"[login] Self-test: FAIL (");
            console_write(r.describe().as_bytes());
            console_write(b")\n");
        }
        Err(_) => console_write(b"[login] Self-test: ERRORE IPC\n"),
    }

    // ---- Login interattivo ----
    //
    // Il login e' il **gate**: la shell parte **solo** se l'autenticazione
    // va a buon fine. Prima partiva sempre, qualunque cosa dicesse la
    // verifica: un login fallito apriva la shell come se fosse riuscito
    // (e l'utente entrava senza autenticarsi).
    //
    // Si prova piu' volte (l'utente sbaglia la password): esauriti i
    // tentativi il processo esce con codice != 0 e **init lo riavvia** (come
    // un getty): l'utente trova un altro prompt invece di un sistema muto.
    for attempt in 1..=MAX_ATTEMPTS {
        console_write(b"\n[login] Nome utente: ");
        // D8c2 — il **contratto del gate**: il prompt e' stato accettato e
        // disegnato dal server (la write sopra ha avuto reply), quindi da qui
        // in poi il terminale e' pronto. Un marker esplicito, non la stringa
        // di UX: la aspetta `run-tests.sh` (e `shell_harness.py`).
        if attempt == 1 {
            let _ = civis::print_string(b"[gate] boot: login-ready\n");
        }
        let name = read_line_from_keyboard(false);
        console_write(b"\n[login] Password: ");
        let pwd = read_line_from_keyboard(true);
        console_write(b"\n");

        // L'hash e' calcolato in locale: la password non lascia mai il processo.
        let hash = blake2s(pwd.as_bytes());
        let payload = proto::build_verify(&name, &hash);

        match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
            Ok(r) if r.ok() => {
                // Il nome utente vive nel server (S3.3c): niente bearer, ma il
                // nome serve anche al DE (prompt). Lo salva qui e lo passa in
                // ambiente (un nome, non un segreto).
                let name_bytes = name.as_bytes();
                unsafe {
                    let n = name_bytes.len().min(31);
                    LOGIN_USER_LEN = n as u8;
                    for i in 0..n {
                        LOGIN_USER[i] = name_bytes[i];
                    }
                }
                console_write(b"[login] SUCCESS\n");
                // D8f — l'identita' e' della **sessione**, non dell'ambiente:
                // il nome va nel server, sulla sessione a cui il login e'
                // attaccato (la 0). Da qui in poi `users`/`create-user` lo
                // leggono con `SESS_AUTH` (attestazione), non da una
                // variabile d'ambiente che chiunque puo' leggere o perdere.
                if civis::sess::set_auth(name_bytes).is_err() {
                    console_write(b"[login] ERRORE: identita' di sessione non registrata\n");
                }
                // Il login e' il **capo-sessione**: lancia il DE dentro la
                // sessione e resta vivo. Non avvia piu' la shell: e' il DE a
                // creare le tile e le shell (una per tile).
                match spawn_de() {
                    Some((_chan, pid)) => {
                        wait_session_end(pid);
                        // Fine sessione: l'identita' muore con lei (niente
                        // identita' stantia alla prossima autenticazione).
                        let _ = civis::sess::set_auth(b"");
                        console_write(b"[login] Sessione conclusa\n");
                        civis::exit(0);
                    }
                    None => {
                        console_write(b"[login] ERRORE: DE non avviato\n");
                        civis::exit(1);
                    }
                }
            }
            Ok(r) => {
                console_write(b"[login] FAIL (");
                console_write(r.describe().as_bytes());
                console_write(b")\n");
            }
            Err(_) => console_write(b"[login] ERRORE IPC\n"),
        }
        if attempt < MAX_ATTEMPTS {
            console_write(b"[login] Credenziali non valide: si riprova\n");
        }
    }

    // Falliti tutti i tentativi: **niente shell**. Il codice di uscita != 0
    // dice a init che il login ha fallito (init lo riavvia).
    console_write(b"[login] Tentativi esauriti: accesso negato\n");
    civis::exit(1);
}

/// Tentativi di login prima di rinunciare (l'utente ha diritto a sbagliare
/// la password: il login riprova, non chiude).
const MAX_ATTEMPTS: u32 = 3;

/// Spawna il **DE** (`impluvium`) dall'oggetto ArcaFS
/// (`sys:bin/impluvium.bin`), dentro la sessione autenticata: passa solo
/// `USER` (il token e' gia' nel server, sulla sessione 0). Ritorna
/// `(canale, pid)`; `None` = non avviato (motivo gia' scritto sul terminale).
///
/// **Verifica prima dello spawn** (ADR-0078/0079, come `init`): la classe di
/// un'immagine arriva al kernel dalla trust table, che cardo popola quando
/// qualcuno **verifica** l'oggetto (`R_RIGHTS_EXECUTE`). Senza, il DE
/// partirebbe `Untrusted` e non potrebbe ne' prendere i tasti ne' disporre il
/// layout. Fail-closed e loud.
///
/// Il DE **non** e' detached: e' un figlio del login, e la sua morte e' il
/// segnale di fine sessione che il login aspetta.
fn spawn_de() -> Option<(i64, i64)> {
    match civis::fs::rights::rights_execute_verdict(b"sys", b"bin/impluvium.bin") {
        Ok((true, _class)) => {}
        Ok((false, _)) => {
            console_write(b"[login] DE non eseguibile (firma assente o negata)\n");
            return None;
        }
        Err(e) => {
            console_write(b"[login] verifica DE fallita: ");
            let msg = format!("{:?}", e);
            console_write(msg.as_bytes());
            console_write(b"\n");
            return None;
        }
    }

    let img = match civis::obj_get(b"sys", b"bin/impluvium.bin") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            console_write(b"[login] DE non trovato su ArcaFS\n");
            return None;
        }
    };

    let user_bytes = unsafe { &LOGIN_USER[..LOGIN_USER_LEN as usize] };
    let user_str = core::str::from_utf8(user_bytes).unwrap_or("");

    let mut meta = match civis::SpawnMeta::new("impluvium", 16, &[]) {
        Some(m) => m,
        None => {
            console_write(b"[login] SpawnMeta non valido\n");
            return None;
        }
    };

    // Env: solo `USER` (un nome, non un segreto). Il token sta nel server.
    let user_env = alloc::format!("USER={}", user_str);
    let Some(packed) = civis::pack_args(&[], &[&user_env.as_bytes()]) else {
        console_write(b"[login] DE: env non impacchettabile\n");
        return None;
    };
    meta = meta.argv(&packed);

    let chan = match civis::spawn_image(&img, &meta) {
        Ok(c) => c,
        Err(_) => {
            console_write(b"[login] spawn del DE fallito\n");
            return None;
        }
    };
    let pid = civis::peer_pid(chan as u64).ok()?;
    console_write(b"[login] DE avviato\n");
    Some((chan, pid))
}

/// Aspetta la **fine della sessione**: la morte del DE. Ignora gli altri
/// messaggi (READY del DE, hint di input residui): l'unico evento che conta
/// qui e' l'`EXIT_NOTIFY` del figlio.
fn wait_session_end(de_pid: i64) {
    loop {
        if let Ok(m) = civis::recv() {
            if civis::is_exit_notify(&m) && m.w1 as i64 == de_pid {
                return;
            }
        }
    }
}

/// Utente di default usato dal self-test.
const DEFAULT_USER: &str = "root";

/// Tentativi in caso di servizio occupato (`R_BUSY`).
const RETRIES: u32 = 20;

/// Legge una riga terminata da newline dalla tastiera. Con `mask` l'eco e'
/// mascherato (password: niente in chiaro nel log).
///
/// E0 (ADR-0082): sottoscrizione a `porta` (focus LIFO) e attesa **bloccante**
/// su `INPUT_NOTIFY` invece del polling a tick. `attach` e' idempotente: il
/// primo campo paga l'handshake, i successivi sono no-op.
fn read_line_from_keyboard(mask: bool) -> String {
    if civis::input::attach().is_err() {
        console_write(b"[login] ERRORE: impossibile aprire tastiera\n");
        civis::exit(1);
    }

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let n = civis::input::read(&mut byte);
        if n == 0 {
            // Niente di nuovo: dorme su un evento (mai un tick a vuoto).
            if !civis::input::wait() {
                console_write(b"[login] ERRORE: input della tastiera perso\n");
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
                // L'eco della cancellazione **anche** nel campo non
                // mascherato (il nome utente): senza, lo schermo mostra
                // piu' caratteri di quanti ne restino nel buffer — e il
                // login autenticava una stringa diversa da quella mostrata.
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
    for _ in 0..50 {
        match civis::service_lookup(civis::Service::Identis) {
            Ok(c) => return Ok(c as u64),
            Err(_) => {
                // Niente di nuovo: dormi 100ms (100 tick a 100 Hz) invece di
                // busy-waitare — la CPU torna al resto del sistema.
                static FUTEX_WORD: u32 = 0;
                let _ = civis::futex_wait(&FUTEX_WORD as *const u32 as usize, 0, (civis::get_ticks() + 100) as u64);
            }
        }
    }
    Err("Identis non trovato")
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    console_write(b"[login] panic\n");
    civis::exit(1)
}
