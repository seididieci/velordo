//! Programma login (ADR-0055, Fase 3.1): chiede utente e password, calcola
//! l'hash BLAKE2s-256 **in locale** e lo verifica tramite Identis via IPC.
//!
//! La password non transita MAI in chiaro: nel payload IPC c'e' solo
//! l'hash. La password digitata non viene neanche ecoata a schermo (era un
//! buco: finiva in chiaro nel log seriale).
//!
//! Output su /dev/console (VGA + serial), input da /dev/input/keyboard.
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
use civis::println;
use identis_proto as proto;

civis::entry!(real_main);

const CONSOLE_PATH: &str = "/dev/console";

/// Token di sessione dell'ultimo login riuscito. Lo userà la shell nativa
/// (Fase 3.2) per passare la sessione ai programmi che ne hanno bisogno
/// (es. `create-user`): per ora e' solo custodito qui.
#[allow(dead_code)]
static mut SESSION_TOKEN: u64 = 0;

/// Nome utente dell'ultimo login riuscito. Passato alla shell come
/// `VELORDO_USER` per il prompt con glifi Nerd Font.
#[allow(dead_code)]
static mut LOGIN_USER: [u8; 32] = [0u8; 32];

/// Lunghezza del nome utente salvato (0 = non impostato).
#[allow(dead_code)]
static mut LOGIN_USER_LEN: u8 = 0;

/// Scrive su console (VGA + serial).
static mut CONSOLE_FD: i64 = -1;

fn console_write(data: &[u8]) {
    unsafe {
        let _ = civis::write_fs(CONSOLE_FD, data, data.len());
    }
    let s = core::str::from_utf8(data).unwrap_or("");
    civis::print_str(s);
    // Flush SEMPRE: la console e' interattiva (un prompt senza newline
    // resterebbe nel buffer seriale e nessuno lo vedrebbe).
    civis::flush();
}

/// Entry: apre console e tastiera, fa il self-test, poi chiede credenziali.
fn real_main(_sp: u64) -> ! {
    let console_fd = match civis::open(CONSOLE_PATH, 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[login] ERRORE: impossibile aprire console");
            civis::exit(1);
        }
    };
    unsafe { CONSOLE_FD = console_fd; }

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
        let name = read_line_from_keyboard(false);
        console_write(b"\n[login] Password: ");
        let pwd = read_line_from_keyboard(true);
        console_write(b"\n");

        // L'hash e' calcolato in locale: la password non lascia mai il processo.
        let hash = blake2s(pwd.as_bytes());
        let payload = proto::build_verify(&name, &hash);

        match proto::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
            Ok(r) if r.ok() => {
                unsafe {
                    SESSION_TOKEN = r.w1;
                    // Salva il nome utente per la shell (prompt Nerd Font).
                    let bytes = name.as_bytes();
                    LOGIN_USER_LEN = bytes.len().min(31) as u8;
                    for i in 0..LOGIN_USER_LEN as usize {
                        LOGIN_USER[i] = bytes[i];
                    }
                }
                console_write(b"[login] SUCCESS\n");
                // La shell prende il posto del login: il login e' il programma
                // di autenticazione, non il terminale. La shell e' la **nativa**
                // (Fase 3.2, S3.2a): eredita la sessione da `VELORDO_SESSION`
                // e offre `help`, `users`, `create-user`, `logout`, `exit`.
                // Il binario e' spawnato *detached*: quando il login esce, la
                // shell viene ri-parentata a init (senza, la cascata della
                // morte del parent la ucciderebbe).
                spawn_shell();
                civis::exit(0);
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

/// Spawla l'interprete nativa dall'oggetto ArcaFS (`sys:bin/interpres.bin`).
/// Passa le variabili d'ambiente `VELORDO_SESSION` (token hex) e
/// `VELORDO_USER` (nome utente): servono alla shell per il prompt con
/// glifi Nerd Font.
///
/// **Verifica prima dello spawn** (ADR-0078/0079, come `init`): la classe di
/// un'immagine arriva al kernel dalla trust table, che cardo popola quando
/// qualcuno **verifica** l'oggetto (`R_RIGHTS_EXECUTE`). Senza questa
/// chiamata il login spawnava la shell come `Untrusted`: `ceiling_for` le
/// dava `DEFAULT_UNKNOWN_OPS` (che non include `RIGHTS_EXECUTE`) e ogni
/// esecuzione di un programma da `/usr/bin` falliva con un `ERR` opaco.
/// Fail-closed: se l'oggetto non e' eseguibile la shell non parte e il
/// motivo e' scritto sul terminale.
fn spawn_shell() {
    match civis::fs::rights::rights_execute_verdict(b"sys", b"bin/interpres.bin") {
        Ok((true, _class)) => {}
        Ok((false, _)) => {
            console_write(b"[login] shell nativa non eseguibile (firma assente o negata)\n");
            return;
        }
        Err(e) => {
            console_write(b"[login] verifica shell fallita: ");
            let msg = format!("{:?}", e);
            console_write(msg.as_bytes());
            console_write(b"\n");
            return;
        }
    }

    let img = match civis::obj_get(b"sys", b"bin/interpres.bin") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            console_write(b"[login] shell nativa non trovata su ArcaFS\n");
            return;
        }
    };

    let token = unsafe { SESSION_TOKEN };
    let token_hex = format!("{:016x}", token);

    // Costruisci il nome utente come byte slice.
    let user_bytes = unsafe { &LOGIN_USER[..LOGIN_USER_LEN as usize] };
    let user_str = core::str::from_utf8(user_bytes).unwrap_or("");

    // Costruisce i metadati, l'env block e spawn. L'env block deve restare
    // vivo fino a dopo lo spawn (il kernel copia il blocco dentro la syscall).
    let mut meta = match civis::SpawnMeta::new("interpres", 16, &[]) {
        Some(m) => m,
        None => {
            console_write(b"[login] SpawnMeta non valido\n");
            return;
        }
    };

    // Env: VELORDO_SESSION + USER. pack_args accetta un array
    // di slice (più env). L'interprete li legge con env_from_stack().
    let session_env = alloc::format!("VELORDO_SESSION={}", token_hex);
    let user_env = alloc::format!("USER={}", user_str);

    if let Some(packed) = civis::pack_args(&[], &[&session_env.as_bytes(), &user_env.as_bytes()]) {
        meta = meta.argv(&packed);
        match civis::spawn_image(&img, &meta.detached()) {
            Ok(_chan) => console_write(b"[login] shell nativa avviata\n"),
            Err(_) => console_write(b"[login] spawn della shell fallito\n"),
        }
    } else {
        // pack_args fallisce: spawn senza env (la shell dira' "nessuna sessione")
        match civis::spawn_image(&img, &meta.detached()) {
            Ok(_chan) => console_write(b"[login] shell nativa avviata\n"),
            Err(_) => console_write(b"[login] spawn della shell fallito\n"),
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
