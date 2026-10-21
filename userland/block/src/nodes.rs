use super::*;
pub(crate) use crate::part::PartLoc;

/// Lunghezza massima del nome nodo in un frame di resolve ("sda1" = 4;
/// bound difensivo: oltre e' spazzatura di un'epoca morta).
pub(crate) const DISK_MAX_NAME: usize = 16;

// ── Nodi ────────────────────────────────────────────────────────────

/// Nodo esposto: whole-disk o partizione (MBR/GPT, Fase 16 + Fase 55).
pub(crate) struct Node {
    /// Nome breve ("sda", "sda1"): prefix registrato = "/dev/" + nome.
    pub(crate) name: String,
    /// Handle codificato (disco<<16|sub, 0 = whole-disk): allocato qui,
    /// mai indovinato altrove.
    pub(crate) handle: u32,
    /// Seriale volume FAT (BPB sniff, Fase 16d): `UUID=` = hex maiuscolo
    /// 8 char. `None` = nodo senza identità stabile (non-FAT o senza firma).
    pub(crate) vol_uuid: Option<u32>,
    /// UUID a 64 bit del volume **ArcaFS** (superblock v4, ADR-0059):
    /// `UUID=` = 16 hex maiuscoli. `None` = il nodo non è un volume ArcaFS
    /// (o il superblock non verifica).
    pub(crate) vol_arca_uuid: Option<u64>,
    /// Label volume trimmata (BPB sniff, Fase 16d): `LABEL=`. `None` se
    /// vuota/assente. Mai con '/' (skippata in registrazione, difensivo).
    pub(crate) vol_label: Option<String>,
}

/// Sniffa l'identità FAT del settore 0 di un nodo (whole-disk: settore 0
/// fisico; partizione: settore `base`). Stesso bar di mount (`fat_bpb_identity`
/// in civis): il nodo annuncia UUID/label sse cardo lo monterebbe davvero.
pub(crate) fn sniff_identity(disk: &block::AtaDisk, base: u64) -> (Option<u32>, Option<String>) {
    let mut sec = [0u8; 512];
    if !disk.read_sector(base, &mut sec) {
        return (None, None);
    }
    match civis::fat_bpb_identity(&sec) {
        Some((serial, raw_label)) => {
            let mut n = raw_label.len();
            while n > 0 && raw_label[n - 1] == b' ' {
                n -= 1;
            }
            let label = if n == 0 {
                None
            } else {
                match core::str::from_utf8(&raw_label[..n]) {
                    Ok(s) if !s.contains('/') => Some(String::from(s)),
                    _ => None,
                }
            };
            (serial, label)
        }
        None => (None, None),
    }
}

/// Sniffa l'identità **ArcaFS** di un nodo (superblock v4): magic + versione
/// + block-size + checksum FNV-1a (gli stessi 4 check del mount, casa
/// `arcafs`). Ritorna l'uuid a 64 bit del volume, o `None` se il nodo non è
/// un volume ArcaFS (o il superblock non verifica: mai un uuid inventato).
pub(crate) fn sniff_arca_uuid(disk: &block::AtaDisk, base: u64) -> Option<u64> {
    let mut sec = [0u8; 512];
    if !disk.read_sector(base, &mut sec) {
        return None;
    }
    let sb = &sec[..civis::ARCA_SUPER_LEN];
    if sb[civis::ARCA_OFF_MAGIC..civis::ARCA_OFF_MAGIC + 4] != *civis::ARCA_MAGIC {
        return None;
    }
    let u32le = |o: usize| u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]]);
    let u64le = |o: usize| {
        u64::from_le_bytes([
            sb[o], sb[o + 1], sb[o + 2], sb[o + 3], sb[o + 4], sb[o + 5], sb[o + 6], sb[o + 7],
        ])
    };
    if u32le(civis::ARCA_OFF_VERSION) != civis::ARCA_VERSION {
        return None;
    }
    if u32le(civis::ARCA_OFF_BLOCK_SIZE) != civis::ARCA_BLOCK_SIZE {
        return None;
    }
    if civis::image_hash(&sb[..civis::ARCA_OFF_CHECK]) != u64le(civis::ARCA_OFF_CHECK) {
        return None;
    }
    Some(u64le(civis::ARCA_OFF_UUID))
}

/// Risolve una chiave (`"sda"`, UUID hex 8 char, label) in nodo (Fase 16d).
/// Priorità: nome esatto → UUID → label. Mai ambigua in pratica (nomi `sd*`
/// non sono hex-8 ne' label convenzionali maiuscole senza spazi... e a pari
/// merito vince il primo in tabella, deterministico per costruzione).
pub(crate) fn resolve_node<'a>(nodes: &'a [Node], key: &str) -> Option<&'a Node> {
    if let Some(n) = nodes.iter().find(|n| n.name == key) {
        return Some(n);
    }
    if key.len() == 8 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(v) = u32::from_str_radix(key, 16) {
            if let Some(n) = nodes.iter().find(|n| n.vol_uuid == Some(v)) {
                return Some(n);
            }
        }
    }
    // ArcaFS (ADR-0059): uuid a 64 bit = 16 hex maiuscoli. Formato
    // diverso dal seriale FAT (8 hex): nessuna ambiguità.
    if key.len() == 16 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(v) = u64::from_str_radix(key, 16) {
            if let Some(n) = nodes.iter().find(|n| n.vol_arca_uuid == Some(v)) {
                return Some(n);
            }
        }
    }
    nodes.iter().find(|n| n.vol_label.as_deref() == Some(key))
}

/// Risolve un handle codificato (disco<<16|sub, 0 = whole-disk) in
/// (indice disco, base settori, settori nodo). La validita' (quante
/// partizioni ha davvero il disco) e' qui. Ritorna None se inesistente.
pub(crate) fn locate(handle: u32, disk_sectors: &[u64], parts: &[Vec<PartLoc>]) -> Option<(usize, u64, u64)> {
    let disk = (handle >> 16) as usize;
    let sub = (handle & 0xFFFF) as usize;
    if disk >= disk_sectors.len() {
        return None;
    }
    if sub == 0 {
        return Some((disk, 0, disk_sectors[disk]));
    }
    let p = parts[disk].get(sub - 1)?;
    Some((disk, p.start, p.sectors))
}

/// Legge il settore `lba` del nodo `handle` (bound check sul nodo).
/// Attraversa la cache settoriale (Fase 25): hit = niente PIO.
pub(crate) fn node_read(
    disks: &[block::AtaDisk],
    disk_sectors: &[u64],
    parts: &[Vec<PartLoc>],
    handle: u32,
    lba: u64,
    out: &mut [u8; 512],
) -> bool {
    let (disk, base, sectors) = match locate(handle, disk_sectors, parts) {
        Some(r) => r,
        None => return false,
    };
    if lba >= sectors {
        return false;
    }
    match disks.get(disk) {
        Some(d) => {
            let phys = base + lba;
            if cache::lookup_into(disk, phys, out) {
                return true;
            }
            let ok = d.read_sector(phys, out);
            if ok {
                cache::insert_from(disk, phys, out);
            }
            ok
        }
        None => false,
    }
}

/// Scrive il settore `lba` del nodo `handle` (bound check sul nodo, come read).
/// Write-through (Fase 25): prima il PIO stabile, poi la cache; a
/// fallimento la entry e' invalidata (mai dati sporchi in cache).
fn node_write(
    disks: &[block::AtaDisk],
    disk_sectors: &[u64],
    parts: &[Vec<PartLoc>],
    handle: u32,
    lba: u64,
    data: &[u8; 512],
) -> bool {
    let (disk, base, sectors) = match locate(handle, disk_sectors, parts) {
        Some(r) => r,
        None => return false,
    };
    if lba >= sectors {
        return false;
    }
    match disks.get(disk) {
        Some(d) => {
            let phys = base + lba;
            let ok = d.write_sector(phys, data);
            if ok {
                debug_assert!(cache::POLICY == cache::Policy::WriteThrough);
                cache::insert_from(disk, phys, data);
            } else {
                cache::invalidate(disk, phys);
            }
            ok
        }
        None => false,
    }
}
/// 24.2 — bound del protocollo (frame nel ring: 8 + 7*512 in request,
/// 16 + 7*512 in response, entrambi < 4087).
pub(crate) const DISK_MAX_SECTORS: usize = 7;

/// Legge `out.len()/512` settori contigui del nodo (bound sul nodo + bound
/// protocollo, 1 comando PIO per run di miss). Gli hit di cache sono copiati
/// senza PIO; i miss contigui restano UN solo `read_sectors` (24.2 preservato).
/// `false` a parametri invalidi o errore IO (le entry del run fallito restano
/// intoccate: niente fill parziale sotto errore).
pub(crate) fn node_read_multi(
    disks: &[block::AtaDisk],
    disk_sectors: &[u64],
    parts: &[Vec<PartLoc>],
    handle: u32,
    lba: u64,
    out: &mut [u8],
) -> bool {
    let n = out.len() / 512;
    if n == 0 || n > DISK_MAX_SECTORS || out.len() % 512 != 0 {
        return false;
    }
    let (disk, base, sectors) = match locate(handle, disk_sectors, parts) {
        Some(r) => r,
        None => return false,
    };
    if lba.checked_add(n as u64).map_or(true, |end| end > sectors) {
        return false;
    }
    let dev = match disks.get(disk) {
        Some(d) => d,
        None => return false,
    };
    let mut k = 0usize;
    while k < n {
        let phys = base + lba + k as u64;
        if cache::contains(disk, phys) {
            let dst = &mut out[k * 512..(k + 1) * 512];
            if !cache::lookup_into(disk, phys, dst) {
                return false; // impossibile: contains appena vero
            }
            k += 1;
            continue;
        }
        // Run di miss contigui (≤ rimanente, ≤ bound protocollo per costruzione).
        let mut m = 1usize;
        while k + m < n && !cache::contains(disk, base + lba + (k + m) as u64) {
            m += 1;
        }
        if !dev.read_sectors(phys, m as u8, &mut out[k * 512..(k + m) * 512]) {
            return false;
        }
        cache::note_misses(m as u64);
        for j in 0..m {
            cache::insert_from(disk, phys + j as u64, &out[(k + j) * 512..(k + j + 1) * 512]);
        }
        k += m;
    }
    true
}

/// Scrive `data.len()/512` settori contigui del nodo (1 comando PIO + 1
/// flush, vedi `AtaDisk::write_sectors`). Write-through: a run stabile le
/// entry sono aggiornate, a fallimento invalidate. Stessi bound di
/// `node_read_multi`.
pub(crate) fn node_write_multi(
    disks: &[block::AtaDisk],
    disk_sectors: &[u64],
    parts: &[Vec<PartLoc>],
    handle: u32,
    lba: u64,
    data: &[u8],
) -> bool {
    let n = data.len() / 512;
    if n == 0 || n > DISK_MAX_SECTORS || data.len() % 512 != 0 {
        return false;
    }
    let (disk, base, sectors) = match locate(handle, disk_sectors, parts) {
        Some(r) => r,
        None => return false,
    };
    if lba.checked_add(n as u64).map_or(true, |end| end > sectors) {
        return false;
    }
    match disks.get(disk) {
        Some(d) => {
            let ok = d.write_sectors(base + lba, n as u8, data);
            if ok {
                debug_assert!(cache::POLICY == cache::Policy::WriteThrough);
                for j in 0..n {
                    cache::insert_from(
                        disk,
                        base + lba + j as u64,
                        &data[j * 512..(j + 1) * 512],
                    );
                }
            } else {
                cache::invalidate_run(disk, base + lba, n);
            }
            ok
        }
        None => false,
    }
}
