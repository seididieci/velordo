use super::*;

/// t33 — mount/umount espliciti (Fase 16b; ADR-0058: il FAT esce, il
/// mount e' sul volume **ArcaFS** root — scoperto dal magic, mai per lettera).
/// mkdir /mnt (ramfs) → mount <dev ArcaFS> /mnt → /mnt/hello.txt col
/// contenuto del fixture → umount busy rifiutato con fd aperto → close →
/// umount ok → /mnt torna ramfs (readdir senza l'entry). Error paths:
/// sorgente inesistente o senza disco, target invalido, doppio mount
/// (idempotente: ok e ancora operativo), umount di non-montato e di `/`.
pub fn t_mount() -> bool {
    helpers::drain_stray();
    if civis::mkdir("/mnt").is_err() {
        println!("[usertests] t33: mkdir /mnt FAILED");
        return false;
    }
    let dev = match helpers::find_arca_device() {
        Some(d) => d,
        None => {
            println!("[usertests] t33: nessun volume ArcaFS trovato");
            return false;
        }
    };
    if civis::mount(&dev, "/mnt").is_err() {
        println!("[usertests] t33: mount {} /mnt FAILED", dev);
        return false;
    }
    // Re-mount identico: idempotente (replace), resta operativo.
    if civis::mount(&dev, "/mnt").is_err() {
        println!("[usertests] t33: re-mount FAILED");
        return false;
    }
    // Contenuto via mount dinamico (stesso volume della root).
    let Ok(fd) = civis::open("/mnt/hello.txt", 0) else {
        println!("[usertests] t33: open /mnt/hello.txt FAILED");
        let _ = civis::umount("/mnt");
        return false;
    };
    let mut hb = [0u8; 32];
    let n = helpers::t33_read_all(fd, &mut hb);
    if n != helpers::ARCA_HELLO.len() || hb[..helpers::ARCA_HELLO.len()] != *helpers::ARCA_HELLO {
        println!("[usertests] t33: /mnt/hello.txt corrotto");
        let _ = civis::close(fd);
        let _ = civis::umount("/mnt");
        return false;
    }
    // Umount busy: fd aperto sul mount → rifiutato.
    if civis::umount("/mnt").is_ok() {
        println!("[usertests] t33: umount busy accettato?!");
        let _ = civis::close(fd);
        return false;
    }
    let _ = civis::close(fd);
    if civis::umount("/mnt").is_err() {
        println!("[usertests] t33: umount /mnt FAILED");
        return false;
    }
    // Dopo umount /mnt e' di nuovo ramfs: niente entry FAT.
    let mut eb = [0u8; 256];
    if civis::readdir("/mnt", &mut eb, 256).is_err() {
        println!("[usertests] t33: readdir /mnt post-umount FAILED");
        return false;
    }
    let mut i = 0usize;
    let mut fat_left = false;
    while i < eb.len() && eb[i] != 0 {
        let start = i;
        while i < eb.len() && eb[i] != 0 {
            i += 1;
        }
        if &eb[start..i] == b"hello.txt" {
            fat_left = true;
        }
        i += 1;
    }
    if fat_left {
        println!("[usertests] t33: entry del volume dopo umount?!");
        return false;
    }
    // Error paths.
    if civis::mount("/dev/xxx", "/mnt").is_ok() {
        println!("[usertests] t33: mount sorgente invalida accettato?!");
        return false;
    }
    if civis::mount("/dev/sdz", "/mnt").is_ok() {
        println!("[usertests] t33: mount disco assente accettato?!");
        return false;
    }
    if civis::mount(&dev, "/a/../b").is_ok() {
        println!("[usertests] t33: mount target invalido accettato?!");
        return false;
    }
    if civis::umount("/mnt").is_ok() {
        println!("[usertests] t33: doppio umount accettato?!");
        return false;
    }
    if civis::umount("/").is_ok() {
        println!("[usertests] t33: umount / accettato?!");
        return false;
    }
    true
}

/// t35 — resolve nome→handle lato driver (Fase 16c).
/// Nomi ignoti (ben formati ma assenti, o malformati) rifiutati SENZA cambio
/// di stato (nessuna spec fantasma: l'umount successivo deve fallire);
/// replace-con-bad-source su target attivo non distrugge il buon mount;
/// mount valido ancora operativo dopo i rifiuti (tabella intatta).
pub fn t_resolve() -> bool {
    helpers::drain_stray();
    // 1. Nome ben formato ma assente (nessun disco sdz: le lettere sono a..h).
    if civis::mount("/dev/sdz1", "/phantom").is_ok() {
        println!("[usertests] t35: mount sdz1 assente accettato?!");
        return false;
    }
    // Nessuna spec fantasma: umount deve fallire.
    if civis::umount("/phantom").is_ok() {
        println!("[usertests] t35: spec fantasma dopo mount fallito?!");
        return false;
    }
    // 2. Nome malformato/ignoto: stesso contratto.
    if civis::mount("/dev/zzz", "/phantom2").is_ok() {
        println!("[usertests] t35: mount nome ignoto accettato?!");
        return false;
    }
    if civis::umount("/phantom2").is_ok() {
        println!("[usertests] t35: spec fantasma (nome ignoto)?!");
        return false;
    }
    // 3. Mount valido ancora operativo dopo i rifiuti (tabella intatta).
    let dev = match helpers::find_arca_device() {
        Some(d) => d,
        None => {
            println!("[usertests] t35: nessun volume ArcaFS trovato");
            return false;
        }
    };
    if civis::mount(&dev, "/mnt").is_err() {
        println!("[usertests] t35: mount {} /mnt FAILED", dev);
        return false;
    }
    let Ok(fd) = civis::open("/mnt/hello.txt", 0) else {
        println!("[usertests] t35: open /mnt/hello.txt FAILED");
        let _ = civis::umount("/mnt");
        return false;
    };
    let mut hb = [0u8; 32];
    let n = helpers::t33_read_all(fd, &mut hb);
    let _ = civis::close(fd);
    if n != helpers::ARCA_HELLO.len() || hb[..helpers::ARCA_HELLO.len()] != *helpers::ARCA_HELLO {
        println!("[usertests] t35: /mnt/hello.txt corrotto");
        let _ = civis::umount("/mnt");
        return false;
    }
    // 4. Replace con bad source non distrugge il buon mount.
    if civis::mount("/dev/zzz", "/mnt").is_ok() {
        println!("[usertests] t35: replace con bad source accettato?!");
        let _ = civis::umount("/mnt");
        return false;
    }
    let Ok(fd4) = civis::open("/mnt/hello.txt", 0) else {
        println!("[usertests] t35: buon mount distrutto dal bad replace?!");
        let _ = civis::umount("/mnt");
        return false;
    };
    let _ = civis::close(fd4);
    if civis::umount("/mnt").is_err() {
        println!("[usertests] t35: umount /mnt FAILED");
        return false;
    }
    true
}

/// t34 — diritti per-canale lato server (Fase 17, self-restriction).
/// Diretto sul canale di usertests (nessun helper: la semantica e' proprio
/// "riduco i MIEI diritti"). ESEGUITO PER ULTIMO: i drop sono irrevocabili.
/// (1) GET default = ALL+root, baseline write+read ok. (2) drop WRITE:
/// write -1, read ok (riapertura: OPEN resta). (3) drop MOUNT + subtree /bin:
/// mount -1, open fuori -1, open dentro + read + readdir dentro ok, readdir
/// fuori -1 (ogni rifiuto e' seguito da un'op valida: nessun disallineamento
/// ring). (4) widen a root rifiutato, GET conferma i diritti invariati.
pub fn t_rights() -> bool {
    helpers::drain_stray();
    // 1. Default {ALL, root}: GET ritorna ALL, subtree vuoto (= root).
    let mut sb = [0u8; 32];
    if civis::rights_get(&mut sb) != Ok(civis::RIGHTS_ALL) || sb[0] != 0 {
        println!("[usertests] t34: GET default non ALL+root");
        return false;
    }
    let Ok(fd) = civis::open("/t34.txt", civis::OPEN_CREATE) else {
        println!("[usertests] t34: open baseline FAILED");
        return false;
    };
    if civis::write_fs(fd, b"abcdef", 6) != Ok(6) {
        println!("[usertests] t34: write baseline FAILED");
        let _ = civis::close(fd);
        return false;
    }
    let _ = civis::close(fd);
    // 2. Drop solo-ops (WRITE via, resto invariato): write -1, read ok.
    if civis::rights_drop(civis::RIGHTS_ALL & !civis::RIGHTS_WRITE, None).is_err() {
        println!("[usertests] t34: rights_drop WRITE FAILED");
        return false;
    }
    let Ok(fd) = civis::open("/t34.txt", 0) else {
        println!("[usertests] t34: reopen dopo drop FAILED");
        return false;
    };
    if civis::write_fs(fd, b"x", 1).is_ok() {
        println!("[usertests] t34: write accettata dopo drop?!");
        let _ = civis::close(fd);
        return false;
    }
    let mut rb = [0u8; 8];
    if civis::read_fs(fd, &mut rb, 6) != Ok(6) || rb[..6] != *b"abcdef" {
        println!("[usertests] t34: read dopo drop FAILED/corrotto");
        let _ = civis::close(fd);
        return false;
    }
    let _ = civis::close(fd);
    // 3. Drop MOUNT + subtree /bin (dir reale sulla radice ArcaFS).
    if civis::rights_drop(
        civis::RIGHTS_ALL & !civis::RIGHTS_WRITE & !civis::RIGHTS_MOUNT,
        Some("/bin"),
    ).is_err()
    {
        println!("[usertests] t34: rights_drop MOUNT+/bin FAILED");
        return false;
    }
    if civis::mount("/dev/sda", "/mnt").is_ok() {
        println!("[usertests] t34: mount accettato senza bit?!");
        return false;
    }
    if civis::open("/hello.txt", 0).is_ok() {
        println!("[usertests] t34: open fuori subtree accettato?!");
        return false;
    }
    let Ok(fd) = civis::open("/bin/gpu.bin", 0) else {
        println!("[usertests] t34: open dentro subtree FAILED");
        return false;
    };
    let _ = civis::close(fd);
    let mut eb = [0u8; 256];
    if civis::readdir("/bin", &mut eb, 256).is_err() {
        println!("[usertests] t34: readdir dentro subtree FAILED");
        return false;
    }
    if civis::readdir("/", &mut eb, 256).is_ok() {
        println!("[usertests] t34: readdir fuori subtree accettato?!");
        return false;
    }
    // 4. Widen a root rifiutato (da /bin): Err e diritti invariati.
    if civis::rights_drop(civis::RIGHTS_ALL, Some("/")).is_ok() {
        println!("[usertests] t34: widen a root accettato?!");
        return false;
    }
    let mut sb2 = [0u8; 32];
    let want = civis::RIGHTS_ALL & !civis::RIGHTS_WRITE & !civis::RIGHTS_MOUNT;
    if civis::rights_get(&mut sb2) != Ok(want) || sb2[..3] != *b"bin" || sb2[3] != 0 {
        println!("[usertests] t34: GET finale non mask+/bin");
        return false;
    }
    true
}

/// t37 — syscall `ps_info` (Fase 19.1): snapshot processi.
pub fn t_ps() -> bool {
    let me = civis::getpid() as u32;
    let mut count = 0u32;
    let mut found_me = false;
    let mut my_ticks = 0u64;
    for pid in 0..civis::PS_SCAN_MAX {
        let Some(e) = civis::ps_info(pid) else { continue; };
        count += 1;
        if e.prio > 31 {
            println!("[usertests] t37: prio assurda pid={}", pid);
            return false;
        }
        if pid == 0 {
            // idle: processo kernel senza padre.
            if e.name_str() != "idle" || e.parent.is_some() {
                println!("[usertests] t37: idle anomalo");
                return false;
            }
        }
        if pid == 1 {
            // init: gira da boot, ha consumato tick di sicuro.
            if e.name_str() != "rector" || e.parent.is_some() || e.ticks == 0 {
                println!("[usertests] t37: init anomalo");
                return false;
            }
        }
        if e.pid == me {
            found_me = true;
            my_ticks = e.ticks;
            // Sto eseguendo: Ready (mai Blocked durante una syscall).
            if e.state != 0 {
                println!("[usertests] t37: self non Ready");
                return false;
            }
        }
    }
    if !found_me {
        println!("[usertests] t37: self assente");
        return false;
    }
    if count < 8 {
        println!("[usertests] t37: solo {} processi?!", count);
        return false;
    }
    // TIME cresce mentre giro: attendo (bound 500 tick) che il contatore del
    // processo avanzi — robusto a qualunque velocita' CPU. Lo spin fisso da
    // 20M iterazioni finiva sotto un tick sulle CPU veloci (flake "TIME fermo"
    // osservato sotto KVM): ora si aspetta l'evento con bound, mai un tempo
    // fisso. Batch di spin puri tra le letture (pattern utspin: niente
    // busy-loop su syscall).
    let bound = civis::get_ticks() + 500;
    let mut my_ticks2 = my_ticks;
    loop {
        let mut x = 0u64;
        for i in 0..1_000_000u64 {
            x = x.wrapping_add(i ^ 0x9E3779B97F4A7C15);
        }
        core::hint::black_box(x);
        for pid in 0..civis::PS_SCAN_MAX {
            if let Some(e) = civis::ps_info(pid) {
                if e.pid == me {
                    my_ticks2 = e.ticks;
                }
            }
        }
        if my_ticks2 > my_ticks {
            break;
        }
        if civis::get_ticks() > bound {
            println!("[usertests] t37: TIME fermo ({} -> {})", my_ticks, my_ticks2);
            return false;
        }
    }
    // Fase 52 (P3 durabilita'): sensore RAM `SYS_MEMINFO` — totali positivi,
    // liberi entro i totali, usati = totali - liberi (coerenza istantanea).
    let (free, total, used) = civis::meminfo();
    if total == 0 || free > total || free + used != total {
        println!("[usertests] t37: meminfo assurda ({}/{}/{})", free, total, used);
        return false;
    }
    println!("[usertests] t37: meminfo free={} total={} used={} frame", free, total, used);
    true
}

/// t38 — `stat` lato cardo (Fase 19.2): metadati senza aprire.
pub fn t_stat() -> bool {
    let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
    // File ramfs: size esatta, non readonly.
    if civis::stat("hello.txt", &mut st).is_err()
        || !st.is_file()
        || st.size as usize != helpers::HELLO.len()
        || st.readonly
    {
        println!("[usertests] t38: stat hello.txt FAILED");
        return false;
    }
    // Root ramfs: dir.
    if civis::stat("/", &mut st).is_err() || !st.is_dir() {
        println!("[usertests] t38: stat / FAILED");
        return false;
    }
    // Dir ramfs creata ad hoc + rimozione (stat segue la vita del nodo).
    if civis::mkdir("/t38dir").is_err() {
        println!("[usertests] t38: mkdir /t38dir FAILED");
        return false;
    }
    if civis::stat("/t38dir", &mut st).is_err() || !st.is_dir() || st.readonly {
        println!("[usertests] t38: stat /t38dir FAILED");
        return false;
    }
    if civis::remove("/t38dir").is_err() || civis::stat("/t38dir", &mut st).is_ok() {
        println!("[usertests] t38: stat dopo rm accettata?!");
        return false;
    }
    // Radice ArcaFS (scrivibile): file + dir NON readonly.
    if civis::stat("/hello.txt", &mut st).is_err()
        || !st.is_file()
        || st.size == 0
        || st.readonly
    {
        println!("[usertests] t38: stat /hello.txt FAILED");
        return false;
    }
    if civis::stat("/bin", &mut st).is_err() || !st.is_dir() || st.readonly {
        println!("[usertests] t38: stat /bin FAILED");
        return false;
    }
    // Device: tipo device, size 0; padri sintetizzati: dir.
    if civis::stat("/dev/null", &mut st).is_err() || !st.is_device() || st.size != 0 {
        println!("[usertests] t38: stat /dev/null FAILED");
        return false;
    }
    if civis::stat("/dev", &mut st).is_err() || !st.is_dir() {
        println!("[usertests] t38: stat /dev FAILED");
        return false;
    }
    // Error paths: inesistente e sotto-device (foglie).
    if civis::stat("/nonexistent-t38", &mut st).is_ok() {
        println!("[usertests] t38: stat inesistente accettata?!");
        return false;
    }
    if civis::stat("/dev/null/trailing", &mut st).is_ok() {
        println!("[usertests] t38: stat sotto-device accettata?!");
        return false;
    }
    // Fase 50 (P1 orologio): servizio Time raggiungibile, monotono e con
    // epoch plausibile; mtime veri su ramfs (creazione+write) e FAT.
    const EPOCH_2020: u64 = 1_577_836_800; // 2020-01-01 00:00:00 UTC
    let (s1, c1) = match civis::time::time_now() {
        Ok(t) => t,
        Err(_) => {
            println!("[usertests] t38: TIME_NOW irraggiungibile");
            return false;
        }
    };
    if s1 < EPOCH_2020 || c1 > 99 {
        println!("[usertests] t38: data/ora implausibile ({}.{:02})", s1, c1);
        return false;
    }
    let (s2, c2) = match civis::time::time_now() {
        Ok(t) => t,
        Err(_) => {
            println!("[usertests] t38: TIME_NOW (2) irraggiungibile");
            return false;
        }
    };
    if (s2, c2) < (s1, c1) {
        println!("[usertests] t38: wall-clock non monotono");
        return false;
    }
    // mtime ramfs: file creato+scritto dal test (Time gia' su a boot).
    let fd = match civis::open("/t38mtime", civis::OPEN_CREATE) {
        Ok(f) => f,
        Err(_) => {
            println!("[usertests] t38: create /t38mtime FAILED");
            return false;
        }
    };
    let w = civis::write_fs(fd, b"t38", 3);
    let _ = civis::close(fd);
    if w != Ok(3) {
        println!("[usertests] t38: write /t38mtime FAILED");
        return false;
    }
    if civis::stat("/t38mtime", &mut st).is_err() || st.mtime < EPOCH_2020 {
        println!("[usertests] t38: mtime /t38mtime implausibile ({})", st.mtime);
        return false;
    }
    let m1 = st.mtime;
    // Dir ramfs creata dal test: timbrata anche lei.
    if civis::stat("/t38dir2", &mut st).is_ok() || civis::mkdir("/t38dir2").is_err() {
        println!("[usertests] t38: setup /t38dir2 FAILED");
        return false;
    }
    if civis::stat("/t38dir2", &mut st).is_err() || st.mtime < EPOCH_2020 {
        println!("[usertests] t38: mtime /t38dir2 implausibile ({})", st.mtime);
        return false;
    }
    // Crescita garantita: 120 tick (1.2 s) attraversano sempre un secondo.
    civis::spin_ticks(120);
    let fd = match civis::open("/t38mtime", 0) {
        Ok(f) => f,
        Err(_) => {
            println!("[usertests] t38: reopen /t38mtime FAILED");
            return false;
        }
    };
    let w = civis::write_fs(fd, b"x", 1);
    let _ = civis::close(fd);
    if w != Ok(1) {
        println!("[usertests] t38: rewrite /t38mtime FAILED");
        return false;
    }
    if civis::stat("/t38mtime", &mut st).is_err() || st.mtime <= m1 {
        println!("[usertests] t38: mtime non cresciuto ({} -> {})", m1, st.mtime);
        return false;
    }
    // Il fixture seedato ha un mtime noto (> 0).
    if civis::stat("/hello.txt", &mut st).is_err() || st.mtime == 0 {
        println!("[usertests] t38: mtime del fixture assente ({})", st.mtime);
        return false;
    }
    let _ = civis::remove("/t38mtime");
    let _ = civis::remove("/t38dir2");
    true
}

/// t39 — servizi da disco (Fase 21; ADR-0058: i binari vivono in ArcaFS,
/// non piu' iniettati in un FAT): i binari in `/bin` e `/test` esistono e
/// sono non vuoti, e tutti i servizi sono up per nome (= il boot da disco ha
/// funzionato: gpu/kbd/porta/shell non sono piu' embedded ma girano).
pub fn t_diskboot() -> bool {
    let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
    for path in [
        "/bin/gpu.bin",
        "/bin/uptime.bin",
        "/bin/vela.bin",
        "/bin/kbd.bin",
        "/bin/porta.bin",
        "/bin/posix/shell.bin",
        "/test/testfs.bin",
        "/test/testfat.bin",
        "/test/tests.bin",
    ] {
        if civis::stat(path, &mut st).is_err() || !st.is_file() || st.size == 0 {
            println!("[usertests] t39: {} mancante/vuoto", path);
            return false;
        }
    }
    for svc in [
        civis::Service::Gpu,
        civis::Service::Cardo,
        civis::Service::Vela,
        civis::Service::Kbd,
        civis::Service::Porta,
        civis::Service::Block,
    ] {
        if civis::service_lookup(svc).is_err() {
            println!("[usertests] t39: servizio {:?} non registrato", svc as u64);
            return false;
        }
    }
    true
}


/// t59 — rename S1.1 su ramfs: move, replace, dir con figli, errori
/// tipizzati, cross-mount rifiutato. Cleanup finale (ramfs condivisa).
pub fn t_rename() -> bool {
    // Fixture.
    if civis::open("/t59a.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC).is_err() {
        println!("[usertests] t59: create a FAILED");
        return false;
    }
    if let Ok(fd) = civis::open("/t59a.txt", civis::OPEN_TRUNC) {
        let w = civis::write_fs(fd, b"rename-59", 9);
        let _ = civis::close(fd);
        if w != Ok(9) {
            println!("[usertests] t59: write a FAILED");
            return false;
        }
    }
    // Move base: vecchio sparisce, nuovo leggibile intatto.
    if civis::rename("/t59a.txt", "/t59b.txt").is_err() {
        println!("[usertests] t59: rename base FAILED");
        return false;
    }
    if civis::open("/t59a.txt", 0).is_ok() {
        println!("[usertests] t59: vecchio ancora presente?!");
        return false;
    }
    let fd = match civis::open("/t59b.txt", 0) {
        Ok(f) => f,
        Err(e) => {
            println!("[usertests] t59: open b = {:?}", e);
            return false;
        }
    };
    let mut buf = [0u8; 9];
    let r = civis::read_fs(fd, &mut buf, 9);
    let _ = civis::close(fd);
    if r != Ok(9) || &buf != b"rename-59" {
        println!("[usertests] t59: contenuto dopo move = {:?}", r);
        return false;
    }
    // Replace: file su file esistente.
    if civis::open("/t59c.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC).is_err() {
        println!("[usertests] t59: create c FAILED");
        return false;
    }
    if civis::rename("/t59c.txt", "/t59b.txt").is_err() {
        println!("[usertests] t59: replace FAILED");
        return false;
    }
    let mut st = civis::Stat { size: 99, kind: 0, readonly: false, mtime: 0 };
    if civis::stat("/t59b.txt", &mut st).is_err() || st.size != 0 {
        println!("[usertests] t59: replace non ha sostituito (size={})", st.size);
        return false;
    }
    // Dir con figli: move subtree.
    if civis::mkdir("/t59d").is_err()
        || civis::open("/t59d/f.txt", civis::OPEN_CREATE).is_err()
    {
        println!("[usertests] t59: setup dir FAILED");
        return false;
    }
    if civis::rename("/t59d", "/t59e").is_err() {
        println!("[usertests] t59: rename dir FAILED");
        return false;
    }
    if civis::open("/t59e/f.txt", 0).is_err() || civis::open("/t59d/f.txt", 0).is_ok() {
        println!("[usertests] t59: subtree non spostato");
        return false;
    }
    // Errori tipizzati: assente, file→dir, cross-mount (/tmp e' un mount
    // ramfs separato: rename tra mount = Invalid).
    if civis::rename("/t59mai", "/t59x").is_ok() {
        println!("[usertests] t59: rename assente ok?!");
        return false;
    }
    if !matches!(civis::rename("/t59b.txt", "/t59e"), Err(civis::Error::IsDir)) {
        println!("[usertests] t59: file→dir non IsDir");
        return false;
    }
    if civis::rename("/t59b.txt", "/tmp/t59b.txt").is_ok() {
        println!("[usertests] t59: cross-mount ok?!");
        return false;
    }
    // Cleanup.
    let _ = civis::remove("/t59b.txt");
    let _ = civis::remove("/t59e/f.txt");
    let _ = civis::remove("/t59e");
    true
}

/// t60 — symlink S1.1 su ramfs: create/readlink/follow/open/stat,
/// dangling, loop, rename-del-link. Cleanup finale.
pub fn t_symlink() -> bool {
    // Create (link=/t60link -> target=/t60real.txt, dangling ammesso: il
    // target si crea dopo) + readlink + stat kind.
    if civis::symlink("/t60link", "/t60real.txt").is_err() {
        println!("[usertests] t60: symlink create FAILED");
        return false;
    }
    match civis::readlink("/t60link") {
        Ok(v) if v == b"/t60real.txt" => {}
        other => {
            println!("[usertests] t60: readlink = {:?}", other.map(|v| v.len()));
            return false;
        }
    }
    let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
    if civis::stat("/t60link", &mut st).is_err() || !st.is_symlink() {
        println!("[usertests] t60: stat link kind={} (atteso symlink)", st.kind);
        return false;
    }
    // Dangling: open fallisce NOTFOUND finche' il target manca.
    if civis::open("/t60link", 0).is_ok() {
        println!("[usertests] t60: open dangling ok?!");
        return false;
    }
    // Crea il target: open segue e legge.
    if civis::open("/t60real.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC).is_err() {
        println!("[usertests] t60: create target FAILED");
        return false;
    }
    if let Ok(fd) = civis::open("/t60real.txt", civis::OPEN_TRUNC) {
        let w = civis::write_fs(fd, b"linkdata", 8);
        let _ = civis::close(fd);
        if w != Ok(8) {
            println!("[usertests] t60: write target FAILED");
            return false;
        }
    }
    let fd = match civis::open("/t60link", 0) {
        Ok(f) => f,
        Err(e) => {
            println!("[usertests] t60: open via link = {:?}", e);
            return false;
        }
    };
    let mut buf = [0u8; 8];
    let r = civis::read_fs(fd, &mut buf, 8);
    let _ = civis::close(fd);
    if r != Ok(8) || &buf != b"linkdata" {
        println!("[usertests] t60: read via link = {:?}", r);
        return false;
    }
    // Loop: a->b, b->a: open rifiutato (niente hang).
    if civis::symlink("/t60b", "/t60a").is_err() || civis::symlink("/t60a", "/t60b").is_err() {
        println!("[usertests] t60: setup loop FAILED");
        return false;
    }
    if civis::open("/t60a", 0).is_ok() {
        println!("[usertests] t60: open loop ok?!");
        return false;
    }
    // Esistente = EXISTS (link path occupato da file); readlink su file = errore.
    if civis::symlink("/t60real.txt", "/t60x").is_ok() {
        println!("[usertests] t60: symlink su esistente ok?!");
        return false;
    }
    if civis::readlink("/t60real.txt").is_ok() {
        println!("[usertests] t60: readlink su file ok?!");
        return false;
    }
    // Rename del link (sposta il link, mai il target).
    if civis::rename("/t60link", "/t60link2").is_err() {
        println!("[usertests] t60: rename link FAILED");
        return false;
    }
    if civis::readlink("/t60link").is_ok()
        || !matches!(civis::readlink("/t60link2"), Ok(v) if v == b"/t60real.txt")
    {
        println!("[usertests] t60: rename link non spostato");
        return false;
    }
    // Cleanup.
    let _ = civis::remove("/t60link2");
    let _ = civis::remove("/t60a");
    let _ = civis::remove("/t60b");
    let _ = civis::remove("/t60real.txt");
    true
}

/// t61 — chdir/getcwd S1.1: default `/`, relativi contro cwd, errori,
/// persistenza del gruppo, cleanup con ritorno a `/`.
pub fn t_chdir() -> bool {
    // Default: `/`.
    match civis::getcwd() {
        Ok(v) if v == b"/" => {}
        other => {
            println!("[usertests] t61: getcwd default = {:?}", other.map(|v| v.len()));
            return false;
        }
    }
    if civis::mkdir("/t61d").is_err() {
        println!("[usertests] t61: mkdir FAILED");
        return false;
    }
    // chdir assoluto + getcwd combacia.
    if civis::chdir("/t61d").is_err() {
        println!("[usertests] t61: chdir FAILED");
        return false;
    }
    match civis::getcwd() {
        Ok(v) if v == b"/t61d" => {}
        other => {
            println!("[usertests] t61: getcwd dopo chdir = {:?}", other.map(|v| v.len()));
            return false;
        }
    }
    // Relativi contro cwd: create + read senza slash iniziale.
    if civis::open("f61.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC).is_err() {
        println!("[usertests] t61: create relativo FAILED");
        return false;
    }
    if let Ok(fd) = civis::open("f61.txt", civis::OPEN_TRUNC) {
        let w = civis::write_fs(fd, b"cwd-ok", 6);
        let _ = civis::close(fd);
        if w != Ok(6) {
            println!("[usertests] t61: write relativo FAILED");
            return false;
        }
    }
    let fd = match civis::open("f61.txt", 0) {
        Ok(f) => f,
        Err(e) => {
            println!("[usertests] t61: open relativo = {:?}", e);
            return false;
        }
    };
    let mut buf = [0u8; 6];
    let r = civis::read_fs(fd, &mut buf, 6);
    let _ = civis::close(fd);
    if r != Ok(6) || &buf != b"cwd-ok" {
        println!("[usertests] t61: read relativo = {:?}", r);
        return false;
    }
    // Il file e' davvero sotto /t61d (assoluto incrociato).
    let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
    if civis::stat("/t61d/f61.txt", &mut st).is_err() || st.size != 6 {
        println!("[usertests] t61: stat incrociato FAILED");
        return false;
    }
    // Errori: file, mancante, relativo-mancante.
    if civis::chdir("/t61d/f61.txt").is_ok() {
        println!("[usertests] t61: chdir su file ok?!");
        return false;
    }
    if civis::chdir("/t61mai").is_ok() {
        println!("[usertests] t61: chdir mancante ok?!");
        return false;
    }
    if civis::chdir("sottomai").is_ok() {
        println!("[usertests] t61: chdir relativo-mancante ok?!");
        return false;
    }
    // Cleanup: torna a `/` (i test dopo pretendono la root) e rimuovi.
    if civis::chdir("/").is_err() {
        println!("[usertests] t61: chdir / FAILED");
        return false;
    }
    match civis::getcwd() {
        Ok(v) if v == b"/" => {}
        _ => {
            println!("[usertests] t61: getcwd finale non root");
            return false;
        }
    }
    let _ = civis::remove("/t61d/f61.txt");
    let _ = civis::remove("/t61d");
    true
}

/// t62 — chmod S1.1 (projection, mai enforcement): Ok su file/dir/link,
/// NOTFOUND su mancante, R/W dopo chmod restrittivo (nessun enforcement
/// fino ad A4: i build non devono fallire).
pub fn t_chmod() -> bool {
    if civis::open("/t62.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC).is_err() {
        println!("[usertests] t62: create FAILED");
        return false;
    }
    // Mode varie accettate (incluse 000 e 777: niente enforcement).
    for mode in [0o644u32, 0o755, 0o600, 0o000, 0o777] {
        if civis::chmod("/t62.txt", mode).is_err() {
            println!("[usertests] t62: chmod {:o} FAILED", mode);
            return false;
        }
    }
    // Dopo chmod 000 si scrive e legge comunque (Strato 0: placeholder).
    if let Ok(fd) = civis::open("/t62.txt", civis::OPEN_TRUNC) {
        let w = civis::write_fs(fd, b"rw", 2);
        let _ = civis::close(fd);
        if w != Ok(2) {
            println!("[usertests] t62: write dopo 000 FAILED");
            return false;
        }
    }
    // Dir + link: Ok. Mancante: NOTFOUND tipizzato.
    if civis::mkdir("/t62d").is_err() || civis::chmod("/t62d", 0o755).is_err() {
        println!("[usertests] t62: chmod dir FAILED");
        return false;
    }
    if civis::symlink("/t62l", "/t62d").is_err() || civis::chmod("/t62l", 0o777).is_err() {
        println!("[usertests] t62: chmod link FAILED");
        return false;
    }
    if !matches!(civis::chmod("/t62mai", 0o644), Err(civis::Error::NotFound)) {
        println!("[usertests] t62: chmod mancante non NotFound");
        return false;
    }
    // Cleanup.
    let _ = civis::remove("/t62.txt");
    let _ = civis::remove("/t62l");
    let _ = civis::remove("/t62d");
    true
}
