use super::*;

// ── Main ───────────────────────────────────────────────────────────

/// Nome del driver dietro il canale `chan` dal suo `image_hash` (Fase 36,
/// identita' misurata): confronto col manifest generato a build-time; `"?"`
/// se il canale e' morto o l'hash e' ignoto (test/helper, cardo stesso:
/// `HASH_CARDO` non esiste — cardo incorpora il manifest e il suo hash
/// sarebbe un ciclo). Solo diagnostica nei log, mai decisioni (la policy
/// confronta gli hash, non i nomi).
fn driver_name_of(chan: u64) -> &'static str {
    match civis::peer_info(chan) {
        Ok(h) if h == HASH_GPU => "gpu",
        Ok(h) if h == HASH_VELA => "vela",
        Ok(h) if h == HASH_BLOCK => "block",
        Ok(h) if h == HASH_KBD => "kbd",
        Ok(h) if h == HASH_VESTIGIA => "vestigia",
        Ok(h) if h == HASH_USERSHELL => "usershell",
        Ok(h) if h == HASH_PORTA => "porta",
        Ok(h) if h == HASH_USERUPTIME => "useruptime",
        _ => "?",
    }
}

/// Tabella seed dei bucket oggetti (Fase 55 N0 + D1): path FAT →
/// (bucket, chiave). Solo i binari che init carica per object_id (stessi
/// byte iniettati su /fat via `inject-bins.sh`: l'hash manifest li copre in
/// entrambi i path). Bucket `vela` = driver (toccano HW), `sys` = servizi.
const SYS_SEED: &[(&str, &[u8], &[u8])] = &[
    ("/fat/bin/gpu.bin", b"vela", b"bin/gpu.bin"),
    ("/fat/bin/uptime.bin", b"sys", b"bin/uptime.bin"),
    ("/fat/bin/vela.bin", b"sys", b"bin/vela.bin"),
    ("/fat/bin/kbd.bin", b"vela", b"bin/kbd.bin"),
    ("/fat/bin/porta.bin", b"sys", b"bin/porta.bin"),
    ("/fat/bin/posix.bin", b"sys", b"bin/posix/posix.bin"),
    ("/fat/bin/shell.bin", b"sys", b"bin/posix/shell.bin"),
    ("/fat/bin/time.bin", b"vela", b"bin/time.bin"),
    // Suite di test (D2, bucket `tst`): init le carica per object_id.
    ("/fat/test/testfs.bin", b"tst", b"test/testfs.bin"),
    ("/fat/test/testfat.bin", b"tst", b"test/testfat.bin"),
    ("/fat/test/testarca.bin", b"tst", b"test/testarca.bin"),
    ("/fat/test/posixtst.bin", b"tst", b"test/posixtst.bin"),
    ("/fat/test/tests.bin", b"tst", b"test/tests.bin"),
    ("/fat/test/bench.bin", b"tst", b"test/bench.bin"),
];

/// Bound seed: i servizi da FAT non superano 256 KiB (S2.5d: disaccoppiato
/// da `SPAWN_IMAGE_MAX` — i binari grandi arrivano via seed host su arca,
/// mai da FAT; cio' che passa da qui resta piccolo per scelta).
const SYS_SEED_MAX: usize = 262144;

/// Legge un file via mount interni (stesso resolve dei client, niente IPC).
/// Ritorna false a mount inattivo/file assente/troppo grande (il chiamante
/// logga e init ripiega su FAT — mai wedge il boot).
/// Chunk da 512 B (mai 4K stack: con LTO la catena seed→FAT si fonde e i 16
/// KiB non bastano — osservato #PF all'auto-bind; il seed e' una tantum).
/// `#[inline(never)]`: firewall di frame come gli handler disco.
#[inline(never)]
fn seed_read(
    mounts: &mut Vec<mount::FsMount>,
    fgen: &mut u64,
    path: &str,
    out: &mut Vec<u8>,
) -> bool {
    let (id, rel) = match mount::resolve_fsmount(mounts, path, fgen) {
        Some(v) => v,
        None => return false,
    };
    let mi = match mount::by_id(mounts, id) {
        Some(i) => i,
        None => return false,
    };
    let h = {
        let m = match mounts.get_mut(mi) {
            Some(m) => m,
            None => return false,
        };
        let fs = match m.local_dyn() {
            Some(f) => f,
            None => return false,
        };
        match fs.open_dyn(rel, 0) {
            Ok(h) => h,
            Err(_) => return false,
        }
    };
    let mut off = 0usize;
    loop {
        if off >= SYS_SEED_MAX {
            return false;
        }
        let mut chunk = [0u8; 512];
        let n = {
            let m = match mounts.get_mut(mi) {
                Some(m) => m,
                None => return false,
            };
            let fs = match m.local_dyn() {
                Some(f) => f,
                None => return false,
            };
            match fs.read_dyn(h, off, &mut chunk) {
                Ok(n) => n,
                Err(_) => return false,
            }
        };
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        off += n;
    }
    !out.is_empty()
}

/// Popola i bucket oggetti del motore disco (N0 su blocchi + D1): seed SOLO
/// delle chiavi assenti (idempotente: all'avvio su volume caldo non riscrive
/// versioni, al bind su volume fresco bootstrappa da /fat). Chiamato al bind
/// root e al bind USEDISK. D1 (niente fallback): a seed mancato init fa panic
/// loud a boot — il log qui resta la diagnosi. Commit singolo se almeno un
/// file seedato.
/// `#[inline(never)]`: firewall anti-inlining (con LTO la catena seed→btree
/// si fonde nel chiamante e sfonda i 16 KiB — osservato #PF all'auto-bind).
#[inline(never)]
fn seed_sys_disk(
    disk: &mut btree_drv::DiskEngine,
    mounts: &mut Vec<mount::FsMount>,
    fgen: &mut u64,
) {
    let mut seeded = false;
    for (path, bucket, key) in SYS_SEED {
        if disk.stat(bucket, key).is_some() {
            continue; // gia' sul volume: niente versioni duplicate
        }
        let mut data = Vec::new();
        if !seed_read(mounts, fgen, path, &mut data) {
            println!("[cardo] seed: {} non seedato (D1: init panic loud)", path);
            continue;
        }
        match btree_drv::seed_put(disk, bucket, key, &data) {
            Some(n) => {
                println!("[cardo] seed: {}B seeded", n);
                seeded = true;
            }
            None => println!("[cardo] seed: {} oltre bound (D1: init panic loud)", path),
        }
    }
    if seeded && !btree_drv::commit(disk) {
        println!("[cardo] seed: commit seed fallito (D1: init panic loud)");
    }
}

civis::entry!(real_main);
#[inline(never)]
fn real_main(_sp: u64) -> ! {
    println!("[cardo] starting");

    // Registra il servizio Fs SUBITO (ADR-0008): il mount FAT32 e' lento, e i
    // client (vela, testfs) risolvono Fs per nome appena partono. Registrarsi
    // prima del mount evita che chi spawa dopo aspetti inutilmente.
    // (L'ACK READY a init parte invece DOPO il populate, prima del loop:
    // READY significa "davvero pronto".)
    let reg_ok = civis::service_register(civis::Service::Cardo).is_ok();
    if reg_ok {
        println!("[cardo] registered as service Fs");
    } else {
        println!("[cardo] FAILED to register service Fs");
    }

    // Mount FAT32 dalle spec statiche (Fase 16b: stesso codice dei mount
    // dinamici; IpcDisk riconnette da solo a ogni restart di block, quindi
    // i mount sopravvivono alla morte del driver — t32). Spec inattive
    // (disco assente) restano in tabella e ritentano lazy al primo accesso.
    let mut fat_mounts: Vec<mount::FsMount> = Vec::new();
    // Identita' stabili dei mount (Fase 49, F2): monotoniche, mai riusate.
    let mut next_mount_id: u64 = 1;
    for (src, tgt) in mount::STATIC_MOUNTS {
        if mount::apply_mount_spec(&mut fat_mounts, src, tgt, "", &mut next_mount_id) {
            println!("[cardo] FAT32 montato a /{} (via block)", tgt);
        } else {
            println!("[cardo] mount {} -> {} inattivo (disco assente?)", src, tgt);
        }
    }
    if fat_mounts.iter().all(|m| !m.is_active()) {
        println!("[cardo] nessun FAT attivo: ramfs only");
    }

    // Fase 2 (root su volume): parse `root=UUID=<8hex>` dalla cmdline PVH,
    // scansiona i dischi per trovare il volume con quell'uuid, monta su `""`
    // (radice). Ramfs solo su `"tmp"`. Se l'uuid non esiste o il parametro e'
    // assente → panic loud (serial+VGA tramite println!).
    let mut root_uuid: Option<u64> = None;
    {
        let cmdline = civis::boot_cmdline();
        if cmdline.is_empty() {
            println!("[cardo] ERRORE: cmdline vuota — root=UUID mancante");
            unsafe { core::arch::asm!("ud2"); }
        }
        let cs = core::str::from_utf8(&cmdline).unwrap_or("");
        if let Some(idx) = cs.find("root=UUID=") {
            let tail = &cs[idx + 10..];
            let hex = if let Some(eq) = tail.find(' ') { &tail[..eq] } else { tail };
            if hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                match u64::from_str_radix(hex, 16) {
                    Ok(u) => root_uuid = Some(u),
                    Err(_) => {
                        println!("[cardo] ERRORE: uuid invalido '{}'", hex);
                        unsafe { core::arch::asm!("ud2"); }
                    }
                }
            } else {
                println!("[cardo] ERRORE: uuid '{}' non 16 esadec", hex);
                unsafe { core::arch::asm!("ud2"); }
            }
        } else {
            println!("[cardo] ERRORE: root=UUID non trovato nella cmdline '{}'", cs);
            unsafe { core::arch::asm!("ud2"); }
        }
    }
    // Scansiona i dischi per trovare il volume con l'uuid richiesto.
    let mut root_store: Option<btree_drv::VolumeStore> = None;
    if let Some(uuid) = root_uuid {
        // Itera sda..sdh + partizioni sdXn (stesso pattern di scan_and_open).
        for disk in 0..8u8 {
            let letter = b'a' + disk;
            let mut path = [0u8; 16];
            path[..5].copy_from_slice(b"/dev/");
            path[5..8].copy_from_slice(&[b's', b'd', letter]);
            // Whole-disk: /dev/sdX
            path[..5].copy_from_slice(b"/dev/");
            path[5..8].copy_from_slice(&[b's', b'd', letter]);
            if let Some(p) = core::str::from_utf8(&path[..8]).ok() {
                println!("[cardo] scan whole-disk {}...", p);
                if let Some(handle) = mount::resolve_mount_source(p) {
                    println!("[cardo] scan whole-disk {} -> handle={}", p, handle);
                    if let Some(v) = btree_drv::open_with_uuid(handle, uuid) {
                        println!("[cardo] root: volume ArcaFS trovato uuid={:016X} handle={} path={}", uuid, handle, p);
                        root_store = Some(v);
                        break;
                    }
                } else {
                    println!("[cardo] scan whole-disk {} -> RESOLVE FAILED", p);
                }
            }
            // Partizioni: /dev/sdXn
            if root_store.is_none() {
                for part in 1..=4u8 {
                    path[..5].copy_from_slice(b"/dev/");
                    path[5..9].copy_from_slice(&[b's', b'd', letter, b'0' + part]);
                    if let Some(p) = core::str::from_utf8(&path[..9]).ok() {
                        println!("[cardo] scan partition {}...", p);
                        if let Some(handle) = mount::resolve_mount_source(p) {
                            println!("[cardo] scan partition {} -> handle={}", p, handle);
                            if let Some(v) = btree_drv::open_with_uuid(handle, uuid) {
                                println!("[cardo] root: volume ArcaFS trovato uuid={:016X} handle={} path={}", uuid, handle, p);
                                root_store = Some(v);
                                break;
                            }
                        } else {
                            println!("[cardo] scan partition {} -> RESOLVE FAILED", p);
                        }
                    }
                }
            }
            if root_store.is_some() {
                break;
            }
        }
    }
    if root_store.is_some() {
        println!("[cardo] ArcaFS montato come root (uuid={:016X})", root_uuid.unwrap_or(0));
    } else {
        println!("[cardo] ERRORE: volume UUID={:016X} non trovato", root_uuid.unwrap_or(0));
        unsafe { core::arch::asm!("ud2"); }
    }
    // Ramfs solo su `/tmp` (Fase 2: volatile, fresh a ogni boot).
    mount::apply_mount_spec(&mut fat_mounts, "ramfs", "tmp", "", &mut next_mount_id);
    println!("[cardo] ramfs montata su /tmp");

    let mut fs = ramfs::RamFs::new();
    let mut ftable = ftable::FileTable::new();
    let mut mounts: Vec<mount_legacy::Mount> = Vec::new();
    // Generazione delle cache FileInfo per-fd (vedi `FileEntry`): bumpata a
    // ogni mutazione FAT (write/create/mount/umount/remount/drop d'epoca).
    // Parte da 1 (0 = mai usato, come le entry appena create per ramfs).
    let mut fat_gen: u64 = 1;

    // Volume on-disk (scaffold RAW + motore): legato da R_ARCA_DEBUG/OPEN
    // come `VolumeStore` (un solo handle: USEDISK lo muove nel motore, mai
    // duplicato). Vive finche' cardo vive (al restart si rilega dal disco).
    let mut dbgvol: Option<btree_drv::VolumeStore> = None;

    // Motore B+tree su disco (backend UNICO): legato all'AVVIO se un volume
    // ArcaFS esiste (56.2c sys-dal-volume), altrimenti lazy da USEDISK(1)
    // (ne consuma l'handle: un solo proprietario, mai due volumi sullo
    // stesso disco). Il seed `sys` corre al bind solo per le chiavi assenti.
    // La tabella snapshot si ricarica dal blocco meta (56.2c). Senza volume
    // gli op nativi danno errore loud e init ripiega su FAT (dual-mode).
    let mut disk: Option<btree_drv::DiskEngine> = None;

    // Fase A (root su volume): il motore si lega al volume root trovato per
    // UUID sopra (un solo proprietario: niente doppia open, niente primo-
    // trovato che diverge dalla root). PRIMA del READY: init spawna
    // console/shell per object_id solo dopo l'ACK e li deve trovare gia'
    // serviti (N0 end-to-end nel gate). Bind fallito = panic loud (la root
    // e' obbligatoria: run.sh attacca sempre arca.img come terzo drive).
    let root_uuid_v = root_uuid.unwrap_or(0);
    let root_gen = root_store.as_ref().and_then(|s| s.generation()).unwrap_or(0);
    dbgvol = root_store;
    if btree_drv::set_backend(&mut dbgvol, &mut disk).is_none() {
        println!("[cardo] ERRORE: bind motore sul volume root fallito");
        unsafe { core::arch::asm!("ud2"); }
    }
    if let Some(d) = disk.as_mut() {
        seed_sys_disk(d, &mut fat_mounts, &mut fat_gen);
    }
    // Mount root: il volume ArcaFS su `""` (radice, catch-all). Ramfs resta
    // solo su `/tmp` (sopra); `/fat` resta montato per dati (niente fallback).
    let root_id = next_mount_id;
    next_mount_id = next_mount_id.wrapping_add(1);
    fat_mounts.push(mount::FsMount::root_arca(root_gen, root_uuid_v, root_id));
    fat_gen = fat_gen.wrapping_add(1);
    println!("[cardo] root ArcaFS montata su / (uuid={:016X})", root_uuid_v);

    // Client registrati: pid → (req_ring_phys, resp_ring_phys).
    let mut rings: BTreeMap<u64, (u64, u64)> = BTreeMap::new();

    // Diritti per-canale (Fase 17): entry assente = {ALL, root}.
    let mut rights: BTreeMap<u64, rights::ChanRights> = BTreeMap::new();
    // Tetto policy su identita' (Fase 45): chan → mask massima, classificato
    // UNA volta all'handshake (peer_info), purgato all'EXIT_NOTIFY come
    // rings/rights. Effettivo = drop_mask & ceiling (mai widen).
    let mut policy: BTreeMap<u64, u32> = BTreeMap::new();

    // Grant single-use per handoff fd (Fase 40, modello B): nonce → snapshot.
    let mut grants = dup::GrantTable::new();

    // Pipe buffer server-side (Fase 42): id → buffer condiviso tra gli fd
    // delle due estremita' (conteggio estremita', libera all'ultima close).
    let mut pipes = pipes::PipeTable::new();

    // Connessione topologia verso block (Fase 51, P2): serve il relay
    // R_DISK_LIST/INFO (lookup+HELLO lazy al primo uso, riconnessione
    // automatica a morte driver come gli IpcDisk dei mount).
    let topo_disk = ipc_disk::IpcDisk::new(0);

    // Aspettative di durabilita' per-canale (Fase 52, P3): modo SYNC_* da
    // R_SYNC (default SYNC_NONE = nessuna pretesa registrata). Effimere
    // come rights/policy: col peer muore la riga (purge su EXIT_NOTIFY).
    let mut sync_expect: BTreeMap<u64, u32> = BTreeMap::new();

    // Fase A: niente piu' populate della ramfs radice — `/` e' il volume
    // ArcaFS (hello.txt/test.txt seedati a build-time, Fase C) e ramfs vive
    // solo su `/tmp`. `fs` resta per le firme handler (rami Ram difensivi).
    println!("[cardo] root ArcaFS attiva, entro in loop");

    // Notifica a init (canale di nascita) che il servizio Fs e' pronto: init
    // aspetta questo ACK prima di spawnare chi usa il filesystem (boot
    // deterministico, ADR-0008). DOPO il populate: READY significa "davvero
    // pronto" (vale anche per i restart: init procede a filesystem completo).
    // READY fire-and-forget (send_async, init-restart): init consuma senza
    // reply → una send sync resterebbe bloccata. Retry bounded, mai hang.
    civis::signal_ready(reg_ok as u64);

    // 24.2-diagnosi: contatore rimosso (era temporaneo); heap_stats resta in
    // civis per future diagnosi.
    loop {
        let msg = match civis::recv() {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Scratch arena per-op (civis): TUTTI i borrow sotto muoiono entro
        // questa iterazione (handler sincroni, reply prima del prossimo
        // recv). Mai tenere `&` scratch oltre il fondo del loop.
        civis::scratch::reset();

        // Il client e' identificato dal canale da cui arriva la richiesta
        // (ADR-0008): ogni client ha il proprio canale verso Fs. La reply e'
        // implicita al messaggio corrente: se il client era bloccato in `send`
        // (sync) il kernel la consegna nel reply_slot; se era async (Fase 13)
        // il kernel accoda la risposta con req_id negativo. cardo non cambia.
        let chan = msg.channel;
        let tag = msg.tag;

        // Handshake ring buffer: il client registra i propri indirizzi fisici.
        if tag == FS_BUF_REG {
            rings.insert(chan, (msg.w0, msg.w1));
            // Fase 45: classifica il peer ORA (tetto in cache, niente
            // syscall per-op). L'handshake resta aperto a tutti: la policy
            // nega op, mai la registrazione (civis ritenta comunque).
            let ceil = policy::ceiling_for(chan);
            policy.insert(chan, ceil);
            println!("[cardo] client chan {} registered rings req={:#x} resp={:#x} ceiling={:#x}", chan, msg.w0, msg.w1, ceil);
            let _ = civis::reply(0, 0, 0);
            continue;
        }

        // Registrazione driver: un processo notifica il proprio prefix di mount.
        // Il prefix viaggia nel request ring del chiamante.
        if tag == FS_REGISTER {
            let (req_phys, _resp_phys) = match rings.get(&chan) {
                Some(&r) => r,
                // Senza handshake (server riavviato): il driver rifa FS_BUF_REG
                // e ripete (civis, come sopra).
                None => { let _ = civis::reply(0, ERR_NOHANDSHAKE, 0); continue; }
            };
            // Mappa il request ring del driver per leggere il prefix
            if civis::map_physical(req_phys, rings::REQ_RING_VA, 1).is_err() {
                let _ = civis::reply(0, ERR, 0);
                continue;
            }
            let (op_tag, _w0, _w1, payload_len) = match rings::req_ring_read() {
                Some(f) => f,
                None => { let _ = civis::reply(0, ERR, 0); continue; }
            };
            if op_tag == R_REGISTER && payload_len > 0 && payload_len <= 514 {
                let mut prefix_buf = [0u8; 514];
                rings::req_ring_read_payload(&mut prefix_buf, payload_len);
                // Fase 35 (hardening) + Fase 36 (identita' misurata, Strato 2):
                // policy sui mount registrati dinamicamente.
                // (a) solo sotto `/dev/` (niente hijack di `/` o voci rogue a
                // root); (b) il replace di un prefix esistente solo dallo
                // STESSO binario (stesso `image_hash` del driver vivo: il
                // restart da disco rilegge gli stessi byte, quindi riesce
                // senza init) o da un figlio di init (bootstrap): impedisce a
                // un processo qualsiasi di squattare `/dev/null` dopo un kill.
                let caller = civis::peer_pid(chan).unwrap_or(-1);
                let init_child = caller >= 0
                    && matches!(civis::ps_info(caller as u32), Some(e) if e.parent == Some(1));
                // Payload = uno o piu' prefix NUL-separati (Fase 16d): vela
                // registra "/dev/null\0/dev/zero" con UNA sola IPC, cosi' non
                // esiste una finestra in cui un mount e' forwardable mentre il
                // driver e' ancora bloccato in un secondo register sincrono.
                for raw in prefix_buf[..payload_len].split(|&b| b == 0) {
                    if raw.is_empty() {
                        continue;
                    }
                    let prefix = match core::str::from_utf8(raw) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    if !prefix.starts_with("/dev/") {
                        println!("[cardo] FS_REGISTER rifiutato: '{}' fuori /dev/", prefix);
                        continue;
                    }
                    // Idempotente sul prefix (init-restart): se il prefix era
                    // gia' registrato (driver morto non ancora purgato o double
                    // register), sostituisci invece di duplicare — lo stale
                    // avvelenerebbe resolve_mount (first-match). Fase 35: il
                    // replace di un prefix di un driver VIVO richiede un figlio
                    // di init; un driver morto (canale invalidato) puo' sempre
                    // essere rimpiazzato (riconnessione legittima). Fase 36: il
                    // replace riesce anche dallo STESSO binario (hash uguale a
                    // quello del driver vivo — restart da disco senza init).
                    let existing = mounts.iter().find(|m| m.prefix.as_str() == prefix).map(|m| m.driver_chan);
                    let stale = match existing {
                        Some(dc) => civis::peer_pid(dc).is_err(), // driver morto
                        None => true,
                    };
                    // Stesso binario? Solo a driver vivo e non-init-child (nei
                    // casi facili la risposta e' gia' nota: niente syscall).
                    let same_image = match existing {
                        Some(dc) if !stale && !init_child => {
                            match (civis::peer_info(chan), civis::peer_info(dc)) {
                                (Ok(a), Ok(b)) => a == b,
                                _ => false,
                            }
                        }
                        _ => false,
                    };
                    if existing.is_some() && !init_child && !stale && !same_image {
                        println!(
                            "[cardo] FS_REGISTER replace '{}' rifiutato (pid {} {}, driver vivo {})",
                            prefix, caller, driver_name_of(chan), driver_name_of(existing.unwrap()),
                        );
                        continue;
                    }
                    mounts.retain(|m| m.prefix.as_str() != prefix);
                    mounts.push(mount_legacy::Mount {
                        prefix: String::from(prefix),
                        driver_chan: chan,
                    });
                    println!("[cardo] registered mount '{}' → driver_chan={} ({})", prefix, chan, driver_name_of(chan));
                }
            } else {
                rings::req_ring_consume(20 + payload_len);
            }
            // Mappa il response ring del client per scrivere la risposta
            rings::map_client_resp_ring(&rings, chan);
            rings::resp_ring_write(0, 0, &[]);
            let _ = civis::reply(0, 0, 0);
            continue;
        }

        // Morte di un peer (client o driver): purga tutto lo stato per-canale
        // (notifica unificata, Fase 14). Senza reply: il peer e' morto.
        if tag == civis::EXIT_NOTIFY {
            let mut remotes = Vec::new();
            let mut pipe_ends = Vec::new();
            ftable.purge(chan, &mut remotes, &mut pipe_ends);
            for (srv, rfd) in remotes {
                // Best-effort: il driver potrebbe essere morto a sua volta.
                let _ = civis::send(srv, DEV_CLOSE, rfd as u64, 0);
            }
            // Estremita' pipe del morto: decrementa (l'ultima libera il
            // buffer; i peer vivi vedono EOF/Closed invece di un hang).
            for (pipe_id, write) in pipe_ends {
                pipes.end_closed(pipe_id, write);
            }
            rings.remove(&chan);
            // Baseline wall-clock (Fase 50): se il morto era il server Time,
            // cade (al prossimo bisogno si rilegge, mai wall stale).
            wall::note_peer_death(chan);
            // Aspettative di durabilita' (Fase 52): effimere come rights —
            // al re-handshake il peer ridichiara (default SYNC_NONE).
            sync_expect.remove(&chan);
            // Diritti effimeri (Fase 17): col peer muore anche la sua riga —
            // al re-handshake riparte da default {ALL, root} (limite dichiarato).
            rights.remove(&chan);
            // Tetto policy (Fase 45): stessa vita — al re-handshake il peer
            // viene riclassificato (hash rimisurato allo spawn, mai stale).
            policy.remove(&chan);
            // Grant orfani del morto (Fase 40): un pid riusato non deve poter
            // riscuotere grant altrui (la doppia attestazione al claim chiude
            // comunque la race, ma senza residui non c'e' race).
            grants.purge_registrant(chan, &mut pipes);
            // Se il morto era un driver, i suoi mount tornano registrabili:
            // lo stale, primo in lista, avvelenerebbe resolve_mount anche
            // dopo una re-registrazione dello stesso prefix.
            mounts.retain(|m| m.driver_chan != chan);
            // Se il morto era block, invalida i client disco di tutti i
            // mount (Fase 16c: drop d'epoca — il prossimo accesso re-risolve
            // per nome e rimonta, t32). Veloce: solo compare dentro IpcDisk.
            // Le istanze cambiano: bumpa la generazione delle cache FileInfo.
            let mut epoch_dropped = false;
            for m in fat_mounts.iter_mut() {
                if m.note_peer_death(chan) {
                    epoch_dropped = true;
                }
            }
            if epoch_dropped {
                fat_gen = fat_gen.wrapping_add(1);
            }
            continue;
        }

        // Ogni altra operazione deve essere un FS_NOTIFY.
        if tag != FS_NOTIFY {
            // Tag ignoto: ERR come prima (nessun cambio semantico).
            let _ = civis::reply(0, ERR, 0);
            continue;
        }
        // Client senza handshake ring (es. server riavviato dopo la sua
        // registrazione, t28): segnale dedicato cosi' il client rifa
        // FS_BUF_REG e ripete l'op UNA volta (civis, Fase 14).
        if !rings.contains_key(&chan) {
            let _ = civis::reply(0, ERR_NOHANDSHAKE, 0);
            continue;
        }
        // Mappa i ring del client nello spazio di cardo.
        if !rings::map_client_req_ring(&rings, chan) || !rings::map_client_resp_ring(&rings, chan) {
            let _ = civis::reply(0, ERR, 0);
            continue;
        }

        // Leggi l'header del request frame (senza consumare: la lunghezza vera
        // e' dichiarata in w0/w1, vedi sotto).
        let (op_tag, w0, w1, avail) = match rings::req_ring_read() {
            Some((t, a, b, avail)) => (t, a, b, avail),
            None => {
                // Ring vuoto a notifica arrivata: spuria/stale, niente da
                // consumare e niente da riallineare (tail==head gia').
                let _ = civis::reply(0, ERR, 0);
                continue;
            }
        };

        // Lunghezza payload dichiarata dal frame. Il formato frame non ha
        // lunghezza esplicita: consumare "tutto il disponibile" inghiotte gli
        // eventuali frame successivi gia' presenti (coalescenza), disallineando
        // il ring per sempre (osservato: tag=0x0 con pay enorme, RINGFULL lato
        // client e stallo senza recovery). Si consuma ESATTAMENTE il dichiarato;
        // il resto resta per la propria notifica.
        let expect: usize = match op_tag {
            R_OPEN | R_MKDIR | R_READDIR | R_REGISTER | R_MOUNT | R_UMOUNT | R_DELETE | R_STAT => {
                w0 as usize
            }
            // RENAME: payload "old\0new\0", w0 = lunghezza totale.
            R_RENAME => w0 as usize,
            // SYMLINK: payload "link\0target\0", w0 = len. READLINK:
            // payload path, w0 = len (reply a frame dedicato). CHMOD:
            // payload path (w0 = len), mode in w1.
            R_SYMLINK | R_READLINK | R_CHMOD => w0 as usize,
            R_WRITE | R_RIGHTS_DROP => w1 as usize,
            // LSEEK: payload 1 byte = whence (fd in w0, offset in w1).
            R_LSEEK => 1,
            // CLAIM/CANCEL: payload `[nonce:8]`; GRANT solo registri (w0 = fd).
            R_DUP_CLAIM | R_DUP_CANCEL => 8,
            // PIPE_CREATE: nessun payload (w0 = hint capacita').
            R_PIPE_CREATE => 0,
            // Topologia dischi (Fase 51): LIST nessun payload, INFO w0 =
            // indice disco nei registri (mai payload).
            R_DISK_LIST | R_DISK_INFO => 0,
            // Durabilita'/sensori (Fase 52): SYNC w0 = modo nei registri
            // (mai payload), STATVFS path in w0 come R_STAT.
            R_SYNC => 0,
            R_STATVFS => w0 as usize,
            // R_GET_HASH: path in w0 (come STAT).
            R_GET_HASH => w0 as usize,
            // R_OBJ_PUT: payload=bucket\0key\0[data], size in w0.
            R_OBJ_PUT => w0 as usize,
            // R_OBJ_GET: payload=bucket\0key\0, payload_len in w0, offset in w1.
            R_OBJ_GET => w0 as usize,
            // Debug volume (Fase 56.2a): UN tag, sub-op nel payload;
            // la lunghezza e' nei formati dei sub (single source in `arcafs`).
            R_ARCA_DEBUG => w0 as usize,
            // Versioni + snapshot (Fase 56.1): lunghezze payload nei formati
            // dei tag (single source nei commenti di `syscall-numbers`).
            R_SNAP_CREATE | R_SNAP_ROLLBACK | R_SNAP_CLONE | R_OBJ_DELETE | R_OBJ_STAT => w0 as usize,
            R_SNAP_DELETE | R_OBJ_GET_ID | R_OBJ_STAT_ID => 8,
            R_READ | R_CLOSE | R_RIGHTS_GET | R_DUP_GRANT => 0,
            _ => {
                // Tag impossibile: scarta tutto e riallinea (vedi req_resync).
                rings::req_resync();
                let _ = civis::reply(0, ERR, 0);
                continue;
            }
        };
        if expect > 4096 || avail < expect {
            // Frame impossibile o incompleto: riallinea e fallisci
            // visibilmente (mai wedge). Il payload perso appartiene a un'epoca
            // disallineata; il client ritenta (tty) o vede -1 (sync).
            rings::req_resync();
            let _ = civis::reply(0, ERR, 0);
            continue;
        }

        // Diritti per-canale, check ops (Fase 17 + tetto Fase 45): CENTRALE,
        // prima di qualunque contatto handler/driver. A diniego il frame va
        // comunque consumato (20 + expect esatti) o il prossimo request del
        // client legge spazzatura — vale anche per il WRITE remoto negato
        // (mai map_in/send al driver in quel caso). Fallback fail-closed a
        // canale senza handshake-policy (non dovrebbe accadere: FS_NOTIFY
        // richiede rings, che richiede handshake).
        // Tetto policy in cache (Fase 45): niente syscall qui dentro.
        let ceiling = policy.get(&chan).copied().unwrap_or(policy::DEFAULT_UNKNOWN_OPS);
        if let Some(bit) = rights::op_bit(op_tag) {
            if rights::rights_ops(&rights, chan) & ceiling & bit == 0 {
                rings::req_ring_consume(20 + expect);
                rings::resp_ring_write(ERR, 0, &[]);
                let _ = civis::reply(0, ERR, 0);
                continue;
            }
        }

        // R_WRITE verso un device remoto: NON consumare il request frame. Il
        // payload resta nel request ring del client e il driver (console/vela),
        // che ha i ring del client iniettati via map_in, lo legge direttamente e
        // avanza la tail di (20 + w1) esatti. Qui scriviamo solo il result frame.
        if op_tag == R_WRITE && ftable.get_remote(chan, w0 as u32).is_some() {
            let result = handlers::handle_write_remote(&ftable, &rings, chan, w0 as u32, w1 as usize);
            rings::resp_ring_write(mount::to_reply_res(result), 0, &[]);
            let _ = civis::reply(0, mount::to_reply_res(result), 0);
            continue;
        }

        // Percorsi locali (o remote non-WRITE): consuma ESATTAMENTE header +
        // payload dichiarato. req_ring_read_payload avanza la tail di
        // (20 + expect); eventuali byte successivi (coalescenza) restano per
        // la loro notifica invece di essere inghiottiti.
        // Il payload vive in scratch (mai heap: e' il temp per-op piu' grosso,
        // fino a 4096 B per chunk di write; consumato entro l'iterazione).
        // `expect` ≤ 4096 per il bound sopra: sta nel backing iniziale.
        let payload: &mut [u8] = match civis::scratch::alloc_bytes(expect) {
            Some(p) => p,
            None => {
                // OOM vera sullo scratch: come frame impossibile (mai wedge).
                rings::req_resync();
                let _ = civis::reply(0, ERR, 0);
                continue;
            }
        };
        rings::req_ring_read_payload(payload, expect);

        // Diritti per-canale, check subtree (Fase 17): solo le op con path.
        // Gli fd restano capability pure (read/write/close non ricontrollano
        // il path aperto). UTF-8 invalido o spec malformata: passa oltre, lo
        // rifiuta l'handler (i diritti non decidono la validita').
        let subtree_ok = match op_tag {
            R_OPEN | R_MKDIR | R_READDIR | R_DELETE | R_STAT | R_STATVFS | R_GET_HASH => match core::str::from_utf8(payload) {
                Ok(p) => rights::within_subtree(rights::rights_subtree(&rights, chan), rights::normalize_sub_view(p)),
                Err(_) => true,
            },
            // RENAME: ENTRAMBI i capi nel subtree (scrivere fuori dal
            // proprio confine via rename del dst e' lo stesso bypass di
            // una write: check su old E new, mai uno solo).
            R_RENAME => match core::str::from_utf8(payload) {
                Ok(p) => match p.split_once('\0') {
                    Some((old, new)) => {
                        let sub = rights::rights_subtree(&rights, chan);
                        rights::within_subtree(sub, rights::normalize_sub_view(old))
                            && rights::within_subtree(sub, rights::normalize_sub_view(new.trim_end_matches('\0')))
                    }
                    None => true,
                },
                Err(_) => true,
            },
            // SYMLINK: check sul LINK (il target e' opaco, mai confine).
            // READLINK/CHMOD: check sul path (leggere/scrivere meta fuori
            // subtree = bypass).
            R_SYMLINK | R_READLINK | R_CHMOD => match core::str::from_utf8(payload) {
                Ok(p) => {
                    let link = p.split_once('\0').map(|(l, _)| l).unwrap_or(p);
                    rights::within_subtree(
                        rights::rights_subtree(&rights, chan),
                        rights::normalize_sub_view(link.trim_end_matches('\0')),
                    )
                }
                Err(_) => true,
            },
            R_MOUNT => match core::str::from_utf8(payload) {
                Ok(spec) => match spec.split_once('\0') {
                    Some((_, target)) => rights::within_subtree(
                        rights::rights_subtree(&rights, chan),
                        rights::normalize_sub_view(target.trim_end_matches('\0')),
                    ),
                    None => true,
                },
                Err(_) => true,
            },
            R_UMOUNT => match core::str::from_utf8(payload) {
                Ok(t) => rights::within_subtree(rights::rights_subtree(&rights, chan), rights::normalize_sub_view(t)),
                Err(_) => true,
            },
            _ => true,
        };
        if !subtree_ok {
            rings::resp_ring_write(ERR, 0, &[]);
            let _ = civis::reply(0, ERR, 0);
            continue;
        }

        // Dispatch in base all'op_tag del ring. Ogni handler riceve gia' il
        // payload estratto: il frame e' stato interamente consumato sopra.
        // Errori tipizzati (Fase 40): gli handler ritornano Result<u64, u64>
        // (Ok = valore, Err = sentinella ERR_*); `to_reply_res` la riversa
        // nel reply IPC e nel response frame.
        let result: Result<u64, u64> = match op_tag {
            R_OPEN => {
                match core::str::from_utf8(&payload) {
                    // w1 del frame R_OPEN = flags (OPEN_CREATE, w0 = len path):
                    // il server li ignorava (creava sempre) — ora POSIX.
                    Ok(path) => handlers::handle_open(&mut fs, &mut ftable, &mut fat_mounts, &mounts, chan, w1, path, &mut fat_gen, &mut disk),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_READ => {
                handlers::handle_read(&mut fs, &mut ftable, &mut pipes, &mut fat_mounts, &rings, chan, w0 as u32, w1 as usize, &mut fat_gen, &mut disk)
            }

            R_WRITE => {
                handlers::handle_write_local(&mut fs, &mut ftable, &mut pipes, &mut fat_mounts, chan, w0 as u32, w1 as usize, &payload, &mut fat_gen, &mut disk)
            }

            R_CLOSE => {
                handlers::handle_close(&mut ftable, &mut pipes, chan, w0 as u32)
            }

            R_READDIR => {
                match core::str::from_utf8(&payload) {
                    Ok("") | Ok("/") => handlers::handle_readdir(&mut fs, &mut fat_mounts, &mounts, &rings, chan, "/", &mut fat_gen, &mut disk),
                    Ok(path) => handlers::handle_readdir(&mut fs, &mut fat_mounts, &mounts, &rings, chan, path, &mut fat_gen, &mut disk),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_MKDIR => {
                match core::str::from_utf8(&payload) {
                    Ok(path) => handlers::handle_mkdir(&mut fs, &mut fat_mounts, path, &mut fat_gen, &mut disk),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_MOUNT => {
                match core::str::from_utf8(&payload) {
                    Ok(spec) => handlers::handle_mount(&mut fat_mounts, spec, &mut fat_gen, &mut next_mount_id),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_UMOUNT => {
                match core::str::from_utf8(&payload) {
                    Ok(target) => handlers::handle_umount(&mut fat_mounts, &ftable, target, &mut fat_gen),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_DELETE => {
                match core::str::from_utf8(&payload) {
                    Ok(path) => handlers::handle_delete(&mut fs, &mut fat_mounts, &mounts, path, &mut fat_gen, &mut disk),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_RENAME => {
                match core::str::from_utf8(&payload) {
                    Ok(p) => match p.split_once('\0') {
                        Some((old, new)) => handlers::handle_rename(
                            &mut fs, &mut fat_mounts, &mounts,
                            old, new.trim_end_matches('\0'), &mut fat_gen, &mut disk,
                        ),
                        None => Err(ERR_INVALID),
                    },
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_SYMLINK => {
                match core::str::from_utf8(&payload) {
                    Ok(p) => match p.split_once('\0') {
                        Some((link, target)) => handlers::handle_symlink(
                            &mut fs, &mut fat_mounts, &mounts,
                            link, target.trim_end_matches('\0'), &mut fat_gen, &mut disk,
                        ),
                        None => Err(ERR_INVALID),
                    },
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_CHMOD => {
                match core::str::from_utf8(&payload) {
                    Ok(path) => handlers::handle_chmod(
                        &mut fs, &mut fat_mounts, &mounts, path, w1 as u32, &mut fat_gen, &mut disk,
                    ),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_READLINK => {
                match core::str::from_utf8(&payload) {
                    Ok(path) => match handlers::handle_readlink(
                        &mut fs, &mut fat_mounts, &mounts, path, &mut fat_gen, &mut disk,
                    ) {
                        Ok(target) => {
                            rings::resp_ring_write(target.len() as u64, 0, target.as_bytes());
                            let _ = civis::reply(0, target.len() as u64, 0);
                            continue;
                        }
                        Err(e) => Err(e),
                    },
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_STAT => {
                match core::str::from_utf8(&payload) {
                    Ok("") | Ok("/") => handlers::handle_stat(&mut fs, &mut fat_mounts, &mounts, &rings, chan, "/", &mut fat_gen, &mut disk),
                    Ok(path) => handlers::handle_stat(&mut fs, &mut fat_mounts, &mounts, &rings, chan, path, &mut fat_gen, &mut disk),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_DISK_LIST => {
                handlers::handle_disk_list(&topo_disk, &rings, chan)
            }

            R_DISK_INFO => {
                handlers::handle_disk_info(&topo_disk, &rings, chan, w0 as u32)
            }

            R_SYNC => {
                handlers::handle_sync(&mut fat_mounts, &mut sync_expect, chan, w0 as u32, &mut disk)
            }

            R_GET_HASH => {
                match core::str::from_utf8(&payload) {
                    Ok("") | Ok("/") => Err(ERR_INVALID),
                    Ok(path) => handlers::handle_get_hash(&mut fs, &mut fat_mounts, &mounts, &rings, chan, path, &mut fat_gen, &mut disk),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_STATVFS => {
                match core::str::from_utf8(&payload) {
                    Ok("") | Ok("/") => handlers::handle_statvfs(&mut fs, &mut fat_mounts, &mounts, &rings, chan, "/", &mut fat_gen, &mut disk),
                    Ok(path) => handlers::handle_statvfs(&mut fs, &mut fat_mounts, &mounts, &rings, chan, path, &mut fat_gen, &mut disk),
                    Err(_) => Err(ERR_INVALID),
                }
            }

            R_OBJ_PUT => {
                // Payload: [bucket_len][bucket]\0[key_len][key]\0[data...]
                // w0 = payload_len (expect), w1 = offset nel blob.
                // Backend unico 56.2b (blocchi, commit per-op); senza bind
                // errore loud e il client ripiega (init → FAT).
                match disk.as_mut() {
                    Some(d) => btree_drv::disk_put(d, payload, w1 as usize),
                    None => Err(ERR),
                }
            }

            R_OBJ_GET => {
                // Payload: [bucket_len][bucket]\0[key_len][key]\0
                // w0 = payload_len, w1 = offset
                match disk.as_mut() {
                    Some(d) => btree_drv::disk_get(d, payload, w1 as usize, RING_MAX_PAYLOAD),
                    None => Err(ERR),
                }
            }

            R_ARCA_DEBUG => {
                // USEDISK (56.2b) payload `[7][1]`: lega il motore B+tree
                // (consuma `dbgvol`, lazy dopo lo scaffold RAW) e seedda
                // `sys` da /fat al primo bind. Gestito qui: muove stati che
                // `handle_arca_debug` non vede. `[7][0]` = no-op (il backend
                // e' unico: niente routing da commutare).
                if payload.first() == Some(&civis::ARCA_SUB_USEDISK) {
                    if payload.len() != 2 || payload[1] > 1 {
                        Err(ERR_INVALID)
                    } else if payload[1] == 0 {
                        Ok(0)
                    } else {
                        match btree_drv::set_backend(&mut dbgvol, &mut disk) {
                            Some(newly) => {
                                if newly {
                                    if let Some(d) = disk.as_mut() {
                                        seed_sys_disk(d, &mut fat_mounts, &mut fat_gen);
                                    }
                                }
                                Ok(0)
                            }
                            None => Err(ERR),
                        }
                    }
                } else if payload.first() == Some(&civis::ARCA_SUB_OPEN) {
                    // OPEN lega lo scaffold RAW come `VolumeStore` (stesso
                    // handle che USEDISK muove nel motore: mai duplicato).
                    // Dopo il bind il re-open e' rifiutato (secondo handle =
                    // freelist/xh divergenti, loud mai silenzioso).
                    if disk.is_some() {
                        Err(ERR)
                    } else {
                        match core::str::from_utf8(payload.get(1..).unwrap_or(&[])) {
                            Ok(path) => match btree_drv::raw_open(path) {
                                Some(store) => {
                                    dbgvol = Some(store);
                                    Ok(0)
                                }
                                None => Err(ERR),
                            },
                            Err(_) => Err(ERR_INVALID),
                        }
                    }
                } else if payload.first() == Some(&civis::ARCA_SUB_QUOTA_SET) {
                    // QUOTA_SET: solo motore (la tabella vive nel BTree, mai
                    // nel backend RAM: pre-bind = ERR loud, A3).
                    let rest = payload.get(1..).unwrap_or(&[]);
                    match disk.as_mut() {
                        Some(d) => btree_drv::disk_quota_set(d, rest),
                        None => Err(ERR),
                    }
                } else if payload.first() == Some(&civis::ARCA_SUB_QUOTA_GET) {
                    // QUOTA_GET: reply a due registri (quota, used) + frame
                    // dedicato, pattern STAT qui sotto.
                    let rest = payload.get(1..).unwrap_or(&[]);
                    let out = match disk.as_mut() {
                        Some(d) => btree_drv::disk_quota_get(d, rest).map(
                            |(q, u)| btree_drv::ArcaDebugOut::Quota(q, u),
                        ),
                        None => Err(ERR),
                    };
                    match out {
                        Ok(btree_drv::ArcaDebugOut::Quota(q, u)) => {
                            rings::resp_ring_write(q, u, &[]);
                            let _ = civis::reply(0, q, u);
                            continue;
                        }
                        _ => Err(ERR),
                    }
                } else {
                    // Sub-op nel payload (`arcafs::proto::ARCA_SUB_*`): scalari
                    // per via generica; READ/STAT scrivono frame dedicato qui
                    // (pattern R_PIPE_CREATE: reply a due registri + `continue`).
                    // Unico handler RAW pre/post bind (stesso store del motore
                    // dopo USEDISK: niente doppi handle, niente divergenze).
                    let out = match disk.as_mut() {
                        Some(d) => btree_drv::handle_raw_debug(&mut d.store, payload),
                        None => match dbgvol.as_mut() {
                            Some(s) => btree_drv::handle_raw_debug(s, payload),
                            None => Err(ERR),
                        },
                    };
                    match out {
                        Ok(btree_drv::ArcaDebugOut::Scalar(v)) => Ok(v),
                        Ok(btree_drv::ArcaDebugOut::Read(block, data)) => {
                            rings::resp_ring_write(block, 0, &data[..]);
                            let _ = civis::reply(0, block, 0);
                            continue;
                        }
                        Ok(btree_drv::ArcaDebugOut::Stats(high, live, free)) => {
                            let f = free.to_le_bytes();
                            rings::resp_ring_write(high, live, &f);
                            let _ = civis::reply(0, high, live);
                            continue;
                        }
                        Ok(btree_drv::ArcaDebugOut::Quota(_, _)) => Err(ERR),
                        Err(e) => Err(e),
                    }
                }
            }

            R_OBJ_GET_ID => {
                // Payload: [id:8], w1 = offset (come GET, per object_id)
                match disk.as_mut() {
                    Some(d) => btree_drv::disk_get_id(d, payload, w1 as usize, RING_MAX_PAYLOAD),
                    None => Err(ERR),
                }
            }

            // STAT a due registri + frame dedicato (pattern R_PIPE_CREATE):
            // il reply generico porta un solo valore, qui servono due.
            R_OBJ_STAT => {
                let out = match disk.as_mut() {
                    Some(d) => btree_drv::disk_stat(d, payload),
                    None => Err(ERR),
                };
                match out {
                    Ok((id, size, frame)) => {
                        rings::resp_ring_write(id, size, &frame);
                        let _ = civis::reply(0, id, size);
                    }
                    Err(e) => {
                        rings::resp_ring_write(e, 0, &[]);
                        let _ = civis::reply(0, e, 0);
                    }
                }
                continue;
            }

            R_OBJ_STAT_ID => {
                let out = match disk.as_mut() {
                    Some(d) => btree_drv::disk_stat_id(d, payload),
                    None => Err(ERR),
                };
                match out {
                    Ok((size, nv, mtime)) => {
                        rings::resp_ring_write(size, nv, &mtime);
                        let _ = civis::reply(0, size, nv);
                    }
                    Err(e) => {
                        rings::resp_ring_write(e, 0, &[]);
                        let _ = civis::reply(0, e, 0);
                    }
                }
                continue;
            }

            R_OBJ_DELETE => match disk.as_mut() {
                Some(d) => btree_drv::disk_delete(d, payload),
                None => Err(ERR),
            },

            R_SNAP_CREATE => match disk.as_mut() {
                Some(d) => btree_drv::disk_snap_create(d, payload),
                None => Err(ERR),
            },

            R_SNAP_DELETE => match disk.as_mut() {
                Some(d) => btree_drv::disk_snap_delete(d, payload),
                None => Err(ERR),
            },

            R_SNAP_ROLLBACK => match disk.as_mut() {
                Some(d) => btree_drv::disk_snap_rollback(d, payload),
                None => Err(ERR),
            },

            R_SNAP_CLONE => match disk.as_mut() {
                Some(d) => btree_drv::disk_snap_clone(d, payload),
                None => Err(ERR),
            },

            R_LSEEK => {
                // w0 = fd, w1 = offset (bit reinterpretati come i64),
                // payload[0] = whence (expect = 1 garantisce il byte).
                let off = w1 as i64;
                let whence = payload.first().copied().unwrap_or(0xFF) as u64;
                handlers::handle_lseek(&fs, &mut ftable, &mut fat_mounts, chan, w0 as u32, off, whence, &mut fat_gen, &mut disk)
            }

            R_DUP_GRANT => {
                dup::handle_grant(&ftable, &mut grants, &mut pipes, chan, w0 as u32)
            }

            R_DUP_CLAIM => {
                match payload.first_chunk::<8>() {
                    Some(nonce) => dup::handle_claim(&mut ftable, &mut grants, chan, u64::from_le_bytes(*nonce)),
                    None => Err(ERR_INVALID),
                }
            }

            R_DUP_CANCEL => {
                match payload.first_chunk::<8>() {
                    Some(nonce) => dup::handle_cancel(&mut grants, &mut pipes, chan, u64::from_le_bytes(*nonce)),
                    None => Err(ERR_INVALID),
                }
            }

            // PIPE_CREATE: risposta a due fd (result = lettura, w1 =
            // scrittura), scritta qui perche' il percorso generico in fondo
            // porta un solo valore. `continue`: salta frame+reply generici.
            R_PIPE_CREATE => {
                match handlers::handle_pipe_create(&mut ftable, &mut pipes, chan, w0 as usize) {
                    Ok((r, w)) => {
                        rings::resp_ring_write(r, w, &[]);
                        let _ = civis::reply(0, r, 0);
                    }
                    Err(e) => {
                        let res = mount::to_reply_res(Err(e));
                        rings::resp_ring_write(res, 0, &[]);
                        let _ = civis::reply(0, res, 0);
                    }
                }
                continue;
            }

            // DROP/GET partono dal tetto policy (Fase 45), non da ALL: un
            // canale restrittivo non puo' "droppare verso l'alto".
            R_RIGHTS_DROP => rights::handle_rights_drop(&mut rights, chan, w0 as u32, &payload, ceiling).ok_or(ERR),

            R_RIGHTS_GET => rights::handle_rights_get(&rights, &rings, chan, ceiling).ok_or(ERR),

            _ => {
                // Tag sconosciuto: frame gia' consumato sopra, ritorna errore.
                Err(ERR)
            }
        };

        // Scrivi il response frame (se non e' gia' stato scritto dall'handler).
        // Gli handler locali (read, readdir) scrivono direttamente nella response
        // ring; qui scriviamo solo il result frame per conferma.
        // NOTA: handle_read, handle_readdir, handle_rights_get, handle_stat,
        // Contratto "un solo frame per op" (test-6): gli handler self-written
        // scrivono il frame SOLO a successo (mai sui rami Err); a errore il
        // client troverebbe la ring vuota e leggerebbe NotReady invece
        // dell'errore tipizzato (es. EMPTY per le pipe: senza frame niente
        // retry throttled). Qui la заявку: un solo frame per op, mai doppio
        // (gli handler non scrivono mai sui rami Err).
        match op_tag {
            R_READ | R_READDIR | R_RIGHTS_GET | R_STAT | R_DISK_LIST | R_DISK_INFO | R_STATVFS
            | R_GET_HASH | R_OBJ_GET | R_OBJ_GET_ID => {
                if let Err(e) = result {
                    if rings.get(&chan).is_some() {
                        rings::map_client_resp_ring(&rings, chan);
                        rings::resp_ring_write(e, 0, &[]);
                    }
                }
            }
            _ => {
                rings::resp_ring_write(mount::to_reply_res(result), 0, &[]);
            }
        }

        let _ = civis::reply(0, mount::to_reply_res(result), 0);
    }
}
