//! usertestcli — helper generico della test suite (testland).
//!
//! La modalita' d'uso e' decisa dall'orchestratore (usertests) via IPC:
//! il primo messaggio ricevuto e' una CFG con `w0=mode`:
//!   - 0 ECHO:     `w1` round di `send(T_REQ, payload)`; ogni reply deve essere
//!                 `2*payload` (identita' per client → rileva cross-talk).
//!   - 1 ZEROREAD: apre `/dev/zero` (con retry) e fa `w1` read da 4096 byte,
//!                 verificando che siano tutti zeri.
//!   - 2 NULLW:    apre `/dev/null`, scrive 512 byte, verifica read==0.
//!   - 3 SRV:      server echo per IPC async (Fase 13): riceve richieste
//!                 `T_REQ` (anche multiple in volo via `send_async` del parent)
//!                 e risponde con reply implicita (`2*w0`); quando arriva
//!                 `T_STOP` termina con `T_DONE`.
//!   - 4 CHURN:    (Fase 14) riserva `w1` KiB di heap, li tocca tutti e poi
//!                 `exit(0)` SENZA T_DONE: il parent osserva il riuso dei PID
//!                 e il no-leak dai frame (notifica EXIT_NOTIFY).
//!   - 5 KILLME:   (Fase 14) resta in busy-wait finche' il parent lo kill(); il
//!                 parent osserva la notifica EXIT_NOTIFY con il code del kill.
//!   - 6 SRVDIE:   (Fase 14, t24) server sacrificale: registra il servizio
//!                 `Test`, poi busy-wait SENZA mai fare recv (i messaggi si
//!                 accumulano, i send sync restano bloccati). Il parent lo
//!                 killa: TUTTI i peer (parent + client via lookup) ricevono
//!                 EXIT_NOTIFY (notifica unificata).
//!   - 7 SYNCWAIT: (Fase 14, t24) client sync sacrificale: risolve `Test`,
//!                 avvisa il parent con T_READY (handshake: il parent killa
//!                 solo dopo, a lookup avvenuto), poi fa send SYNC verso il
//!                 server e resta bloccato. Sbloccato con Err alla morte del
//!                 server, attende l'EXIT_NOTIFY unificata, poi attende il
//!                 via-libera T_GO del parent e riporta T_DONE(w0=1, w1=code).
//!   - 8 MNTDIE:   (Fase 14, t25) driver sacrificale: registra il prefix
//!                 "/dev/tdie" presso cardo e serve il minimo (DEV_OPEN → fake
//!                 fd, DEV_CLOSE → ok). T_READY(w0=1) a registrazione avvenuta.
//!                 Il parent lo killa: cardo deve purgare il mount (altrimenti
//!                 lo stale avvelena resolve_mount anche dopo re-registrazione).
//!   - 9 OPENDIE:  (Fase 14, t26) client sacrificale: apre /dev/null +
//!                 /dev/zero + hello.txt e poi esce SENZA close e SENZA T_DONE.
//!                 Il parent osserva via wait_exit; cardo deve purgare
//!                 rings/ftable (con DEV_CLOSE inoltrato ai driver).
//!   - 10 MAPHAMMER: (Fase 14, t29) martella map_physical di due pagine scratch
//!                 sulla stessa VA (0x4000003E0000) in loop, verificando sempre
//!                 i propri marker. Con un peer che fa lo stesso su pagine
//!                 diverse (stesso numero VA, tabelle diverse) rivela cross-talk
//!                 TLB/mapping: mismatch = FAIL rumoroso via T_DONE(w0).
//!   - 11 FLOOD:    (t30, buon vicinato) client "cattivo vicino": martella
//!                 open+write+close di /dev/null alla massima velocita',
//!                 finche' il parent manda T_STOP (controllato ogni 64 op via
//!                 `recv_poll`). Intenzionalmente SENZA throttling: riproduce la
//!                 tempesta di open che affamava la registrazione di vela
//!                 (lezione t27: /dev smontato → open veloci falliti in loop).
//!                 Termina via T_STOP con T_DONE(w0=1, w1=ops).
//!   - 12 NEST:     (Fase 22, t40) genitore intermedio: spawna due KILLME (uno
//!                 detached via flag spawn, uno normale), li riporta al parent
//!                 con T_READY(w0=pid_det, w1=pid_norm) e poi esce: la SUA morte
//!                 fa scattare il caso (cascata sul normale, reparent a init
//!                 del detached). Il parent osserva tutto via `ps` (non e'
//!                 peer delle foglie: niente EXIT_NOTIFY diretta).
//!   - 24 REG51:    (Fase 36, t51) come MNTDIE ma sul prefix dedicato
//!                 "/dev/t51": registra, T_READY(w0=1), serve il minimo
//!                 (DEV_OPEN → fake fd). Due istanze sono lo STESSO binario
//!                 (stesso image_hash): la seconda puo' rimpiazzare la prima
//!                 viva (same-image, Fase 36.5) pur non essendo figlia di init.
//!   - 25 EXECDEMO: (Fase 37, t52) attende T_GO, poi exec_into testspin.bin:
//!                 il processo diventa spin (stesso PID, hash rimisurato) e
//!                 serve il T_CFG che l'orchestratore manda dopo. Successo =
//!                 mai ritorno; fallimento = T_DONE(w0=0) + exit.
//!   - 26 DUPCLAIM / 27 DUPGRANT / 28 DUPSIBCLAIM / 29 SEEKDENY: (Fase 40,
//!                 t54) handoff grant/claim modello B e diniego SEEK: vedi
//!                 costanti MODE_* sotto. Sempre T_DONE(ok, detail) + exit(0).
//!
//! In ogni caso termina con `send(T_DONE, ok, dettagli)` e `exit(0)`.
//! Il processo e' sempre "garantito che risponde": l'orchestratore reply ad
//! ogni messaggio ricevuto per non lasciare il client bloccato.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::vec;

use civis::{print_str, println};
use testproto::*;



const MODE_NULLW: u64 = 2;

// Fase 29: fault di protezione (il kernel deve terminare il processo).


// Fase 37 (exec in-place, t52): attende T_GO, poi exec_into testspin.bin.
// Successo = mai ritorno (si diventa spin); fallimento = T_DONE(w0=0) + exit.



// Tag DEV_* + errore IPC (A1): single source in `civis` (prima letterali qui).
use civis::{DEV_CLOSE, DEV_OPEN, ERR};

libr::entry!(real_main);
fn real_main(sp: u64) -> ! {
    // Fase 37.1 (t52-argv, env in 43a): sonda argv+env post-exec —
    // convenzione di TEST esplicita, solo testland la usa: se argv[0] ==
    // "ARGPROBE", niente flusso CFG; riporta T_DONE(argc, fnv(argv[1..])) e
    // esce. La FNV e' calcolata come il parent (`image_hash` sul join NUL,
    // trailing NUL incluso). L'env atteso (`T52E=envok`, messo nel blocco
    // dal ramo EXECDEMO sotto) e' verificato qui: assente/diverso →
    // T_DONE(0, 0) + exit(1) (il parent se ne accorge dal report).
    if let Some(args) = civis::args_from_stack(sp) {
        if args.get(0) == Some(b"ARGPROBE".as_slice()) {
            let env_ok = match civis::env_from_stack(sp) {
                Some(env) => env.get("T52E") == Some(b"envok".as_slice()),
                None => false,
            };
            if !env_ok {
                let _ = civis::send(civis::CHANNEL_PARENT, T_DONE, 0, 0);
                civis::exit(1);
            }
            let mut joined = alloc::vec::Vec::new();
            let mut i = 1u64;
            while let Some(a) = args.get(i) {
                joined.extend_from_slice(a);
                joined.push(0);
                i += 1;
            }
            let _ = civis::send(
                civis::CHANNEL_PARENT,
                T_DONE,
                args.argc(),
                civis::image_hash(&joined),
            );
            civis::exit(0);
        }
    }
    let my_pid = civis::getpid();
    println!("[utcli] pid={} up", my_pid);

    let cfg = match civis::recv() {
        Ok(m) => m,
        Err(_) => {
            println!("[utcli] recv cfg failed");
            civis::exit(1);
        }
    };
    // Il parent e' raggiungibile sul canale di nascita (ADR-0008).
    let parent = civis::CHANNEL_PARENT;
    let mode = cfg.w0;
    let rounds = cfg.w1 as usize;

    // ACK con il nostro pid: l'orchestratore verifica spawn/getpid.
    let _ = civis::reply(T_ACK, civis::getpid() as u64, 0);
    println!("[utcli] pid={} acked, sending done...", my_pid);

    match mode {
        M_CHURN => {
            // Lifecycle (Fase 14): materializza `rounds` KiB di heap e termina
            // SENZA T_DONE: il parent segue la notifica EXIT_NOTIFY.
            let ok = churn_heap(rounds);
            civis::exit(if ok { 0 } else { 1 });
        }
        M_KILLME => {
            // Lifecycle (Fase 14): dorme in recv finche' il parent lo kill().
            // (Prima: busy-spin a pari priorita' — ogni round-trip FS degli
            // altri processi aspettava i nostri quanti: load da 15t a 3500t
            // in t24. Bloccato si kill() uguale, a costo zero.)
            // Usato anche come foglia parcheggiata per t40 (NEST): vivo ma
            // idle, killabile, osservabile via `ps`.
            loop {
                let _ = civis::recv();
            }
        }
        M_SIGCATCH => {
            // Fase 44b (t56): catcher cooperativo — come KILLME (costo zero)
            // ma al cancel job control esce con code dedicato (42, scelto per
            // distinguersi dal 130 dell'escalation via kill: la prova del
            // catch e' il code stesso).
            loop {
                match civis::recv() {
                    Ok(m) if m.tag == civis::JOB_CANCEL => civis::exit(42),
                    Ok(m) if civis::is_exit_notify(&m) => {}
                    Ok(_) => {
                        let _ = civis::reply(0, 0, 0);
                    }
                    Err(_) => civis::exit(1),
                }
            }
        }
        M_ORPHAN => {
            // Parcheggiato come KILLME (stesso costo zero), ma autonomo nella
            // morte: alla prima EXIT_NOTIFY (il parent e' morto: unico peer
            // possibile) attende ~100 tick (finestra di osservazione per chi
            // verifica il reparent via `ps`) ed esce 0 da solo.
            loop {
                match civis::recv() {
                    Ok(m) if civis::is_exit_notify(&m) => {
                        civis::spin_ticks(100);
                        civis::exit(0);
                    }
                    Ok(_) => {
                        let _ = civis::reply(0, 0, 0);
                    }
                    Err(_) => civis::exit(1),
                }
            }
        }
        M_SRVDIE => {
            // Server sacrificale (t24, notifica unificata): registra `Test`
            // e dorme in recv SENZA MAI RISPONDERE (i sender restano bloccati
            // come col vecchio spin; i messaggi vengono consumati ma senza
            // reply nessuno si sblocca). Se la registrazione fallisce (slot
            // occupato) esci subito: il test fallira' in modo rumoroso (kill
            // su processo gia' morto), mai hang.
            // (Prima: busy-spin — vedi KILLME sopra.)
            if civis::service_register(civis::Service::Test).is_err() {
                println!("[utcli] pid={} srvdie: register Test FAILED", my_pid);
                civis::exit(1);
            }
            println!("[utcli] pid={} srvdie: registered, parking in recv", my_pid);
            loop {
                let _ = civis::recv();
            }
        }
        M_SYNCWAIT => {
            // Client sync sacrificale (t24): risolve `Test`, handshake T_READY
            // al parent, poi send SYNC verso il server (che non risponde mai).
            // Sbloccato con Err alla morte del server, attende l'EXIT_NOTIFY
            // unificata, poi attende il via-libera T_GO del parent (che lo
            // manda solo dopo il proprio wait_reply: cosi' il nostro T_DONE
            // non puo' mai anticipare la notifica nella sua coda) e riporta
            // T_DONE(w0=1, w1=code). Ogni deviazione: T_DONE(w0=0, w1=detail).
            let srv = match civis::service_lookup(civis::Service::Test) {
                Ok(c) => c as u64,
                Err(_) => {
                    let _ = civis::send(parent, T_DONE, 0, 10);
                    civis::exit(0);
                }
            };
            if civis::send(parent, T_READY, 1, 0).is_err() {
                civis::exit(1);
            }
            match civis::send(srv, T_REQ, 0xCAFE, 0) {
                Ok(_) => {
                    // Il server non risponde mai: successo impossibile.
                    let _ = civis::send(parent, T_DONE, 0, 11);
                    civis::exit(0);
                }
                Err(_) => {}
            }
            let code = loop {
                match civis::recv() {
                    Ok(m) if civis::is_exit_notify(&m) => break m.w0,
                    Ok(_) => {
                        let _ = civis::reply(T_ACK, 0, 0);
                    }
                    Err(_) => {
                        let _ = civis::send(parent, T_DONE, 0, 12);
                        civis::exit(0);
                    }
                }
            };
            // Via-libera del parent (dopo il suo wait_reply).
            loop {
                match civis::recv() {
                    Ok(m) if m.tag == T_GO => {
                        let _ = civis::reply(T_ACK, 0, 0);
                        break;
                    }
                    Ok(_) => {
                        let _ = civis::reply(T_ACK, 0, 0);
                    }
                    Err(_) => {
                        let _ = civis::send(parent, T_DONE, 0, 13);
                        civis::exit(0);
                    }
                }
            }
            let _ = civis::send(parent, T_DONE, 1, code);
            civis::exit(0);
        }
        M_EXECDEMO => {
            // Demo exec (t52, Fase 37): attende il via T_GO, poi exec. `rounds`
            // (cfg.w1) seleziona il target: 0 = testspin senza argv (nucleo
            // 37.0: stesso PID, hash rimisurato); 1 = testcli con argv
            // ["ARGPROBE","hello","world"] (37.1: la nuova immagine vede gli
            // argv e riporta T_DONE da sola). Successo = mai ritorno;
            // fallimento = T_DONE(w0=0, w1=detail) + exit, mai hang.
            loop {
                match civis::recv() {
                    Ok(m) if m.tag == T_GO => {
                        let _ = civis::reply(T_ACK, 0, 0);
                        break;
                    }
                    Ok(_) => {
                        let _ = civis::reply(T_ACK, 0, 0);
                    }
                    Err(_) => {
                        civis::exit(1);
                    }
                }
            }
            if rounds == 1 {
                let img = match civis::load_file("/test/testcli.bin") {
                    Some(b) if !b.is_empty() => b,
                    _ => {
                        let _ = civis::send(parent, T_DONE, 0, 30);
                        civis::exit(0);
                    }
                };
                // Blocco argv+env via serialize (single source col kernel:
                // formato `[argc][envc][payload]`, mai hand-rolled). L'env
                // e' verificato dalla sonda ARGPROBE nella nuova immagine.
                let buf = match libr::serialize_argv_redir_env(
                    &["ARGPROBE", "hello", "world"],
                    &[("T52E", "envok")],
                    &[],
                ) {
                    Some(b) => b,
                    None => {
                        let _ = civis::send(parent, T_DONE, 0, 32);
                        civis::exit(0);
                    }
                };
                match civis::exec_image_args(&img, &buf) {
                    Ok(()) => civis::exit(1), // irraggiungibile
                    Err(_) => {
                        let _ = civis::send(parent, T_DONE, 0, 31);
                        civis::exit(0);
                    }
                }
            }
            let img = match civis::load_file("/test/testspin.bin") {
                Some(b) if !b.is_empty() => b,
                _ => {
                    let _ = civis::send(parent, T_DONE, 0, 20);
                    civis::exit(0);
                }
            };
            match civis::exec_image(&img) {
                Ok(()) => civis::exit(1), // irraggiungibile: success non ritorna
                Err(_) => {
                    let _ = civis::send(parent, T_DONE, 0, 21);
                    civis::exit(0);
                }
            }
        }
        M_MNTDIE => {
            // Driver sacrificale (t25): registra "/dev/tdie", handshake T_READY e
            // serve il minimo. Se la registrazione fallisce: T_READY(w0=0) +
            // exit(1) — il test fallisce rumoroso, mai hang.
            if civis::fs_register(b"/dev/tdie").is_err() {
                println!("[utcli] pid={} mntdie: fs_register FAILED", my_pid);
                let _ = civis::send(parent, T_READY, 0, 0);
                civis::exit(1);
            }
            println!("[utcli] pid={} mntdie: registered /tdie", my_pid);
            if civis::send(parent, T_READY, 1, 0).is_err() {
                civis::exit(1);
            }
            loop {
                match civis::recv() {
                    Ok(m) => match m.tag {
                        DEV_OPEN => {
                            let _ = civis::reply(0, 1, 0);
                        }
                        DEV_CLOSE => {
                            let _ = civis::reply(0, 0, 0);
                        }
                        _ => {
                            let _ = civis::reply(0, ERR, 0);
                        }
                    },
                    Err(_) => {}
                }
            }
        }
        M_REG51 => {
            // Driver sacrificale (t51, Fase 36): identico a MNTDIE ma sul
            // prefix dedicato "/dev/t51" (nessuna interferenza con t25).
            if civis::fs_register(b"/dev/t51").is_err() {
                println!("[utcli] pid={} reg51: fs_register FAILED", my_pid);
                let _ = civis::send(parent, T_READY, 0, 0);
                civis::exit(1);
            }
            println!("[utcli] pid={} reg51: registered /t51", my_pid);
            if civis::send(parent, T_READY, 1, 0).is_err() {
                civis::exit(1);
            }
            loop {
                match civis::recv() {
                    Ok(m) => match m.tag {
                        DEV_OPEN => {
                            let _ = civis::reply(0, 1, 0);
                        }
                        DEV_CLOSE => {
                            let _ = civis::reply(0, 0, 0);
                        }
                        _ => {
                            let _ = civis::reply(0, ERR, 0);
                        }
                    },
                    Err(_) => {}
                }
            }
        }
        M_OPENDIE => {
            // Client sacrificale (t26): apre e muore senza close.
            if civis::open("/dev/null", 0).is_err()
                || civis::open("/dev/zero", 0).is_err()
                || civis::open("hello.txt", 0).is_err()
            {
                println!("[utcli] opendie: open failed");
                civis::exit(2);
            }
            civis::exit(0);
        }
        M_NEST => {
            // Genitore intermedio (Fase 22, t40): spawna due foglie parcheggiate
            // (detached in ORPHAN, normale in KILLME), le riporta al parent e
            // poi ESCE: la sua morte fa scattare il caso (cascata sul normale,
            // reparent a init della detached, che poi esce da sola su notify).
            // Qualunque spaw fallito: T_READY(w0=0) + exit(1), mai hang (il
            // test fallisce rumoroso).
            let det = match spawn_killme(true, M_ORPHAN) {
                Some(c) => c,
                None => {
                    let _ = civis::send(parent, T_READY, 0, 0);
                    civis::exit(1);
                }
            };
            let norm = match spawn_killme(false, M_KILLME) {
                Some(c) => c,
                None => {
                    let _ = civis::send(parent, T_READY, 0, 0);
                    civis::exit(1);
                }
            };
            println!("[utcli] pid={} nest: det={} norm={}", my_pid, det, norm);
            let _ = civis::send(parent, T_READY, det, norm);
            civis::exit(0);
        }
        M_FAULT_RO | M_FAULT_NONE | M_FAULT_NX | M_FAULT_GUARD | M_FAULT_GPF | M_FAULT_CODE => {
            run_fault(mode);
        }
        _ => {
            let (ok, detail) = match mode {
                M_ECHO => run_echo(parent, rounds),
                M_ZERO => run_zeroread(parent, rounds),
                MODE_NULLW => run_nullw(),
                M_SRV => run_srv(),
                M_MAPHAMMER => run_maphammer(rounds),
                M_FLOOD => run_flood(parent),
                M_SHMDEMO => run_shmdemo(rounds as u32),
                M_COWDEMO => run_cowdemo(rounds as u32),
                M_FORKDEMO => run_forkdemo(),
                M_HARDEN => run_harden(rounds as i64),
                M_DUPCLAIM => run_dupclaim(rounds as u64),
                M_DUPGRANT => run_dupgrant(),
                M_DUPSIBCLAIM => run_dupsibclaim(rounds as u64),
                M_SEEKDENY => run_seekdeny(),
                M_GRANTDENY => run_grantdeny(),
                M_SUSPENDENY => run_suspenddeny(rounds as i64),
                _ => (false, 1),
            };
            let _ = civis::send(parent, T_DONE, ok as u64, detail as u64);
            println!("[utcli] pid={} done ok={} detail={}", my_pid, ok, detail);
            civis::exit(0);
        }
    }
}

/// Fase 30: mappa la regione condivisa `id` (passata dal parent in `param`),
/// verifica il pattern scritto dal parent, scrive un marker a offset 4096
/// (che il parent deve vedere: prova la visibilita' bidirezionale) e riporta
/// l'esito. Le pagine sono le stesse del parent: zero-copy tra processi.
fn run_shmdemo(id: u32) -> (bool, usize) {
    let base = match civis::shm_map(id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => return (false, 1),
    };
    for i in 0..8192usize {
        if unsafe { core::ptr::read_volatile((base + i) as *const u8) } != (i % 251) as u8 {
            return (false, 2);
        }
    }
    unsafe { core::ptr::write_volatile((base + 4096) as *mut u8, 0xAB); }
    (true, 0)
}

/// Fase 33: mappa la regione `id` in COW, verifica il pattern del parent
/// (shared-read), scrive un marker su ENTRAMBE le pagine (2 COW fault → copie
/// private) e verifica marker + pattern circostante. Il parent non deve vedere
/// i marker (isolamento). Esito come `(bool, detail)`.
fn run_cowdemo(id: u32) -> (bool, usize) {
    let base = match civis::shm_map_cow(id, 0) {
        Ok(b) => b,
        Err(_) => return (false, 1),
    };
    for i in 0..8192usize {
        if unsafe { core::ptr::read_volatile((base + i) as *const u8) } != (i % 251) as u8 {
            return (false, 2);
        }
    }
    unsafe {
        core::ptr::write_volatile(base as *mut u8, 0xAB);
        core::ptr::write_volatile((base + 4096) as *mut u8, 0xCD);
    }
    if unsafe { core::ptr::read_volatile(base as *const u8) } != 0xAB {
        return (false, 3);
    }
    if unsafe { core::ptr::read_volatile((base + 4096) as *const u8) } != 0xCD {
        return (false, 4);
    }
    // Il resto del pattern e' intatto attorno ai marker.
    for i in [1usize, 100, 4095, 4097, 5000, 8191] {
        if unsafe { core::ptr::read_volatile((base + i) as *const u8) } != (i % 251) as u8 {
            return (false, 5);
        }
    }
    (true, 0)
}

/// Fase 34: fork COW di se stesso. Il globale `FORK_G` (pagina owned privata)
/// e' condiviso in COW al fork: padre e figlio scrivono valori diversi e
/// nessuno deve vedere la scrittura dell'altro (isolamento). Il figlio
/// riporta il valore letto/scritto sul canale di nascita (SYNC: il padre
/// risponde) ed esce 0; il padre verifica isolamento + report + exit code.
/// Solo nascita+exit: niente FS/IPC extra nel figlio (34.2).
static mut FORK_G: u64 = 0;

const FORK_PAT: u64 = 0x1111_1111_1111_1111;
const FORK_CHILD_VAL: u64 = 0xAAAA_AAAA_AAAA_AAAA;
const FORK_PARENT_VAL: u64 = 0x5555_5555_5555_5555;

fn run_forkdemo() -> (bool, usize) {
    unsafe { FORK_G = FORK_PAT; }
    match libr::fork() {
        Err(_) => (false, 1),
        Ok(libr::ForkResult::Child { .. }) => {
            // Shared-read: il pattern del padre e' visibile prima del write.
            if unsafe { FORK_G } != FORK_PAT {
                civis::exit(2);
            }
            unsafe { FORK_G = FORK_CHILD_VAL; }
            if unsafe { FORK_G } != FORK_CHILD_VAL {
                civis::exit(3);
            }
            // Report SYNC al padre (canale 0): il padre risponde, poi esco.
            match civis::send(civis::CHANNEL_PARENT, T_FORKREP, FORK_CHILD_VAL, FORK_CHILD_VAL) {
                Ok(_) => civis::exit(0),
                Err(_) => civis::exit(4),
            }
        }
        Ok(libr::ForkResult::Parent { pid, chan }) => {
            if pid == 0 {
                return (false, 5);
            }
            unsafe { FORK_G = FORK_PARENT_VAL; }
            // Isolamento: la scrittura COW del figlio non e' visibile.
            if unsafe { FORK_G } != FORK_PARENT_VAL {
                return (false, 6);
            }
            // Report del figlio (SYNC: rispondo per sbloccarlo).
            let rep = match civis::recv() {
                Ok(m) if m.tag == T_FORKREP && m.channel == chan => m,
                Ok(m) => {
                    let _ = civis::reply(0, 0, 0);
                    return (false, 7 + (m.tag as usize % 10));
                }
                Err(_) => return (false, 8),
            };
            let _ = civis::reply(0, 0, 0);
            if rep.w0 != FORK_CHILD_VAL || rep.w1 != FORK_CHILD_VAL {
                return (false, 9);
            }
            // Il figlio e' uscito 0 (EXIT_NOTIFY sul canale di nascita).
            loop {
                match civis::recv() {
                    Ok(m) if m.channel == chan && civis::is_exit_notify(&m) => {
                        return (m.w0 == 0, 10);
                    }
                    Ok(m) if civis::is_exit_notify(&m) => {}
                    Ok(_) => {
                        let _ = civis::reply(0, 0, 0);
                    }
                    Err(_) => return (false, 11),
                }
            }
        }
    }
}

/// Fase 35 (hardening, t50): tentativi ostili che DEVONO essere rifiutati.
/// `target` = pid di un processo che NON e' nostro figlio (un fratello
/// spawnato dall'orchestratore): il kill deve fallire (parent-scoped). Poi
/// proviamo a registrare un servizio di sistema (`Init`) da non-figlio di
/// init: deve fallire. Ritorna (ok, detail) coi due esiti.
fn run_harden(target: i64) -> (bool, usize) {
    let kill_rejected = civis::kill(target, 0).is_err();
    let reg_rejected = civis::service_register(civis::Service::Init).is_err();
    // Fase 39: anche il nuovo slot Posix e' gatato (non-figlio-di-init).
    // Esercita service_from_disc(8) + braccio nome "posix" nel kernel.
    let posix_rejected = civis::service_register(civis::Service::Posix).is_err();
    let ok = kill_rejected && reg_rejected && posix_rejected;
    let detail =
        (kill_rejected as usize) | ((reg_rejected as usize) << 1) | ((posix_rejected as usize) << 2);
    (ok, detail)
}

/// Fase 40 (t54): riscuote il grant del parent (`nonce` in w1) e verifica
/// contenuto + offset copiato (il parent scrive "ABCDEF" e fa lseek(3) prima
/// del grant: qui si devono leggere "DEF"). Poi un secondo claim dello stesso
/// nonce deve dare Invalid (single-use). Ritorna (ok, detail).
fn run_dupclaim(nonce: u64) -> (bool, usize) {
    let fd = match civis::dup_claim(nonce) {
        Ok(f) => f,
        Err(_) => return (false, 10),
    };
    let mut b = [0u8; 3];
    let n = match civis::read_fs(fd, &mut b, 3) {
        Ok(n) => n,
        Err(_) => return (false, 11),
    };
    let _ = civis::close(fd);
    if n != 3 || &b != b"DEF" {
        return (false, 12);
    }
    match civis::dup_claim(nonce) {
        Err(civis::Error::Invalid) => (true, 0),
        _ => (false, 13),
    }
}

/// Fase 40 (t54): apre /t54sib.txt e ne fa grant; il nonce viaggia in
/// T_DONE(w1) all'orchestratore (che lo gira al sibling). Ritorna (ok, nonce
/// come detail: il T_DONE generico lo mette in w1).
fn run_dupgrant() -> (bool, usize) {
    let fd = match civis::open("/t54sib.txt", 0) {
        Ok(f) => f,
        Err(_) => return (false, 0),
    };
    let nonce = match civis::dup_grant(fd) {
        Ok(n) => n,
        Err(_) => return (false, 0),
    };
    // Il grant e' uno snapshot server-side: l'fd si puo' chiudere subito.
    let _ = civis::close(fd);
    (true, nonce as usize)
}

/// Fase 40 (t54): claim del grant di un ALTRO helper (registrante != nostro
/// parent) — l'attestazione parentela deve rifiutare con Invalid.
/// Ritorna (ok, detail).
fn run_dupsibclaim(nonce: u64) -> (bool, usize) {
    match civis::dup_claim(nonce) {
        Err(civis::Error::Invalid) => (true, 0),
        _ => (false, 20),
    }
}

/// Fase 40 (t54): droppa RIGHTS_SEEK sul proprio canale (solo shrink, resto
/// intatto) e verifica che lseek venga rifiutato con Failed (il choke
/// centrale risponde ERR generico). Ritorna (ok, detail).
fn run_seekdeny() -> (bool, usize) {
    let fd = match civis::open("/t54seek.txt", civis::OPEN_CREATE) {
        Ok(f) => f,
        Err(_) => return (false, 0),
    };
    if civis::rights_drop(civis::RIGHTS_ALL & !civis::RIGHTS_SEEK, None).is_err() {
        return (false, 1);
    }
    let r = civis::lseek(fd, 0, civis::SEEK_START);
    let _ = civis::close(fd);
    match r {
        Err(civis::Error::Failed) => (true, 0),
        _ => (false, 2),
    }
}

/// Fase 45 (t57): diritti GRANT+PIPE sul proprio canale. Il GET default deve
/// essere ALL (prova che la riga test-policy e' applicata: senza, il default
/// ignoto negherebbe gia' qui). Poi drop GRANT → `dup_grant` negato, drop
/// PIPE → `pipe()` negata, e una read valida dopo i rifiuti (anti-wedge).
/// Detail = bitmask esiti (1=get, 2=grant-denied, 4=pipe-denied, 8=read-ok).
fn run_grantdeny() -> (bool, usize) {
    let mut detail = 0usize;
    let mut sb = [0u8; 32];
    if civis::rights_get(&mut sb) != Ok(civis::RIGHTS_ALL) {
        return (false, detail);
    }
    detail |= 1;
    let fd = match civis::open("/t57grant.txt", civis::OPEN_CREATE) {
        Ok(f) => f,
        Err(_) => return (false, detail),
    };
    if civis::rights_drop(civis::RIGHTS_ALL & !civis::RIGHTS_GRANT, None).is_err() {
        let _ = civis::close(fd);
        return (false, detail);
    }
    if civis::dup_grant(fd).is_err() {
        detail |= 2;
    }
    if civis::rights_drop(
        civis::RIGHTS_ALL & !civis::RIGHTS_GRANT & !civis::RIGHTS_PIPE,
        None,
    )
    .is_err()
    {
        let _ = civis::close(fd);
        return (false, detail);
    }
    if civis::pipe().is_err() {
        detail |= 4;
    }
    // Op valida DOPO i rifiuti (round trip veri, non count-0 vacuo): il bit
    // WRITE e' intatto, quindi write+read-back devono funzionare.
    let mut b = [0u8; 4];
    if civis::write_fs(fd, b"qwer", 4) == Ok(4)
        && civis::lseek(fd, 0, civis::SEEK_START).is_ok()
        && civis::read_fs(fd, &mut b, 4) == Ok(4)
        && b == *b"qwer"
    {
        detail |= 8;
    }
    let _ = civis::close(fd);
    (detail == 15, detail)
}

/// Fase 44a (t55): tentativi suspend/resume ostili che DEVONO essere rifiutati.
/// `target` = pid di un processo che NON e' nostro figlio (un fratello
/// spawnato dall'orchestratore, come in `run_harden`). Ritorna (ok, detail).
fn run_suspenddeny(target: i64) -> (bool, usize) {
    let s_denied = civis::suspend(target).is_err();
    let r_denied = civis::resume(target).is_err();
    let ok = s_denied && r_denied;
    (ok, (s_denied as usize) | ((r_denied as usize) << 1))
}

/// Fase 29: provoca un fault di memoria non recuperabile (write su RO,
/// accesso a NONE, exec su NX, accesso alla guard page). Il kernel deve
/// terminare il processo con `FAULT_EXIT_CODE` (osservato dal parent via
/// EXIT_NOTIFY). Se questa funzione ritorna, il fault NON e' stato
/// intercettato: exit(1) → il test fallisce rumoroso.
fn run_fault(mode: u64) -> ! {
    match mode {
        M_FAULT_RO => {
            let p = civis::mmap(0, 4096).expect("mmap");
            unsafe { core::ptr::write_volatile(p as *mut u8, 0x41); }
            let _ = civis::mprotect(p, 4096, civis::PROT_READ);
            unsafe { core::ptr::write_volatile(p as *mut u8, 0x42); } // #PF
        }
        M_FAULT_NONE => {
            let p = civis::mmap(0, 4096).expect("mmap");
            unsafe { core::ptr::write_volatile(p as *mut u8, 0x41); }
            let _ = civis::mprotect(p, 4096, civis::PROT_NONE);
            let _ = unsafe { core::ptr::read_volatile(p as *const u8) }; // #PF
        }
        M_FAULT_NX => {
            let p = civis::mmap(0, 4096).expect("mmap");
            unsafe { core::ptr::write_volatile(p as *mut u8, 0xC3); } // ret
            let f: extern "C" fn() = unsafe { core::mem::transmute(p) };
            f(); // fetch su pagina NX → #PF
        }
        M_FAULT_GUARD => {
            let g = civis::USER_STACK_GUARD as *mut u8;
            unsafe { core::ptr::write_volatile(g, 0x41); } // #PF (guard)
        }
        M_FAULT_GPF => {
            // Nessuna porta concessa a questo helper (io_count == 0): `in` su
            // una porta qualsiasi → #GP → il kernel termina il processo.
            unsafe { civis::pio::inb(0x80); }
        }
        M_FAULT_CODE => {
            // Fase 31: il codice e' mappato RX (W^X): scrivere all'indirizzo
            // del codice (USER_CODE) → #PF protection-violation → kill.
            unsafe { core::ptr::write_volatile(civis::USER_CODE as *mut u8, 0x41); }
        }
        _ => {}
    }
    println!("[utcli] fault mode={} NON ha faultato", mode);
    civis::exit(1);
}

/// Legge un file intero in heap (bound 256 KiB, chunk 4000 = RING_MAX_PAYLOAD).
/// Spawna una foglia KILLME parcheggiata (Fase 22, NEST): come `spawn_cfg` di
/// usertests ma eseguito da dentro l'helper (il MID e' parent delle foglie).
/// `detached` = flag spawn (la foglia sopravvive alla morte del MID), `mode`
/// = modalita' della foglia (KILLME/ORPHAN). Ritorna il pid della foglia
/// (dall'ACK) o None.
fn spawn_killme(detached: bool, mode: u64) -> Option<u64> {
    let img = civis::load_file("/test/testcli.bin")?;
    let base = civis::SpawnMeta::new("utcli", 16, &[])?;
    let meta = if detached { base.detached() } else { base };
    let chan = civis::spawn_image(&img, &meta).ok()? as u64;
    let ack = civis::send(chan, T_CFG, mode, 0).ok()?;
    Some(ack.w0)
}

/// Materializza `kib` KiB di heap on-demand (sbrk + touch ogni pagina) e
/// verifica il contenuto. Usato dal lifecycle test (riuso PID + no frame leak).
fn churn_heap(kib: usize) -> bool {
    let bytes = kib * 1024;
    let mut buf = vec![0u8; bytes];
    let mut ok = true;
    let step = 4096usize;
    let mut i = 0usize;
    while i < bytes {
        buf[i] = (i & 0xFF) as u8;
        i += step;
    }
    let mut j = 0usize;
    while j < bytes {
        if buf[j] != (j & 0xFF) as u8 {
            ok = false;
        }
        j += step;
    }
    drop(buf);
    ok
}

/// Martella map_physical di P2 su VA_X verificando i marker B (t29): con un
/// peer che fa lo stesso su P1 (stessa VA, altre tabelle), un mismatch prova
/// cross-talk di mapping/TLB. Ritorna (ok, mismatches).
fn run_maphammer(rounds: usize) -> (bool, usize) {
    const VA_X: u64 = 0x0000_4000_003E_0000;
    let p2 = civis::MAP_TEST_PHYS + 0x1000;
    let mut bad = 0usize;
    for _ in 0..rounds {
        if civis::map_physical(p2, VA_X, 1).is_err() {
            return (false, 0xFFFF);
        }
        unsafe {
            let p = VA_X as *mut u8;
            for i in 0..64 {
                core::ptr::write_volatile(p.add(i), 0x55);
            }
            for i in 0..64 {
                if core::ptr::read_volatile(p.add(i)) != 0x55 {
                    bad += 1;
                    break;
                }
            }
        }
        if bad > 0 {
            break;
        }
    }
    (bad == 0, bad)
}

/// Client "cattivo vicino" (t30, buon vicinato): martella open+write+close di
/// /dev/null alla massima velocita' (a /dev smontato sono open veloci falliti
/// in loop — lo scenario t27-red). Dopo WARMUP_OPS operazioni segnala T_READY
/// al parent (cosi' il kill avviene a flood stabilizzato, non in startup) e
/// continua finche' arriva T_STOP (ogni 64 op via `recv_poll`). Termina con
/// (true, ops): il chiamante generico invia poi T_DONE(w0=1, w1=ops).
fn run_flood(parent: u64) -> (bool, usize) {
    const WARMUP_OPS: usize = 1000;
    let mut ops = 0usize;
    let mut warmed = false;
    let data = [0x5Au8; 16];
    loop {
        if let Ok(fd) = civis::open("/dev/null", 0) {
            let _ = civis::write_fs(fd, &data, 16);
            let _ = civis::close(fd);
        }
        ops += 1;
        if !warmed && ops >= WARMUP_OPS {
            warmed = true;
            // Il parent risponde (recv_expect): poi il flood continua.
            let _ = civis::send(parent, T_READY, 1, 0);
        }
        if ops % 64 == 0 {
            match civis::recv_poll() {
                Some(m) if m.tag == T_STOP => {
                    let _ = civis::reply(T_ACK, 0, 0);
                    return (true, ops);
                }
                // Se il parent muore, la suite e' finita: esci in silenzio
                // invece di floodare per sempre (igiene, mai wedge).
                Some(m)
                    if m.channel == civis::CHANNEL_PARENT
                        && civis::is_exit_notify(&m) =>
                {
                    civis::exit(0);
                }
                Some(m) if civis::is_exit_notify(&m) => {}
                Some(_) => {
                    let _ = civis::reply(T_ACK, 0, 0);
                }
                None => {}
            }
        }
    }
}

/// Server echo per IPC async (Fase 13): loop di `recv`; per ogni `T_REQ`
/// risponde con reply implicita `2*w0`. Un `T_STOP` (da raccogliere in coda)
/// fa terminare. Il server non distingue richieste sync da async: risponde
/// sempre; il kernel recapita la reply al parent bloccato (reply_slot) o
/// accodandola come messaggio (req_id negativo) se il parent era async.
fn run_srv() -> (bool, usize) {
    let mut served = 0usize;
    loop {
        match civis::recv() {
            Ok(m) => match m.tag {
                T_REQ => {
                    let _ = civis::reply(0, 2 * m.w0, 0);
                    served += 1;
                }
                T_STOP => {
                    let _ = civis::reply(T_ACK, 0, 0);
                    return (true, served);
                }
                _ => {
                    let _ = civis::reply(T_ACK, 0, 0);
                }
            },
            Err(_) => return (false, served),
        }
    }
}

fn run_echo(parent: u64, rounds: usize) -> (bool, usize) {
    let mut mismatches = 0usize;
    for seq in 0..rounds {
        let payload = (seq + 1) as u64;
        match civis::send(parent, T_REQ, payload, 0) {
            Ok(r) => {
                if r.w0 != 2 * payload {
                    mismatches += 1;
                }
            }
            Err(_) => mismatches += 1,
        }
    }
    (mismatches == 0, mismatches)
}

fn run_zeroread(parent: u64, rounds: usize) -> (bool, usize) {
    // Throttled (Livello 1, buon vicinato): vedi `civis::open_wait`.
    let fd = civis::open_wait("/dev/zero", 0, 1000, civis::POLL_PERIOD_TICKS);
    // Il buffer FS e' per-processo (Fase 9.6), quindi piu' client possono
    // leggere /dev/zero senza corrompersi a vicenda. L'handshake T_OPENED resta
    // come barriera di coordinamento: l'orchestratore attende che tutti abbiano
    // aperto prima di rilasciare le read con T_GO.
    let open_ok = fd.is_ok();
    let _ = civis::send(parent, T_OPENED, open_ok as u64, 0);    match civis::recv() {
        Ok(m) if m.tag == T_GO => {
            let _ = civis::reply(T_ACK, 0, 0);
        }
        _ => return (false, 1),
    }
    let Ok(fd) = fd else {
        println!("[utcli] zeroread: open /dev/zero failed");
        return (false, 1);
    };
    let mut bad = 0usize;
    let mut buf = vec![0u8; 4096];
    for i in 0..rounds {
        let n = civis::read_fs(fd, &mut buf, 4096);
        if n != Ok(4096) {
            bad += 1;
            println!("[utcli] zeroread read#{} n={:?}", i, n);
            continue;
        }
        if buf.iter().any(|&b| b != 0) {
            bad += 1;
            print_str!("[utcli] zeroread read#{} nonzero: ", i);
            for j in 0..16 {
                print_str!("{:02x} ", buf[j]);
            }
            println!();
        }
    }
    let _ = civis::close(fd);
    (bad == 0, bad)
}

fn run_nullw() -> (bool, usize) {
    let Ok(fd) = civis::open_wait("/dev/null", 0, 1000, civis::POLL_PERIOD_TICKS) else {
        println!("[utcli] nullw: open /dev/null failed");
        return (false, 1);
    };
    let data = [0x5Au8; 512];
    let n = civis::write_fs(fd, &data, 512);
    let mut buf = [0u8; 64];
    let r = civis::read_fs(fd, &mut buf, 64);
    let _ = civis::close(fd);
    (n == Ok(512) && r == Ok(0), 1)
}

#[panic_handler]
fn panic_handler(_info: &core::panic::PanicInfo) -> ! {
    println!("[utcli] panic");
    civis::exit(1)
}
