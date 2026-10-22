//! Processo init (Fase 8.1): primo processo user (PID 1), antenato degli altri.
//!
//! init e' l'unico processo che spawna i servizi user (via syscall `spawn`).
//! Il kernel spawna solo init (parent `None`); tutti gli altri processi user
//! comunicano con init sul canale di nascita creato da spawn (ADR-0008):
//! `spawn` ritorna il channel id verso il figlio, il figlio usa il canale 0
//! (= parent) per rispondere (es. TEST_DONE).

#![no_std]
#![no_main]

extern crate alloc;

use civis;
use civis::{println, print_str};
// Tag di fine-test e servizio-pronto (DocsB): single source in
// `syscall-numbers`, via `civis` (prima duplicati qui).
use civis::{SVC_READY, TEST_DONE};

/// Manifest degli hash dei servizi (Fase 36, identita' misurata, Strato 2 di
/// ADR-0026): generato a build-time da scripts/gen-service-hashes.sh sui
/// `.bin` finali, incluso qui via `VELORDO_SERVICE_HASHES` (esportata da
/// build-userland.sh; senza, questa compilazione fallisce loud).
include!(env!("VELORDO_SERVICE_HASHES"));

/// Hash atteso del servizio `bin` (nome display, es. `b"gpu"`), o
/// `None` se non e' nel manifest: embedded disk/fs (TCB del kernel — init non
/// ha i byte in mano per verificarli, li spawna per nome) e binari di test
/// (solo suite, mai servizi). Solo i servizi da disco sono pinnati.
fn expected_hash(bin: &[u8]) -> Option<u64> {
    match bin {
        b"gpu" => Some(HASH_GPU),
        b"console" => Some(HASH_CONSOLE),
        b"useruptime" => Some(HASH_USERUPTIME),
        b"vela" => Some(HASH_VELA),
        b"kbd" => Some(HASH_KBD),
        b"porta" => Some(HASH_PORTA),
        b"userposix" => Some(HASH_USERPOSIX),
        b"usershell" => Some(HASH_USERSHELL),
        b"usertime" => Some(HASH_USERTIME),
        b"vestigia" => Some(HASH_VESTIGIA),
        b"identis" => Some(HASH_IDENTIS),
        b"identis-login" => Some(HASH_IDENTIS_LOGIN),
        _ => None,
    }
}

/// Content-hash BLAKE2s-256 atteso (Fase 55, N0: `sys.content_hash`,
/// arcafs.md §8): generato a build-time dagli stessi byte (gen-service-
/// hashes.sh via hashlib; la crate `blake2s` e' l'implementazione guest,
/// tenute d'accordo da testsarca). Stesse esclusioni di `expected_hash`.
fn expected_blake(bin: &[u8]) -> Option<[u8; 32]> {
    match bin {
        b"gpu" => Some(BLAKE_GPU),
        b"console" => Some(BLAKE_CONSOLE),
        b"useruptime" => Some(BLAKE_USERUPTIME),
        b"vela" => Some(BLAKE_VELA),
        b"kbd" => Some(BLAKE_KBD),
        b"porta" => Some(BLAKE_PORTA),
        b"userposix" => Some(BLAKE_USERPOSIX),
        b"usershell" => Some(BLAKE_USERSHELL),
        b"usertime" => Some(BLAKE_USERTIME),
        b"vestigia" => Some(BLAKE_VESTIGIA),
        b"identis" => Some(BLAKE_IDENTIS),
        b"identis-login" => Some(BLAKE_IDENTIS_LOGIN),
        _ => None,
    }
}

/// Doppio pinning (Fase 55, N0): FNV-1a (manifest storico, veloce) +
/// BLAKE2s-256 (tamper-evidence, TCB). Ritorna true se OGNI pin esistente
/// passa; un binario senza pin (embedded/test) passa sempre (niente da
/// confrontare). Stesso predicato usato da t58 per provare il rifiuto.
fn verify_image(bin: &[u8], img: &[u8]) -> bool {
    if let Some(expected) = expected_hash(bin) {
        if civis::image_hash(img) != expected {
            return false;
        }
    }
    if let Some(expected) = expected_blake(bin) {
        if blake2s::blake2s(img) != expected {
            return false;
        }
    }
    true
}

/// Attende dal canale `chan` un messaggio con tag `tag` e lo consuma SENZA
/// reply (i READY sono fire-and-forget via send_async: rispondere accoderebbe
/// uno spurious message nel server). Usato per sincronizzare l'avvio.
fn wait_msg(chan: i64, tag: u64) {
    loop {
        match civis::recv() {
            Ok(m) if m.channel == chan as u64 && m.tag == tag => {
                return;
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

/// Attende il tag `tag` su TUTTI i canali `chans` (spawn parallelo, Fase 57):
/// loop `recv` finche' ogni canale ha mandato il suo READY (consumati senza
/// reply, fire-and-forget); la morte di un atteso e' FAIL LOUD con nome.
fn wait_any(chans: &[i64], tag: u64) {
    let mut seen = [false; 4];
    loop {
        let mut done = true;
        for (i, _) in chans.iter().enumerate() {
            if !seen[i] {
                done = false;
            }
        }
        if done {
            return;
        }
        match civis::recv() {
            Ok(m) if m.tag == tag => {
                for (i, c) in chans.iter().enumerate() {
                    if *c as u64 == m.channel {
                        seen[i] = true;
                    }
                }
            }
            Ok(m) if m.tag == civis::EXIT_NOTIFY => {
                println!("[init] boot FAILED (morte pre-ready pid={})", m.w1);
                civis::exit(1);
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

/// Spawna un binario embedded per nome e logga il canale figlio ottenuto.
/// Ritorna il channel id se lo spawn e' riuscito, altrimenti None.
fn spawn_child(name: &[u8]) -> Option<i64> {
    print_str!("[init] spawn ");
    civis::write_raw(name.as_ptr(), name.len());
    match civis::spawn(name) {
        Ok(chan) => {
            println!(" -> child chan={}", chan);
            Some(chan)
        }
        Err(_) => {
            println!(" -> FAILED");
            None
        }
    }
}

/// Metadati di un servizio avviabile: `obj=None` = binario embedded (spawn
/// per nome, solo disk/fs/log restano embedded — log per primo per disegno
/// ADR-0039); `obj=Some((bucket, key))` = oggetto ArcaFS letto col
/// filesystem e spawnato con `spawn_image` (Fase 55, N0).
///
/// **Il FAT non e' piu' una sorgente** (ADR-0059): il boot non attacca piu'
/// dischi FAT e i binari vivono tutti in ArcaFS (bucket di classe: `sys`
/// per i servizi, `vela` per i driver, `tst` per la suite, `usr` per i
/// programmi utente). `io`/`prio` valgono per il path da disco (l'embedded
/// li prende dalla tabella kernel).
struct SvcMeta {
    bin: &'static [u8],
    obj: Option<(&'static [u8], &'static [u8])>,
    prio: u8,
    io: &'static [(u16, u16)],
}

/// Porte I/O per i manifest da disco (copia dei range kernel in
/// `user_binary.rs`: init e' TCB e li dichiara al kernel via SpawnMeta).
/// `gpu` (L0b, ADR-0057): spazio di configurazione PCI (`0xCF8-0xCFF`) per
/// enumerare e programmare `virtio-gpu-pci`. Chi ha il conf PCI puo'
/// riprogrammare qualunque device: contenimento scarso senza IOMMU
/// (ADR-0026); il broker PCI arriva col secondo consumer PCI (audio).
/// D3: il cursore VGA non e' piu' suo (vive nella console).
const GPU_IO_RANGES: &[(u16, u16)] = &[(0xCF8, 0xCFF)];
/// `console` (D3, ADR-0060): cursore hardware del fallback VGA (CRTC). Il
/// path pixel non usa porte (cursore software).
const CONSOLE_IO_RANGES: &[(u16, u16)] = &[(0x3D4, 0x3D5)];
const KBD_PS2_RANGES: &[(u16, u16)] = &[(0x60, 0x64)];
/// CMOS/RTC per il fornitore di data/ora (Fase 50, P1 orologio): indice
/// `0x70` + dati `0x71` (accessi a byte, una sola entry).
const TIME_CMOS_RANGES: &[(u16, u16)] = &[(0x70, 0x71)];

/// Spawna un servizio da ArcaFS via `obj_get()` (Fase 55, N0).
fn spawn_object(bucket: &[u8], key: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    match civis::obj_get(bucket, key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Spawna un binario da un vettore di byte (Fase 55, N0).
fn spawn_image_from_vec(img: &[u8], meta: &SvcMeta) -> Option<i64> {
    if img.len() > civis::syscall_numbers::SPAWN_IMAGE_MAX {
        println!(" -> FAILED (image too large, {}B)", img.len());
        return None;
    }
    let name = match core::str::from_utf8(meta.bin) {
        Ok(s) => s,
        Err(_) => return None,
    };
    // Doppio pinning N0 (vedi `verify_image`): mismatch = None (a boot e'
    // panic come prima; in restart e' retry-con-hold con log loud). Il log
    // dice `hash-ok` solo se c'era un pinning dal manifest (mai claim senza
    // verifica).
    let pinned = expected_hash(meta.bin).is_some() || expected_blake(meta.bin).is_some();
    if !verify_image(meta.bin, img) {
        println!(" -> FAILED (hash mismatch)");
        return None;
    }
    let sm = match civis::SpawnMeta::new(name, meta.prio, meta.io) {
        Some(m) => m,
        None => return None,
    };
    match civis::spawn_image(img, &sm) {
        Ok(chan) => {
            if pinned {
                println!("{}B hash-ok -> child chan={}", img.len(), chan);
            } else {
                println!("{}B -> child chan={}", img.len(), chan);
            }
            Some(chan)
        }
        Err(_) => {
            println!(" -> FAILED (spawn_image rifiutato)");
            None
        }
    }
}

/// Spawna da manifest (embedded o oggetto ArcaFS) + log unificato.
/// D1 (niente fallback): a oggetto assente/diverso ritorna None senza
/// ripiegare su un'altra sorgente — a boot il chiamante fa panic loud, al
/// restart ritenta con hold (vedi `restart_service`). **Il FAT non e' piu'
/// una sorgente** (ADR-0059): il boot carica tutto da ArcaFS.
fn spawn_entry(meta: &SvcMeta) -> Option<i64> {
    if let Some((bucket, key)) = meta.obj {
        print_str!("[init] load obj ");
        civis::write_raw(key.as_ptr(), key.len());
        let img = match spawn_object(bucket, key) {
            Some(v) => v,
            _ => {
                println!(" -> oggetto assente, NIENTE fallback (D1)");
                return None;
            }
        };
        return spawn_image_from_vec(&img, meta);
    }
    spawn_child(meta.bin)
}

/// Cerca il token `s25=1` nella cmdline di boot (S2.5d: run dedicato allo
/// staticone, `RUN_S25=1` in run.sh). Match a token intero (delimitato da
/// spazi/nul/inizio/fine): niente falsi positivi dentro altre parole.
fn cmdline_has_s25() -> bool {
    let cmd = civis::boot_cmdline();
    let mut i = 0usize;
    while i + 5 <= cmd.len() {
        if &cmd[i..i + 5] == b"s25=1"
            && (i == 0 || cmd[i - 1] == b' ' || cmd[i - 1] == 0)
            && (i + 5 == cmd.len() || cmd[i + 5] == b' ' || cmd[i + 5] == 0)
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Spawna un binario di test da disco (Fase 21: `/test/*.bin`) e aspetta che
/// segnali la fine (IPC TEST_DONE sul canale di nascita). I test girano in
/// SEQUENZA: condividono la ramfs di cardo (path e file di lavoro) e la
/// sequenza rende output e PID deterministici.
/// Gestisce anche le morti dei servizi supervisionati (es. t27 uccide vela a
/// suite in corso): senza, il restart arriverebbe solo dopo la suite.
fn run_test(meta: &SvcMeta, supervised: &mut [Supervised]) {
    let Some(chan) = spawn_entry(meta) else {
        println!("[init] test mancante, FAIL loud");
        civis::exit(1);
    };
    loop {
        drain_stray_deaths(supervised);
        match civis::recv() {
            Ok(m) if m.channel == chan as u64 && m.tag == TEST_DONE => {
                let _ = civis::reply(TEST_DONE, 0, 0);
                return;
            }
            Ok(m) if m.tag == civis::INIT_BOUNCE => {
                let r = handle_bounce(supervised, m.w0);
                let _ = civis::reply(0, r, 0);
            }
            Ok(m) if m.tag == civis::EXIT_NOTIFY => {
                handle_child_death(supervised, m.w1 as i64, m.w0 as i64);
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

/// Bounce di un servizio supervisionato (Fase 35, hardening): uccide il figlio
/// (init e' parent: sempre consentito, anche col kill parent-scoped) e lascia
/// che il restart avvenga per la via normale (EXIT_NOTIFY → restart_service).
/// Ritorna il pid ucciso o `u64::MAX` se il servizio e' ignoto. Chiamato dai
/// loop che ricevono (run_test + supervisore): il chiamante risponde con
/// reply (i test guidano il caos tramite init invece di killare direttamente).
fn handle_bounce(supervised: &[Supervised], svc_disc: u64) -> u64 {
    match supervised.iter().find(|e| e.svc as u64 == svc_disc) {
        Some(e) => {
            println!("[init] bounce: uccido pid={} ({})", e.pid, e.svc as u64);
            let _ = civis::kill(e.pid, 0);
            e.pid as u64
        }
        None => {
            println!("[init] bounce: servizio ignoto ({})", svc_disc);
            u64::MAX
        }
    }
}

/// Gestisce EXIT_NOTIFY di un figlio: se supervisionato → restart, altrimenti
/// log. Usato sia da run_test che dal loop supervisore (single path).
fn handle_child_death(supervised: &mut [Supervised], pid: i64, code: i64) {
    match supervised.iter_mut().find(|e| e.pid == pid) {
        Some(e) if !e.held => {
            if e.on_death == OnDeath::LoginGate {
                restart_login(e, code);
                return;
            }
            println!(
                "[init] supervisione: pid={} morto (code {}), riavvio",
                pid, code
            );
            restart_service(e);
        }
        Some(_) => {
            println!(
                "[init] supervisione: pid={} morto ma HELD, ignoro",
                pid
            );
        }
        None => {
            println!(
                "[init] child pid={} morto (code {}), non supervisionato",
                pid, code
            );
        }
    }
}

/// Il **login** e' morto: si riavvia **solo se ha fallito** (codice != 0).
///
/// Il login e' il gate del sistema (la shell parte solo se l'autenticazione
/// e' riuscita): fallito, init lo riavvia come un getty — l'utente trova un
/// altro prompt invece di un sistema muto. Riuscito (codice 0), la shell e'
/// gia' partita e non si tocca niente.
///
/// Anti spawn-storm come `restart_service`: 3 riavvii in 300 tick, poi HELD
/// (accesso negato, ma il sistema vivo e loggato).
fn restart_login(e: &mut Supervised, code: i64) {
    if code == 0 {
        println!("[init] login ok (shell avviata): niente riavvio");
        e.pid = -1;
        return;
    }
    // Anti spawn-storm come `restart_service`: 3 riavvii in 300 tick, poi
    // HELD (accesso negato, ma il sistema vivo e loggato).
    loop {
        let now = civis::get_ticks();
        if now - e.window_start > 300 {
            e.window_start = now;
            e.restarts = 0;
        }
        e.restarts += 1;
        if e.restarts > 3 {
            e.held = true;
            println!("[init] login: riavvii falliti, HELD (accesso negato)");
            return;
        }
        println!(
            "[init] login: tentativo fallito (code {}), riavvio {}",
            code, e.restarts
        );
        // Pausa prima del riavvio: se il login muore subito (Identis
        // irraggiungibile) il ciclo non diventa una macina da respawn.
        civis::spin_ticks(50);
        let Some(chan) = spawn_entry(e.meta) else {
            println!("[init] login: spawn FAILED, riprovo");
            continue;
        };
        e.chan = chan;
        // Il READY attende che il login abbia **registrato il servizio**: e'
        // da li' che init ricava il pid. Senza l'attesa il pid sarebbe -1 e
        // la morte successiva non sarebbe riconosciuta (niente altri
        // riavvii: il sistema resterebbe muto).
        if wait_ready(chan) {
            // Il pid dal canale (peer_pid): deterministico, non una race
            // sulla registrazione del servizio.
            e.pid = civis::peer_pid(chan as u64).unwrap_or(-1);
            println!("[init] login riavviato pid={}", e.pid);
            return;
        }
        println!("[init] login: READY mancante (morte/timeout), riprovo");
    }
}

/// Servizio supervisionato da init (Fase 14, init-restart): alla morte viene
/// riavviato dalla sua sorgente (embedded o disco, Fase 21). Solo
/// gpu/disk/fs/vela/kbd/tty; gli altri figli (uptime/shell/test) sono
/// loggati ma non riavviati.
/// Cosa fare quando un figlio supervisionato muore.
#[derive(Clone, Copy, PartialEq)]
enum OnDeath {
    /// Riavvia il servizio (la via normale).
    Restart,
    /// Il **login**: la shell parte solo se l'autenticazione e' riuscita,
    /// quindi si riavvia **solo se il login ha fallito** (codice di uscita
    /// != 0). Se ha riescito (codice 0) la shell e' gia' partita: un altro
    /// prompt qui sarebbe una seconda porta aperta.
    LoginGate,
}

/// Voce della tabella di supervisione.
struct Supervised {
    meta: &'static SvcMeta,
    svc: civis::Service,
    chan: i64,
    pid: i64,
    restarts: u32,
    window_start: i64,
    held: bool,
    /// Politica alla morte (vedi `OnDeath`).
    on_death: OnDeath,
}

impl Supervised {
    /// Voce nuova (nessun canale, nessun pid, nessun restart ancora).
    const fn new(meta: &'static SvcMeta, svc: civis::Service) -> Self {
        Supervised {
            meta,
            svc,
            chan: -1,
            pid: -1,
            restarts: 0,
            window_start: 0,
            held: false,
            on_death: OnDeath::Restart,
        }
    }
}

/// Attesa di `n` tick con spin puri IF=1 a batch (non affama il timer).
/// Attende SVC_READY sul canale di nascita del figlio appena respawnato.
/// Consuma SENZA reply (fire-and-forget, vedi wait_msg). Ritorna false se il
/// figlio muore prima del READY (EXIT_NOTIFY sul suo stesso canale) o se
/// scade il bound (500 tick ~ 5 s, restart atteso ~50): mai wedge il
/// supervisore. La notifica di morte e' consumata qui, il chiamante riprova.
/// Le EXIT_NOTIFY di ALTRI figli (una morte durante un restart) NON si
/// scartano: vanno nello stash e il chiamante le processa (sotto). Scartarle
/// perde restart (osservato t32: block morto durante il restart di vela
/// → mai riavviato → cascata fino al panic di init).
fn wait_ready(chan: i64) -> bool {
    let t0 = civis::get_ticks();
    loop {
        match civis::recv() {
            Ok(m) if m.channel == chan as u64 && m.tag == SVC_READY => {
                println!(
                    "[init] wait_ready: READY su canale {} (peer {:?})",
                    chan,
                    civis::peer_pid(m.channel)
                );
                return true;
            }
            Ok(m) if m.channel == chan as u64 && m.tag == civis::EXIT_NOTIFY => {
                return false;
            }
            Ok(m) if m.tag == civis::EXIT_NOTIFY => {
                stash_death(m.w1 as i64, m.w0 as i64);
            }
            Ok(_) => {}
            Err(_) => {}
        }
        if civis::get_ticks() - t0 > 500 {
            return false;
        }
    }
}

/// Morti altrui viste dentro `wait_ready` (vedi sopra): init e' single-thread
/// e single-loop, un array statico basta (cap 8 > 7 servizi supervisionati).
/// Accesso via `addr_of_mut!` (edition 2024: niente `static_mut_refs`).
static mut STRAY_DEATHS: [(i64, i64); STRAY_CAP] = [(0, 0); STRAY_CAP];
static mut N_STRAY: usize = 0;
const STRAY_CAP: usize = 8;

fn stash_death(pid: i64, code: i64) {
    unsafe {
        let n = core::ptr::addr_of_mut!(N_STRAY).read();
        if n < STRAY_CAP {
            core::ptr::addr_of_mut!(STRAY_DEATHS).cast::<(i64, i64)>().add(n).write((pid, code));
            core::ptr::addr_of_mut!(N_STRAY).write(n + 1);
        } else {
            println!("[init] supervisione: stash morti pieno, perdo pid={}", pid);
        }
    }
}

/// Processa le morti stashtate (restart/log come quelle viste in recv).
/// Da chiamare nei loop di attesa (run_test + supervisore) cosi' nessuna
/// morte va persa mentre un restart e' in corso.
fn drain_stray_deaths(supervised: &mut [Supervised]) {
    loop {
        let next = unsafe {
            let n = core::ptr::addr_of_mut!(N_STRAY).read();
            if n == 0 {
                None
            } else {
                core::ptr::addr_of_mut!(N_STRAY).write(n - 1);
                Some(core::ptr::addr_of_mut!(STRAY_DEATHS).cast::<(i64, i64)>().add(n - 1).read())
            }
        };
        match next {
            Some((pid, code)) => handle_child_death(supervised, pid, code),
            None => return,
        }
    }
}

/// Riavvia un servizio morto (respawn + attesa prontezza). Backoff anti
/// spawn-storm: 20 tick prima di ogni tentativo; oltre 3 restart in 300 tick
/// il servizio va in hold (stop + log, sistema degradato ma vivo).
fn restart_service(e: &mut Supervised) {
    loop {
        if e.held {
            return;
        }
        let now = civis::get_ticks();
        if now - e.window_start > 300 {
            e.window_start = now;
            e.restarts = 0;
        }
        e.restarts += 1;
        if e.restarts > 3 {
            e.held = true;
            println!("[init] supervisione: restart falliti, HELD (stop)");
            return;
        }
        println!("[init] supervisione: riavvio (tentativo {})", e.restarts);
        civis::spin_ticks(20);
        let Some(chan) = spawn_entry(e.meta) else {
            println!("[init] supervisione: spawn FAILED, riprovo");
            continue;
        };
        e.chan = chan;
        if wait_ready(chan) {
            e.pid = civis::service_pid(e.svc).unwrap_or(-1);
            println!("[init] supervisione: riavviato pid={}", e.pid);
            return;
        }
        println!("[init] supervisione: READY mancante (morte/timeout), riprovo");
    }
}

/// Manifest dei servizi (Fase 21, servizi da disco): ordine = ordine di boot.
/// Solo disk/fs restano embedded (storage-TCB: init li spawna per nome prima
/// che il FS esista); tutto il resto vive in `/bin` su /fat (iniettati a
/// build via mcopy) e parte via `spawn_image`. Priorita': 16 Normal, 1 Low.
/// Driver del display (D3, ADR-0060): virtio-gpu, pubblica `/dev/display`.
const SVC_GPU: SvcMeta = SvcMeta {
    bin: b"gpu",
    obj: Some((b"vela", b"bin/gpu.bin")),
    prio: 16,
    io: GPU_IO_RANGES,
};
/// Console server (D3): modello a celle, font, surface; serve `/dev/console`.
const SVC_CONSOLE: SvcMeta = SvcMeta {
    bin: b"console",
    obj: Some((b"sys", b"bin/console.bin")),
    prio: 16,
    io: CONSOLE_IO_RANGES,
};
const SVC_UPTIME: SvcMeta = SvcMeta {
    bin: b"useruptime",
    obj: Some((b"sys", b"bin/uptime.bin")),
    prio: 1,
    io: &[],
};
const SVC_VELA: SvcMeta = SvcMeta {
    bin: b"vela",
    obj: Some((b"sys", b"bin/vela.bin")),
    prio: 16,
    io: &[],
};
const SVC_KBD: SvcMeta = SvcMeta {
    bin: b"kbd",
    obj: Some((b"vela", b"bin/kbd.bin")),
    prio: 16,
    io: KBD_PS2_RANGES,
};
const SVC_PORTA: SvcMeta = SvcMeta {
    bin: b"porta",
    obj: Some((b"sys", b"bin/porta.bin")),
    prio: 16,
    io: &[],
};
const SVC_POSIX: SvcMeta = SvcMeta {
    bin: b"userposix",
    obj: Some((b"sys", b"bin/posix/posix.bin")),
    prio: 16,
    io: &[],
};
const SVC_SHELL: SvcMeta = SvcMeta {
    bin: b"usershell",
    obj: Some((b"sys", b"bin/posix/shell.bin")),
    prio: 16,
    io: &[],
};
// Servizio identita'/autenticazione utenti (ADR-0055): identis gestisce il
// bucket `identis:config` (nome -> hash password). identis-login e' il primo
// programma interattivo (chiede utente/password, stampa SUCCESS/FAIL).
const SVC_IDENTIS: SvcMeta = SvcMeta {
    bin: b"identis",
    obj: Some((b"sys", b"bin/identis.bin")),
    prio: 16,
    io: &[],
};
const SVC_LOGIN: SvcMeta = SvcMeta {
    bin: b"identis-login",
    obj: Some((b"sys", b"bin/identis-login.bin")),
    prio: 16,
    io: &[],
};
const SVC_TIME: SvcMeta = SvcMeta {
    bin: b"usertime",
    obj: Some((b"vela", b"bin/time.bin")),
    prio: 16,
    io: TIME_CMOS_RANGES,
};
/// Gateway centrale di logging L1 (Fase 57, ADR-0039, R8):
/// EMBEDDED come block/cardo (deve partire prima che il FS esista — spawn
/// per nome, mai da disco). Primo dopo init, supervisionato con restart.
const SVC_VESTIGIA: SvcMeta = SvcMeta {
    bin: b"vestigia",
    obj: None,
    prio: 16,
    io: &[],
};
// Suite di test (D2): caricate per object_id dal bucket `tst` (hash-pinned
// come i servizi); `path` resta come gemello FAT documentato (t39 lo prova).
const TEST_FS: SvcMeta = SvcMeta { bin: b"usertestfs", obj: Some((b"tst", b"test/testfs.bin")), prio: 16, io: &[] };
const TEST_FAT: SvcMeta = SvcMeta { bin: b"usertestfat", obj: Some((b"tst", b"test/testfat.bin")), prio: 16, io: &[] };
const TEST_ARCA: SvcMeta = SvcMeta { bin: b"usertestsarca", obj: Some((b"tst", b"test/testarca.bin")), prio: 16, io: &[] };
// Suite della personalita' POSIX (Fase 58.5, ADR-0041): gira PRIMA di
// usertests (t54 deve precedere i drop di diritti di t34 in usertests).
const TEST_POSIX: SvcMeta = SvcMeta { bin: b"userposixtests", obj: Some((b"tst", b"test/posixtst.bin")), prio: 16, io: &[] };
// Protocollo di autenticazione/creazione utenti (ADR-0055, Fase 3.1):
// sessione con token, add_user, duplicati, nomi non conformi, elenco.
const TEST_IDENTIS: SvcMeta = SvcMeta { bin: b"identistest", obj: Some((b"tst", b"test/idtest.bin")), prio: 16, io: &[] };
const TESTS: SvcMeta = SvcMeta { bin: b"usertests", obj: Some((b"tst", b"test/tests.bin")), prio: 16, io: &[] };
// Thread 1:1 (S-T, ADR-0046): dopo usertests (nessuna dipendenza dai diritti),
// prima della shell interattiva.
const TEST_THREADS: SvcMeta = SvcMeta { bin: b"userthreadtest", obj: Some((b"tst", b"test/thread.bin")), prio: 16, io: &[] };
// Fuse staticone (S2.5d): ricompone i chunk rustc e lo spawna bigstack.
// Solo obj (niente FAT: i chunk vivono solo su arca grande), solo se la
// cmdline contiene `s25=1` (run dedicato, mai nel gate).
const TEST_S25: SvcMeta = SvcMeta { bin: b"s25fuse", obj: Some((b"tst", b"tool/s25fuse.bin")), prio: 16, io: &[] };
#[cfg(feature = "bench")]
const TEST_BENCH: SvcMeta = SvcMeta { bin: b"userbench", obj: Some((b"tst", b"test/bench.bin")), prio: 16, io: &[] };

/// Spawna dal manifest SENZA attesa (spawn parallelo, Fase 57): il chiamante
/// sincronizza con `wait_any`. Fallimento = None (fail loud al chiamante).
fn boot_svc_nowait(meta: &'static SvcMeta) -> Option<i64> {
    spawn_entry(meta)
}

/// Spawna dal manifest e attende SVC_READY se richiesto. A boot il fallimento
/// e' FAIL LOUD (panic via exit: senza servizi il sistema e' inutilizzabile e
/// un boot silenzioso degradato nasconderebbe l'errore di build).
fn boot_svc(meta: &'static SvcMeta, wait: bool) -> Option<i64> {
    let chan = spawn_entry(meta)?;
    if wait {
        wait_msg(chan, SVC_READY);
    }
    Some(chan)
}

/// Entry di init: spawna i servizi e resta vivo come root della process tree.
civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    let my_pid = civis::getpid();
    println!("[init] up, pid={}", my_pid);

    // Spawna i servizi user. Fase 57: log+disk in PARALLELO (log non aspetta
    // nessuno — prova che il chicken-egg e' morto; disk fa detection). Poi
    // fs (serve Disk registrato), time (serve /fat), FLUSH a vestigia (dopo
    // time: backdate/re-key), gpu (prima di kbd), uptime, vela, kbd,
    // tty, posix. A boot ogni spawn mancato e' FAIL LOUD (exit → panic).
    // vestigia PRIMA di cardo per disegno (ADR-0039): assorbe tutto in RAM e
    // riversa alla FLUSH; block fa READY subito dopo detection +
    // service_register (prima del mount dei nodi, che aspetta Fs).
    let Some(vestigia_chan) = boot_svc_nowait(&SVC_VESTIGIA) else {
        println!("[init] boot FAILED (log spawn), panic");
        civis::exit(1);
    };
    let Some(disk_chan) = spawn_child(b"block") else {
        println!("[init] boot FAILED (disk), panic");
        civis::exit(1);
    };
    wait_any(&[vestigia_chan, disk_chan], SVC_READY);
    let _ = civis::vestigia::log(b"init", b"log ready");
    let _ = civis::vestigia::log(b"init", b"disk ready");

    // cardo SUBITO DOPO disk (serve Disk registrato: resta dopo per non
    // spendere il bound HELLO — il mount aspetterebbe comunque il disco).
    // Chi usa il FS parte solo dopo che cardo e' pronto (READY = pronto).
    let Some(fs_chan) = spawn_child(b"cardo") else {
        println!("[init] boot FAILED (fs), panic");
        civis::exit(1);
    };
    wait_msg(fs_chan, SVC_READY);
    let _ = civis::vestigia::log(b"init", b"fs ready");
    // Time da disco (Fase 50, P1 orologio): registra Time + ack; chi serve
    // data/ora (cardo per mtime, vestigia per i timbri) lo risolve per nome.
    if boot_svc(&SVC_TIME, true).is_none() {
        println!("[init] boot FAILED (time), panic");
        civis::exit(1);
    }
    let _ = civis::vestigia::log(b"init", b"time ready");
    // FLUSH a vestigia (Fase 57, ADR-0039): dopo fs (backend) + time (epoch
    // per backdate/re-key). Da qui dual-write RAM+volume; il pre-boot resta
    // anche su seriale (duplicazione dichiarata, non perdita).
    if civis::vestigia::log_flush().is_err() {
        println!("[init] boot FAILED (log flush), panic");
        civis::exit(1);
    }
    // Driver del display (D3, ADR-0060): `gpu` enumera virtio-gpu e
    // pubblica `/dev/display`. Deve partire PRIMA della console (che lo
    // apre come client).
    let Some(gpu_chan) = boot_svc(&SVC_GPU, true) else {
        println!("[init] boot FAILED (gpu), panic");
        civis::exit(1);
    };
    let _ = gpu_chan;
    let _ = civis::vestigia::log(b"init", b"gpu ready");
    // Console server (D3): registra `Console` + ack subito dopo la
    // registrazione (prima del mount /dev/input che richiede cardo, gia'
    // pronto qui). kbd la risolve per nome al passo 4.
    let Some(console_chan) = boot_svc(&SVC_CONSOLE, true) else {
        println!("[init] boot FAILED (console), panic");
        civis::exit(1);
    };
    let _ = console_chan;
    let _ = civis::vestigia::log(b"init", b"console ready");
    // uptime: nessuna attesa (solo informativo, come prima).
    boot_svc(&SVC_UPTIME, false);
    let Some(vela_chan) = boot_svc(&SVC_VELA, true) else {
        println!("[init] boot FAILED (vela), panic");
        civis::exit(1);
    };
    let _ = vela_chan;
    let _ = civis::vestigia::log(b"init", b"vela ready");
    // 4. kbd + attesa READY (Fase 15: registra Kbd + mount /dev/kbd; Fs
    //    garantito dal passo 2, quindi riesce subito a boot).
    if boot_svc(&SVC_KBD, true).is_none() {
        println!("[init] boot FAILED (kbd), panic");
        civis::exit(1);
    }
    let _ = civis::vestigia::log(b"init", b"kbd ready");
    // 5. porta + attesa READY (Fase 15: registra /dev/input; /dev/kbd e
    //    /dev/console garantiti dai passi precedenti, riesce subito a boot).
    if boot_svc(&SVC_PORTA, true).is_none() {
        println!("[init] boot FAILED (tty), panic");
        civis::exit(1);
    }
    let _ = civis::vestigia::log(b"init", b"tty ready");
    // 6. userposix + attesa READY (Fase 40.3, P1): skeleton senza dipendenze
    //    (registra solo il servizio e resta in recv), riesce subito a boot.
    if boot_svc(&SVC_POSIX, true).is_none() {
        println!("[init] boot FAILED (posix), panic");
        civis::exit(1);
    }
    let _ = civis::vestigia::log(b"init", b"posix ready");

    // Identis (servizio identita'): servizio di SISTEMA come gli altri —
    // parte PRIMA della suite (il test `identistest` parla con lui) e
    // prima di login.
    if boot_svc(&SVC_IDENTIS, true).is_none() {
        println!("[init] boot FAILED (identis), panic");
        civis::exit(1);
    }
    let _ = civis::vestigia::log(b"init", b"identis ready");

    // Tabella supervisione (Fase 14, init-restart): gpu/fs/vela/kbd/tty/
    // disk/posix/time/log vengono riavviati alla morte (dalla loro sorgente: embedded per
    // disk/fs, disco per gli altri — Fase 21); gli altri figli solo loggati.
    // Costruita prima dei test cosi' anche run_test supervisiona (t27 uccide
    // vela a suite in corso). NOTA: un restart di cardo wipa la ramfs
    // (fixture dei test) — in suite solo t28 lo uccide (Fase 14.12) e
    // ricostruisce la fixture al restart.
    //
    // Disk/fs embedded: manifest inline (path=None) con gli stessi nomi: il
    // restart riusa `spawn_child` come a boot.
    const META_DISK: SvcMeta = SvcMeta { bin: b"block", obj: None, prio: 16, io: &[] };
    const META_FS: SvcMeta = SvcMeta { bin: b"cardo", obj: None, prio: 16, io: &[] };
    let mut supervised = [
        Supervised::new(&SVC_GPU, civis::Service::Gpu),
        Supervised::new(&SVC_CONSOLE, civis::Service::Console),
        Supervised::new(&META_DISK, civis::Service::Block),
        Supervised::new(&META_FS, civis::Service::Cardo),
        Supervised::new(&SVC_VELA, civis::Service::Vela),
        Supervised::new(&SVC_KBD, civis::Service::Kbd),
        Supervised::new(&SVC_PORTA, civis::Service::Porta),
        Supervised::new(&SVC_POSIX, civis::Service::Posix),
        Supervised::new(&SVC_TIME, civis::Service::Time),
        Supervised::new(&SVC_VESTIGIA, civis::Service::Vestigia),
        // Il **login** (programma di autenticazione): politica di morte
        // propria — si riavvia solo se ha fallito (vedi `OnDeath`).
        Supervised {
            on_death: OnDeath::LoginGate,
            ..Supervised::new(&SVC_LOGIN, civis::Service::Login)
        },
    ];
    for e in supervised.iter_mut() {
        e.pid = civis::service_pid(e.svc).unwrap_or(-1);
    }

    // Test suite in sequenza: usertestfs, usertestfat, usertests (Fase 9.5).
    // Di default (feature `skip_tests`, run di produzione) SALTATA: boot
    // veloce dritto alla shell. Con `--no-default-features` (RUN_TESTS=1,
    // run-tests.sh) eseguita come gate di regressione.
    #[cfg(feature = "skip_tests")]
    println!("[init] test suite saltata (production run)");
    #[cfg(not(feature = "skip_tests"))]
    {
        println!("[init] avvio test suite");
        run_test(&TEST_FS, &mut supervised);
        // ADR-0059: la suite FAT32 esce dal gate (il FAT non e' piu' il
        // filesystem di sistema). Resta nel repo e gira solo con il feature
        // `fat_tests` (run dedicato con un disco FAT attaccato).
        #[cfg(feature = "fat_tests")]
        run_test(&TEST_FAT, &mut supervised);
        run_test(&TEST_ARCA, &mut supervised);
        run_test(&TEST_POSIX, &mut supervised);
        run_test(&TEST_IDENTIS, &mut supervised);
        run_test(&TESTS, &mut supervised);
        run_test(&TEST_THREADS, &mut supervised);
    }

    // Bench throughput (Fase 23, feature `bench`): dopo l'eventuale suite,
    // prima della shell. Mai nel gate (rumore di timing + log dedicato).
    #[cfg(feature = "bench")]
    {
        println!("[init] avvio bench");
        run_test(&TEST_BENCH, &mut supervised);
    }

    // Staticone rustc (S2.5d): solo se la cmdline contiene `s25=1` (run.sh
    // con RUN_S25=1: volume + RAM grandi). Dopo suite/bench, prima della
    // shell (che resta usabile dopo). Mai nel gate.
    if cmdline_has_s25() {
        println!("[init] avvio s25fuse (staticone rustc)");
        run_test(&TEST_S25, &mut supervised);
    }

    // Login al posto della shell: chiede utente/password, stampa SUCCESS/FAIL.
    // Resta ULTIMO: e' il programma interattivo (legge la tastiera: dopo la
    // suite, non in concorrenza con essa).
    //
    // Il login e' il **gate**: la shell parte solo se l'autenticazione va a
    // buon fine (nel login, non in init). Se il login fallisce esce con
    // codice != 0 e init lo **riavvia** (politica `OnDeath::LoginGate`):
    // l'utente trova un altro prompt invece di un sistema muto.
    let Some(login_chan) = boot_svc(&SVC_LOGIN, false) else {
        println!("[init] boot FAILED (login), panic");
        civis::exit(1);
    };
    // Il READY attende che il login abbia registrato il servizio: e' da li'
    // che init ricava il pid per supervisionarlo.
    if !wait_ready(login_chan) {
        println!("[init] boot FAILED (login READY), panic");
        civis::exit(1);
    }
    for e in supervised.iter_mut() {
        if e.svc == civis::Service::Login {
            // Il pid dal **canale** (peer_pid), non dal servizio registrato:
            // il canale di nascita e' la fonte di verita' (il peer e' il
            // login) e non dipende da un messaggio ne' da una registrazione
            // che potrebbe non essere ancora visibile.
            e.pid = civis::peer_pid(login_chan as u64).unwrap_or(-1);
            println!("[init] login ready pid={}", e.pid);
        }
    }

    // Supervisore init-restart (Fase 14): se un servizio e' morto prima della
    // supervisione (es. tra boot e run_test), riavvialo subito; poi loop.
    for e in supervised.iter_mut() {
        if e.pid < 0 {
            e.pid = civis::service_pid(e.svc).unwrap_or(-1);
        }
        if e.pid < 0 {
            println!("[init] supervisione: servizio assente all'avvio, riavvio");
            restart_service(e);
        }
    }

    println!("[init] supervisione attiva, hanging in recv");
    loop {
        drain_stray_deaths(&mut supervised);
        match civis::recv() {
            Ok(m) if m.tag == civis::EXIT_NOTIFY => {
                handle_child_death(&mut supervised, m.w1 as i64, m.w0 as i64);
            }
            Ok(m) if m.tag == civis::INIT_BOUNCE => {
                let r = handle_bounce(&supervised, m.w0);
                let _ = civis::reply(0, r, 0);
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[init] panic");
    civis::exit(1)
}
