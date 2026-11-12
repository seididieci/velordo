use super::*;

// ── singoli test (ritornano true = pass) ─────────────────────────────

pub fn t_getpid() -> bool {
    civis::getpid() > 0
}

pub fn t_ticks() -> bool {
    let t1 = civis::get_ticks();
    civis::spin_ticks(1);
    let t2 = civis::get_ticks();
    // Attendi esplicitamente che il contatore cambi (timer attivo + IF in user).
    let t2b = t2;
    let mut guard = 0u32;
    while civis::get_ticks() == t2b && guard < 1_000_000 {
        guard += 1;
        core::hint::spin_loop();
    }
    t2 >= t1 && civis::get_ticks() > t2b
}

/// Deve girare PRIMA di altre allocazioni heap rilevanti (prime pagine sbrk
/// mai toccate → demand-zero → lette come 0).
pub fn t_heap_fresh_zero() -> bool {
    let mut a = vec![0u8; 65536];
    let fresh = a.iter().all(|&b| b == 0);
    for i in 0..a.len() {
        a[i] = (i % 251) as u8;
    }
    let mut verify = true;
    for i in (0..a.len()).step_by(997) {
        if a[i] != (i % 251) as u8 {
            verify = false;
            break;
        }
    }
    drop(a);
    fresh && verify
}

pub fn t_heap_reuse() -> bool {
    let a = vec![0x11u8; 4096];
    drop(a);
    let b = vec![0x22u8; 65536];
    let ok1 = b[0] == 0x22 && b[65535] == 0x22;
    drop(b);
    let c = vec![0x33u8; 65536];
    let ok2 = c[0] == 0x33 && c[65535] == 0x33 && c[32768] == 0x33;
    drop(c);
    ok1 && ok2
}

pub fn t_spawn_identity() -> bool {
    // spawn ritorna un canale verso il figlio (ADR-0008); il figlio conferma
    // con ACK portando il proprio pid. Il parent non conosce il pid (identita'
    // interna al kernel): verifica che il canale sia valido e che il figlio
    // abbia risposto (ack > 0) e completato (DONE).
    match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_ECHO, 0) {
        Some((chan, ack_pid)) => {
            // rounds=0 → nessun REQ, solo DONE. Risponde/scarta eventuali
            // residui finche' non arriva il DONE del figlio.
            chan > 0 && ack_pid > 0 && helpers::recv_expect(chan, helpers::T_DONE)
        }
        None => false,
    }
}

pub fn t_hello() -> bool {
    let Ok(fd) = civis::open("hello.txt", 0) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let n = match civis::read_fs(fd, &mut buf, 64) {
        Ok(n) => n,
        Err(_) => {
            let _ = civis::close(fd);
            return false;
        }
    };
    if !(n >= helpers::HELLO.len() && buf[..helpers::HELLO.len()] == *helpers::HELLO) {
        let _ = civis::close(fd);
        return false;
    }
    // Contratto EOF (Fase 18.2-bis): leggere oltre la fine torna Ok(0)
    // (il server scrive sempre il response frame, anche vuoto).
    let mut one = [0u8; 1];
    let eof = civis::read_fs(fd, &mut one, 1);
    let _ = civis::close(fd);
    if eof != Ok(0) {
        println!("[usertests] t6: read oltre EOF = {:?} (atteso Ok(0))", eof);
        return false;
    }
    true
}

pub fn t_ramfs_write_chunk() -> bool {
    let Ok(fd) = civis::open("utdata.bin", civis::OPEN_CREATE) else {
        return false;
    };
    // 3 chunk da 3000 (9 KiB totali > 1 pagina ring da 4088 B): multi-call
    // write con chunking client (Fase 10.2).
    for c in 0..3u32 {
        let chunk: Vec<u8> = (0..3000).map(|i| (((c as usize) * 7 + i) % 251) as u8).collect();
        if civis::write_fs(fd, &chunk, 3000) != Ok(3000) {
            let _ = civis::close(fd);
            return false;
        }
    }
    let _ = civis::close(fd);

    let Ok(fd2) = civis::open("utdata.bin", 0) else {
        return false;
    };
    let mut all = Vec::new();
    let ok = helpers::read_all(fd2, &mut all, 9000);
    let _ = civis::close(fd2);
    if !ok || all.len() != 9000 {
        return false;
    }
    for c in 0..3usize {
        for i in 0..3000 {
            if all[c * 3000 + i] != ((c * 7 + i) % 251) as u8 {
                return false;
            }
        }
    }
    true
}

pub fn t_ramfs_mkdir() -> bool {
    if civis::mkdir("utdir").is_err() {
        return false;
    }
    helpers::dir_contains("/", "utdir")
}

pub fn t_fs_errors() -> bool {
    // open di path vuoto → Err (path_len 0).
    let a = civis::open("", 0).is_err();
    // read/write/close su fd inesistente → Err (fd non nella tabella server).
    let b = civis::read_fs(-1, &mut [0u8; 8], 8).is_err();
    let c = civis::write_fs(-1, &[0u8; 8], 8).is_err();
    let d = civis::close(-1).is_err();
    a && b && c && d
}

pub fn t_dev_null() -> bool {
    // Throttled (Livello 1): un device non ancora registrato non giustifica
    // mai una tempesta di open verso cardo.
    let Ok(fd) = civis::open_wait("/dev/null", 0, 1000, civis::POLL_PERIOD_TICKS) else {
        return false;
    };
    let data = [0x5Au8; 512];
    let w = civis::write_fs(fd, &data, 512);
    let mut b = [0u8; 16];
    let r = civis::read_fs(fd, &mut b, 16);
    let _ = civis::close(fd);
    w == Ok(512) && r == Ok(0)
}

pub fn t_dev_zero() -> bool {
    let Ok(fd) = civis::open_wait("/dev/zero", 0, 1000, civis::POLL_PERIOD_TICKS) else {
        return false;
    };
    let mut ok = true;
    let mut buf = vec![0xFFu8; 4096];
    for _ in 0..2 {
        let n = civis::read_fs(fd, &mut buf, 4096);
        if n != Ok(4096) || buf.iter().any(|&b| b != 0) {
            ok = false;
        }
    }
    let _ = civis::close(fd);
    ok
}

/// t44 — mmap anonimo nel basso canonico (Fase 28): pattern R/W, multi-PT,
/// fixed/overlap, munmap intero + riuso, integrazione syscall (write da
/// buffer mappato). Solo path suite-safe (rifiuti = -1, mai fault): i
/// negativi-con-fault fermerebbero il sistema, come il NULL test kernel.
pub fn t_mmap() -> bool {
    // 1. Anonima 3 pagine: base bassa + allineata, pattern oltre le pagine.
    let a = match civis::mmap(0, 3 * 4096) {
        Ok(a) => a,
        Err(_) => return false,
    };
    if a < 0x10_0000 || a & 0xFFF != 0 {
        return false;
    }
    for i in 0..3 * 4096usize {
        unsafe { core::ptr::write_volatile((a + i) as *mut u8, i.wrapping_mul(7) as u8); }
    }
    for i in 0..3 * 4096usize {
        if unsafe { core::ptr::read_volatile((a + i) as *const u8) } != i.wrapping_mul(7) as u8 {
            return false;
        }
    }
    // 2. Multi-PT: 3 MiB, spot-check per pagina (1536 fault demand-zero).
    let big = match civis::mmap(0, 3 * 1024 * 1024) {
        Ok(a) => a,
        Err(_) => return false,
    };
    if big == a {
        return false; // basi distinte (no alias)
    }
    let npages = 3 * 1024 * 1024 / 4096;
    for p in 0..npages {
        unsafe { core::ptr::write_volatile((big + p * 4096) as *mut u8, (p & 0xFF) as u8); }
    }
    for p in 0..npages {
        if unsafe { core::ptr::read_volatile((big + p * 4096) as *const u8) } != (p & 0xFF) as u8 {
            return false;
        }
    }
    // 3. Fixed + overlap: libero ok, occupati/disallineati/len-0 rifiutati.
    let f = match civis::mmap_fixed(0x50_0000, 8192) {
        Ok(x) => x,
        Err(_) => return false,
    };
    if f != 0x50_0000 {
        return false;
    }
    if civis::mmap_fixed(a, 4096).is_ok() {
        return false; // dentro `a`
    }
    if civis::mmap(big + 4096, 4096).is_ok() {
        return false; // hint dentro `big` (strict: niente fallback)
    }
    if civis::mmap(0, 0).is_ok() {
        return false; // len 0
    }
    if civis::mmap(a + 1, 4096).is_ok() {
        return false; // hint disallineato
    }
    // 4. Munmap: parziale rifiutato senza stato, interi ok + riuso fixed.
    if civis::munmap(a + 4096, 4096).is_ok() {
        return false; // split = Err in 28
    }
    if unsafe { core::ptr::read_volatile(a as *const u8) } != 0 {
        return false; // ancora intatta (pattern[0] = 0)
    }
    if civis::munmap(a, 3 * 4096).is_err() {
        return false;
    }
    if civis::munmap(big, 3 * 1024 * 1024).is_err() {
        return false;
    }
    if civis::munmap(f, 8192).is_err() {
        return false;
    }
    let a2 = match civis::mmap_fixed(a, 3 * 4096) {
        Ok(x) => x,
        Err(_) => return false,
    };
    if a2 != a {
        return false;
    }
    for i in 0..64usize {
        if unsafe { core::ptr::read_volatile((a2 + i) as *const u8) } != 0 {
            return false; // fresca = zeri (frame nuovi, mai stale)
        }
    }
    let _ = civis::munmap(a2, 3 * 4096);
    // 5. Integrazione syscall: sys_write (fd 2, seriale) da buffer mappato —
    // is_user_range accetta le VMA (rifiuto = -1, non fault).
    let m = match civis::mmap(0, 4096) {
        Ok(x) => x,
        Err(_) => return false,
    };
    let msg = b"mmap-write-ok\n";
    for (i, &b) in msg.iter().enumerate() {
        unsafe { core::ptr::write_volatile((m + i) as *mut u8, b); }
    }
    let n = civis::write(2, m as *const u8, msg.len());
    let _ = civis::munmap(m, 4096);
    if n != msg.len() as i64 {
        return false;
    }
    true
}

/// Fase 29 — protezioni: `mmap_prot`/`mprotect` in-process (transizioni
/// RO/RW/NONE, error paths) + fault di protezione che termina il processo
/// (helper, osservato via EXIT_NOTIFY con FAULT_EXIT_CODE).
pub fn t_mprotect() -> bool {
    // 1. mmap PROT_READ: prima lettura materializza RO (zero fresco).
    let ro = match civis::mmap_prot(0, 4096, civis::PROT_READ) {
        Ok(x) => x,
        Err(_) => return false,
    };
    if unsafe { core::ptr::read_volatile(ro as *const u8) } != 0 {
        return false;
    }
    // 2. RO → RW: scrittura + rilettura.
    if civis::mprotect(ro, 4096, civis::PROT_READ | civis::PROT_WRITE).is_err() {
        return false;
    }
    unsafe { core::ptr::write_volatile(ro as *mut u8, 0x5A); }
    if unsafe { core::ptr::read_volatile(ro as *const u8) } != 0x5A {
        return false;
    }
    // 3. RW → RO: il contenuto resta leggibile.
    if civis::mprotect(ro, 4096, civis::PROT_READ).is_err() {
        return false;
    }
    if unsafe { core::ptr::read_volatile(ro as *const u8) } != 0x5A {
        return false;
    }
    // 4. → NONE: le pagine cadono (nessun accesso qui, sarebbe kill).
    if civis::mprotect(ro, 4096, civis::PROT_NONE).is_err() {
        return false;
    }
    // 5. NONE → RW: riuso con zeri freschi (frame liberati a NONE, rimappati).
    if civis::mprotect(ro, 4096, civis::PROT_READ | civis::PROT_WRITE).is_err() {
        return false;
    }
    if unsafe { core::ptr::read_volatile(ro as *const u8) } != 0 {
        return false;
    }
    let _ = civis::munmap(ro, 4096);

    // 6. Error paths: prot non valido (W solo) rifiutato; mprotect parziale
    // rifiutato SENZA cambiare stato; intero/invalido come atteso.
    if civis::mmap_prot(0, 4096, civis::PROT_WRITE).is_ok() {
        return false;
    }
    let a = match civis::mmap(0, 3 * 4096) {
        Ok(x) => x,
        Err(_) => return false,
    };
    if civis::mprotect(a, 4096, civis::PROT_READ).is_ok() {
        return false; // parziale su 3 VMA-contigue = rifiutato
    }
    if civis::mprotect(a, 3 * 4096, civis::PROT_READ).is_err() {
        return false; // intera = ok
    }
    if civis::mprotect(a, 3 * 4096, 0x40).is_ok() {
        return false; // prot invalido
    }
    let _ = civis::munmap(a, 3 * 4096);

    // 7. Fault di protezione → kill con FAULT_EXIT_CODE (helper, EXIT_NOTIFY).
    helpers::drain_stray();
    for (mode, name) in [
        (helpers::M_FAULT_RO, "RO-write"),
        (helpers::M_FAULT_NONE, "NONE-read"),
        (helpers::M_FAULT_NX, "NX-exec"),
        (helpers::M_FAULT_GUARD, "guard"),
        (helpers::M_FAULT_GPF, "port-GP"),
        (helpers::M_FAULT_CODE, "code-write"),
    ] {
        let (chan, _pid) = match helpers::spawn_cfg(
            "/test/testcli.bin", "utcli", 16, mode, 0,
        ) {
            Some(x) => x,
            None => {
                println!("[usertests] t45: spawn {} FAILED", name);
                return false;
            }
        };
        match helpers::wait_exit(chan) {
            Some((c, _)) if c == civis::FAULT_EXIT_CODE => {}
            Some((c, _)) => {
                println!(
                    "[usertests] t45: {} exit code {} (atteso {})",
                    name, c, civis::FAULT_EXIT_CODE
                );
                return false;
            }
            None => return false,
        }
    }
    true
}

/// Fase 30 — memoria condivisa: il parent crea una regione, ci scrive un
/// pattern, un helper la mappa e (a) verifica il pattern, (b) scrive un
/// marker che il parent deve vedere (visibilita' bidirezionale, pagine
/// fisiche condivise). Poi refcount/teardown e riuso dello slot.
pub fn t_shm() -> bool {
    let id = match civis::shm_create(8192) {
        Ok(i) => i,
        Err(_) => return false,
    };
    let base = match civis::shm_map(id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => return false,
    };
    for i in 0..8192usize {
        unsafe { core::ptr::write_volatile((base + i) as *mut u8, (i % 251) as u8); }
    }
    // Helper: mappa la stessa regione (id in param), verifica, scrive marker.
    helpers::drain_stray();
    let (chan, _pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_SHMDEMO, id as u64,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t47: spawn helper FAILED");
            return false;
        }
    };
    let (ok, _) = helpers::recv_done(&[chan]);
    if !ok {
        println!("[usertests] t47: helper shm FAIL");
        return false;
    }
    // Visibilita': il marker scritto dall'helper (offset 4096) e' visibile.
    if unsafe { core::ptr::read_volatile((base + 4096) as *const u8) } != 0xAB {
        return false;
    }
    if civis::munmap(base, 8192).is_err() {
        return false;
    }
    // Fase 30.1: l'owner chiude la regione (il ref del create non e' un
    // mapping: senza `shm_destroy` la regione resterebbe viva).
    if civis::shm_destroy(id).is_err() {
        return false;
    }
    // Error path: id inesistente rifiutato.
    if civis::shm_map(9999, 0, civis::PROT_READ).is_ok() {
        return false;
    }
    // Riuso: la regione e' stata liberata (helper morto + munmap), un nuovo
    // `shm_create` deve riuscire.
    match civis::shm_create(4096) {
        Ok(id2) => {
            let b2 = match civis::shm_map(id2, 0, civis::PROT_READ | civis::PROT_WRITE) {
                Ok(b) => b,
                Err(_) => return false,
            };
            // Fresca = zeri (frame nuovi azzerati alla creazione).
            let zero = unsafe { core::ptr::read_volatile(b2 as *const u8) } == 0;
            let _ = civis::munmap(b2, 4096);
            // Fase 30.1: anche il secondo create ha il ref owner: senza
            // destroy lo slot resterebbe occupato fino al teardown.
            let _ = civis::shm_destroy(id2);
            zero
        }
        Err(_) => false,
    }
}

/// Fase 32 — shared text: istanze concorrenti dello stesso binario condividono
/// i segmenti immutabili (`RX`/`RO`). `text_stats` osserva hit/refcount; al
/// teardown delle istanze i ref sono rilasciati (frame liberati a 0).
/// NB: il baseline assoluto di `live` non e' stabile (altri test lasciano
/// reclaim pendenti) → si confronta il delta attorno alle PROPRIE operazioni.
pub fn t_text() -> bool {
    helpers::drain_stray();
    let (h0, _m0, _l0) = civis::text_stats();
    // 3 helper parcheggiati (vivi) dallo stesso binario `testcli`.
    let mut chans = [0u64; 3];
    let mut pids = [0i64; 3];
    for i in 0..3 {
        match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_KILLME, 0) {
            Some((c, pid)) => {
                chans[i] = c;
                pids[i] = pid as i64;
            }
            None => {
                println!("[usertests] t47: spawn {} FAILED", i);
                return false;
            }
        }
    }
    let (h1, _m1, live1) = civis::text_stats();
    // Almeno 1 hit: la 2a/3a istanza condivide il testo della 1a.
    if h1 <= h0 {
        println!("[usertests] t47: no sharing (hits {}->{})", h0, h1);
        return false;
    }
    // Cleanup: uccidi e attendi l'exit (reclaim rilascia il ref).
    for i in 0..3 {
        if civis::kill(pids[i], 0).is_err() {
            return false;
        }
        if helpers::wait_exit(chans[i]).is_none() {
            return false;
        }
    }
    let (_h2, _m2, live2) = civis::text_stats();
    // I 3 helper hanno rilasciato esattamente 1 ref ciascuno.
    if live1 < 3 || live1 - live2 != 3 {
        println!("[usertests] t47: release errato (live {}->{}, atteso -3)", live1, live2);
        return false;
    }
    true
}

/// Fase 33 — COW su regione condivisa: il parent crea una regione (mappata
/// normale RW) con un pattern; l'helper la mappa COW, legge il pattern
/// (shared-read) e scrive due pagine (copie private). Il parent non deve
/// vedere le scritture (isolamento); `cow_count` cresce di 2; alla fine la
/// regione e' liberata (riuso slot + zeri freschi, come t46).
pub fn t_cow() -> bool {
    let id = match civis::shm_create(8192) {
        Ok(i) => i,
        Err(_) => return false,
    };
    let base = match civis::shm_map(id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => return false,
    };
    for i in 0..8192usize {
        unsafe { core::ptr::write_volatile((base + i) as *mut u8, (i % 251) as u8); }
    }
    let c0 = civis::cow_count();
    helpers::drain_stray();
    let (chan, _pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_COWDEMO, id as u64,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t48: spawn helper FAILED");
            return false;
        }
    };
    let (ok, _) = helpers::recv_done(&[chan]);
    if !ok {
        println!("[usertests] t48: helper cow FAIL");
        return false;
    }
    // Isolamento: le scritture COW dell'helper non sono visibili al parent.
    if unsafe { core::ptr::read_volatile(base as *const u8) } != 0 {
        return false;
    }
    if unsafe { core::ptr::read_volatile((base + 4096) as *const u8) } != (4096 % 251) as u8 {
        return false;
    }
    let c1 = civis::cow_count();
    if c1 < c0 + 2 {
        println!("[usertests] t48: cow {}->{} (atteso +2)", c0, c1);
        return false;
    }
    if civis::munmap(base, 8192).is_err() {
        return false;
    }
    // Fase 30.1: l'owner chiude la regione (il ref del create non e' un
    // mapping: senza `shm_destroy` la regione resterebbe viva).
    if civis::shm_destroy(id).is_err() {
        return false;
    }
    // Error path: COW su id inesistente rifiutato.
    if civis::shm_map_cow(9999, 0).is_ok() {
        return false;
    }
    // Riuso: slot e frame liberati (helper morto + munmap), zeri freschi.
    match civis::shm_create(4096) {
        Ok(id2) => {
            let b2 = match civis::shm_map(id2, 0, civis::PROT_READ | civis::PROT_WRITE) {
                Ok(b) => b,
                Err(_) => return false,
            };
            let zero = unsafe { core::ptr::read_volatile(b2 as *const u8) } == 0;
            let _ = civis::munmap(b2, 4096);
            // Fase 30.1: anche il secondo create ha il ref owner: senza
            // destroy lo slot resterebbe occupato fino al teardown.
            let _ = civis::shm_destroy(id2);
            zero
        }
        Err(_) => false,
    }
}

/// Fase 30.1 — ownership + destroy: `shm_create` senza `shm_map` non lascia
/// frame orfani; `shm_destroy` e' owner-only, chiude la regione (i `shm_map`
/// successivi falliscono, le mappature esistenti restano valide); un
/// non-owner non puo' chiudere. La regione si libera all'ultimo `munmap`.
pub fn t_shm_destroy() -> bool {
    // 1. create + destroy senza map: i frame tornano subito (slot riusabile).
    let id = match civis::shm_create(4096) {
        Ok(i) => i,
        Err(_) => return false,
    };
    if civis::shm_destroy(id).is_err() {
        return false;
    }
    // Idempotenza: un secondo destroy fallisce (gia' chiusa).
    if civis::shm_destroy(id).is_ok() {
        return false;
    }
    // 2. create + map + destroy: la mappa resta valida, i nuovi map no.
    let id = match civis::shm_create(4096) {
        Ok(i) => i,
        Err(_) => return false,
    };
    let base = match civis::shm_map(id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => return false,
    };
    unsafe { core::ptr::write_volatile(base as *mut u8, 0x5A); }
    if civis::shm_destroy(id).is_err() {
        return false;
    }
    // La mappa esistente resta leggibile/scrivibile (i frame non sono
    // liberati finche' il mapper non fa `munmap`).
    if unsafe { core::ptr::read_volatile(base as *const u8) } != 0x5A {
        return false;
    }
    unsafe { core::ptr::write_volatile(base as *mut u8, 0xA5); }
    if unsafe { core::ptr::read_volatile(base as *const u8) } != 0xA5 {
        return false;
    }
    // Nuovi `shm_map` su una regione chiusa falliscono (fail-closed).
    if civis::shm_map(id, 0, civis::PROT_READ).is_ok() {
        return false;
    }
    // 3. destroy da non-owner: fallisce (helper spawnato).
    helpers::drain_stray();
    let (chan, _pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_SHDESTROY, id as u64,
    ) {
        Some(x) => x,
        None => return false,
    };
    let (ok, _) = helpers::recv_done(&[chan]);
    if !ok {
        return false;
    }
    // 4. Il mapper rilascia: `munmap` libera i frame (owner ref gia' caduto).
    if civis::munmap(base, 4096).is_err() {
        return false;
    }
    // 5. Riuso: lo slot e' libero (un nuovo create deve riuscire).
    match civis::shm_create(4096) {
        Ok(id2) => {
            let _ = civis::shm_destroy(id2);
            true
        }
        Err(_) => false,
    }
}

pub fn t_map_alias() -> bool {    if civis::map_physical(civis::MAP_TEST_PHYS, helpers::VA_A, 1).is_err() {
        return false;
    }
    if civis::map_physical(civis::MAP_TEST_PHYS, helpers::VA_B, 1).is_err() {
        return false;
    }
    let pa = helpers::VA_A as *mut u8;
    let pb = helpers::VA_B as *const u8;
    for i in 0..64usize {
        unsafe {
            core::ptr::write_volatile(pa.add(i), (i * 7) as u8);
        }
    }
    for i in 0..64usize {
        let v = unsafe { core::ptr::read_volatile(pb.add(i)) };
        if v != (i * 7) as u8 {
            return false;
        }
    }
    true
}

