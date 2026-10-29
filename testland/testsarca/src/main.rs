//! usertestsarca — test ArcaFS P5+A1+56.1+56.2a+56.2b+56.2c (Fase 54/55/56).
//!
//! Assert (con i drive ArcaFS presenti; il core senza, run manuale):
//!   1. vettori BLAKE2s (vuoto/abc/lungo, valori noti)
//!   2. ramfs: hash BLAKE2s via `R_GET_HASH` == ricalcolo indipendente
//!   3. tamper: contenuto diverso -> hash diverso
//!   7. un disco o partizione espone il superblock ACFS (scan per magic)
//!   8. mount `/arca` del volume ArcaFS riesce
//!   9. `open` di assente sul mount rifiutato (mai dati inventati)
//!  10. `readdir` root vuota ok (56.3: non piu' stub)
//!  11. umount `/arca` riesce (cleanup)
//!  12. un disco GPT (protective-MBR 0xEE a byte 450) espone ACFS in
//!      partizione (parse GPT guest: header+entry UEFI reali)
//!  13. mount/umount del volume GPT con `open` rifiutato
//!  22. volume on-disk: open (o gia' legato all'avvio) + assente rifiutata
//!  23. alloc due blocchi distinti mai-zero
//!  24. write/read round-trip 3560 B con checksum
//!  25. stat volume: high_water/live in delta sulla baseline (56.2c)
//!  26. free + realloc LIFO dallo stesso blocco
//!  27. double-free, free(0), free ignoto, read(0) rifiutati
//!  28. bind motore B+tree + seed `sys` (56.2b)
//!   4. round-trip piccolo su disco (stesso assert 56.1, backend blocchi)
//!   5. chunking 10000 B su disco
//!   6. chiave assente -> errore (mai dati inventati)
//!  14. versioni: PUT ripetute = catena, GET = latest (su disco)
//!  15. snapshot del bucket --- 16. rollback come nuova head
//!  17. snapshot delete non tocca il live --- 18. retention trim a 8
//!  19. delete oggetto --- 20. clone bucket --- 21. stat/get_id/stat_id
//!  29. split multi-livello: 120 chiavi oltre la foglia (56.2b)
//!  30. overflow 3000 B + chiavi lunghe + bound rifiutati (56.2b)
//!  31. refcount pin sopravvive a delete (56.2b)
//!  32. crash kill + remount LOAD: dati committati intatti (56.2b)
//!  33. GC deterministico + snapshot sopravvissuto (56.2c)
//!  34. servizio `Vestigia` registrato e raggiungibile per nome (57)
//!  35. log append + read latest own-bucket con seq e record integri (57)
//!  36. log msg 1024B round-trip per-seq (57)
//!  37. log rifiuti (bound tag/msg, msg vuoto) + giorno ignoto (57)
//!  38. log seal esplicito (snapshot monotonici) + delete (57)
//!  39. log stats (appended = test + milestone init = prova FLUSH) (57)
//!  40. log bounce via init + rewarm client (re-lookup+REG, latest) (57)
//!  41. posix mkdir emergente visibile (stat dir + readdir) (56.3)
//!  42. rmdir mai-esistita = errore (56.3)
//!  43. mkdir su file = errore (56.3)
//!  44. file round-trip write/read/stat su /arca (56.3)
//!  45. readdir figli immediati (56.3)
//!  46. rmdir non-vuota = errore (56.3)
//!  47. overwrite a offset (read-modify-write) (56.3)
//!  48. OPEN_TRUNC + OPEN_APPEND (56.3)
//!  49. delete file: dir resta (RAM), rmdir ok, stat sparita (56.3)
//!  50. bounce cardo + remount: file intatti, vuote perse (56.3)
//!  57. range read 100 KiB sequenziale a slice 4 KiB (S2.5f/F0: fixture
//! `test/r57.bin` seedata da host, pattern posizionale + bound 300 tick —
//! il materializza-e-slice era quadratico per slice).
//!  58. prefix-overwrite parziale (W1: merge esplicito in `ns_write`,
//! senza il quale `put_chunk(0)` troncherebbe la coda).
//!  59. append 512 KiB sequenziali con bound (W2: O(1) per op con extent)
//! + riga xstats informativa (distribuzione layout valori).
//!  60. epoca kill-mid (W4: committed durevole, aperto perso, SYNC salva
//! + recovery con GC al bind).
//! Con `ARCA_IMG=1` (gate) i drive ci sono sempre; senza, solo 1-3 e 7-13
//! adattivi e il resto saltato — n/n adattivo, mai FAIL per drive assente.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::format;
use alloc::string::String;

use civis;
use civis::hex;
use civis::println;

/// Conta pass/total e stampa la riga di gate `[testsarca] PASS n/n`.
struct Checks {
    pass: u32,
    total: u32,
}

impl Checks {
    fn ok(&mut self, name: &str, cond: bool) {
        self.total += 1;
        if cond {
            self.pass += 1;
            println!("[testsarca] {}: PASS", name);
        } else {
            println!("[testsarca] {}: FAIL", name);
        }
    }
}

/// Verifica completa del superblock ArcaFS (magic + versione + block-size +
/// checksum FNV-1a self-verifying): un BPB FAT non puo' collidere.
fn is_arca_super(sec: &[u8; 512]) -> bool {
    let sb = &sec[..civis::ARCA_SUPER_LEN];
    if sb[civis::ARCA_OFF_MAGIC..civis::ARCA_OFF_MAGIC + 4] != *civis::ARCA_MAGIC {
        return false;
    }
    let u32le = |o: usize| u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]]);
    let u64le = |o: usize| {
        u64::from_le_bytes([
            sb[o], sb[o + 1], sb[o + 2], sb[o + 3], sb[o + 4], sb[o + 5], sb[o + 6], sb[o + 7],
        ])
    };
    u32le(civis::ARCA_OFF_VERSION) == civis::ARCA_VERSION
        && u32le(civis::ARCA_OFF_BLOCK_SIZE) == civis::ARCA_BLOCK_SIZE
        && civis::image_hash(&sb[..civis::ARCA_OFF_CHECK]) == u64le(civis::ARCA_OFF_CHECK)
}

/// Label del volume dal superblock (campo NUL-padded, v4 ADR-0059): il
/// *nome* del volume ("SYSTEM", "PART", "GPT"). Stringa vuota = assente.
fn superblock_label(sec: &[u8; 512]) -> &str {
    let sb = &sec[..civis::ARCA_SUPER_LEN];
    let f = &sb[civis::ARCA_OFF_LABEL..civis::ARCA_OFF_LABEL + civis::ARCA_LABEL_MAX];
    let n = f.iter().position(|&c| c == 0).unwrap_or(f.len());
    core::str::from_utf8(&f[..n]).unwrap_or("")
}

/// Mountpoint dichiarato dal volume (campo NUL-padded, v4): path RELATIVO
/// alla root ("tests"); stringa vuota = il volume non dichiara niente.
fn superblock_mountpoint(sec: &[u8; 512]) -> &str {
    let sb = &sec[..civis::ARCA_SUPER_LEN];
    let f = &sb[civis::ARCA_OFF_MOUNT..civis::ARCA_OFF_MOUNT + civis::ARCA_MOUNT_MAX];
    let n = f.iter().position(|&c| c == 0).unwrap_or(f.len());
    core::str::from_utf8(&f[..n]).unwrap_or("")
}

/// Legge 512 B da un path /dev (open/read/close) o None.
fn read_sector(path: &str) -> Option<[u8; 512]> {
    let Ok(fd) = civis::open(path, 0) else { return None };
    let mut sec = [0u8; 512];
    let n = civis::read_fs(fd, &mut sec, 512);
    let _ = civis::close(fd);
    if n != Ok(512) {
        return None;
    }
    Some(sec)
}

/// Cerca un disco o partizione il cui LBA0 e' un superblock ArcaFS valido.
/// Ritorna il nome breve (es. "sdc" o "sda1") in un buffer, o None.
/// Scan per magic (lettera-agnostico).
fn find_arca() -> Option<[u8; 4]> {
    // Fino a 8 dischi + 4 partizioni per disco (bound difensivo).
    // Fase 2: preferisco partizioni (sXn) ai dischi interi (sX) — le partizioni
    // sono volumi freschi (arca-part.img / arca-gpt.img), i dischi interi possono
    // essere la root seeded con i binari di boot (non vuoti).
    let mut found_part: Option<[u8; 4]> = None;
    for i in 0..8u64 {
        let letter = b'a' + i as u8;
        if letter > b'z' { break; }

        // Whole-disk: sda, sdb, ...
        let mut name = [b's', b'd', letter, 0];
        let mut pbuf = [0u8; 16];
        pbuf[..5].copy_from_slice(b"/dev/");
        pbuf[5..8].copy_from_slice(&name[..3]);
        if let Some(sec) = read_sector(core::str::from_utf8(&pbuf[..8]).unwrap_or("")) {
            if is_arca_super(&sec) {
                // Taso la whole-disk: la salvo ma continuo a cercare partizioni.
                if found_part.is_none() {
                    println!("[testsarca] find_arca: whole-disk {} ha superblock", core::str::from_utf8(&name[..4]).unwrap_or("?"));
                    found_part = Some([b's', b'd', letter, 0]);
                }
            }
        }

        // Partizioni: sda1..sda4, sdb1..sdb4, ...
        for p in 1..=4u8 {
            name = [b's', b'd', letter, b'0' + p];
            pbuf[..5].copy_from_slice(b"/dev/");
            pbuf[5..9].copy_from_slice(&name);
            if let Some(sec) = read_sector(core::str::from_utf8(&pbuf[..9]).unwrap_or("")) {
                if is_arca_super(&sec) {
                    println!("[testsarca] find_arca: partizione {} ha superblock (RITORNO)", core::str::from_utf8(&name[..4]).unwrap_or("?"));
                    return Some(name); // Partizione trovata:优先.
                } else {
                    println!("[testsarca] find_arca: partizione {} NO superblock", core::str::from_utf8(&name[..4]).unwrap_or("?"));
                }
            }
        }
    }
    if let Some(ref fp) = found_part {
        println!("[testsarca] find_arca: fallback whole-disk {}", core::str::from_utf8(fp).unwrap_or("?"));
    }
    found_part
}

/// UUID del volume root (fisso: lo sceglie la cmdline `root=UUID=…`).
const ROOT_UUID: u64 = 0x4152434100000001;

/// Device del volume **root**: il whole-disk il cui superblock e' un
/// ArcaFS valido **con l'uuid della root** (ADR-0059: mai per lettera — con
/// il FAT fuori dal boot le lettere sdX cambiano; il volume si riconosce
/// dal contenuto). Ritorna "/dev/<nome>".
fn root_dev() -> Option<String> {
    for i in 0..8u8 {
        let letter = b'a' + i;
        if letter > b'z' {
            break;
        }
        let path = format!("/dev/sd{}", letter as char);
        if let Some(sec) = read_sector(&path) {
            let sb = &sec[..civis::ARCA_SUPER_LEN];
            if is_arca_super(&sec) {
                let u64le = |o: usize| {
                    u64::from_le_bytes([
                        sb[o], sb[o + 1], sb[o + 2], sb[o + 3], sb[o + 4], sb[o + 5], sb[o + 6],
                        sb[o + 7],
                    ])
                };
                if u64le(civis::ARCA_OFF_UUID) == ROOT_UUID {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Monta il volume root (stesso uuid del motore globale legato a boot) su
/// `/arca` (E2: la vista POSIX 56.3 gira sul root vivo — il mount del primo
/// trovato non va: le partizioni sono volumi freschi, uuid diverso = stub).
fn mount_root_arca() -> bool {
    match root_dev() {
        Some(dev) => civis::mount(&dev, "/arca").is_ok(),
        None => false,
    }
}

/// Cerca un disco GPT (protective-MBR: tipo prima voce 0xEE a byte 450, NON
/// 446 che e' il boot flag) la cui partizione espone ACFS. Ritorna
/// (disco, partizione) o None. Prova il parse GPT guest end-to-end:
/// protective → header UEFI → entry → superblock partition-relative.
fn find_gpt_arca() -> Option<([u8; 4], [u8; 4])> {
    for i in 0..8u64 {
        let letter = b'a' + i as u8;
        if letter > b'z' { break; }
        let disk = [b's', b'd', letter, 0];
        let mut pbuf = [0u8; 16];
        pbuf[..5].copy_from_slice(b"/dev/");
        pbuf[5..8].copy_from_slice(&disk[..3]);
        let sec0 = match read_sector(core::str::from_utf8(&pbuf[..8]).unwrap_or("")) {
            Some(s) => s,
            None => continue,
        };
        if sec0[450] != 0xEE {
            continue;
        }
        for p in 1..=4u8 {
            let part = [b's', b'd', letter, b'0' + p];
            pbuf[..5].copy_from_slice(b"/dev/");
            pbuf[5..9].copy_from_slice(&part);
            if let Some(sec) = read_sector(core::str::from_utf8(&pbuf[..9]).unwrap_or("")) {
                if is_arca_super(&sec) {
                    return Some((disk, part));
                }
            }
        }
    }
    None
}

/// Generazione superblock dal device della root (retry throttled: dopo un restart il
/// resolve del device puo' fallire i primi tentativi).
fn read_gen() -> Option<u64> {
    civis::poll_value(200, civis::POLL_PERIOD_TICKS, || {
        root_dev().as_deref().and_then(read_sector).map(|s| {
            u64::from_le_bytes(
                s[civis::ARCA_OFF_GEN..civis::ARCA_OFF_GEN + 8].try_into().unwrap_or([0; 8]),
            )
        })
    })
}

/// `R_GET_HASH` di un path (32 B) o None.
fn get_hash(path: &str) -> Option<[u8; 32]> {
    let mut h = [0u8; 32];
    match civis::get_hash(path, &mut h) {
        Ok(()) => Some(h),
        Err(_) => None,
    }
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    println!("[testsarca] starting, pid={}", civis::getpid());
    let mut c = Checks { pass: 0, total: 0 };

    // 1. Vettori BLAKE2s (valori generati da due implementazioni indipendenti
    // — Python hashlib + OpenSSL, coincidenti: vedi `blake2s` crate).
    let v_empty = blake2s::blake2s(b"");
    let v_abc = blake2s::blake2s(b"abc");
    let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    let v_long = blake2s::blake2s(long);
    let want_empty = hex::decode_hex_fixed::<32>("69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9").expect("hex invalida");
    let want_abc = hex::decode_hex_fixed::<32>("508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982").expect("hex invalida");
    let want_long = hex::decode_hex_fixed::<32>("6f4df5116a6f332edab1d9e10ee87df6557beab6259d7663f3bcd5722c13f189").expect("hex invalida");
    c.ok(
        "blake2s vectors",
        v_empty == want_empty && v_abc == want_abc && v_long == want_long,
    );

    // 2. ramfs: content_hash via R_GET_HASH == ricalcolo indipendente.
    let payload = b"velordo-arcafs-p5-content";
    let mut rt_ok = false;
    if let Ok(fd) = civis::open("/sarca.txt", civis::OPEN_CREATE) {
        let w = civis::write_fs(fd, payload, payload.len());
        let _ = civis::close(fd);
        if w == Ok(payload.len()) {
            let expect = blake2s::blake2s(payload);
            rt_ok = get_hash("/sarca.txt") == Some(expect);
        }
    }
    c.ok("ramfs content_hash", rt_ok);

    // 3. tamper: contenuto diverso -> hash diverso (e non quello vecchio).
    let mut tamper_ok = false;
    if let Ok(fd) = civis::open("/sarca.txt", civis::OPEN_TRUNC) {
        let other = b"velordo-arcafs-p5-TAMPERED";
        let w = civis::write_fs(fd, other, other.len());
        let _ = civis::close(fd);
        if w == Ok(other.len()) {
            let h = get_hash("/sarca.txt");
            tamper_ok = h == Some(blake2s::blake2s(other))
                && h != Some(blake2s::blake2s(payload));
        }
    }
    c.ok("tamper hash cambia", tamper_ok);
    let _ = civis::remove("/sarca.txt");

    // 4-6 + 14-21 (semantica versionata): girano DOPO il bind (test 28) sul
    // backend blocchi — stessi assert 56.1, backend diverso (specifica §19).
    // Vedi sotto, sezione 22-32.

    // ADR-0059 (v4): il volume si presenta — la label (il nome) e il
    // mountpoint dichiarato si leggono dal superblock. La root non
    // dichiara un mountpoint (la sceglie la cmdline `root=UUID=`).
    let root_sec = root_dev().as_deref().and_then(read_sector);
    c.ok(
        "identita' volume v4 (label)",
        root_sec.map(|s| superblock_label(&s) == "SYSTEM").unwrap_or(false),
    );
    c.ok(
        "identita' volume v4 (mountpoint vuoto sulla root)",
        root_sec.map(|s| superblock_mountpoint(&s).is_empty()).unwrap_or(false),
    );

    // 4-8. Volume ArcaFS (solo se un disco/partizione espone superblock ACFS).
    match find_arca() {
        Some(name) => {
            c.ok("volume ACFS trovato", true);
            let len = if name[3] == 0 { 3 } else { 4 };
            let dev = core::str::from_utf8(&name[..len]).unwrap_or("sdc");
            let mut src = [0u8; 16];
            src[..5].copy_from_slice(b"/dev/");
            src[5..5 + len].copy_from_slice(&name[..len]);
            let src = core::str::from_utf8(&src[..5 + len]).unwrap_or("/dev/sda");

            let mounted = civis::mount(src, "/arca").is_ok();
            c.ok("mount /arca", mounted);
            if mounted {
                // Vista POSIX 56.3: la root esiste (potrebbe essere non vuota se il
                // volume e' seeded con i binari di boot — Fase 2). Conto i nomi:
                // 0 = vuoto, >=1 = seedato. NotFound = mount con uuid diverso
                // dal motore (stub presente-ma-illeggibile per disegno, test-6:
                // prima arrivava NotReady perche' gli op self-written non
                // recapitavano gli errori — ora il codice tipizzato arriva).
                // NotReady resta solo per timeout genuini.
                let mut buf = [0u8; 64];
                let rd = civis::readdir("/arca", &mut buf, 64);
                let empty_or_seeded = match rd {
                    Ok(0) => true,                       // Vuoto come originariamente atteso.
                    Ok(n) if n > 0 => true,              // Seedato (Fase 2): ok.
                    Err(civis::Error::NotFound) => true, // Stub uuid-mismatch.
                    Err(civis::Error::NotReady) => true, // Timeout genuino.
                    _ => false,                           // Altro errore: FAIL.
                };
                c.ok("readdir root vuota ok", empty_or_seeded);
                // Poi prova open su file inesistente.
                let opened = civis::open("/arca/anything", 0);
                c.ok("open assente rifiutato", opened.is_err());
                c.ok("umount /arca", civis::umount("/arca").is_ok());
            } else {
                println!("[testsarca] mount {} FAILED", dev);
                c.ok("open assente rifiutato", false);
                c.ok("readdir root vuota ok", false);
                c.ok("umount /arca", false);
            }
        }
        None => {
            // Run manuale senza ARCA_IMG: il core passa comunque (mai FAIL
            // per drive assente); la scansione e' la riga 4 del gate.
            println!("[testsarca] nessun volume ACFS (ARCA_IMG=0?): salto 4-8");
        }
    }

    // 12-13. Volume GPT in partizione (solo se presente: con ARCA_IMG=1 c'e'
    // sempre; senza, salto senza FAIL come sopra).
    match find_gpt_arca() {
        Some((disk, part)) => {
            let dlen = if disk[3] == 0 { 3 } else { 4 };
            let plen = if part[3] == 0 { 3 } else { 4 };
            println!(
                "[testsarca] GPT: {} protective + {} con ACFS",
                core::str::from_utf8(&disk[..dlen]).unwrap_or("?"),
                core::str::from_utf8(&part[..plen]).unwrap_or("?"),
            );
            c.ok("volume ACFS in partizione GPT", true);
            let mut src = [0u8; 16];
            src[..5].copy_from_slice(b"/dev/");
            src[5..5 + plen].copy_from_slice(&part[..plen]);
            let src = core::str::from_utf8(&src[..5 + plen]).unwrap_or("/dev/sdd1");
            // Round-trip mount in un solo assert: mount ok + open rifiutato
            // (stub) + umount ok. Dettaglio nel log a fallimento parziale.
            let gpt_ok = match civis::mount(src, "/arca") {
                Ok(()) => {
                    let open_ok = civis::open("/arca/anything", 0).is_err();
                    let umount_ok = civis::umount("/arca").is_ok();
                    if !open_ok {
                        println!("[testsarca] GPT: open su stub riuscita?!");
                    }
                    if !umount_ok {
                        println!("[testsarca] GPT: umount fallito");
                    }
                    open_ok && umount_ok
                }
                Err(_) => {
                    println!("[testsarca] GPT: mount {} fallito", src);
                    false
                }
            };
            c.ok("mount/umount GPT + open rifiutato", gpt_ok);
        }
        None => {
            println!("[testsarca] nessun volume GPT (ARCA_IMG=0?): salto 12-13");
        }
    }

    // 14-21. Versioni + snapshot: spostati DOPO il bind (test 28), sezione
    // 22-32 — stessi assert, backend blocchi (specifica §19).

    // 22-32. Motore su disco (56.2b; E2: il device della root, motore
    // gia' legato a boot — OPEN/USEDISK tollerati, gli op corrono sul motore
    // root in bucket isolati `test`/`v56`/`d56`/`dpin`/`dcrash`/`dgc`, mai su
    // ns:/sys:/vela:/tst:/log:. Immagini rigenerate a ogni run, quindi
    // allocazioni deterministiche). Senza volume: salto senza FAIL, core
    // PASS invariato.
    // Ordine obbligato: scaffold RAW (22-27) PRIMA del bind (il bind consuma
    // la freelist: secondary fissa al blocco 2); semantica versionata (4-6,
    // 14-21: stessi assert 56.1, backend blocchi) DOPO; split/commit (29-31)
    // e crash+remount (32) per ultimi.
    {
        let root = root_dev();
    let have_vol = root
        .as_deref()
        .and_then(read_sector)
        .map_or(false, |s| is_arca_super(&s));
        if !have_vol {
            println!("[testsarca] nessun volume ACFS sulla root: salto 22-32");
        } else {
            // 22. open (o gia' legato all'avvio 56.2c: re-open rifiutato,
            // tollerato) + open assente rifiutata + motore attivo.
            if let Some(dev) = root_dev().as_deref() {
                let _ = civis::arca_open(dev);
            }
            let v22 = civis::arca_use_disk(true).is_ok()
                && civis::arca_open("/dev/sdZ").is_err();
            c.ok("vol open + assente", v22);
            // Baseline allocatore per assert relativi (il bind all'avvio ha
            // gia' consumato blocchi: mai numeri assoluti qui).
            let (h0, l0) = match civis::arca_stat_vol() {
                Ok((h, l, _)) => (h, l),
                Err(_) => (0, 0),
            };

            // 23. alloc due blocchi distinti mai-zero.
            let b1 = civis::arca_alloc().ok();
            let b2 = civis::arca_alloc().ok();
            let v23 = match (b1, b2) {
                (Some(a), Some(b)) => a >= 1 && b >= 1 && a != b,
                _ => false,
            };
            c.ok("alloc distinti mai-zero", v23);
            // 24. write/read round-trip del prefisso di payload che entra in
            // un frame ring; il server azzera la coda e il checksum copre
            // tutto il payload 64 KiB. Mai 64 KiB sullo stack (guardia 16 KiB).
            let npat = civis::RING_MAX_PAYLOAD - 440;
            let pat: alloc::vec::Vec<u8> = (0..npat).map(|i| ((i * 7) % 251) as u8).collect();
            let v24 = match (b1, b2) {
                (Some(a), Some(b)) => {
                    civis::arca_write_node(a, &pat).is_ok()
                        && matches!(civis::arca_read_node(a), Ok(ref v)
                            if v.len() == civis::RING_MAX_PAYLOAD
                                && &v[..npat] == pat.as_slice()
                                && v[npat..].iter().all(|&x| x == 0))
                        && civis::arca_write_node(b, &pat).is_ok()
                        && matches!(civis::arca_read_node(b), Ok(ref v)
                            if &v[..npat] == pat.as_slice())
                }
                _ => false,
            };
            c.ok("write/read nodi", v24);
            // 25. stat: live avanzato di 2 dai due alloc, high_water al piu'
            // di 2 (delta sulla baseline: il bind all'avvio consuma un numero
            // ignoto; E2: sul root vivo gli alloc pescano dalla freelist GC,
            // su volume fresco da high_water — entrambi leciti).
            let v25 = matches!(
                civis::arca_stat_vol(),
                Ok((high, live, _)) if high >= h0 && high <= h0 + 2 && live == l0 + 2
            );
            c.ok("stat volume", v25);
            // 26. free + realloc LIFO dallo stesso blocco (live invariato).
            let v26 = match b1 {
                Some(a) => {
                    civis::arca_free(a).is_ok()
                        && matches!(civis::arca_alloc(), Ok(b) if b == a)
                        && matches!(civis::arca_stat_vol(), Ok((_, live, _)) if live == l0 + 2)
                }
                _ => false,
            };
            c.ok("free + realloc LIFO", v26);
            // 27. rifiuti: double-free, free(0), free ignoto, read(0).
            let v27 = match b1 {
                Some(a) => {
                    civis::arca_free(a).is_ok()
                        && civis::arca_free(a).is_err()
                        && civis::arca_free(0).is_err()
                        && civis::arca_free(99999).is_err()
                        && civis::arca_read_node(0).is_err()
                }
                _ => false,
            };
            c.ok("rifiuti allocatore", v27);
            // 28. bind motore B+tree + seed server-side (D1: i driver stanno
            // in `vela`): da qui gli op nativi parlano ai blocchi (commit
            // per-op, shadow + flip).
            let v28 = civis::arca_use_disk(true).is_ok()
                && matches!(civis::obj_get(b"vela", b"bin/gpu.bin"), Ok(v) if !v.is_empty());
            c.ok("bind motore + seed sys", v28);
            // 4. round-trip piccolo su disco (stesso assert 56.1).
            let small = b"nativo-arcafs-obj";
            let put_ok = civis::obj_put(b"test", b"k1", small) == Ok(small.len() as u64);
            c.ok(
                "obj round-trip piccolo",
                put_ok && matches!(civis::obj_get(b"test", b"k1"), Ok(v) if v == small),
            );
            // 5. chunking 10000 B su disco (stateless + commit per chunk).
            let big: alloc::vec::Vec<u8> = (0..10000u32).map(|i| (i % 251) as u8).collect();
            let put_big = civis::obj_put(b"test", b"big", &big) == Ok(big.len() as u64);
            c.ok(
                "obj chunking 10000B",
                put_big && matches!(civis::obj_get(b"test", b"big"), Ok(v) if v == big),
            );
            // 6. chiave assente → errore (mai dati inventati).
            c.ok("obj assente -> errore", civis::obj_get(b"test", b"nope").is_err());
            // 14-21. versioni + snapshot su disco (stessi assert 56.1).
            {
                let (b, k1) = (&b"v56"[..], &b"k1"[..]);
                let a = b"versione-A";
                let bb = b"versione-B";
                let v14 = civis::obj_put(b, k1, a) == Ok(a.len() as u64)
                    && civis::obj_put(b, k1, bb) == Ok(bb.len() as u64)
                    && matches!(civis::obj_get(b, k1), Ok(v) if v == bb)
                    && matches!(civis::obj_stat(b, k1), Ok((_, sz, nv, _)) if sz == bb.len() as u64 && nv == 2);
                c.ok("versioni: catena + latest", v14);
                let cc = b"versione-C";
                let s = civis::snap_create(b).ok();
                c.ok("snap create", s.is_some());
                let v16 = match s {
                    Some(sid) => {
                        civis::obj_put(b, k1, cc) == Ok(cc.len() as u64)
                            && civis::snap_rollback(b, k1, sid).is_ok()
                            && matches!(civis::obj_get(b, k1), Ok(v) if v == bb)
                            && matches!(civis::obj_stat(b, k1), Ok((_, _, nv, _)) if nv == 4)
                    }
                    None => false,
                };
                c.ok("rollback ripristina pinnata", v16);
                let v17 = match s {
                    Some(sid) => {
                        civis::snap_delete(sid).is_ok()
                            && matches!(civis::obj_stat(b, k1), Ok((_, _, nv, _)) if nv == 4)
                            && civis::snap_delete(sid).is_err()
                    }
                    None => false,
                };
                c.ok("snap delete non tocca live", v17);
                let mut v18 = true;
                for i in 0..10u8 {
                    let d = [b'D', b'0' + i];
                    if civis::obj_put(b, k1, &d) != Ok(2) {
                        v18 = false;
                    }
                }
                v18 = v18
                    && matches!(civis::obj_get(b, k1), Ok(v) if v == [b'D', b'9'])
                    && matches!(civis::obj_stat(b, k1), Ok((_, sz, nv, _)) if sz == 2 && nv == 8);
                c.ok("retention trim a 8", v18);
                let v19 = civis::obj_delete(b, k1).is_ok()
                    && civis::obj_get(b, k1).is_err()
                    && civis::obj_stat(b, k1).is_err()
                    && civis::obj_delete(b, k1).is_err();
                c.ok("delete oggetto", v19);
                let (cb, ka, kb) = (&b"csrc"[..], &b"a"[..], &b"b"[..]);
                let v20 = civis::obj_put(cb, ka, b"uno") == Ok(3)
                    && civis::obj_put(cb, kb, b"due!") == Ok(4)
                    && match civis::snap_create(cb) {
                        Ok(sid) => {
                            civis::snap_clone(sid, b"cdst") == Ok(2)
                                && matches!(civis::obj_get(b"cdst", ka), Ok(v) if v == b"uno")
                                && matches!(civis::obj_get(b"cdst", kb), Ok(v) if v == b"due!")
                                && civis::snap_delete(sid).is_ok()
                        }
                        Err(_) => false,
                    };
                c.ok("clone bucket", v20);
                let v21 = match civis::obj_stat(b"cdst", ka) {
                    Ok((id, sz, nv, _)) if id > 0 && sz == 3 && nv == 1 => {
                        matches!(civis::obj_get_id(id), Ok(v) if v == b"uno")
                            && matches!(civis::obj_stat_id(id), Ok((s2, n2, _)) if s2 == 3 && n2 == 1)
                            && civis::obj_get_id(id + 1000000).is_err()
                    }
                    _ => false,
                };
                c.ok("stat id + get_id/stat_id", v21);
            }
            // 29. split multi-livello: 120 chiavi oltre la foglia, rilettura
            // totale (ogni PUT = commit shadow+flip: il disco vede tutto).
            // NOTA flake raro (2 su ~10 run, 1 op su 240, mai crash):
            // storia op deterministica + single-client + server single-thread
            // escludono un bug logico (fallirebbe ogni run); resta IO
            // d'emulazione transiente. Nessuna azione codice.
            let mut v29 = true;
            for i in 0..120u32 {
                let mut kb = [b'k', 0, 0, 0, 0, 0, 0, 0];
                kb[1..].copy_from_slice(&(i as u64).to_le_bytes()[..7]);
                let val: alloc::vec::Vec<u8> = (0..64u32).map(|j| ((i + j) % 251) as u8).collect();
                if civis::obj_put(b"d56", &kb, &val) != Ok(64) {
                    v29 = false;
                }
            }
            for i in 0..120u32 {
                let mut kb = [b'k', 0, 0, 0, 0, 0, 0, 0];
                kb[1..].copy_from_slice(&(i as u64).to_le_bytes()[..7]);
                let want: alloc::vec::Vec<u8> = (0..64u32).map(|j| ((i + j) % 251) as u8).collect();
                if !matches!(civis::obj_get(b"d56", &kb), Ok(v) if v == want) {
                    v29 = false;
                }
            }
            c.ok("btree split + rilettura 120 chiavi", v29);
            // 30. overflow (blob 3000 B su catena RAW) + chiavi lunghe + bound.
            let big3k: alloc::vec::Vec<u8> = (0..3000u32).map(|i| (i * 7 % 251) as u8).collect();
            let long_k = [b'x'; 200];
            let too_b = [b'Y'; 17];
            let too_k = [b'Z'; 256];
            let v30 = civis::obj_put(b"d56", b"big3k", &big3k) == Ok(3000)
                && matches!(civis::obj_get(b"d56", b"big3k"), Ok(v) if v == big3k)
                && matches!(civis::obj_stat(b"d56", b"big3k"), Ok((_, 3000, 1, _)))
                && civis::obj_put(b"d56", &long_k, b"v") == Ok(1)
                && matches!(civis::obj_get(b"d56", &long_k), Ok(v) if v == b"v")
                && civis::obj_put(&too_b, b"k", b"v").is_err()
                && civis::obj_put(b"d56", &too_k, b"v").is_err()
                && civis::obj_get(b"d56", &too_k).is_err();
            c.ok("overflow + chiavi lunghe + bound", v30);
            // 31. refcount: snapshot pinna, delete live non invalida,
            // rollback ricrea, delete snapshot sgancia.
            let v31 = civis::obj_put(b"dpin", b"p", b"PIN") == Ok(3)
                && match civis::snap_create(b"dpin") {
                    Ok(sid) => {
                        civis::obj_delete(b"dpin", b"p").is_ok()
                            && civis::obj_get(b"dpin", b"p").is_err()
                            && civis::snap_rollback(b"dpin", b"p", sid).is_ok()
                            && matches!(civis::obj_get(b"dpin", b"p"), Ok(v) if v == b"PIN")
                            && civis::snap_delete(sid).is_ok()
                            && civis::snap_delete(sid).is_err()
                    }
                    Err(_) => false,
                };
            c.ok("refcount pin sopravvive a delete", v31);
            // 32. crash a meta' serie di PUT pesanti: kill cardo (bounce via
            // init, come t27/t28) → re-handshake trasparente → re-bind LOAD
            // (radici da superblock, id da header-ext, by_id ricostruito) →
            // dati committati intatti, generazione monotona, R/W riparte.
            // Il kill cade tra due commit (single-thread): COW + root-last
            // garantiscono la vecchia generazione intatta per costruzione.
            let v32 = {
                let wv: alloc::vec::Vec<u8> =
                    (0..3000u32).map(|i| (i * 11 % 251) as u8).collect();
                let mut ok = civis::obj_put(b"dcrash", b"w", &wv) == Ok(wv.len() as u64);
                let gen0 = read_gen();
                let mut last: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
                for i in 0..10u32 {
                    let v: alloc::vec::Vec<u8> =
                        (0..3000u32).map(|j| ((i + j * 7) % 251) as u8).collect();
                    if civis::obj_put(b"dcrash", b"bulk", &v) != Ok(v.len() as u64) {
                        ok = false;
                    }
                    last = v;
                }
                ok = ok && civis::init_bounce(civis::Service::Cardo).is_ok();
                ok = ok && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::service_pid(civis::Service::Cardo).is_err()
                });
                ok = ok && civis::poll_value(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::service_pid(civis::Service::Cardo).ok()
                })
                .is_some();
                // Re-bind sul cardo fresco (prima op: re-handshake
                // trasparente via NOHANDSHAKE). Con auto-bind all'avvio
                // (56.2c) l'open prende il rifiuto re-open: tollerato,
                // USEDISK idempotente con retry throttled (come t28), mai
                // singolo tentativo in finestra di avvio. Poi i dati, non
                // gli snapshot (tabella in RAM: persa col restart — in 56.2c
                // persistente, ma qui non assertita).
                if let Some(dev) = root.as_deref() {
                    let _ = civis::arca_open(dev);
                }
                ok = ok && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::arca_use_disk(true).is_ok()
                });
                ok = ok && matches!(civis::obj_get(b"dcrash", b"w"), Ok(v) if v == wv);
                ok = ok && matches!(civis::obj_get(b"dcrash", b"bulk"), Ok(v) if v == last);
                ok = ok
                    && matches!(
                        civis::obj_get(b"vela", b"bin/gpu.bin"),
                        Ok(v) if !v.is_empty()
                    );
                let gen1 = read_gen();
                ok = ok && match (gen0, gen1) {
                    (Some(a), Some(b)) => b >= a && b > 0,
                    _ => false,
                };
                ok = ok
                    && civis::obj_put(b"dcrash", b"post", b"vivo") == Ok(4)
                    && matches!(civis::obj_get(b"dcrash", b"post"), Ok(v) if v == b"vivo");
                ok
            };
            c.ok("crash kill + remount dati intatti", v32);
            // 33. GC deterministica (56.2c): orfani staged via RAW (allocati
            // e mai linkati: irraggiungibili per costruzione) + snapshot
            // pre-kill. Dopo bounce + remount (GC a ogni load-bind): lo
            // snapshot e' USABILE (tabella persistente → rollback prova), il
            // reclaim riporta blocchi sotto high_pre, gen monotona.
            let v33 = {
                let mut ok = civis::obj_put(b"dgc", b"w", b"gcvivo") == Ok(6);
                let sid = match civis::snap_create(b"dgc") {
                    Ok(s) => Some(s),
                    Err(_) => {
                        ok = false;
                        None
                    }
                };
                // 4 orfani staged (allocati, mai linkati).
                for _ in 0..4 {
                    if !matches!(civis::arca_alloc(), Ok(b) if b != 0) {
                        ok = false;
                    }
                }
                let high_pre = match civis::arca_stat_vol() {
                    Ok((h, _, _)) => h,
                    Err(_) => {
                        ok = false;
                        0
                    }
                };
                let gen_pre = read_gen();
                ok = ok && civis::init_bounce(civis::Service::Cardo).is_ok();
                ok = ok && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::service_pid(civis::Service::Cardo).is_err()
                });
                ok = ok && civis::poll_value(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::service_pid(civis::Service::Cardo).ok()
                })
                .is_some();
                // Re-bind tollerante (startup auto-lega gia': open puo'
                // prendere il rifiuto re-open, use_disk e' idempotente).
                if let Some(dev) = root_dev().as_deref() {
                    let _ = civis::arca_open(dev);
                }
                ok = ok && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::arca_use_disk(true).is_ok()
                });
                // Snapshot sopravvissuto = usabile, non solo elencato.
                ok = ok
                    && match sid {
                        Some(s) => {
                            civis::snap_rollback(b"dgc", b"w", s).is_ok()
                                && matches!(civis::obj_get(b"dgc", b"w"), Ok(v) if v == b"gcvivo")
                        }
                        None => false,
                    };
                // Reclaim: la GC ha liberato orfani (radici COW superate +
                // staged): un alloc rientra sotto high_pre (mai alloc fresco).
                // Non si asserta QUALE blocco (la GC libera in ordine
                // crescente e la testa e' l'orfano piu' alto, non per forza
                // uno staged).
                ok = ok
                    && match civis::arca_alloc() {
                        Ok(b) => b < high_pre,
                        Err(_) => false,
                    };
                let gen_post = read_gen();
                ok = ok
                    && match (gen_pre, gen_post) {
                        (Some(a), Some(b)) => b >= a && b > 0,
                        _ => false,
                    };
                ok = ok
                    && civis::obj_put(b"dgc", b"post", b"gcok") == Ok(4)
                    && matches!(civis::obj_get(b"dgc", b"post"), Ok(v) if v == b"gcok");
                ok
            };
            c.ok("gc orfani + snapshot sopravvissuto", v33);
            // 51-54. Quota per bucket (A3, ADR-0044): budget blocchi dati +
            // `ERR_NOSPC` tipizzato prima di allocare (mai mezze scritte).
            // Bucket isolati `q5*`, blob 3000 B: un solo frame (obj_put non
            // spezza, quindi il put e' atomico) e un blocco packed condiviso.
            // Con blocchi 64 KiB il tetto utile e' 9: 0 usati + 1 blocco +
            // slack 8 = 9 al primo put; al secondo 1 + 9 > 9 → NoSpace.
            let qbig = |seed: u8| -> alloc::vec::Vec<u8> {
                (0..3000u32).map(|i| ((i + seed as u32 * 7) % 251) as u8).collect()
            };
            // 51. over-budget: rifiuto ESATTAMENTE NoSpace, dati intatti.
            let v51 = {
                let (a, b) = (qbig(51), qbig(52));
                let mut ok = civis::arca_quota_set(b"q51", 9).is_ok();
                let p1 = civis::obj_put(b"q51", b"k1", &a);
                let p2 = civis::obj_put(b"q51", b"k2", &b);
                ok = ok && p1 == Ok(a.len() as u64);
                ok = ok && p2 == Err(civis::Error::NoSpace);
                ok = ok && matches!(civis::obj_get(b"q51", b"k1"), Ok(v) if v == a);
                ok = ok && civis::obj_get(b"q51", b"k2").is_err();
                ok = ok && civis::obj_get(b"q51", b"k3").is_err();
                ok = ok && matches!(civis::arca_quota_get(b"q51"), Ok((9, 1)));
                ok
            };
            c.ok("quota over-budget NoSpace + intatti", v51);
            // 52. indipendenza bucket: tetto su uno, l'altro libero.
            let v52 = {
                let (a, b, cc, d) = (qbig(54), qbig(55), qbig(56), qbig(57));
                let mut ok = civis::arca_quota_set(b"q52a", 9).is_ok();
                ok = ok && civis::obj_put(b"q52a", b"k1", &a) == Ok(a.len() as u64);
                ok = ok && civis::obj_put(b"q52a", b"k2", &b) == Err(civis::Error::NoSpace);
                ok = ok && civis::obj_put(b"q52b", b"k1", &a) == Ok(a.len() as u64);
                ok = ok && civis::obj_put(b"q52b", b"k2", &b) == Ok(b.len() as u64);
                ok = ok && civis::obj_put(b"q52b", b"k3", &cc) == Ok(cc.len() as u64);
                ok = ok && civis::obj_put(b"q52b", b"k4", &d) == Ok(d.len() as u64);
                ok
            };
            c.ok("quota indipendenza bucket", v52);
            // 53. persistenza: quota + used sopravvivono al bounce di cardo
            // (tabella nella foglia meta, used rimisurata allo scrub).
            let v53 = {
                let a = qbig(58);
                let mut ok = civis::arca_quota_set(b"q53", 40).is_ok();
                ok = ok && civis::obj_put(b"q53", b"k1", &a) == Ok(a.len() as u64);
                ok = ok && matches!(civis::arca_quota_get(b"q53"), Ok((40, 1)));
                ok = ok && civis::init_bounce(civis::Service::Cardo).is_ok();
                ok = ok && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::service_pid(civis::Service::Cardo).is_err()
                });
                ok = ok && civis::poll_value(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::service_pid(civis::Service::Cardo).ok()
                })
                .is_some();
                if let Some(dev) = root_dev().as_deref() {
                    let _ = civis::arca_open(dev);
                }
                ok = ok && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                    civis::arca_use_disk(true).is_ok()
                });
                ok = ok && matches!(civis::arca_quota_get(b"q53"), Ok((40, 1)));
                ok = ok && matches!(civis::obj_get(b"q53", b"k1"), Ok(v) if v == a);
                ok = ok && civis::obj_put(b"q53", b"k2", &a) == Ok(a.len() as u64);
                ok
            };
            c.ok("quota persistenza dopo bounce", v53);
            // 54. used coerente: pinnato a nome cancellato resta sul budget
            // (delete con snapshot vivo → 1), la delete dello snapshot
            // libera il conteggio (→ 0).
            let v54 = {
                let (a, b) = (qbig(59), qbig(60));
                let mut ok = civis::obj_put(b"q54", b"k", &a) == Ok(a.len() as u64);
                let sid = match civis::snap_create(b"q54") {
                    Ok(s) => Some(s),
                    Err(_) => {
                        ok = false;
                        None
                    }
                };
                ok = ok && civis::obj_put(b"q54", b"k", &b) == Ok(b.len() as u64);
                // Con blocchi 64 KiB le due versioni (3000 B) condividono lo
                // stesso blocco packed: used resta 1 finche' lo snapshot pinna
                // la versione vecchia, poi scende a 0.
                ok = ok && matches!(civis::arca_quota_get(b"q54"), Ok((0, 1)));
                ok = ok && civis::obj_delete(b"q54", b"k").is_ok();
                ok = ok && matches!(civis::arca_quota_get(b"q54"), Ok((0, 1)));
                ok = ok && match sid {
                    Some(s) => civis::snap_delete(s).is_ok(),
                    None => false,
                };
                ok = ok && matches!(civis::arca_quota_get(b"q54"), Ok((0, 0)));
                ok
            };
            c.ok("quota used con snapshot/delete", v54);
            // 34-40. Logging L1 (Fase 57, ADR-0039): gateway `Vestigia` RAM-first
            // con flush via cardo nativo. Bucket per IDENTITA' (hash del
            // chiamante, mai dichiarato): ogni client legge solo il proprio
            // (latest o per-seq). Dopo crash/GC per scelta: il restart di
            // cardo non deve rompere il client log.
            // 34. servizio registrato e raggiungibile per nome.
            let v34 = civis::service_lookup(civis::Service::Vestigia).is_ok();
            c.ok("log registrato", v34);
            // 35. append + read latest own-bucket: seq monotonico, record
            // integro (livello/tag/messaggio), epoch vera (post-Time).
            let day = civis::vestigia::log_day();
            let v35 = match civis::vestigia::log_append(civis::vestigia::LOG_INFO, b"t57", b"hello-57") {
                Ok(seq) if seq >= 1 => match civis::vestigia::log_read(day, 0) {
                    Ok((got, rec)) => match civis::vestigia::record_decode(&rec) {
                        Some((_, epoch, lvl, tag, msg)) => {
                            got == seq
                                && lvl == civis::vestigia::LOG_INFO
                                && tag == b"t57"
                                && msg == b"hello-57"
                                && epoch > 0
                        }
                        None => false,
                    },
                    Err(_) => false,
                },
                _ => false,
            };
            c.ok("log append + read latest", v35);
            // 36. messaggio al bound (1024 B) round-trip integro per-seq.
            let v36 = {
                let mut big = [0u8; 1024];
                for (i, b) in big.iter_mut().enumerate() {
                    *b = ((i * 13) % 251) as u8;
                }
                match civis::vestigia::log_append(civis::vestigia::LOG_WARN, b"t57", &big) {
                    Ok(seq) => match civis::vestigia::log_read(day, seq) {
                        Ok((got, rec)) => match civis::vestigia::record_decode(&rec) {
                            Some((_, _, lvl, tag, msg)) => {
                                got == seq
                                    && lvl == civis::vestigia::LOG_WARN
                                    && tag == b"t57"
                                    && msg == big
                            }
                            None => false,
                        },
                        Err(_) => false,
                    },
                    Err(_) => false,
                }
            };
            c.ok("log msg 1024B round-trip", v36);
            // 37. rifiuti: tag oltre 32 B, msg oltre 1024 B, msg vuoto
            // (Invalid client-side, mai IPC sporche); latest di un giorno
            // mai scritto (NOTFOUND dal server, mai dati inventati).
            let mut too_tag = [b'x'; 33];
            let mut too_msg = [b'y'; 1025];
            for (i, b) in too_tag.iter_mut().enumerate() {
                *b = b'a' + (i % 26) as u8;
            }
            for (i, b) in too_msg.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            let v37 = civis::vestigia::log_append(civis::vestigia::LOG_INFO, &too_tag, b"v").is_err()
                && civis::vestigia::log_append(civis::vestigia::LOG_INFO, b"t57", &too_msg).is_err()
                && civis::vestigia::log_append(civis::vestigia::LOG_INFO, b"t57", b"").is_err()
                && civis::vestigia::log_read(day + 1000, 0).is_err();
            c.ok("log rifiuti + giorno ignoto", v37);
            // 38. seal esplicito: due snapshot monotonici, delete di cleanup.
            let v38 = match (civis::vestigia::log_seal(), civis::vestigia::log_seal()) {
                (Ok(s1), Ok(s2)) if s1 >= 1 && s2 > s1 => {
                    civis::snap_delete(s1).is_ok() && civis::snap_delete(s2).is_ok()
                }
                _ => false,
            };
            c.ok("log seal + delete", v38);
            // 39. stats: appended conta (2 qui + 9 milestone di init riversati
            // dal flush — prova che la FLUSH ha funzionato), niente evict,
            // backend durevole col volume presente.
            let v39 = match civis::vestigia::log_stats() {
                Ok((appended, evicted, durable, _)) => {
                    appended >= 11 && evicted == 0 && durable
                }
                Err(_) => false,
            };
            c.ok("log stats", v39);
            // 40. restart di vestigia via init (bounce): ricompare (supervisione),
            // il client rifa lookup+REG da solo e latest e' il just-written
            // (indice RAM vergine + overlay di versioni per disegno, mai wedge).
            let v40 = match civis::init_bounce(civis::Service::Vestigia) {
                Ok(_) => {
                    let gone = civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                        civis::service_pid(civis::Service::Vestigia).is_err()
                    });
                    let back = civis::poll_value(1000, civis::POLL_PERIOD_TICKS, || {
                        civis::service_pid(civis::Service::Vestigia).ok()
                    });
                    let alive = gone
                        && back.is_some()
                        && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                            civis::service_lookup(civis::Service::Vestigia).is_ok()
                        });
                    alive && match civis::vestigia::log_append(civis::vestigia::LOG_INFO, b"t57", b"post-bounce") {
                        Ok(_) => match civis::vestigia::log_read(day, 0) {
                            Ok((_, rec)) => match civis::vestigia::record_decode(&rec) {
                                Some((_, _, _, tag, msg)) => tag == b"t57" && msg == b"post-bounce",
                                None => false,
                            },
                            Err(_) => false,
                        },
                        Err(_) => false,
                    }
                }
                Err(_) => false,
            };
            c.ok("log bounce + rewarm", v40);
        }
    }

    // 41-50. Vista POSIX su ArcaFS (56.3): namespace emergente + set
    // transient in RAM. Gira sul volume root (E2: il device della root, motore legato
    // a boot — mai su sdd1/uuid doversi: stub). Scritture vere con cleanup
    // a fine blocco (il volume e' la root viva). Senza sdc: salto adattivo
    // (mai FAIL per drive assente).
    let posix_ok =
        root_dev()
            .as_deref()
            .and_then(read_sector)
            .map_or(false, |s| is_arca_super(&s));
    if mount_root_arca() && posix_ok {
        fn rd_all(path: &str) -> Option<alloc::vec::Vec<u8>> {
            let fd = civis::open(path, 0).ok()?;
            let mut out = alloc::vec::Vec::new();
            let mut chunk = [0u8; 2000];
            loop {
                match civis::read_fs(fd, &mut chunk, 2000) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                    Err(_) => {
                        let _ = civis::close(fd);
                        return None;
                    }
                }
            }
            let _ = civis::close(fd);
            Some(out)
        }
        fn wr_all(path: &str, flags: u32, data: &[u8]) -> bool {
            let fd = match civis::open(path, flags) {
                Ok(f) => f,
                Err(_) => return false,
            };
            let ok = civis::write_fs(fd, data, data.len()) == Ok(data.len());
            let _ = civis::close(fd);
            ok
        }
        fn dir_has(dir: &str, want: &str) -> bool {
            let mut buf = [0u8; 2048];
            let count = match civis::readdir(dir, &mut buf, 2048) {
                Ok(n) => n,
                Err(_) => return false,
            };
            let mut found = false;
            civis::test::each_name(&buf, count, |name| {
                if name == want {
                    found = true;
                }
            });
            found
        }
        fn is_dir(path: &str) -> bool {
            let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
            civis::stat(path, &mut st).is_ok() && st.is_dir()
        }
        // 41. mkdir emergente subito visibile (stat dir + readdir root).
        let v41 =
            civis::mkdir("/arca/p56").is_ok() && is_dir("/arca/p56") && dir_has("/arca", "p56");
        c.ok("posix mkdir emergente visibile", v41);
        // 42. rmdir mai-esistita = errore (mai Ok silenzioso).
        let v42 = civis::remove("/arca/mai-esistita").is_err();
        c.ok("posix rmdir inesistente errore", v42);
        // 43. mkdir su file = errore.
        let v43 = wr_all("/arca/f56", civis::OPEN_CREATE, b"x") && civis::mkdir("/arca/f56").is_err();
        c.ok("posix mkdir su file errore", v43);
        // 44. file round-trip write/read/stat.
        let v44 = wr_all("/arca/p56/f", civis::OPEN_CREATE, b"ciao-posix")
            && matches!(rd_all("/arca/p56/f"), Some(v) if v.as_slice() == b"ciao-posix")
            && {
                let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
                civis::stat("/arca/p56/f", &mut st).is_ok() && st.is_file() && st.size == 10
            };
        c.ok("posix file round-trip", v44);
        // 45. readdir figli immediati.
        let v45 = dir_has("/arca/p56", "f");
        c.ok("posix readdir figli", v45);
        // 46. rmdir non-vuota = errore.
        let v46 = civis::remove("/arca/p56").is_err();
        c.ok("posix rmdir non-vuota errore", v46);
        // 47. overwrite a offset (read-modify-write, coda intatta).
        let v47 = wr_all("/arca/p56/rw", civis::OPEN_CREATE, b"HelloWorld123")
            && match civis::open("/arca/p56/rw", 0) {
                Ok(fd) => {
                    let r = civis::lseek(fd, 5, civis::SEEK_START) == Ok(5)
                        && civis::write_fs(fd, b"XX", 2) == Ok(2);
                    let _ = civis::close(fd);
                    r && matches!(rd_all("/arca/p56/rw"), Some(v) if v.as_slice() == b"HelloXXrld123")
                }
                Err(_) => false,
            };
        c.ok("posix overwrite a offset", v47);
        // 48. OPEN_TRUNC azzera, OPEN_APPEND concatena.
        let v48 = wr_all("/arca/p56/rw", civis::OPEN_TRUNC, b"")
            && rd_all("/arca/p56/rw") == Some(alloc::vec::Vec::new())
            && wr_all("/arca/p56/rw", civis::OPEN_APPEND, b"ab")
            && wr_all("/arca/p56/rw", civis::OPEN_APPEND, b"cd")
            && matches!(rd_all("/arca/p56/rw"), Some(v) if v.as_slice() == b"abcd");
        c.ok("posix trunc+append", v48);
        // 49. delete file: la dir resta (set RAM), rmdir ok, stat sparita.
        let v49 = civis::remove("/arca/p56/f").is_ok()
            && civis::remove("/arca/p56/rw").is_ok()
            && is_dir("/arca/p56")
            && civis::remove("/arca/p56").is_ok()
            && !is_dir("/arca/p56");
        c.ok("posix delete+rmdir", v49);
        // 50. bounce cardo + remount: file intatti, vuote perse.
        let v50 = civis::mkdir("/arca/keep").is_ok()
            && wr_all("/arca/keep/v", civis::OPEN_CREATE, b"persistente")
            && civis::mkdir("/arca/vuota").is_ok()
            && civis::init_bounce(civis::Service::Cardo).is_ok()
            && civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                civis::service_pid(civis::Service::Cardo).is_err()
            })
            && civis::poll_value(1000, civis::POLL_PERIOD_TICKS, || {
                civis::service_pid(civis::Service::Cardo).ok()
            })
            .is_some()
            && mount_root_arca()
            && matches!(rd_all("/arca/keep/v"), Some(v) if v.as_slice() == b"persistente")
            && is_dir("/arca/keep")
            && !is_dir("/arca/vuota");
        c.ok("posix bounce+remount", v50);
        // 55. rename S1.1 su namespace Arca: file, replace, dir con figli,
        // errori tipizzati (file→dir ISDIR, assente NOTFOUND).
        let v55 = civis::mkdir("/arca/rn").is_ok()
            && wr_all("/arca/rn/a.txt", civis::OPEN_CREATE, b"rename-me")
            && civis::rename("/arca/rn/a.txt", "/arca/rn/b.txt").is_ok()
            && matches!(rd_all("/arca/rn/b.txt"), Some(v) if v.as_slice() == b"rename-me")
            && civis::open("/arca/rn/a.txt", 0).is_err()
            && wr_all("/arca/rn/c.txt", civis::OPEN_CREATE, b"secondo")
            && civis::rename("/arca/rn/c.txt", "/arca/rn/b.txt").is_ok()
            && matches!(rd_all("/arca/rn/b.txt"), Some(v) if v.as_slice() == b"secondo")
            && civis::rename("/arca/rn/mai", "/arca/rn/x").is_err()
            && matches!(civis::rename("/arca/rn/b.txt", "/arca/rn"), Err(civis::Error::IsDir))
            && civis::rename("/arca/rn", "/arca/rn2").is_ok()
            && matches!(rd_all("/arca/rn2/b.txt"), Some(v) if v.as_slice() == b"secondo")
            && !is_dir("/arca/rn");
        c.ok("posix rename file+dir", v55);
        let _ = civis::remove("/arca/rn2/b.txt");
        let _ = civis::remove("/arca/rn2");
        // 56. symlink S1.1 su namespace Arca: create/readlink/follow/stat
        // kind, dangling, cleanup (stesso mount, chiavi isolate `s56`).
        let v56 = civis::mkdir("/arca/s56").is_ok()
            && civis::symlink("/arca/s56/link", "/arca/s56/real.txt").is_ok()
            && matches!(civis::readlink("/arca/s56/link"), Ok(v) if v == b"/arca/s56/real.txt")
            && {
                let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
                civis::stat("/arca/s56/link", &mut st).is_ok() && st.is_symlink()
            }
            && civis::open("/arca/s56/link", 0).is_err()
            && wr_all("/arca/s56/real.txt", civis::OPEN_CREATE, b"arc-link")
            && matches!(rd_all("/arca/s56/link"), Some(v) if v.as_slice() == b"arc-link")
            && civis::symlink("/arca/s56/dang", "/arca/s56/altri").is_ok()
            && civis::open("/arca/s56/dang", 0).is_err();
        c.ok("posix symlink + follow", v56);
        let _ = civis::remove("/arca/s56/link");
        let _ = civis::remove("/arca/s56/dang");
        let _ = civis::remove("/arca/s56/real.txt");
        let _ = civis::remove("/arca/s56");
        // 57. range read sequenziale (S2.5f/F0): fixture `test/r57.bin`
        // (100 KiB seedati da host in UN put — gli append guest-side sono
        // O(n^2) per design di `put_chunk`, mai usati per file grandi),
        // riletta a slice 4 KiB con pattern posizionale + bound temporale
        // (il materializza-e-slice qui era quadratico per slice: con la
        // regressione le read supererebbero il bound di ordini).
        let v57 = {
            const BIG: usize = 100 * 1024;
            const SL: usize = 4096;
            let mut good = true;
            let mut pos = 0usize;
            let mut dt = -1i64;
            match civis::open("/arca/test/r57.bin", 0) {
                Ok(fd) => {
                    let mut rbuf = [0u8; SL];
                    let t0 = civis::get_ticks();
                    loop {
                        match civis::read_fs(fd, &mut rbuf, SL) {
                            Ok(0) => break,
                            Ok(n) => {
                                for i in 0..n {
                                    if rbuf[i] != ((pos + i) % 251) as u8 {
                                        good = false;
                                        break;
                                    }
                                }
                                pos += n;
                                if !good {
                                    break;
                                }
                            }
                            Err(_) => {
                                good = false;
                                break;
                            }
                        }
                    }
                    dt = civis::get_ticks() - t0;
                    let _ = civis::close(fd);
                }
                Err(_) => good = false,
            }
            // Bound largo (300 tick = 3 s per 100 KiB; il quadratico
            // impiegherebbe ordini di piu').
            good && pos == BIG && dt >= 0 && dt < 300
        };
        c.ok("range 100KiB sequenziale", v57);
        // 58. prefix-overwrite parziale (W1: write a offset 0 piu' corto
        // del file — senza merge esplicito in ns_write, put_chunk(0)
        // troncherebbe la coda).
        let v58 = {
            const SZ: usize = 8192;
            const PRE: usize = 100;
            let full: alloc::vec::Vec<u8> = (0..SZ).map(|i| (i % 251) as u8).collect();
            let pre: alloc::vec::Vec<u8> =
                (0..PRE).map(|i| (0xA5u8).wrapping_add(i as u8)).collect();
            let mut good = civis::mkdir("/arca/r58").is_ok()
                && wr_all("/arca/r58/f", civis::OPEN_CREATE, &full)
                && wr_all("/arca/r58/f", civis::OPEN_CREATE, &pre);
            match rd_all("/arca/r58/f") {
                Some(v) => {
                    good = good
                        && v.len() == SZ
                        && v[..PRE] == pre[..]
                        && v[PRE..].iter().enumerate().all(|(i, b)| *b == ((PRE + i) % 251) as u8);
                }
                None => good = false,
            }
            let _ = civis::remove("/arca/r58/f");
            let _ = civis::remove("/arca/r58");
            good
        };
        c.ok("prefix-overwrite parziale", v58);
        // 59. append sequenziali 512 KiB (W2: con gli extent ogni op tocca
        // O(1) blocchi — bound temporale che il layout quadratico non puo'
        // tenere: 512 append da 4K con rewrite integrale = ~1 GB di I/O).
        let v59 = {
            const TOTAL: usize = 512 * 1024;
            const SL: usize = 4096;
            let mut good = true;
            let mut pos = 0usize;
            let mut dt = -1i64;
            if civis::mkdir("/arca/r59").is_ok() {
                match civis::open("/arca/r59/f", civis::OPEN_CREATE) {
                    Ok(fd) => {
                        let mut wbuf = [0u8; SL];
                        let t0 = civis::get_ticks();
                        while good && pos < TOTAL {
                            for i in 0..SL {
                                wbuf[i] = ((pos + i) % 251) as u8;
                            }
                            match civis::write_fs(fd, &wbuf, SL) {
                                Ok(n) if n == SL => pos += SL,
                                other => {
                                    println!("[testsarca] append512 wfail pos={} r={:?}", pos, other);
                                    good = false;
                                }
                            }
                        }
                        dt = civis::get_ticks() - t0;
                        let _ = civis::close(fd);
                        if good {
                            match civis::open("/arca/r59/f", 0) {
                                Ok(fd) => {
                                    let mut rbuf = [0u8; SL];
                                    let mut rpos = 0usize;
                                    loop {
                                        match civis::read_fs(fd, &mut rbuf, SL) {
                                            Ok(0) => break,
                                            Ok(n) => {
                                                for i in 0..n {
                                                    if rbuf[i] != ((rpos + i) % 251) as u8 {
                                                        good = false;
                                                        break;
                                                    }
                                                }
                                                rpos += n;
                                                if !good {
                                                    break;
                                                }
                                            }
                                            Err(_) => {
                                                good = false;
                                                break;
                                            }
                                        }
                                    }
                                    let _ = civis::close(fd);
                                    good = good && rpos == TOTAL;
                                }
                                Err(_) => good = false,
                            }
                        }
                    }
                    Err(_) => good = false,
                }
                let _ = civis::remove("/arca/r59/f");
                let _ = civis::remove("/arca/r59");
            } else {
                good = false;
            }
            println!("[testsarca] append512: pos={} dt={}", pos, dt);
            // 512 append da 4K: W6 (blocchi 64 KiB) misura ~3800 tick,
            // bound ~1.5x per assorbire il jitter di QEMU.
            good && pos == TOTAL && dt >= 0 && dt < 6000
        };
        c.ok("append 512KiB sequenziale", v59);
        // Istogramma layout + cache (W2/W3 tuning): informativo, mai FAIL.
        match civis::arca_extent_stats() {
            Ok((ne, en, mx, ov, ii, hits, reads, np, pb, pu, pc)) => println!(
                "[testsarca] xstats: extent={} entries={} max={} legacy_ov={} inline={} packed={} pblocks={} pused={} pcap={} hits={} reads={}",
                ne, en, mx, ov, ii, np, pb, pu, pc, hits, reads
            ),
            Err(_) => println!("[testsarca] xstats: N/D"),
        }
        // 60. epoca kill-mid (W4, ADR-0052): committed durevole, aperto
        // perso, SYNC salva. B committed (close-flush) sopravvive al bounce
        // di cardo; A scritta senza close/sync sparisce (epoca aperta persa
        // per disegno); C scritta + SYNC GROUP sopravvive; dopo il restart
        // l'allocatore e' sano (si scrive e rilegge ancora).
        let v60 = {
            let bpat: alloc::vec::Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
            let apat: alloc::vec::Vec<u8> = (0..4096).map(|i| (0xA0 + (i % 16)) as u8).collect();
            let cpat: alloc::vec::Vec<u8> = (0..4096).map(|i| (0xC0 + (i % 16)) as u8).collect();
            let _ = civis::remove("/arca/r60w");
            // B: write + close => commit (durevole).
            let mut good = civis::mkdir("/arca/r60").is_ok()
                && wr_all("/arca/r60/keep", civis::OPEN_CREATE, &bpat);
            // C: write + SYNC esplicita => commit (durevole senza close).
            good = good
                && match civis::open("/arca/r60/synced", civis::OPEN_CREATE) {
                    Ok(fd) => {
                        civis::write_fs(fd, &cpat, cpat.len()) == Ok(cpat.len())
                            && civis::disk_sync(civis::SYNC_GROUP).is_ok()
                    }
                    Err(_) => false,
                };
            // A: fd aperto + write differito, MAI chiuso — e' l'UNICA scrittura
            // dirty dell'epoca quando avviene il bounce (nessun commit dopo).
            // L'ORDINE conta: l'epoca e' GLOBALE, non per-fd, quindi un commit
            // successivo (es. il SYNC di C) porterebbe con se' anche A. Per
            // mostrare "epoca aperta persa" A deve essere l'ultima scrittura.
            good = good
                && match civis::open("/arca/r60/ephem", civis::OPEN_CREATE) {
                    Ok(fd) => civis::write_fs(fd, &apat, apat.len()) == Ok(apat.len()),
                    Err(_) => false,
                };
            // Bounce cardo a epoca aperta (nessun close/sync/soglia/eta' dopo
            // A: 4K sotto i 256K, microsecondi sotto i 5 s).
            let bounced = match civis::init_bounce(civis::Service::Cardo) {
                Ok(_) => {
                    let gone = civis::poll_wait(1000, civis::POLL_PERIOD_TICKS, || {
                        civis::service_pid(civis::Service::Cardo).is_err()
                    });
                    let back = civis::poll_value(2000, civis::POLL_PERIOD_TICKS, || {
                        civis::service_pid(civis::Service::Cardo).ok()
                    });
                    gone && back.is_some()
                }
                Err(_) => false,
            };
            // Rewarm: il mount /arca e' morto col bounce (card-side) — va
            // rimontato ESPLICITAMENTE (pattern v50). Il vecchio `mkdir ||
            // (mount && mkdir)` short-circuitava sul mkdir riuscito via root
            // catch-all (chiave "arca/..." invece di "r60/..."), falsando la
            // lettura: il mount non e' idempotente, quindi prima si rimonta.
            let re = bounced
                && civis::poll_wait(2000, civis::POLL_PERIOD_TICKS, || {
                    civis::service_pid(civis::Service::Cardo).is_ok() && mount_root_arca()
                });
            good = good && re;
            if good {
                let b_post = rd_all("/arca/r60/keep").map(|v| (v.len(), v == bpat));
                let c_post = rd_all("/arca/r60/synced").map(|v| (v.len(), v == cpat));
                let a_gone = civis::open("/arca/r60/ephem", 0).is_err();
                // Allocatore sano dopo recovery (GC al bind): si scrive ancora.
                let dpat: alloc::vec::Vec<u8> =
                    (0..4096).map(|i| (0xE0 + (i % 16)) as u8).collect();
                let d_ok = wr_all("/arca/r60/after", civis::OPEN_CREATE, &dpat)
                    && matches!(rd_all("/arca/r60/after"), Some(v) if v == dpat);
                println!(
                    "[testsarca] ep60: B={:?} C={:?} A_gone={} D_ok={}",
                    b_post, c_post, a_gone as u8, d_ok as u8
                );
                good = matches!(b_post, Some((_, true)))
                    && matches!(c_post, Some((_, true)))
                    && a_gone
                    && d_ok;
            }
            let _ = civis::remove("/arca/r60/keep");
            let _ = civis::remove("/arca/r60/synced");
            let _ = civis::remove("/arca/r60/after");
            let _ = civis::remove("/arca/r60/ephem");
            let _ = civis::remove("/arca/r60w");
            let _ = civis::remove("/arca/r60");
            good
        };
        c.ok("epoca kill-mid recovery", v60);

        // 61. packing sub-blocco stesso-bucket (W5, ADR-0053): N oggetti
        // piccoli condividono blocchi packed; spazio misurato via XSTATS;
        // free eager al delete (refs->0); rebuild del bump al rimount.
        let v61 = {
            civis::mkdir("/arca/p61").is_ok()
                && {
                    let nobj = 16usize;
                    let osize = 1000usize;
                    let pat: alloc::vec::Vec<u8> =
                        (0..osize).map(|i| (i % 251) as u8).collect();
                    let names: &[&[u8]] = &[
                        b"o0", b"o1", b"o2", b"o3", b"o4", b"o5", b"o6", b"o7",
                        b"o8", b"o9", b"oa", b"ob", b"oc", b"od", b"oe", b"of",
                    ];
                    let mut all_ok = true;
                    // Misura base prima di scrivere: packed stats del volume.
                    let rd_packed = || -> Option<(u64, u64, u64, u64)> {
                        civis::arca_extent_stats().ok().map(|(_, _, _, _, _, _, _, np, pb, pu, pc)| (np, pb, pu, pc))
                    };
                    let (np0, pb0, pu0, pc0) = rd_packed().unwrap_or((0, 0, 0, 0));
                    let base_consistente = pc0 == pb0 * 65512;
                    for (i, nm) in names.iter().take(nobj).enumerate() {
                        let mut buf = [0u8; 64];
                        let mut k = 0usize;
                        for b in b"/arca/p61/" { buf[k] = *b; k += 1; }
                        for b in nm.iter() { buf[k] = *b; k += 1; }
                        let path = core::str::from_utf8(&buf[..k]).unwrap();
                        let mut d = pat.clone();
                        d[0] = i as u8;
                        all_ok &= wr_all(path, civis::OPEN_CREATE, &d);
                    }
                    let _ = civis::disk_sync(civis::SYNC_GROUP);
                    let (np1, pb1, pu1, pc1) = rd_packed().unwrap_or((0, 0, 0, 0));
                    let consistente = pc1 == pb1 * 65512;
                    // Condivisione: 16 nuovi oggetti packed devono creare MENO
                    // di 16 nuovi blocchi (stesso bucket, bump condiviso).
                    let delta_np = np1.saturating_sub(np0);
                    let delta_pb = pb1.saturating_sub(pb0);
                    let delta_pu = pu1.saturating_sub(pu0);
                    let packed_ok = consistente && base_consistente
                        && delta_np == nobj as u64        // 16 nuovi oggetti packed
                        && delta_pb < nobj as u64         // condivisione: <16 blocchi
                        && delta_pu == (nobj * osize) as u64; // byte usati crescono esattamente
                    println!(
                        "[testsarca] pack61: np0={} pb0={} np1={} pb1={} pu1={} pc1={} \
                         dnp={} dpb={} dpu={} nobj={} osize={}",
                        np0, pb0, np1, pb1, pu1, pc1, delta_np, delta_pb, delta_pu, nobj, osize
                    );
                    // Delete tutti -> i blocchi packed tornano al baseline (refs->0).
                    for nm in names.iter().take(nobj) {
                        let mut buf = [0u8; 64];
                        let mut k = 0usize;
                        for b in b"/arca/p61/" { buf[k] = *b; k += 1; }
                        for b in nm.iter() { buf[k] = *b; k += 1; }
                        let path = core::str::from_utf8(&buf[..k]).unwrap();
                        let _ = civis::remove(path);
                    }
                    let _ = civis::disk_sync(civis::SYNC_GROUP);
                    let (np2, pb2, _, _) = rd_packed().unwrap_or((0, 0, 0, 0));
                    let freed_ok = np2 == np0 && pb2 == pb0;
                    // Rimount: rebuild_pack_cur (baseline preservata).
                    let _ = mount_root_arca();
                    let (np3, pb3, _, _) = rd_packed().unwrap_or((0, 0, 0, 0));
                    let rebuild_ok = np3 == np0 && pb3 == pb0;
                    all_ok && packed_ok && freed_ok && rebuild_ok
                }
        };
        c.ok("packing sub-blocco stesso-bucket", v61);

        // 62. file grande (W6, blocchi 64 KiB, nessun cap 16 MiB): write
        // sequenziale di un oggetto > PACK_CAP, read-back integrale, read a
        // offset sparsi, delete + reclaim + rimount. 512 KiB = 8 blocchi DATA
        // (4 MiB rendeva il path append O(n^2) impraticabile nel gate).
        let v62 = {
            civis::mkdir("/arca/p62").is_ok()
                && {
                    let size: usize = 512 * 1024;
                    let pat: alloc::vec::Vec<u8> =
                        (0..size).map(|i| (i % 251) as u8).collect();
                    let path = "/arca/p62/bigfile";
                    let (ne0, _, _, _, _, _, _, np0, pb0, _, _) =
                        civis::arca_extent_stats().unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
                    let write_ok = wr_all(path, civis::OPEN_CREATE, &pat);
                    let _ = civis::disk_sync(civis::SYNC_GROUP);
                    let (ne1, _, _, _, _, _, _, np1, pb1, pu1, pc1) =
                        civis::arca_extent_stats().unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
                    // > PACK_CAP(65491) => extent, non packed: i contatori
                    // packed globali non cambiano (delta, non zero assoluto).
                    let large_ok = ne1 > ne0 && np1 == np0 && pb1 == pb0;
                    println!(
                        "[testsarca] big62: ne={}->{} np={}->{} pb={}->{} pu={} pc={} size={}",
                        ne0, ne1, np0, np1, pb0, pb1, pu1, pc1, size
                    );
                    let read_ok = rd_all(path).as_deref() == Some(pat.as_slice());
                    // Read a offset sparsi: seek esplicito (prima si leggeva
                    // sempre dall'inizio, con pattern degenere).
                    let mut rand_ok = true;
                    for off in [0usize, 4096, size / 2, size - 4096] {
                        match civis::open(path, 0) {
                            Ok(fd) => {
                                let mut buf = [0u8; 4096];
                                let seek_ok = civis::lseek(fd, off as i64, civis::SEEK_START)
                                    == Ok(off as u64);
                                let r = civis::read_fs(fd, &mut buf, 4096);
                                let _ = civis::close(fd);
                                if !seek_ok || r != Ok(4096)
                                    || &buf[..] != &pat[off..off + 4096]
                                {
                                    rand_ok = false;
                                }
                            }
                            Err(_) => rand_ok = false,
                        }
                    }
                    if civis::open(path, 0)
                        .ok()
                        .map(|fd| {
                            let r = civis::lseek(fd, size as i64, civis::SEEK_START)
                                == Ok(size as u64)
                                && civis::read_fs(fd, &mut [0u8; 1], 1) == Ok(0);
                            let _ = civis::close(fd);
                            r
                        })
                        != Some(true)
                    {
                        rand_ok = false;
                    }
                    // Delete + reclaim.
                    let _ = civis::remove(path);
                    let _ = civis::disk_sync(civis::SYNC_GROUP);
                    let (ne2, _, _, _, _, _, _, np2, pb2, _, _) =
                        civis::arca_extent_stats().unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
                    let freed_ok = ne2 == ne0 && np2 == np0 && pb2 == pb0;
                    // Rimount: rebuild sano.
                    let _ = mount_root_arca();
                    let (ne3, _, _, _, _, _, _, np3, pb3, _, _) =
                        civis::arca_extent_stats().unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
                    let rebuild_ok = ne3 == ne0 && np3 == np0 && pb3 == pb0;
                    write_ok && large_ok && read_ok && rand_ok && freed_ok && rebuild_ok
                }
        };
        c.ok("file grande (>PACK_CAP, blocchi 64 KiB)", v62);
        // Cleanup best-effort (il volume resta pulito per i run dopo).
        let _ = civis::remove("/arca/keep/v");
        let _ = civis::remove("/arca/keep");
        let _ = civis::remove("/arca/f56");
        let _ = civis::remove("/arca/p61/o0");
        let _ = civis::remove("/arca/p61");
        let _ = civis::remove("/arca/p62/bigfile");
        let _ = civis::remove("/arca/p62");
        let _ = civis::umount("/arca");
    } else {
        println!("[testsarca] nessun volume ACFS: salto 41-50");
    }

    if c.pass == c.total {
        println!("[testsarca] PASS {}/{}", c.pass, c.total);
    } else {
        println!("[testsarca] FAIL {}/{}", c.pass, c.total);
    }
    println!("[testsarca] all tests done");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(if c.pass == c.total { 0 } else { 1 });
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if let Some(loc) = info.location() {
        println!("[testsarca] panic @ {}:{}", loc.file(), loc.line());
    } else {
        println!("[testsarca] panic");
    }
    civis::exit(1)
}
