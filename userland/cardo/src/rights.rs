use super::*;

// ── Diritti per-canale (Fase 17, self-restriction only) ─────────────
// Un `Channel` e' tutto-o-niente: chi ha l'id manda qualunque cosa. Primo
// passo verso IPC a capability, senza kernel (cardo conosce gia' ogni peer
// dal canale): tabella `chan → {ops bitmask, subtree prefix}`, SOLO in
// riduzione (DROP fa AND, mai widen, nessuna auth: nessuno puo' darsi
// diritti, solo toglierseli — nessun GRANT, i canali non sono trasferibili).
// Default (entry assente): {tetto policy, root} — ALL per i canali noti/TCB
// (suite invariata), restrittivo per gli ignoti (Fase 45). Zero alloc.
// Purge su EXIT_NOTIFY come rings/ftable. Effimeri: restart cardo =
// re-handshake full (limite dichiarato).
//
// Check su DUE livelli nel dispatch FS_NOTIFY:
// - ops bit: CENTRALE, prima di qualunque contatto handler/driver;
// - subtree: solo alle op con path (OPEN/MKDIR/READDIR/MOUNT-target/
//   UMOUNT-target); gli fd restano capability pure (read/write/close non
//   ricontrollano il path aperto).
// CLOSE sempre consentito (rilascia stato, mai escalation: nessun bit).
// DROP/GET sempre consentiti (gestire i propri diritti non si nega).
// Registrazione driver (FS_REGISTER, altro IPC tag) non gatata: handshake
// server-to-server, fuori dal modello self-restriction (limite dichiarato).

/// Diritti di un canale client: mask ops + subtree normalizzato senza slash
/// ("" = root).
#[derive(Clone)]
pub struct ChanRights {
    ops: u32,
    subtree: String,
}

/// Mask ops effettiva (default ALL a entry assente).
pub fn rights_ops(rights: &BTreeMap<u64, ChanRights>, chan: u64) -> u32 {
    rights.get(&chan).map_or(civis::RIGHTS_ALL, |r| r.ops)
}

/// Subtree effettivo (default root "" a entry assente).
pub fn rights_subtree<'a>(rights: &'a BTreeMap<u64, ChanRights>, chan: u64) -> &'a str {
    rights.get(&chan).map_or("", |r| r.subtree.as_str())
}

/// Normalizza subtree/path ("//fat//" → "fat", "/" o "" → "").
fn normalize_sub(path: &str) -> String {
    String::from(path.trim().trim_matches('/'))
}

/// Vista normalizzata (solo trim, ZERO alloc): per i CHECK per-op nel choke
/// point (ogni op con path la attraversa). Per gli STORE nella tabella diritti
/// (long-lived oltre la richiesta) resta `normalize_sub` owned.
pub fn normalize_sub_view(path: &str) -> &str {
    path.trim().trim_matches('/')
}

/// true se il path normalizzato `p` e' dentro il subtree `sub` ("" = root).
pub fn within_subtree(sub: &str, p: &str) -> bool {
    sub.is_empty()
        || p == sub
        || (p.len() > sub.len()
            && p.as_bytes().get(sub.len()) == Some(&b'/')
            && p.starts_with(sub))
}

/// Bit ops richiesto dall'op_tag. None = sempre consentito (CLOSE, DROP, GET,
/// CLAIM/CANCEL — gestire i propri fd/grant pendenti non si nega; CLAIM e
/// CANCEL non toccano path e operano solo su nonce propri o del parent).
pub fn op_bit(op_tag: u32) -> Option<u32> {
    match op_tag {
        R_OPEN => Some(civis::RIGHTS_OPEN),
        R_READ => Some(civis::RIGHTS_READ),
        R_WRITE => Some(civis::RIGHTS_WRITE),
        R_READDIR => Some(civis::RIGHTS_READDIR),
        R_MKDIR => Some(civis::RIGHTS_MKDIR),
        R_MOUNT => Some(civis::RIGHTS_MOUNT),
        R_UMOUNT => Some(civis::RIGHTS_UMOUNT),
        R_DELETE => Some(civis::RIGHTS_DELETE),
        R_LSEEK => Some(civis::RIGHTS_SEEK),
        // R_STAT e' metadato di listing: stesso bit di READDIR (Fase 19.2).
        R_STAT => Some(civis::RIGHTS_READDIR),
        // Topologia dischi (Fase 51, P2): lettura globale come il listing
        // (niente subtree: non e' un path del VFS).
        R_DISK_LIST | R_DISK_INFO => Some(civis::RIGHTS_READDIR),
        // Barriera di durabilita' (Fase 52, P3): bit proprio (anti
        // sync-storm: negabile senza negare le write, gia' stabili).
        R_SYNC => Some(civis::RIGHTS_SYNC),
        // Spazio del mount (Fase 52): lettura globale come R_STAT.
        R_STATVFS => Some(civis::RIGHTS_READDIR),
        // Hash contenuto (Fase 54): e' una lettura (come R_STAT), con subtree.
        R_GET_HASH => Some(civis::RIGHTS_READDIR),
        // Handoff fd e pipe (Fase 45): GRANT crea capability per altri
        // (negabile), PIPE crea stato condiviso nel server (negabile).
        R_DUP_GRANT => Some(civis::RIGHTS_GRANT),
        R_PIPE_CREATE => Some(civis::RIGHTS_PIPE),
        _ => None,
    }
}

/// R_RIGHTS_DROP: w0 = mask da tenere, payload = subtree (vuoto = solo-ops).
/// Solo shrink (ops &= mask&ALL); subtree sostituito solo se dentro il
/// corrente, altrimenti widen = None senza NESSUN cambio (prima valida, poi
/// applica). Crea l'entry partendo dal tetto policy (Fase 45: mai drop
/// verso l'alto da un canale restrittivo). Ritorna Some(0) o None.
pub fn handle_rights_drop(
    rights: &mut BTreeMap<u64, ChanRights>,
    chan: u64,
    keep: u32,
    payload: &[u8],
    ceiling: u32,
) -> Option<u64> {
    let sub = match core::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let (cur_ops, cur_sub) = match rights.get(&chan) {
        Some(r) => (r.ops, r.subtree.clone()),
        None => (ceiling, String::new()),
    };
    // Subtree richiesto (raw non-vuoto: "/" esplicita conta come richiesta di
    // root, NON come no-op — da "/fat" sarebbe widen e va rifiutata).
    let norm = if sub.is_empty() {
        None
    } else {
        let n = normalize_sub(sub);
        if !within_subtree(&cur_sub, &n) {
            return None;
        }
        Some(n)
    };
    let entry = rights.entry(chan).or_insert(ChanRights {
        ops: ceiling,
        subtree: String::new(),
    });
    entry.ops = cur_ops & (keep & civis::RIGHTS_ALL) & ceiling;
    if let Some(n) = norm {
        entry.subtree = n;
    }
    Some(0)
}

/// R_RIGHTS_GET: scrive il response frame `[ops:8][sublen:8][subtree]`
/// (self-written come read/readdir: il dispatch generico NON riscrive) e
/// ritorna Some(ops). Sempre consentito. Senza entry riporta il tetto
/// policy (Fase 45: il default visibile e' quello effettivo).
pub fn handle_rights_get(
    rights: &BTreeMap<u64, ChanRights>,
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    ceiling: u32,
) -> Option<u64> {
    let (ops, sub) = match rights.get(&chan) {
        Some(r) => (r.ops, r.subtree.as_str()),
        None => (ceiling, ""),
    };
    if rings.contains_key(&chan) {
        rings::map_client_resp_ring(rings, chan);
        rings::resp_ring_write(ops as u64, sub.len() as u64, sub.as_bytes());
    }
    Some(ops as u64)
}

/// Verifica EXECUTE su un oggetto ArcaFS (S3.2b-1): il canale ha sia READ
/// che EXECUTE per il bucket specificato? Payload `[bucket\0key\0]` nel ring;
/// reply w0 = 0 OK / ERR_DENIED. Zero allocazioni — usa buffer stack da 64 B
/// (bucket + key non superano mai i bound civis: 32+32).
pub fn handle_rights_execute(
    rights: &BTreeMap<u64, ChanRights>,
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    payload: &[u8],
) -> Option<u64> {
    // Parse payload: `[bucket_len:1][bucket\0][key_len:1][key\0]` (formato
    // identico a R_OBJ_GET/PUT, single source in `arcafs::wire`).
    if payload.is_empty() || payload.len() > 64 {
        return Some(civis::ERR_INVALID);
    }
    let mut off = 0usize;
    let bucket_len = payload[off] as usize;
    off += 1;
    if bucket_len == 0 || bucket_len >= off + 1 || off + bucket_len > payload.len() {
        return Some(civis::ERR_INVALID);
    }
    let _bucket = core::str::from_utf8(&payload[off..off + bucket_len]).ok()?;
    off += bucket_len + 1; // skip NUL delimiter
    if off >= payload.len() {
        return Some(civis::ERR_INVALID);
    }
    let key_len = payload[off] as usize;
    off += 1;
    if key_len == 0 || off + key_len > payload.len() {
        return Some(civis::ERR_INVALID);
    }
    // Non serve parsare il key: il controllo e' per-bucket (EXECUTE + READ).
    // Il key viene validato dallo spawn handler dopo questo check.

    // Controlla i diritti del canale: deve avere sia READ che EXECUTE.
    let ops = rights.get(&chan).map_or(civis::RIGHTS_ALL, |r| r.ops);
    let has_read = ops & civis::RIGHTS_READ != 0;
    let has_execute = ops & civis::RIGHTS_EXECUTE != 0;
    if has_read && has_execute {
        // OK: scrive reply nel response ring del client.
        if rings.contains_key(&chan) {
            rings::map_client_resp_ring(rings, chan);
            rings::resp_ring_write(0, 0, &[]); // w0=0 = OK, nessun payload
        }
        Some(0)
    } else {
        Some(civis::ERR_DENIED)
    }
}
