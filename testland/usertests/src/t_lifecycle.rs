use super::*;

// ── Lifecycle tests (Fase 14, ADR-0010) ────────────────────────────

/// t22 — lifecycle churn: spawna e termina molti piu' processi del vecchio
/// limite cumulativo (32 dal boot). Ogni helper CHURN materializza ~2 MiB di
/// heap e poi esce SENZA T_DONE: il parent osserva la notifica EXIT_NOTIFY.
/// Verifica:
///   1. ogni spawn riesce (il riuso dei PID evita l'esaurimento);
///   2. per ogni figlio arriva la EXIT_NOTIFY con exit code 0;
///   3. i PID vengono riusati (pid distinti < spawn totali);
///   4. niente frame leak: se il teardown non liberasse heap/stack i frame si
///      accumulerebbero (2 MiB × 42 = 84 MiB) finche' l'allocazione fallisce
///      e un figlio esplode o uno spawn fallisce.
pub fn t_lifecycle_churn() -> bool {
    const N: usize = 42;
    const KIB: u64 = 2048; // ~2 MiB materializzati da ogni figlio
    let mut pids = Vec::new();
    for i in 0..N {
        // Spawn + CFG (modo CHURN): l'ACK porta il pid del figlio.
        let (chan, pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_CHURN, KIB) {
            Some(x) => x,
            None => {
                println!("[usertests] t22: spawn #{} FAILED (pool esaurito?)", i);
                return false;
            }
        };
        pids.push(pid as i64);
        // Attendi la morte di QUESTO figlio (notifica kernel→parent).
        match helpers::wait_exit(chan) {
            Some((code, _)) if code == 0 => {}
            Some((code, _)) => {
                println!("[usertests] t22: child #{} exit code {} (atteso 0)", i, code);
                return false;
            }
            None => return false,
        }
    }
    // Riuso PID: con N > 32 spawn sequenziali i pid devono essersi ripetuti.
    let mut distinct = Vec::new();
    for &p in &pids {
        if !distinct.contains(&p) {
            distinct.push(p);
        }
    }
    let reused = distinct.len() < pids.len();
    println!(
        "[usertests] t22: {} spawn, {} pid distinti (riuso={})",
        pids.len(),
        distinct.len(),
        reused
    );
    reused
}

/// t23 — kill(pid) + notifica exit. Spawna un helper KILLME (busy-wait), ne
/// ricava il pid dall'ACK, lo kill() con un code noto e attende la notifica
/// EXIT_NOTIFY con quel code e quel pid. Infine verifica che il pool non sia
/// esaurito (spawn + exit di un altro helper riescono ancora).
pub fn t_kill() -> bool {
    helpers::drain_stray();
    let (chan, pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_KILLME, 0) {
        Some(x) => x,
        None => return false,
    };
    let code = -7i64;
    if civis::kill(pid as i64, code).is_err() {
        println!("[usertests] t23: kill(pid={}) FAILED", pid);
        return false;
    }
    match helpers::wait_exit(chan) {
        Some((c, p)) if c == code && p == pid as i64 => {}
        _ => {
            println!(
                "[usertests] t23: exit notify mismatch (atteso code {} pid {})",
                code, pid
            );
            return false;
        }
    }
    // Dopo la kill il pool deve accettare ancora spawn/exit (riuso sicuro).
    let (chan2, _pid2) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_CHURN, 256) {
        Some(x) => x,
        None => {
            println!("[usertests] t23: spawn post-kill FAILED");
            return false;
        }
    };
    match helpers::wait_exit(chan2) {
        Some((0, _)) => true,
        _ => false,
    }
}

/// t24 — notifica unificata di morte a TUTTI i peer (Fase 14). Un server
/// sacrificale SRVDIE registra `Service::Test` e non risponde mai; un client
/// SYNCWAIT lo risolve per nome e resta bloccato in `send` sync. Il test fa
/// 2 `send_async` e poi killa il server. Verifica:
///   1. path async: `wait_reply` ritorna `Err(ServerDied)` con pid+code esatti;
///   2. path sync: il client (sbloccato da `wake_senders`) osserva a sua
///      volta l'EXIT_NOTIFY e riporta T_DONE(w0=1);
///   3. lo slot servizio e' liberato (lookup → Err) e il pool resta sano.
/// Determinismo senza sleep: handshake T_READY (kill solo a lookup avvenuto)
/// e T_GO (il T_DONE del client non puo' anticipare la notifica nella coda
/// del test). Ogni attesa ha garanzia di terminazione (reclaim a ogni tick).
/// NOTA: la EXIT_NOTIFY del server viene consumata da `wait_reply`, quindi
/// niente `wait_exit` per lui qui (il path parent e' gia' coperto da t23).
pub fn t_server_death_notify() -> bool {
    helpers::drain_stray();
    let (s_chan, s_pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_SRVDIE, 0) {
        Some(x) => x,
        None => {
            println!("[usertests] t24: spawn SRVDIE FAILED");
            return false;
        }
    };
    let (h_chan, _h_pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_SYNCWAIT, 0) {
        Some(x) => x,
        None => {
            println!("[usertests] t24: spawn SYNCWAIT FAILED");
            return false;
        }
    };
    // Handshake: il client ha risolto Test (server vivo).
    if !helpers::recv_expect(h_chan, helpers::T_READY) {
        println!("[usertests] t24: T_READY dal client mancante");
        return false;
    }
    // Due richieste async in volo (mai risposte: il server non fa recv).
    let r1 = match civis::send_async(s_chan, helpers::T_REQ, 0xBEEF, 0) {
        Ok(r) => r,
        Err(_) => {
            println!("[usertests] t24: send_async FAILED");
            return false;
        }
    };
    let _ = civis::send_async(s_chan, helpers::T_REQ, 0xBEEF + 1, 0);
    // Kill: wake_senders sblocca il client sync, il reclaim notifica tutti.
    let code = -9i64;
    if civis::kill(s_pid as i64, code).is_err() {
        println!("[usertests] t24: kill(pid={}) FAILED", s_pid);
        return false;
    }
    // Path async: la reply non arrivera' mai → ServerDied con pid+code.
    // (Notifiche di altri pid = stale di helper precedenti: consumate.)
    loop {
        match civis::wait_reply(r1) {
            Err(civis::WaitReplyError::ServerDied { pid, code: c })
                if pid == s_pid && c == code => break,
            Err(civis::WaitReplyError::ServerDied { pid, .. }) if pid != s_pid => continue,
            Err(civis::WaitReplyError::ServerDied { pid, code: c }) => {
                println!(
                    "[usertests] t24: ServerDied errato (pid={}, code={}, attesi pid={} code={})",
                    pid, c, s_pid, code
                );
                return false;
            }
            other => {
                println!("[usertests] t24: wait_reply inatteso: {:?}", other);
                return false;
            }
        }
    }
    // Via-libera al client: ora puo' inviare T_DONE senza race.
    if civis::send(h_chan, helpers::T_GO, 0, 0).is_err() {
        println!("[usertests] t24: T_GO al client FAILED");
        return false;
    }
    // Path sync: il client riporta T_DONE(w0=1) dopo aver visto EXIT_NOTIFY.
    if !helpers::recv_expect(h_chan, helpers::T_DONE) {
        println!("[usertests] t24: T_DONE(w0=1) dal client mancante");
        return false;
    }
    // Il client e' uscito pulito dopo il report.
    match helpers::wait_exit(h_chan) {
        Some((0, _)) => {}
        _ => {
            println!("[usertests] t24: exit del client anomala");
            return false;
        }
    }
    // Slot servizio liberato dal morto.
    if civis::service_lookup(civis::Service::Test).is_ok() {
        println!("[usertests] t24: slot Test ancora occupato dopo la morte");
        return false;
    }
    // Pool sano: spawn/exit post-mortem.
    let (chan2, _) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_CHURN, 64) {
        Some(x) => x,
        None => {
            println!("[usertests] t24: spawn post-kill FAILED");
            return false;
        }
    };
    match helpers::wait_exit(chan2) {
        Some((0, _)) => true,
        _ => false,
    }
}

/// t25 — purge dei mount alla morte di un driver (Fase 14). Un driver
/// sacrificale MNTDIE registra "/dev/tdie"; il test apre /dev/tdie/null (routing al
/// driver provato), killa il driver e ne registra un secondo sullo stesso
/// prefix. Senza purge lo stale (primo in lista per resolve_mount)
/// avvelenerebbe il routing anche dopo la re-registrazione → open fallisce.
/// Con purge: serve il nuovo driver. Deterministico, nessun timing.
pub fn t_driver_death_mount() -> bool {
    helpers::drain_stray();
    let (d1_chan, d1_pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_MNTDIE, 0) {
        Some(x) => x,
        None => {
            println!("[usertests] t25: spawn MNTDIE#1 FAILED");
            return false;
        }
    };
    if !helpers::recv_expect(d1_chan, helpers::T_READY) {
        println!("[usertests] t25: T_READY da MNTDIE#1 mancante");
        let _ = helpers::wait_exit(d1_chan);
        return false;
    }
    let Ok(fd1) = civis::open("/dev/tdie/null", 0) else {
        println!("[usertests] t25: open /dev/tdie/null via D1 FAILED");
        return false;
    };
    if civis::kill(d1_pid as i64, -11).is_err() {
        println!("[usertests] t25: kill D1 FAILED");
        return false;
    }
    match helpers::wait_exit(d1_chan) {
        Some((c, p)) if c == -11 && p == d1_pid as i64 => {}
        _ => {
            println!("[usertests] t25: exit notify D1 anomala");
            return false;
        }
    }
    // Re-registrazione stesso prefix: deve servire il NUOVO driver.
    let (d2_chan, d2_pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_MNTDIE, 0) {
        Some(x) => x,
        None => {
            println!("[usertests] t25: spawn MNTDIE#2 FAILED");
            return false;
        }
    };
    if !helpers::recv_expect(d2_chan, helpers::T_READY) {
        println!("[usertests] t25: T_READY da MNTDIE#2 mancante");
        let _ = helpers::wait_exit(d2_chan);
        return false;
    }
    let Ok(fd2) = civis::open("/dev/tdie/null", 0) else {
        println!("[usertests] t25: open /dev/tdie/null via D2 FAILED (mount stale?)");
        return false;
    };
    // Igiene: chiudi e uccidi D2 (nessun mount orfano per i test/shell dopo).
    let _ = civis::close(fd1);
    let _ = civis::close(fd2);
    if civis::kill(d2_pid as i64, 0).is_err() {
        println!("[usertests] t25: kill D2 FAILED");
        return false;
    }
    match helpers::wait_exit(d2_chan) {
        Some((0, _)) => {}
        _ => {
            println!("[usertests] t25: exit notify D2 anomala");
            return false;
        }
    }
    // Smoke ramfs: la purge non ha corrotto lo stato vivo.
    let Ok(fd) = civis::open("hello.txt", 0) else {
        println!("[usertests] t25: smoke hello.txt FAILED");
        return false;
    };
    let mut buf = [0u8; 64];
    let n = civis::read_fs(fd, &mut buf, 64).unwrap_or(0);
    let _ = civis::close(fd);
    n >= helpers::HELLO.len() && buf[..helpers::HELLO.len()] == *helpers::HELLO
}

/// t26 — purge rings/ftable alla morte di client (Fase 14). N helper OPENDIE
/// aprono /dev/null + /dev/zero + hello.txt e muoiono SENZA close: cardo deve
/// purgare rings/ftable (con DEV_CLOSE inoltrato ai driver) senza corrompere
/// lo stato vivo. Poi smoke FS completo (null/zero/hello/write/mkdir/readdir).
pub fn t_client_death_purge() -> bool {
    helpers::drain_stray();
    const N: usize = 10;
    for i in 0..N {
        let (chan, pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_OPENDIE, 0) {
            Some(x) => x,
            None => {
                println!("[usertests] t26: spawn OPENDIE#{} FAILED", i);
                return false;
            }
        };
        match helpers::wait_exit(chan) {
            Some((0, p)) if p == pid as i64 => {}
            other => {
                println!("[usertests] t26: exit OPENDIE#{} anomala: {:?}", i, other);
                return false;
            }
        }
    }
    // Smoke completo: il server e' sano dopo N purge.
    let Ok(fd) = civis::open("/dev/null", 0) else {
        println!("[usertests] t26: smoke open /dev/null FAILED");
        return false;
    };
    let data = [0x5Au8; 16];
    if civis::write_fs(fd, &data, 16) != Ok(16) {
        println!("[usertests] t26: smoke write /dev/null FAILED");
        return false;
    }
    let mut b = [0u8; 16];
    if civis::read_fs(fd, &mut b, 16) != Ok(0) {
        println!("[usertests] t26: smoke read /dev/null FAILED");
        return false;
    }
    let _ = civis::close(fd);
    let Ok(fdz) = civis::open("/dev/zero", 0) else {
        println!("[usertests] t26: smoke open /dev/zero FAILED");
        return false;
    };
    let mut z = [0xFFu8; 16];
    if civis::read_fs(fdz, &mut z, 16) != Ok(16) || z.iter().any(|&x| x != 0) {
        println!("[usertests] t26: smoke read /dev/zero FAILED");
        return false;
    }
    let _ = civis::close(fdz);
    let Ok(fdh) = civis::open("hello.txt", 0) else {
        println!("[usertests] t26: smoke open hello.txt FAILED");
        return false;
    };
    let mut hb = [0u8; 64];
    let n = civis::read_fs(fdh, &mut hb, 64).unwrap_or(0);
    let _ = civis::close(fdh);
    if n >= helpers::HELLO.len() && hb[..helpers::HELLO.len()] != *helpers::HELLO {
        println!("[usertests] t26: smoke content hello.txt FAILED");
        return false;
    }
    if n < helpers::HELLO.len() {
        println!("[usertests] t26: smoke short read hello.txt");
        return false;
    }
    let Ok(fw) = civis::open("ut26.bin", civis::OPEN_CREATE) else {
        println!("[usertests] t26: smoke open ut26.bin FAILED");
        return false;
    };
    let wb = [0xA5u8; 64];
    if civis::write_fs(fw, &wb, 64) != Ok(64) {
        println!("[usertests] t26: smoke write ut26.bin FAILED");
        return false;
    }
    let _ = civis::close(fw);
    let Ok(fr) = civis::open("ut26.bin", 0) else {
        println!("[usertests] t26: smoke reopen ut26.bin FAILED");
        return false;
    };
    let mut rb = [0u8; 64];
    let nr = civis::read_fs(fr, &mut rb, 64);
    let _ = civis::close(fr);
    if nr != Ok(64) || rb.iter().any(|&x| x != 0xA5) {
        println!("[usertests] t26: smoke verify ut26.bin FAILED");
        return false;
    }
    if civis::mkdir("utdir26").is_err() {
        println!("[usertests] t26: smoke mkdir FAILED");
        return false;
    }
    if !helpers::dir_contains("/", "utdir26") {
        println!("[usertests] t26: smoke readdir FAILED");
        return false;
    }
    true
}

/// t27 — init-restart di vela (Fase 14). Bounce via init (`init_bounce`:
/// init e' parent e riavvia per la via normale) e attesa: prima sparizione dallo slot,
/// poi ricomparsa, poi /dev/null di nuovo operativo. Bound: la sparizione e'
/// solo registry (1000 tick larghi); la ricomparsa include il RELOAD DA DISCO
/// del binario (Fase 21: ~8 read FS × ~15 round-trip DISK l'uno, ognuno dei
/// quali puo' attendere un quanto sotto carico — misurato ~730 tick con
/// usertests che polla) → bound 2000, o PASS o FAIL rumoroso, mai hang.
/// cardo non viene mai toccato (il canale FS del test resta vivo).
/// NOTA: non confronta pid vecchio/nuovo (il riuso PID puo' ridare lo stesso
/// numero); osserva sparizione → ricomparsa.
pub fn t_vela_restart() -> bool {
    helpers::drain_stray();
    let Ok(fd) = civis::open("/dev/null", 0) else {
        println!("[usertests] t27: baseline open /dev/null FAILED");
        return false;
    };
    let _ = civis::close(fd);
    // Fase 35 (hardening): i servizi supervisionati si uccidono tramite init
    // (bounce: init e' parent e riavvia per la via normale). Il kill diretto
    // e' parent-scoped e qui fallirebbe (vela e' figlio di init, non nostro).
    let p1 = match civis::init_bounce(civis::Service::Vela) {
        Ok(p) => p,
        Err(_) => {
            println!("[usertests] t27: bounce vela FAILED");
            return false;
        }
    };
    // Fase A: attendi sparizione dallo slot (morte osservata dal registry).
    // Poll throttled (Livello 1, buon vicinato): vedi `civis::poll_wait`.
    if !civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
        civis::service_pid(civis::Service::Vela).is_err()
    }) {
        println!("[usertests] t27: vela mai sparito (timeout)");
        return false;
    }
    // Fase B: attendi ricomparsa (init ha riavviato + registrato).
    // Bound 2000 (vedi sopra: include il reload da disco sotto carico).
    let p2 = match civis::poll_value(2000, civis::POLL_PERIOD_TICKS, || {
        civis::service_pid(civis::Service::Vela).ok()
    }) {
        Some(p) => p,
        None => {
            println!("[usertests] t27: vela mai riapparso (timeout)");
            return false;
        }
    };
    println!("[usertests] t27: vela riavviato (pid {} -> {})", p1, p2);
    // Fase C: operativita' — open finche' riesce (bound come sopra: il driver
    // puo' aver registrato lo slot ma non ancora i mount).
    // Throttled via `civis::open_wait` (igiene Livello 1, buon vicinato).
    // NOTA (esperimento B): t27 PASSA anche in busy-loop non throttled —
    // lo storm del test NON e' causale del vecchio FAIL (N=1, confound).
    let Ok(fd2) = civis::open_wait("/dev/null", 0, 2000, civis::POLL_PERIOD_TICKS) else {
        println!("[usertests] t27: /dev/null mai tornato (timeout)");
        return false;
    };
    let data = [0x5Au8; 16];
    let ok = civis::write_fs(fd2, &data, 16) == Ok(16);
    let mut b = [0u8; 16];
    let okr = civis::read_fs(fd2, &mut b, 16) == Ok(0);
    let _ = civis::close(fd2);
    if !ok || !okr {
        println!("[usertests] t27: write/read post-restart FAILED");
        return false;
    }
    // Smoke ramfs: cardo mai toccato dal restart.
    let Ok(fdh) = civis::open("hello.txt", 0) else {
        println!("[usertests] t27: smoke hello.txt FAILED");
        return false;
    };
    let mut hb = [0u8; 64];
    let n = civis::read_fs(fdh, &mut hb, 64).unwrap_or(0);
    let _ = civis::close(fdh);
    n >= helpers::HELLO.len() && hb[..helpers::HELLO.len()] == *helpers::HELLO
}

/// t28 — restart di cardo end-to-end (Fase 14). Uccide cardo (pid via
/// `service_pid`) e attende che init lo riavvii. Poi verifica: fixture fresh
/// funzionanti (mkdir/write/read su /tmp scratch ramfs), hello.txt persistente
/// su ArcaFS (seedato, non toccato da cardo restart), la root leggibile
/// (persistente su disco: contrasto), /dev/null operativo (driver
/// re-registrati via ensure_mounted). Bound generosi, mai hang.
pub fn t_cardo_restart() -> bool {
    helpers::drain_stray();
    // Baseline: hello.txt da ArcaFS (seedato) + /dev/null.
    let Ok(fdh) = civis::open("/hello.txt", 0) else {
        println!("[usertests] t28: baseline /hello.txt FAILED");
        return false;
    };
    let _ = civis::close(fdh);
    let Ok(fdn) = civis::open("/dev/null", 0) else {
        println!("[usertests] t28: baseline /dev/null FAILED");
        return false;
    };
    let _ = civis::close(fdn);
    // Probe su /tmp scratch ramfs (wipe check dopo il restart).
    let Ok(fp) = civis::open("/tmp/td28probe", 0x200) else {
        println!("[usertests] t28: create probe FAILED");
        return false;
    };
    let pwb = [0xBEu8; 32];
    if civis::write_fs(fp, &pwb, 32) != Ok(32) {
        println!("[usertests] t28: write probe FAILED");
        return false;
    }
    let _ = civis::close(fp);
    // Bounce via init (Fase 35: cardo e' figlio di init, kill diretto qui
    // fallirebbe col kill parent-scoped).
    let p1 = match civis::init_bounce(civis::Service::Cardo) {
        Ok(p) => p,
        Err(_) => {
            println!("[usertests] t28: bounce cardo FAILED");
            return false;
        }
    };
    // Kill + sparizione + ricomparsa (come t27, poll throttled Livello 1).
    if !civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
        civis::service_pid(civis::Service::Cardo).is_err()
    }) {
        println!("[usertests] t28: cardo mai sparito (timeout)");
        return false;
    }
    let p2 = match civis::poll_value(1000, civis::POLL_PERIOD_TICKS, || {
        civis::service_pid(civis::Service::Cardo).ok()
    }) {
        Some(p) => p,
        None => {
            println!("[usertests] t28: cardo mai riapparso (timeout)");
            return false;
        }
    };
    println!("[usertests] t28: cardo riavviato (pid {} -> {})", p1, p2);
    // Fixture fresh su /tmp scratch ramfs (re-handshake trasparente via NOHANDSHAKE se serve).
    // Throttled (lezione t27/t28): martellare cardo in busy-loop affama la
    // re-registrazione dei driver (vela/console ricreano il mount proprio
    // su questo cardo).
    if !civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
        civis::mkdir("/tmp/td28").is_ok()
    }) {
        println!("[usertests] t28: mkdir /tmp/td28 post-restart mai riuscito (timeout)");
        return false;
    }
    let Ok(fw) = civis::open("/tmp/td28/f", 0x200) else {
        println!("[usertests] t28: create /tmp/td28/f FAILED");
        return false;
    };
    let wb = [0xD8u8; 32];
    if civis::write_fs(fw, &wb, 32) != Ok(32) {
        println!("[usertests] t28: write /tmp/td28/f FAILED");
        return false;
    }
    let _ = civis::close(fw);
    let Ok(fr) = civis::open("/tmp/td28/f", 0) else {
        println!("[usertests] t28: reopen /tmp/td28/f FAILED");
        return false;
    };
    let mut rb = [0u8; 32];
    let nr = civis::read_fs(fr, &mut rb, 32);
    let _ = civis::close(fr);
    // Dettaglio diagnostico (solo su FAIL): nr e primo byte diverso.
    if nr != Ok(32) {
        println!("[usertests] t28: verify nr={:?} (atteso Ok(32))", nr);
        return false;
    }
    if let Some((i, &x)) = rb.iter().enumerate().find(|&(_, &x)| x != 0xD8) {
        println!("[usertests] t28: verify mismatch i={} val={:#x}", i, x);
        return false;
    }
    // hello.txt persistente su ArcaFS (non toccata da cardo restart).
    let Ok(fh) = civis::open("/hello.txt", 0) else {
        println!("[usertests] t28: /hello.txt persistente mancante");
        return false;
    };
    let mut hb = [0u8; 64];
    let n = civis::read_fs(fh, &mut hb, 64).unwrap_or(0);
    let _ = civis::close(fh);
    if n < helpers::HELLO.len() || hb[..helpers::HELLO.len()] != *helpers::HELLO {
        println!("[usertests] t28: hello.txt ricreato corrotto");
        return false;
    }
    // Wipe: il probe pre-restart non esiste piu' (ramfs volatile). NOTA: via
    // readdir, MAI via open (su ramfs open crea il file se manca!).
    if helpers::dir_contains("/", "td28probe") {
        println!("[usertests] t28: probe sopravvissuto al restart?!");
        return false;
    }
    // Persistente: la root ArcaFS leggibile (rimontata dal disco).
    let Ok(ff) = civis::open(helpers::ARCA_HELLO_PATH, 0) else {
        println!("[usertests] t28: /hello.txt illeggibile");
        return false;
    };
    let mut fb = [0u8; 32];
    let nf = civis::read_fs(ff, &mut fb, 32).unwrap_or(0);
    let _ = civis::close(ff);
    if nf == 0 {
        println!("[usertests] t28: /hello.txt vuoto");
        return false;
    }
    // Driver re-registrati: /dev/null operativo. Retry con bound (throttled):
    // vela ricrea il mount in modo asincrono su EXIT_NOTIFY e puo' laggare
    // dietro il fresh cardo; un singolo tentativo darebbe falsi FAIL.
    let Ok(fd2) = civis::open_wait("/dev/null", 0, 1000, civis::POLL_PERIOD_TICKS) else {
        println!("[usertests] t28: /dev/null post-restart FAILED");
        return false;
    };
    let data = [0x5Au8; 16];
    let ok = civis::write_fs(fd2, &data, 16) == Ok(16);
    let mut b = [0u8; 16];
    let okr = civis::read_fs(fd2, &mut b, 16) == Ok(0);
    let _ = civis::close(fd2);
    if !ok || !okr {
        println!("[usertests] t28: write/read /dev/null FAILED");
        return false;
    }
    true
}


/// t49 — fork COW (Fase 34, ADR-0024): l'helper duplica se stesso; padre e
/// figlio scrivono un globale COW e verificano l'isolamento (nessuno vede la
/// scrittura dell'altro); il figlio riporta sul canale di nascita ed esce 0.
/// L'orchestratore osserva solo il T_DONE dell'helper (dettagli dentro).
pub fn t_fork() -> bool {
    helpers::drain_stray();
    let (chan, _pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_FORKDEMO, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t49: spawn helper FAILED");
            return false;
        }
    };
    let (ok, _) = helpers::recv_done(&[chan]);
    if !ok {
        println!("[usertests] t49: helper fork FAIL");
    }
    ok
}

/// t52 — exec in-place (Fase 37.0 nucleo + 37.1 argv): un helper testcli
/// (EXECDEMO) diventa testspin via `exec_image` su T_GO. Verifiche nucleo:
/// stesso PID (T_ACK pre/post con pid), hash rimisurato (diverso da prima,
/// uguale a uno spin fresco di riferimento), nuova immagine operativa (T_DONE
/// dopo il budget). Verifiche argv: secondo helper con w1=1, exec con argv
/// ["ARGPROBE","hello","world"] — il fresh `_start` riporta T_DONE(argc, fnv)
/// da solo; il parent confronta. Reap via `poll_gone` (NON `wait_exit`: i
/// `recv_done` consumano e scartano le EXIT_NOTIFY altrui — aspettarle dopo
/// sarebbe hang garantito). Entrambi escono da soli.
pub fn t_exec_core() -> bool {
    helpers::drain_stray();
    let (h_chan, h_pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_EXECDEMO, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t52: spawn EXECDEMO FAILED");
            return false;
        }
    };
    let h_before = match civis::peer_info(h_chan) {
        Ok(h) => h,
        Err(_) => {
            println!("[usertests] t52: peer_info pre-exec FAILED");
            let _ = civis::kill(h_pid as i64, 0);
            let _ = helpers::wait_exit(h_chan);
            return false;
        }
    };
    // Via (sync) + T_CFG da spin (budget=100, ~1s di vita: la query post-exec
    // lo trova vivo; il T_DONE arriva dopo). La send si blocca finche' lo
    // spin risponde: nessun polling, nessuna race sul momento dell'exec.
    if civis::send(h_chan, helpers::T_GO, 0, 0).is_err() {
        println!("[usertests] t52: T_GO FAILED");
        let _ = civis::kill(h_pid as i64, 0);
        let _ = helpers::wait_exit(h_chan);
        return false;
    }
    let ack = match civis::send(h_chan, helpers::T_CFG, 100, 0) {
        Ok(a) => a,
        Err(_) => {
            println!("[usertests] t52: T_CFG post-exec FAILED (exec non passato?)");
            let _ = civis::kill(h_pid as i64, 0);
            let _ = helpers::wait_exit(h_chan);
            return false;
        }
    };
    if ack.w0 != h_pid {
        println!("[usertests] t52: pid cambiato ({} -> {})", h_pid, ack.w0);
        let _ = civis::kill(h_pid as i64, 0);
        let _ = helpers::wait_exit(h_chan);
        return false;
    }
    let h_after = match civis::peer_info(h_chan) {
        Ok(h) => h,
        Err(_) => {
            println!("[usertests] t52: peer_info post-exec FAILED");
            let _ = helpers::wait_exit(h_chan);
            return false;
        }
    };
    if h_after == h_before {
        println!("[usertests] t52: hash NON rimisurato ({:#x})", h_after);
        let _ = helpers::wait_exit(h_chan);
        return false;
    }
    // Spin di riferimento (stesso binario, budget corto): hash atteso.
    // Come sopra, ack.w0 = pid (spin risponde T_ACK con getpid).
    let (r_chan, r_pid) = match helpers::spawn_cfg(
        "/test/testspin.bin", "utspin", 16, 100, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t52: spawn spin-rif FAILED");
            let _ = helpers::poll_gone(h_pid as u64, 200);
            return false;
        }
    };
    let h_ref = civis::peer_info(r_chan).unwrap_or(0);
    let (ok_h, _) = helpers::recv_done(&[h_chan]);
    let (ok_r, _) = helpers::recv_done(&[r_chan]);
    // Reap senza EXIT_NOTIFY (consumate dai recv_done sopra): poll throttled
    // sulla sparizione dallo ps. Nessun altro spawn nel mezzo (sequenza
    // deterministica) → niente rischio di scambiare un riuso del PID.
    let gone_h = helpers::poll_gone(h_pid as u64, 200);
    let gone_r = helpers::poll_gone(r_pid as u64, 200);
    if h_ref != h_after {
        println!("[usertests] t52: hash post-exec {:#x} != riferimento {:#x}", h_after, h_ref);
        return false;
    }
    if !ok_h || !ok_r {
        println!("[usertests] t52: T_DONE mancante (h={}, r={})", ok_h, ok_r);
        return false;
    }
    if !gone_h || !gone_r {
        println!("[usertests] t52: reap mancato (h={}, r={})", gone_h, gone_r);
        return false;
    }
    // Leg argv+env (37.1, env in 43a): secondo helper, exec con argv
    // ["ARGPROBE","hello","world"] + env `T52E=envok`. Il fresh _start vede
    // argc=3 e riporta T_DONE(argc, fnv) da solo (nessun T_CFG: la nuova
    // immagine non parla il protocollo CFG); l'env e' verificato dalla sonda
    // (assente/diverso = T_DONE(0,0), che fallisce il check sotto).
    // Stesso binario: hash coerente (uguale al pre-exec); la PROVA dell'exec
    // e' il report argv (la vecchia immagine non poteva produrlo).
    let (a_chan, a_pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_EXECDEMO, 1,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t52: spawn argv-helper FAILED");
            return false;
        }
    };
    let a_pre = match civis::peer_info(a_chan) {
        Ok(h) => h,
        Err(_) => {
            println!("[usertests] t52: peer_info argv-helper FAILED");
            let _ = civis::kill(a_pid as i64, 0);
            let _ = helpers::wait_exit(a_chan);
            return false;
        }
    };
    if civis::send(a_chan, helpers::T_GO, 0, 0).is_err() {
        println!("[usertests] t52: T_GO argv FAILED");
        let _ = civis::kill(a_pid as i64, 0);
        let _ = helpers::wait_exit(a_chan);
        return false;
    }
    // Stesso binario prima/dopo: il valore e' deterministico comunque
    // (l'exec puo' essere gia' avvenuto o no, l'hash non cambia).
    match civis::peer_info(a_chan) {
        Ok(h) if h == a_pre => {}
        Ok(h) => {
            println!("[usertests] t52: hash incoerente ({:#x} -> {:#x})", a_pre, h);
            let _ = civis::kill(a_pid as i64, 0);
            let _ = helpers::wait_exit(a_chan);
            return false;
        }
        Err(_) => {
            println!("[usertests] t52: peer_info post-GO FAILED");
            let _ = civis::kill(a_pid as i64, 0);
            let _ = helpers::wait_exit(a_chan);
            return false;
        }
    }
    let expected_fnv = civis::image_hash(b"hello\0world\0");
    let (argc_rep, fnv_rep) = loop {
        match civis::recv() {
            Ok(m) if m.tag == helpers::T_DONE && m.channel == a_chan => {
                let _ = civis::reply(helpers::T_ACK, 0, 0);
                break (m.w0, m.w1);
            }
            Ok(m) if civis::is_exit_notify(&m) => {}
            Ok(_) => {
                let _ = civis::reply(helpers::T_ACK, 0, 0);
            }
            Err(_) => {
                println!("[usertests] t52: recv report argv FAILED");
                let _ = civis::kill(a_pid as i64, 0);
                let _ = helpers::wait_exit(a_chan);
                return false;
            }
        }
    };
    // Reap senza EXIT_NOTIFY (consumata sopra come stray): poll throttled.
    let gone_a = helpers::poll_gone(a_pid as u64, 200);
    if argc_rep != 3 || fnv_rep != expected_fnv {
        println!(
            "[usertests] t52: report argv errato (argc={}, fnv={:#x}, attesi 3 e {:#x})",
            argc_rep, fnv_rep, expected_fnv
        );
        return false;
    }
    if !gone_a {
        println!("[usertests] t52: reap argv-helper mancato");
        return false;
    }
    true
}
