use super::*;

// ── Policy su identita' (Fase 45, sandbox build) ───────────────────
// La self-restriction (Fase 17) non vincola chi non droppa: qui cardo
// applica un TETTO server-side per hash misurato. Al primo handshake
// (FS_BUF_REG) il canale viene classificato UNA volta (peer_info = hash
// FNV-1a misurato dal kernel allo spawn, ADR-0027) e il tetto resta in cache
// fino all'EXIT_NOTIFY (stessa vita di rings/rights: niente TOCTOU oltre la
// purga esistente, niente syscall per-op nel choke point).
//
// Effettivo nel choke point: `drop_mask & ceiling` (la policy non allarga
// mai: un DROP resta irrevocabile, t34 invariato).
//
// Classificazione (ordine):
// 1. pid 1 (init): ALL. init e' escluso dal manifest (ciclo hash-di-se',
//    Fase 36) ma e' TCB spawnato dal kernel (mai riusato: la sua morte e'
//    panic). Senza, init non caricherebbe i binari da /fat.
// 2. hash noto al manifest servizi (`SERVICE_POLICY`, generato a build):
//    mask della riga (TCB = ALL, programmi di terzi = restrittiva).
// 3. hash noto ai test (`TEST_POLICY`, solo build test): ALL (la suite
//    esercita ogni op; i negativi stanno in t57 sul default).
// 4. ignoto: DEFAULT_UNKNOWN_OPS (niente MOUNT/UMOUNT/GRANT/PIPE — le op
//    che creano stato globale o capability per altri; file e dir restano
//    usabili). Fail-closed: a peer_info fallito, stesso default.
//
// La tabella test e' un file SEPARATO incluso SOLO da cardo: i binari test
// incorporano service_hashes (t51) ma NON test_policy, quindi niente ciclo
// (stesso motivo dell'esclusione rector/cardo in Fase 36).

/// Default fail-closed per hash ignoto (Fase 45, SYNC escluso in Fase 52):
/// ALL senza MOUNT (0x20), UMOUNT (0x40), GRANT (0x200), PIPE (0x400),
/// SYNC (0x800) = 0x19F. OPEN/READ/WRITE/READDIR/MKDIR/DELETE/SEEK restano:
/// un programma ignoto legge/scrive file ma non monta, non crea pipe, non
/// concede fd e non pilota FLUSH (anti sync-storm).
pub const DEFAULT_UNKNOWN_OPS: u32 = 0x19F;

/// Tetto ops per il canale `chan` (chiamato una volta all'handshake).
pub fn ceiling_for(chan: u64) -> u32 {    // (1) init: TCB pid 1, fuori manifest per costruzione.
    if civis::peer_pid(chan).unwrap_or(-1) == 1 {
        return civis::RIGHTS_ALL;
    }
    // (2+3) hash noto: cerca prima nei servizi, poi nei test.
    if let Ok(h) = civis::peer_info(chan) {
        for &(ph, mask) in SERVICE_POLICY {
            if ph == h {
                return mask;
            }
        }
        for &(ph, mask) in TEST_POLICY {
            if ph == h {
                return mask;
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
        // Codice di sistema, programmi utente, suite e namespace stesso.
        b"sys" | b"usr" | b"tst" | b"ns" => Some(civis::Service::Init),
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
