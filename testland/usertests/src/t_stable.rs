use super::*;

/// t40 — detach + reparent a init (Fase 22). usertests spawna un MID (NEST)
/// che spawna due foglie parcheggiate (detached in ORPHAN, normale in KILLME)
/// e poi esce: la sua morte fa scattare cascata sulla normale e reparent a
/// init della detached. Osservazione SOLO via `ps` (usertests non e' peer
/// delle foglie, quindi niente EXIT_NOTIFY diretta): normale sparita,
/// detached viva con parent == init (pid 1); poi la detached esce DA SOLA
/// (~100 tick dopo la notify di morte del MID, igiene senza kill: il kill
/// diretto e' parent-scoped dal Fase 35 e nessuno fuori puo' pulirla).
/// Bound 500 tick per fase, mai hang.
pub fn t_detach() -> bool {
    helpers::drain_stray();
    let (mid_chan, _mid_pid) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_NEST, 0) {
        Some(x) => x,
        None => {
            println!("[usertests] t40: spawn NEST FAILED");
            return false;
        }
    };
    let (det, norm) = match helpers::recv_ready(mid_chan) {
        Some((d, n)) if d != 0 && n != 0 => (d, n),
        _ => {
            println!("[usertests] t40: T_READY dal MID mancante");
            return false;
        }
    };
    // Il MID esce (exit 0): la cascata scatta qui.
    match helpers::wait_exit(mid_chan) {
        Some((0, _)) => {}
        other => {
            println!("[usertests] t40: exit del MID anomala: {:?}", other);
            return false;
        }
    }
    // Foglia normale: cascata → deve sparire da ps.
    if !helpers::poll_gone(norm, 500) {
        println!("[usertests] t40: foglia normale ancora viva (pid={})", norm);
        return false;
    }
    // Detached: viva e ri-parentata a init (pid 1).
    match helpers::poll_parent(det, 500) {
        Some(Some(1)) => {}
        other => {
            println!("[usertests] t40: detached parent errato: {:?}", other);
            return false;
        }
    }
    // Cleanup senza kill (Fase 35): la detached esce da sola ~100 tick dopo
    // la notify (ORPHAN); qui si attende solo la sparizione via ps.
    if !helpers::poll_gone(det, 500) {
        println!("[usertests] t40: detached ancora viva (self-exit mancato)");
        return false;
    }
    true
}

/// t36 — identità stabile del volume **ArcaFS** + discovery (ADR-0058).
/// Il FAT esce dal boot: l'identità non è più quella del BPB (seriale +
/// label), ma quella del superblock ArcaFS (uuid a 64 bit). Resta la
/// dimensione UUID: mount per UUID, open raw del by-path, listing di
/// /dev/disk/by-uuid. **La dimensione LABEL è rinviata** (è roba da USB:
/// ADR-0058) e con lei il by-label.
pub fn t_stable_id() -> bool {
    helpers::drain_stray();
    if civis::mkdir("/u2").is_err() {
        println!("[usertests] t36: mkdir /u2 FAILED");
        return false;
    }
    // 1. Mount per UUID (16 hex, il formato del superblock ArcaFS).
    if civis::mount("UUID=4152434100000001", "/u2").is_err() {
        println!("[usertests] t36: mount UUID=4152434100000001 FAILED");
        return false;
    }
    let Ok(fd) = civis::open("/u2/hello.txt", 0) else {
        println!("[usertests] t36: open hello.txt via UUID FAILED");
        let _ = civis::umount("/u2");
        return false;
    };
    let mut mb = [0u8; 32];
    let n = helpers::t33_read_all(fd, &mut mb);
    let _ = civis::close(fd);
    if n != helpers::ARCA_HELLO.len() || mb[..helpers::ARCA_HELLO.len()] != *helpers::ARCA_HELLO {
        println!("[usertests] t36: hello.txt via UUID corrotto (volume sbagliato?)");
        let _ = civis::umount("/u2");
        return false;
    }
    if civis::umount("/u2").is_err() {
        println!("[usertests] t36: umount /u2 FAILED");
        return false;
    }
    // 2. Open raw del by-path: settore 0 col magic ACFS (l'identità misurata
    //    del volume, non piu' la firma 0x55AA del BPB FAT).
    let Ok(fd) = civis::open("/dev/disk/by-uuid/4152434100000001", 0) else {
        println!("[usertests] t36: open raw by-uuid FAILED");
        return false;
    };
    let mut sec = [0u8; 512];
    let r = civis::read_fs(fd, &mut sec, 512);
    let _ = civis::close(fd);
    if r != Ok(512) || &sec[..4] != helpers::ACFS_MAGIC {
        println!("[usertests] t36: settore 0 raw by-uuid non ArcaFS");
        return false;
    }
    // 3. Listing sintetizzato.
    let mut eb = [0u8; 512];
    if civis::readdir("/dev", &mut eb, 512).is_err()
        || !helpers::readdir_contains(&eb, b"disk")
        || !helpers::readdir_contains(&eb, b"sda")
    {
        println!("[usertests] t36: readdir /dev senza disk/sda");
        return false;
    }
    let mut eb = [0u8; 256];
    if civis::readdir("/dev/disk/by-uuid", &mut eb, 256).is_err()
        || !helpers::readdir_contains(&eb, b"4152434100000001")
    {
        println!("[usertests] t36: readdir by-uuid senza 4152434100000001");
        return false;
    }
    true
}

/// t32 — disk driver in userspace (Fase 16).
/// (A) Baseline: /dev/sda leggibile raw con firma boot. (B) Bounce via init
/// (`init_bounce`, Fase 35) e attesa init-restart come t27/t28: sparizione dallo slot, ricomparsa, poi /dev/sda di nuovo
/// operativo + smoke della root ArcaFS (riconnessione lazy di cardo al driver
/// rinato, senza rimontare: il mount sopravvive). Bound generosi (1000 tick
/// ~ 10 s contro restart atteso ~50), mai hang; poll throttled Livello 1.
pub fn t_disk() -> bool {
    helpers::drain_stray();
    if !helpers::disk_sector0_ok() {
        println!("[usertests] t32: baseline /dev/sda FAILED");
        return false;
    }
    // Bounce via init (Fase 35: block e' figlio di init, kill diretto qui
    // fallirebbe col kill parent-scoped).
    let p1 = match civis::init_bounce(civis::Service::Block) {
        Ok(p) => p,
        Err(_) => {
            println!("[usertests] t32: bounce block FAILED");
            return false;
        }
    };
    // Fase A: sparizione dallo slot (morte osservata dal registry).
    if !civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
        civis::service_pid(civis::Service::Block).is_err()
    }) {
        println!("[usertests] t32: block mai sparito (timeout)");
        return false;
    }
    // Fase B: ricomparsa (init ha riavviato + registrato).
    let p2 = match civis::poll_value(1000, civis::POLL_PERIOD_TICKS, || {
        civis::service_pid(civis::Service::Block).ok()
    }) {
        Some(p) => p,
        None => {
            println!("[usertests] t32: block mai riapparso (timeout)");
            return false;
        }
    };
    println!("[usertests] t32: block riavviato (pid {} -> {})", p1, p2);
    // Fase C: operativita' raw dopo il restart.
    if !civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, helpers::disk_sector0_ok) {
        println!("[usertests] t32: /dev/sda mai tornato (timeout)");
        return false;
    }
    // Smoke della root via riconnessione (il driver e' nuovo, il volume e'
    // quello di boot).
    let Ok(fdf) = civis::open_wait(helpers::ARCA_HELLO_PATH, 0, 1000, civis::POLL_PERIOD_TICKS) else {
        println!("[usertests] t32: open /hello.txt post-restart FAILED");
        return false;
    };
    let mut fb = [0u8; 32];
    let n = civis::read_fs(fdf, &mut fb, 32).unwrap_or(0);
    let _ = civis::close(fdf);
    if n != helpers::ARCA_HELLO.len() || fb[..helpers::ARCA_HELLO.len()] != *helpers::ARCA_HELLO {
        println!("[usertests] t32: /hello.txt post-restart corrotto");
        return false;
    }
    // Fase 51 (P2 vocabolario disco): topologia via protocollo (relay R_
    // verso DISK_*), non dal log. QEMU ne ha 2 (fat+fat2, come t36).
    let list = match civis::disk_list() {
        Ok(v) => v,
        Err(_) => {
            println!("[usertests] t32: disk_list FAILED");
            return false;
        }
    };
    if list.len() < 2 {
        println!("[usertests] t32: disk_list count={} (< 2)", list.len());
        return false;
    }
    for (i, (sectors, flags)) in list.iter().enumerate() {
        if *sectors == 0 {
            println!("[usertests] t32: disco {} senza settori", i);
            return false;
        }
        let d = match civis::disk_info(i as u32) {
            Ok(d) => d,
            Err(_) => {
                println!("[usertests] t32: disk_info({}) FAILED", i);
                return false;
            }
        };
        // Coerenza LIST vs INFO + fatti strutturali (mai valori QEMU
        // hardcodati: il gate gira su qualunque ATA reale).
        if d.sectors != *sectors || d.flags != *flags {
            println!("[usertests] t32: LIST/INFO incoerenti su disco {}", i);
            return false;
        }
        if d.model_len == 0 || d.sec_logical() == 0 || d.sec_physical() < d.sec_logical() {
            println!("[usertests] t32: descrittore assurdo su disco {}", i);
            return false;
        }
        if let Some(m) = d.udma() {
            if m > 2 {
                println!("[usertests] t32: UDMA{} oltre il cap PIIX3", m);
                return false;
            }
        }
        // Dump topologia (dati S1/S2 per ArcaFS, §14: mai a stima).
        println!(
            "[usertests] t32: sd{} settori={} lba48={} trim={} udma={} rot={} sec={}/{}B modello='{}' seriale='{}'",
            (b'a' + i as u8) as char,
            d.sectors,
            d.lba48(),
            d.trim(),
            d.udma().map(|m| m as i64).unwrap_or(-1),
            d.rotation(),
            d.sec_logical(),
            d.sec_physical(),
            d.model_str(),
            d.serial_str(),
        );
    }
    // Indice oltre i dischi: rifiuto, mai frame parziale.
    if civis::disk_info(99).is_ok() {
        println!("[usertests] t32: disk_info(99) accettato?!");
        return false;
    }
    // Fase 52 (P3 durabilita'): ciclo modi R_SYNC (ritorna il precedente,
    // umask-like, deterministico da qualunque stato: None -> Group ->
    // PerWrite -> None) + modo ignoto rifiutato senza stato.
    match civis::disk_sync(civis::SYNC_GROUP) {
        Ok(_) => {}
        Err(_) => {
            println!("[usertests] t32: disk_sync(GROUP) FAILED");
            return false;
        }
    }
    match civis::disk_sync(civis::SYNC_PERWRITE) {
        Ok(prev) if prev == civis::SYNC_GROUP as u64 => {}
        Ok(prev) => {
            println!("[usertests] t32: prev inatteso ({}, atteso GROUP)", prev);
            return false;
        }
        Err(_) => {
            println!("[usertests] t32: disk_sync(PERWRITE) FAILED");
            return false;
        }
    }
    match civis::disk_sync(civis::SYNC_NONE) {
        Ok(prev) if prev == civis::SYNC_PERWRITE as u64 => {}
        Ok(prev) => {
            println!("[usertests] t32: prev inatteso ({}, atteso PERWRITE)", prev);
            return false;
        }
        Err(_) => {
            println!("[usertests] t32: disk_sync(NONE) FAILED");
            return false;
        }
    }
    if civis::disk_sync(9).is_ok() {
        println!("[usertests] t32: disk_sync(9) accettato?!");
        return false;
    }
    // Barriera Group riuscita sopra (primo disk_sync): i FLUSH sono
    // atterrati senza Err. Sensore spazio: la root ArcaFS (Fase A: bsize
    // 65536, illimitata MAX come la ramfs prima).
    let mut vfs = civis::StatVfs { bsize: 0, blocks: 0, bfree: 0, bavail: 0 };
    if civis::statvfs("/", &mut vfs).is_err() || vfs.bsize != 65536 || vfs.bfree != u64::MAX {
        println!("[usertests] t32: statvfs / assurdo ({}/{}/{})", vfs.bsize, vfs.blocks, vfs.bfree);
        return false;
    }
    println!("[usertests] t32: statvfs / bsize={} blocks={} bfree={}", vfs.bsize, vfs.blocks, vfs.bfree);
    // Device/sintetici: nessuno spazio da contabilizzare.
    if civis::statvfs("/dev/null", &mut vfs).is_ok() {
        println!("[usertests] t32: statvfs /dev/null accettato?!");
        return false;
    }
    true
}

/// t50 — hardening (Fase 35, ADR-0026): i cancelli kernel/FS respingono i
/// tentativi ostili di un processo locale. (A) un helper prova a killare un
/// fratello (non suo figlio) e a registrare un servizio di sistema (`Init`):
/// entrambi rifiutati. (B) usertests prova a mappare RAM del kernel con
/// `map_physical`: rifiutato. (C) usertests prova a killare un servizio che
/// non e' suo figlio (vela): rifiutato (il servizio resta vivo).
pub fn t_hardening() -> bool {
    helpers::drain_stray();
    // Vittima: un KILLME parcheggiato, figlio di usertests (fratello
    // dell'helper HARDEN, quindi NON suo figlio).
    let (b_chan, b_pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_KILLME, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t50: spawn vittima FAILED");
            return false;
        }
    };
    let (h_chan, _) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_HARDEN, b_pid,
    ) {
        Some(x) => x,
        None => {
            let _ = civis::kill(b_pid as i64, 0);
            let _ = helpers::wait_exit(b_chan);
            println!("[usertests] t50: spawn harden FAILED");
            return false;
        }
    };
    let (ok, detail) = helpers::recv_done(&[h_chan]);
    if !ok {
        println!("[usertests] t50: helper harden FAIL (detail={})", detail);
        let _ = civis::kill(b_pid as i64, 0);
        let _ = helpers::wait_exit(b_chan);
        return false;
    }
    // La vittima deve essere ancora viva (il kill ostile non e' passato).
    if civis::ps_info(b_pid as u32).is_none() {
        println!("[usertests] t50: vittima uccisa da kill non-figlio!");
        return false;
    }
    // (B) map_physical di RAM del kernel (0x100000) rifiutato.
    if civis::map_physical(0x10_0000, helpers::VA_A, 1).is_ok() {
        println!("[usertests] t50: map_physical RAM kernel NON rifiutato!");
        let _ = civis::kill(b_pid as i64, 0);
        let _ = helpers::wait_exit(b_chan);
        return false;
    }
    // (C) kill di un servizio non-figlio (vela, figlio di init) rifiutato.
    if let Ok(vela_pid) = civis::service_pid(civis::Service::Vela) {
        if civis::kill(vela_pid, 0).is_ok() {
            println!("[usertests] t50: kill di un servizio non-figlio NON rifiutato!");
            let _ = civis::kill(b_pid as i64, 0);
            let _ = helpers::wait_exit(b_chan);
            return false;
        }
    }
    // Cleanup: la vittima e' nostra figlia (kill consentito).
    let _ = civis::kill(b_pid as i64, 0);
    let _ = helpers::wait_exit(b_chan);
    true
}

/// t51 — identita' misurata (Fase 36, Strato 2 di ADR-0026).
/// (A) `peer_info` sui servizi caricati da disco (Console, Devfs) == manifest
/// generato: il kernel misura gli stessi byte che init ha verificato al boot.
/// (Su cardo embedded non c'e' pinning da manifest: `HASH_CARDO` non esiste
/// per costruzione — cardo incorpora il manifest e il suo hash sarebbe un
/// ciclo instabile, vedi gen-service-hashes.sh.)
/// (B) stabilita': due istanze dello stesso helper hanno lo stesso hash.
/// (C) same-image positivo: due istanze NON-figlie-di-init dello stesso
/// binario si passano un prefix (X2 rimpiazza X1 vivo) — con le sole regole
/// Fase 35 sarebbe rifiutato. Osservabile: kill X1 → il mount sopravvive
/// (driver X2), open ancora ok.
/// (D) squat negativo: Y (binario diverso) tenta il replace del prefix di X
/// vivo → rifiutato; alla morte di X il mount e' purgato (open fallisce).
/// Se il replace fosse passato, Y servirebbe e l'open riuscirebbe.
/// (E) `peer_info` a canale morto → Err.
pub fn t_identity() -> bool {
    helpers::drain_stray();
    // (A) hash dal kernel == manifest di build, per due servizi da disco.
    for (svc, expected, name) in [
        (civis::Service::Gpu, crate::HASH_GPU, "Gpu"),
        (civis::Service::Vela, crate::HASH_VELA, "Vela"),
    ] {
        let chan = match civis::service_lookup(svc) {
            Ok(c) => c as u64,
            Err(_) => {
                println!("[usertests] t51: lookup {} FAILED", name);
                return false;
            }
        };
        match civis::peer_info(chan) {
            Ok(h) if h == expected => {}
            Ok(h) => {
                println!("[usertests] t51: peer_info({})={:#x} != manifest {:#x}", name, h, expected);
                return false;
            }
            Err(_) => {
                println!("[usertests] t51: peer_info({}) FAILED", name);
                return false;
            }
        }
    }
    // (B) due istanze dello stesso helper: stesso hash, entrambe vive.
    let (k1_chan, k1_pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_KILLME, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t51: spawn K1 FAILED");
            return false;
        }
    };
    let (k2_chan, k2_pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_KILLME, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t51: spawn K2 FAILED");
            let _ = civis::kill(k1_pid as i64, 0);
            let _ = helpers::wait_exit(k1_chan);
            return false;
        }
    };
    let (h1, h2) = (civis::peer_info(k1_chan), civis::peer_info(k2_chan));
    // spawn_cfg riporta in ack.w0 il pid (testcli risponde T_ACK con getpid,
    // come usa t50): kill diretto, nostre figlie.
    if h1.is_err() || h1 != h2 {
        println!("[usertests] t51: hash instabili tra istanze");
        let _ = civis::kill(k1_pid as i64, 0);
        let _ = helpers::wait_exit(k1_chan);
        let _ = civis::kill(k2_pid as i64, 0);
        let _ = helpers::wait_exit(k2_chan);
        return false;
    }
    let _ = civis::kill(k1_pid as i64, 0);
    let _ = helpers::wait_exit(k1_chan);
    let _ = civis::kill(k2_pid as i64, 0);
    let _ = helpers::wait_exit(k2_chan);
    // (C) same-image: X1 registra /dev/t51, X2 (stesso binario, non init-child)
    // lo rimpiazza da vivo. Kill X1 → il mount deve sopravvivere (driver X2).
    let (x1_chan, _) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_REG51, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t51: spawn X1 FAILED");
            return false;
        }
    };
    if helpers::recv_ready(x1_chan).is_none() {
        println!("[usertests] t51: T_READY X1 mancante");
        return false;
    }
    let (x2_chan, _) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_REG51, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t51: spawn X2 FAILED");
            let _ = civis::kill(civis::peer_pid(x1_chan).unwrap_or(-1), 0);
            let _ = helpers::wait_exit(x1_chan);
            return false;
        }
    };
    if helpers::recv_ready(x2_chan).is_none() {
        println!("[usertests] t51: T_READY X2 mancante");
        return false;
    }
    let x1_pid = civis::peer_pid(x1_chan).unwrap_or(-1);
    let x2_pid = civis::peer_pid(x2_chan).unwrap_or(-1);
    let Ok(fd) = civis::open("/dev/t51/null", 0) else {
        println!("[usertests] t51: open pre-kill FAILED");
        let _ = civis::kill(x1_pid, 0);
        let _ = helpers::wait_exit(x1_chan);
        let _ = civis::kill(x2_pid, 0);
        let _ = helpers::wait_exit(x2_chan);
        return false;
    };
    let _ = civis::close(fd);
    let _ = civis::kill(x1_pid, 0);
    let _ = helpers::wait_exit(x1_chan);
    let Ok(fd) = civis::open("/dev/t51/null", 0) else {
        println!("[usertests] t51: open post-kill X1 FAILED (replace same-image non passato?)");
        let _ = civis::kill(x2_pid, 0);
        let _ = helpers::wait_exit(x2_chan);
        return false;
    };
    let _ = civis::close(fd);
    // (D) squat: X2 resta vivo e proprietario; Y (binario diverso) tenta il
    // replace → rifiutato. Kill X2 → mount purgato → open deve FALLIRE (se il
    // replace fosse passato, Y servirebbe e l'open riuscirebbe).
    let (y_chan, _) = match helpers::spawn_cfg(
        "/test/testspin.bin", "utspin", 16, helpers::SPIN_SQUAT_MAGIC, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t51: spawn Y FAILED");
            let _ = civis::kill(x2_pid, 0);
            let _ = helpers::wait_exit(x2_chan);
            return false;
        }
    };
    if helpers::recv_ready(y_chan).is_none() {
        println!("[usertests] t51: T_READY Y mancante");
        let _ = civis::kill(x2_pid, 0);
        let _ = helpers::wait_exit(x2_chan);
        return false;
    }
    let y_pid = civis::peer_pid(y_chan).unwrap_or(-1);
    let _ = civis::kill(x2_pid, 0);
    let _ = helpers::wait_exit(x2_chan);
    // (E) canale morto → Err (stesso canale di X2, peer reclamato).
    if civis::peer_info(x2_chan).is_ok() {
        println!("[usertests] t51: peer_info a canale morto NON rifiutato!");
        let _ = civis::kill(y_pid, 0);
        let _ = helpers::wait_exit(y_chan);
        return false;
    }
    let fd = civis::open("/dev/t51/null", 0);
    let _ = civis::kill(y_pid, 0);
    let _ = helpers::wait_exit(y_chan);
    if let Ok(fd) = fd {
        println!("[usertests] t51: open dopo purge RIUSCITO (squat passato?)");
        let _ = civis::close(fd);
        return false;
    }
    true
}

/// t58 — bucket oggetti nativi + content-hash BLAKE2s (Fase 55, N0; 56.2b su
/// blocchi; D1: driver in `vela`). Il test lega il motore in proprio (open +
/// USEDISK: load o init + seed server-side), cosi' sopravvive a qualunque
/// restart di cardo precedente (es. t28): senza volume skip adattivo, mai FAIL.
/// Prova: (1) l'oggetto nativo e' byte-identico al path ns; (2) il suo
/// blake2s e' il manifest BLAKE (stesso predicato di `verify_image` in
/// init); (3) un byte flippato cambia il digest (init lo rifiuterebbe);
/// (4) la chiave assente da' errore (mai dati inventati); (5) gpu vive in
/// `vela`, non piu' in `sys`.
pub fn t_sys_native() -> bool {
    // Il gemello su disco del bucket oggetti: la stessa immagine esposta
    // come path della root (seed `ns:bin/gpu.bin` = `sys`/`vela` class).
    let ns_file = match civis::load_file("/bin/gpu.bin") {
        Some(b) if !b.is_empty() => b,
        _ => {
            println!("[usertests] t58: /bin/gpu.bin illeggibile");
            return false;
        }
    };
    // Bind in proprio: primo volume ArcaFS che apre (gate: /dev/sdc1).
    // Con auto-bind all'avvio (56.2c) l'open prende il rifiuto re-open:
    // tollerato se USEDISK riesce (motore gia' legato). Nessun volume =
    // run senza drive: skip adattivo (mai FAIL per assenza).
    let mut opened = false;
    for dev in ["/dev/sdc1", "/dev/sdd1", "/dev/sdc", "/dev/sdd"] {
        if civis::arca_open(dev).is_ok() {
            opened = true;
            break;
        }
    }
    if civis::arca_use_disk(true).is_err() {
        if opened {
            println!("[usertests] t58: USEDISK fallito");
            return false;
        }
        println!("[usertests] t58: nessun volume ArcaFS (ARCA_IMG=0?): salto");
        return true;
    }
    let obj = match civis::obj_get(b"vela", b"bin/gpu.bin") {
        Ok(v) => v,
        Err(_) => {
            println!("[usertests] t58: obj vela/bin/gpu.bin assente");
            return false;
        }
    };
    if obj != ns_file {
        println!("[usertests] t58: vela != path ns ({} vs {} B)", obj.len(), ns_file.len());
        return false;
    }
    if blake2s::blake2s(&obj) != crate::BLAKE_GPU {
        println!("[usertests] t58: blake vela != manifest");
        return false;
    }
    // D1: i driver hanno traslocato in `vela` — in `sys` non resta nulla.
    if civis::obj_get(b"sys", b"bin/gpu.bin").is_ok() {
        println!("[usertests] t58: gpu ancora in sys?!");
        return false;
    }
    let mut bad = obj.clone();
    bad[0] ^= 0xFF;
    if blake2s::blake2s(&bad) == crate::BLAKE_GPU {
        println!("[usertests] t58: digest invariato dopo flip?!");
        return false;
    }
    if civis::obj_get(b"sys", b"bin/shell-missing.bin").is_ok() {
        println!("[usertests] t58: chiave assente restituisce dati?!");
        return false;
    }
    // Bound nomi (hygiene Fase 55): bucket > 16B e chiave > 255B rifiutati
    // loud su entrambi i lati (mai troncamento `as u8`).
    let long_bucket = [b'B'; 17];
    if civis::obj_put(&long_bucket, b"k", b"v").is_ok() {
        println!("[usertests] t58: bucket oltre bound accettato?!");
        return false;
    }
    let long_key = [b'K'; 256];
    if civis::obj_put(b"sys", &long_key, b"v").is_ok()
        || civis::obj_get(b"sys", &long_key).is_ok()
    {
        println!("[usertests] t58: chiave oltre bound accettata?!");
        return false;
    }
    true
}
