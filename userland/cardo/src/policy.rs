use super::*;

// ── Policy su identita' (Fase 45, sandbox build) ───────────────────
// La self-restriction (Fase 17) non vincola chi non droppa: qui cardo
// applica un TETTO server-side per identita' misurata. Al primo handshake
// (FS_BUF_REG) il canale viene classificato UNA volta (`peer_info` = `ImageId`
// BLAKE2s-128 misurata dal kernel allo spawn, ADR-0078) e il tetto resta in
// cache fino all'EXIT_NOTIFY (stessa vita di rings/rights: niente TOCTOU oltre
// la purga esistente, niente syscall per-op nel choke point).
//
// Effettivo nel choke point: `drop_mask & ceiling` (la policy non allarga
// mai: un DROP resta irrevocabile, t34 invariato).
//
// Classificazione (ordine):
// 1. pid 1 (init): ALL. init e' TCB spawnato dal kernel (mai riusato: la sua
//    morte e' panic).
// 2. identita' nota al manifest servizi (`sys:policy`, **dato** seedato a
//    build-time e letto dal volume all'avvio): mask della riga (TCB = ALL,
//    programmi di terzi = restrittiva).
// 3. identita' nota ai test (`sys:test-policy`, solo build test): ALL (la
//    suite esercita ogni op; i negativi stanno in t57 sul default).
// 4. ignoto: DEFAULT_UNKNOWN_OPS (niente MOUNT/UMOUNT/GRANT/PIPE — le op
//    che creano stato globale o capability per altri; file e dir restano
//    usabili). Fail-closed: a peer_info fallito, stesso default.
//
// ADR-0078 (fase 1): le tabelle non sono piu' compilate. Aggiungere un
// servizio o ricompilare un test non tocca cardo ne' il kernel.

/// Voci massime per tabella (servizi + test): il manifest reale ne ha ~25.
const POLICY_MAX: usize = 48;

/// `(identita', mask)` caricata da `sys:policy` / `sys:test-policy`.
static mut SERVICE_POLICY: [(civis::ImageId, u32); POLICY_MAX] =
    [(civis::ImageId { lo: 0, hi: 0 }, 0); POLICY_MAX];
static mut SERVICE_N: usize = 0;
static mut TEST_POLICY: [(civis::ImageId, u32); POLICY_MAX] =
    [(civis::ImageId { lo: 0, hi: 0 }, 0); POLICY_MAX];
static mut TEST_N: usize = 0;

/// Nomi (diagnostica) allineati alle due tabelle: `load_one` li riempie con la
/// quarta colonna della riga (`hi lo mask nome`).
static mut SERVICE_NAMES: [[u8; 24]; POLICY_MAX] = [[0; 24]; POLICY_MAX];
static mut TEST_NAMES: [[u8; 24]; POLICY_MAX] = [[0; 24]; POLICY_MAX];

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Hex di 1..=16 cifre (`hi`/`lo` sono 16, la mask 3-4).
fn hex_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &b in s {
        v = (v << 4) | hex_nibble(b)? as u64;
    }
    Some(v)
}

// ── Porte I/O dal manifest (ADR-0079, fase 4) ───────────────────────
// `sys:manifest` = `nome <blake2s-256 hex> <io>`; `<io>` = `-` o intervalli
// inclusivi esadecimali `start-end[,...]`. La tabella e' indicizzata
// dall'`ImageId` (contenuto): le porte seguono i byte, come la classe. Cardo
// la usa quando registra la trust entry (`SYS_TRUST_IMAGE`), cosi' il TSS del
// processo prende le porte dal manifest firmato e non da `SpawnMeta`.

static mut MANIFEST_IO: [(civis::ImageId, [(u16, u16); civis::TRUST_IO_MAX], u8); POLICY_MAX] =
    [(civis::ImageId { lo: 0, hi: 0 }, [(0, 0); civis::TRUST_IO_MAX], 0); POLICY_MAX];
static mut MANIFEST_IO_N: usize = 0;

/// Porte I/O dichiarate per `id` (`&[]` = nessuna). Lookup lineare: la tabella
/// e' piccola (una voce per servizio) e il chiamante e' gia' nel choke point.
pub fn manifest_io(id: civis::ImageId) -> &'static [(u16, u16)] {
    unsafe {
        let n = *core::ptr::addr_of!(MANIFEST_IO_N);
        let table = core::ptr::addr_of!(MANIFEST_IO)
            .cast::<(civis::ImageId, [(u16, u16); civis::TRUST_IO_MAX], u8)>();
        for i in 0..n {
            let e = &*table.add(i);
            if e.0 == id {
                return &e.1[..e.2 as usize];
            }
        }
    }
    &[]
}

/// Un intervallo `start-end` in esadecimale (inclusivo).
fn parse_io_range(s: &[u8]) -> Option<(u16, u16)> {
    let dash = s.iter().position(|&b| b == b'-')?;
    let start = hex_u64(&s[..dash])?;
    let end = hex_u64(&s[dash + 1..])?;
    if start > end || end > u16::MAX as u64 {
        return None;
    }
    Some((start as u16, end as u16))
}

/// Carica le porte dal manifest (`sys:manifest`, **dato** seedato a
/// build-time). Righe malformate vengono scartate loud (come per policy).
pub fn load_manifest_io(disk: &Option<btree_drv::DiskEngine>) {
    let Some(engine) = disk.as_ref() else {
        return;
    };
    let Some(data) = engine.get(b"sys", b"manifest") else {
        println!("[cardo] manifest assente: nessuna porta I/O dichiarata");
        return;
    };
    let mut n = 0usize;
    let mut bad = 0usize;
    for line in data.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let mut it = line.split(|&b| b == b' ');
        let (_name, Some(hex), Some(io_s)) = (it.next(), it.next(), it.next()) else {
            bad += 1;
            continue;
        };
        // Digest esadecimale: i primi 32 caratteri sono i 16 byte di `ImageId`.
        if hex.len() < 32 {
            bad += 1;
            continue;
        }
        let mut bytes = [0u8; 16];
        let mut ok = true;
        for i in 0..16 {
            let (Some(hi), Some(lo)) =
                (hex_nibble(hex[i * 2]), hex_nibble(hex[i * 2 + 1]))
            else {
                ok = false;
                break;
            };
            bytes[i] = (hi << 4) | lo;
        }
        if !ok {
            bad += 1;
            continue;
        }
        let id = civis::ImageId {
            lo: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            hi: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        };
        let mut ranges = [(0u16, 0u16); civis::TRUST_IO_MAX];
        let mut count = 0usize;
        if io_s != b"-" {
            for part in io_s.split(|&b| b == b',') {
                if count >= civis::TRUST_IO_MAX {
                    println!("[cardo] manifest: oltre {} range per una voce, resto ignorato", civis::TRUST_IO_MAX);
                    break;
                }
                match parse_io_range(part) {
                    Some(r) => {
                        ranges[count] = r;
                        count += 1;
                    }
                    None => {
                        println!("[cardo] manifest: range I/O invalido, voce scartata");
                        bad += 1;
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            continue;
        }
        if n >= POLICY_MAX {
            println!("[cardo] manifest: oltre {} voci I/O, resto ignorato", POLICY_MAX);
            break;
        }
        unsafe {
            core::ptr::write(
                core::ptr::addr_of_mut!(MANIFEST_IO)
                    .cast::<(civis::ImageId, [(u16, u16); civis::TRUST_IO_MAX], u8)>()
                    .add(n),
                (id, ranges, count as u8),
            );
        }
        n += 1;
    }
    unsafe { *core::ptr::addr_of_mut!(MANIFEST_IO_N) = n };
    println!("[cardo] manifest: {} voci I/O ({} righe scartate)", n, bad);
}

/// Riempie una tabella da un oggetto `sys:<key>` (`hi lo mask [nome]`).
fn load_one(
    disk: &btree_drv::DiskEngine,
    key: &[u8],
    out: *mut (civis::ImageId, u32),
    names: *mut [u8; 24],
    n_out: *mut usize,
    tag: &str,
) {
    let Some(data) = disk.get(b"sys", key) else {
        println!("[cardo] {} assente (sys:{})", tag, core::str::from_utf8(key).unwrap_or("?"));
        return;
    };
    let mut n = 0usize;
    for line in data.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let mut it = line.split(|&b| b == b' ');
        let (Some(hi_s), Some(lo_s), Some(mask_s)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Some(hi), Some(lo), Some(mask)) = (hex_u64(hi_s), hex_u64(lo_s), hex_u64(mask_s))
        else {
            continue;
        };
        if n >= POLICY_MAX {
            break;
        }
        unsafe {
            core::ptr::write(out.add(n), (civis::ImageId { lo, hi }, mask as u32));
            let mut name = [0u8; 24];
            if let Some(raw) = it.next() {
                let k = raw.len().min(24);
                name[..k].copy_from_slice(&raw[..k]);
            }
            core::ptr::write(names.add(n), name);
        }
        n += 1;
    }
    unsafe { *n_out = n };
    println!(
        "[cardo] {}: {} righe da sys:{} ({} B)",
        tag,
        n,
        core::str::from_utf8(key).unwrap_or("?"),
        data.len()
    );
}

/// Carica `sys:policy` e `sys:test-policy` (ADR-0078). Chiamata all'avvio,
/// dopo il bind del motore e prima del READY: nessun client e' ancora
/// classificato, quindi le tabelle sono complete prima del primo handshake.
/// cardo e' il server FS: legge dal proprio motore, non via IPC.
pub fn load_tables(disk: &Option<btree_drv::DiskEngine>) {
    let Some(engine) = disk.as_ref() else {
        println!("[cardo] policy: nessun volume, tabelle vuote (default restrittivo)");
        return;
    };
    unsafe {
        load_one(
            engine,
            b"policy",
            core::ptr::addr_of_mut!(SERVICE_POLICY).cast(),
            core::ptr::addr_of_mut!(SERVICE_NAMES).cast(),
            core::ptr::addr_of_mut!(SERVICE_N),
            "policy",
        );
        load_one(
            engine,
            b"test-policy",
            core::ptr::addr_of_mut!(TEST_POLICY).cast(),
            core::ptr::addr_of_mut!(TEST_NAMES).cast(),
            core::ptr::addr_of_mut!(TEST_N),
            "test-policy",
        );
    }
    load_anchor(disk);
    load_manifest_io(disk);
}

// ── Anchor delle chiavi di firma (ADR-0078, fase 2) ─────────────────────
// `sys:anchor` (dato seedato a build-time, come manifest/policy): righe
// `key_id_hex pubkey_hex classe nome`, con `classe` = `system|thirdparty`.
// La `key_id` dichiarata e' un indice, non una prova: viene RICALCOLATA dalla
// pubkey e una riga incoerente viene scartata loud (una chiave non entra
// nell'anchor per un errore di trascrizione).
//
// Anchor assente = tabella vuota = nessuna immagine firmata: i bucket
// obbligatori negano l'esecuzione (fail-closed), il boot non si rompe (i
// servizi sono spawati da init per object_id, non via `R_RIGHTS_EXECUTE`).

/// Chiavi di firma massime (due oggi: system + thirdparty di test).
const ANCHOR_MAX: usize = 8;

#[derive(Clone, Copy)]
pub struct AnchorKey {
    /// Primi 8 byte di BLAKE2s-256(pk), come nel sidecar.
    pub key_id: u64,
    /// Chiave pubblica Ed25519 autorevole.
    pub pk: [u8; sig::PUBLIC_KEY_LEN],
    /// Classe che questa chiave trasmette alle immagini che firma.
    pub class: civis::ImageClass,
}

static mut ANCHOR: [Option<AnchorKey>; ANCHOR_MAX] = [None; ANCHOR_MAX];

/// Carica `sys:anchor` (prima del READY, come le altre tabelle).
pub fn load_anchor(disk: &Option<btree_drv::DiskEngine>) {
    let Some(engine) = disk.as_ref() else {
        println!("[cardo] anchor: nessun volume, nessuna chiave (fail-closed)");
        return;
    };
    let Some(data) = engine.get(b"sys", b"anchor") else {
        println!("[cardo] anchor assente (sys:anchor): nessuna firma accettata");
        return;
    };
    let mut n = 0usize;
    let mut bad = 0usize;
    for line in data.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let mut it = line.split(|&b| b == b' ');
        let (Some(kid_s), Some(pk_s), Some(cls_s)) = (it.next(), it.next(), it.next()) else {
            bad += 1;
            continue;
        };
        let mut kid_b = [0u8; 8];
        let mut pk = [0u8; sig::PUBLIC_KEY_LEN];
        if !sig::hex_decode(kid_s, &mut kid_b) || !sig::hex_decode(pk_s, &mut pk) {
            println!("[cardo] anchor: riga con hex invalido, scartata");
            bad += 1;
            continue;
        }
        let declared = u64::from_be_bytes(kid_b);
        let real = sig::key_id(&pk);
        if declared != real {
            println!(
                "[cardo] anchor: key_id {:016x} != {:016x} della pubkey, riga scartata",
                declared, real
            );
            bad += 1;
            continue;
        }
        let class = match cls_s {
            b"system" => civis::ImageClass::System,
            b"thirdparty" => civis::ImageClass::ThirdParty,
            _ => {
                println!(
                    "[cardo] anchor: classe '{}' ignota, riga scartata",
                    core::str::from_utf8(cls_s).unwrap_or("?")
                );
                bad += 1;
                continue;
            }
        };
        if n >= ANCHOR_MAX {
            println!("[cardo] anchor: oltre {} chiavi, resto ignorato", ANCHOR_MAX);
            break;
        }
        unsafe {
            core::ptr::write(
                core::ptr::addr_of_mut!(ANCHOR).cast::<Option<AnchorKey>>().add(n),
                Some(AnchorKey { key_id: real, pk, class }),
            );
        }
        n += 1;
    }
    println!(
        "[cardo] anchor: {} chiavi da sys:anchor ({} righe scartate, {} B)",
        n,
        bad,
        data.len()
    );
}

/// Chiave autorevole per `key_id` (None = chiave ignota → firma rifiutata).
pub fn anchor_key(key_id: u64) -> Option<AnchorKey> {
    unsafe {
        let anchor = core::ptr::addr_of!(ANCHOR).cast::<Option<AnchorKey>>();
        for i in 0..ANCHOR_MAX {
            if let Some(k) = core::ptr::read(anchor.add(i)) {
                if k.key_id == key_id {
                    return Some(k);
                }
            }
        }
    }
    None
}

/// Default fail-closed per hash ignoto (Fase 45, SYNC escluso in Fase 52):
/// ALL senza MOUNT (0x20), UMOUNT (0x40), GRANT (0x200), PIPE (0x400),
/// SYNC (0x800) = 0x19F. OPEN/READ/WRITE/READDIR/MKDIR/DELETE/SEEK restano:
/// un programma ignoto legge/scrive file ma non monta, non crea pipe, non
/// concede fd e non pilota FLUSH (anti sync-storm).
pub const DEFAULT_UNKNOWN_OPS: u32 = 0x19F;

/// Nome diagnostico dell'identita' `id` (dal manifest), `"?"` se ignota.
pub fn name_of(id: civis::ImageId) -> &'static str {
    unsafe {
        let n = *core::ptr::addr_of!(SERVICE_N);
        for i in 0..n {
            let (pid, _) = *core::ptr::addr_of!(SERVICE_POLICY[i]);
            if pid == id {
                // `&'static` sul buffer statico (mai un temporaneo).
                let name: &'static [u8; 24] = &*core::ptr::addr_of!(SERVICE_NAMES[i]);
                let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                return core::str::from_utf8(&name[..len]).unwrap_or("?");
            }
        }
    }
    "?"
}

/// Tetto ops per il canale `chan` (chiamato una volta all'handshake).
pub fn ceiling_for(chan: u64) -> u32 {    // (1) init: TCB pid 1, fuori manifest per costruzione.
    if civis::peer_pid(chan).unwrap_or(-1) == 1 {
        return civis::RIGHTS_ALL;
    }
    // (2+3) identita' nota: cerca prima nei servizi, poi nei test.
    if let Ok(id) = civis::peer_info(chan) {
        unsafe {
            let n = *core::ptr::addr_of!(SERVICE_N);
            for i in 0..n {
                let (pid, mask) = *core::ptr::addr_of!(SERVICE_POLICY[i]);
                if pid == id {
                    return mask;
                }
            }
            let n = *core::ptr::addr_of!(TEST_N);
            for i in 0..n {
                let (pid, mask) = *core::ptr::addr_of!(TEST_POLICY[i]);
                if pid == id {
                    return mask;
                }
            }
        }
    }
    // (4) ignoto o canale morente: fail-closed.
    DEFAULT_UNKNOWN_OPS
}

// ── Bucket di fiducia protetti (ADR-0077, fase 0) ──────────────────
//
// Il tetto di policy decide QUALI op un peer puo' fare; qui si decide SU COSA.
// Senza questo guard `DEFAULT_UNKNOWN_OPS` (che include WRITE) lascia a
// qualunque processo la riscrittura di `sys:ca.der`, dei binari in `usr`, del
// namespace `ns` e di `identis:config`.
//
// Tre regole (in ordine, vedi ADR-0077):
//   1. op amministrative (snapshot/rollback/clone/delete): `ceiling == ALL`;
//   2. target protetti: `ceiling == ALL` oppure init (pid 1);
//   3. carve-out per owner di servizio (`identis:` → Identis, bucket `log` →
//      Vestigia): la classificazione e' fatta UNA volta all'handshake
//      (`classify`), come il tetto, e purgata all'EXIT_NOTIFY.
//
// `RIGHTS_ALL` e' la classificazione che gia' esiste: la hanno init, i
// servizi TCB (`SERVICE_POLICY`) e i binari della suite (`TEST_POLICY`).

/// Chi e' il peer, per il guard dei bucket.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// pid 1: il supervisore, seeda e amministra.
    Init,
    /// Owner di un servizio con un bucket proprio.
    Owner(civis::Service),
    /// Tutti gli altri (policy normale).
    Other,
}

/// Classifica il peer del canale (una volta per handshake).
///
/// Richiede che il servizio si sia registrato PRIMA del primo uso del FS
/// (identis e vestigia lo fanno in `real_main`, prima di aprire il bucket).
pub fn classify(chan: u64) -> Trust {
    let Ok(pid) = civis::peer_pid(chan) else {
        return Trust::Other;
    };
    if pid == 1 {
        return Trust::Init;
    }
    for svc in [civis::Service::Identis, civis::Service::Vestigia] {
        if civis::service_pid(svc).ok() == Some(pid) {
            return Trust::Owner(svc);
        }
    }
    Trust::Other
}

/// Owner richiesto da un bucket dell'API oggetti (None = non protetto).
pub fn protected_bucket(bucket: &[u8]) -> Option<civis::Service> {
    match bucket {
        // Codice di sistema, programmi utente, suite, namespace stesso e
        // **firme** (ADR-0078 fase 2: solo init scrive i sidecar).
        b"sys" | b"usr" | b"tst" | b"ns" | b"sig" => Some(civis::Service::Init),
        // Driver (ADR-0078 fase 2): un `vela` scrivibile da chiunque
        // permetteva di sostituire un driver firmato — la firma senza
        // write-protection del bucket che la contiene e' teatro.
        // L'unico writer *runtime* di `vela` è `seed_sys_disk` in server.rs:
        // cardo stesso, prima del READY, nessun client ancora esistente. È
        // intenzionale e non passa dal choke point (è il seed di boot, non
        // un client arbitrario); le immagini che ne escono sono comunque non
        // firmate -> classe Untrusted.
        b"vela" => Some(civis::Service::Init),
        // Bucket con un solo scrittore legittimo.
        b"log" => Some(civis::Service::Vestigia),
        b"identis" => Some(civis::Service::Identis),
        _ => None,
    }
}

/// Owner richiesto da un path del namespace (None = non protetto).
///
/// I path sono normalizzati come nel check subtree (`normalize_sub_view`):
/// `usr/bin/x` e `/usr/bin/x` sono lo stesso target.
pub fn protected_path(path: &str) -> Option<civis::Service> {
    let p = rights::normalize_sub_view(path);
    if p.starts_with("bin/") || p.starts_with("usr/bin/") || p.starts_with("test/") {
        return Some(civis::Service::Init);
    }
    if p.starts_with("identis:") {
        return Some(civis::Service::Identis);
    }
    None
}

/// Il peer puo' scrivere un target protetto di proprieta' `owner`?
fn allowed(owner: civis::Service, ceiling: u32, trust: Trust) -> bool {
    if ceiling == civis::RIGHTS_ALL || trust == Trust::Init {
        return true;
    }
    matches!(trust, Trust::Owner(s) if s == owner)
}

/// Op amministrative sul volume: solo TCB.
fn admin_op(op_tag: u32) -> bool {
    matches!(
        op_tag,
        civis::R_SNAP_CREATE
            | civis::R_SNAP_ROLLBACK
            | civis::R_SNAP_CLONE
            | civis::R_SNAP_DELETE
    )
}

/// Guard di scrittura: `true` = op consentita.
///
/// Chiamato nel choke point DOPO il tetto e il subtree, prima del dispatch
/// (il payload e' gia' nel buffer scratch). `w0`/`w1` sono i registri del
/// frame: per `R_WRITE` servono a recuperare il path dell'fd aperto.
pub fn write_allowed(
    op_tag: u32,
    w0: u64,
    w1: u64,
    payload: &[u8],
    ceiling: u32,
    trust: Trust,
    ftable: &ftable::FileTable,
    chan: u64,
) -> bool {
    if admin_op(op_tag) {
        return ceiling == civis::RIGHTS_ALL || trust == Trust::Init;
    }

    // API oggetti: il bucket e' nel prefisso `bucket\0key\0`.
    if matches!(op_tag, civis::R_OBJ_PUT | civis::R_OBJ_DELETE) {
        let bucket = match arcafs::wire::parse_obj_prefix(payload) {
            Some((b, _, _)) => b,
            None => return true, // malformato: lo rifiuta l'handler
        };
        return match protected_bucket(bucket) {
            Some(owner) => allowed(owner, ceiling, trust),
            None => true,
        };
    }

    // Scrittura su un fd gia' aperto: il target e' il path dell'fd.
    if op_tag == civis::R_WRITE {
        return match ftable.get(chan, w0 as u32) {
            Some((path, _, _, _)) => match protected_path(path) {
                Some(owner) => allowed(owner, ceiling, trust),
                None => true,
            },
            // Pipe, fd remoto (gia' servito prima) o fd ignoto: non e' un
            // path del namespace, nessun bucket da proteggere.
            None => true,
        };
    }

    // Op con path nel payload. `R_OPEN` conta solo con intento di scrittura:
    // un'apertura in sola lettura di un binario e' legittima (la shell la fa).
    let path_payload = match op_tag {
        civis::R_OPEN => {
            let flags = w1 as u32;
            if flags & (civis::OPEN_CREATE | civis::OPEN_TRUNC | civis::OPEN_APPEND) == 0 {
                return true;
            }
            payload
        }
        civis::R_DELETE | civis::R_MKDIR | civis::R_CHMOD => payload,
        // SYMLINK: il target e' opaco, conta il path del link.
        civis::R_SYMLINK => payload,
        civis::R_RENAME => payload,
        _ => return true,
    };

    let Ok(text) = core::str::from_utf8(path_payload) else {
        return true; // malformato: lo rifiuta l'handler
    };
    // RENAME porta due path: servono entrambi consentiti.
    for part in text.split('\0') {
        let p = part.trim_end_matches('\0');
        if p.is_empty() {
            continue;
        }
        if let Some(owner) = protected_path(p) {
            if !allowed(owner, ceiling, trust) {
                return false;
            }
        }
    }
    true
}
