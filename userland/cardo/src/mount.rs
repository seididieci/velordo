use super::*;
use super::provider::MountedFs;
use crate::fat32::BlockSource;
use alloc::boxed::Box;

/// Sonda superblock ArcaFS a LBA0 del nodo (Fase 54, P5): magic + versione +
/// block-size + checksum FNV-1a self-verifying su [0..120] (stessa
/// `image_hash` del manifest: single source). Ritorna (generation, uuid).
/// Qualunque fallimento (IO, formato) = None (il chiamante prova vfat).
pub fn probe_arca(handle: u32) -> Option<civis::Superblock> {
    let disk = IpcDisk::new(handle);
    let mut sec = [0u8; 512];
    if !disk.read_sector(0, &mut sec) {
        return None;
    }
    let sb = &sec[..civis::ARCA_SUPER_LEN];
    if sb[civis::ARCA_OFF_MAGIC..civis::ARCA_OFF_MAGIC + 4] != *civis::ARCA_MAGIC {
        return None;
    }
    let u32le = |o: usize| {
        u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]])
    };
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
    let label = read_nul(&sb[civis::ARCA_OFF_LABEL..civis::ARCA_OFF_LABEL + civis::ARCA_LABEL_MAX]);
    let mount =
        read_nul(&sb[civis::ARCA_OFF_MOUNT..civis::ARCA_OFF_MOUNT + civis::ARCA_MOUNT_MAX]);
    let mut id = civis::Superblock {
        generation: u64le(civis::ARCA_OFF_GEN),
        uuid: u64le(civis::ARCA_OFF_UUID),
        ..Default::default()
    };
    id.label[..label.len()].copy_from_slice(label);
    id.label_len = label.len();
    id.mountpoint[..mount.len()].copy_from_slice(mount);
    id.mountpoint_len = mount.len();
    Some(id)
}

/// Campo NUL-padded: byte fino al primo NUL.
fn read_nul(field: &[u8]) -> &[u8] {
    let n = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    &field[..n]
}

// ── Mount locali dinamici (Fase 16b) ─────────────────────────────────
// Tabella VFS userspace (nessun kernel coinvolto, ADR-0005): binding
// target → filesystem montato. La radice resta sempre ramfs. Il contenitore
// e' generico (`FsMount` + `MountedFs`): FAT32, ramfs montata (Fase 49),
// domani ArcaFS aggiunge negotiate senza reshuffle della tabella.

// ── Sorgente di mount (Fase 49, F3) ──────────────────────────────
// Forma opaca per-variante: il mount non conosce piu' l'encoding
// `disco<<16|sub` (era `FsMount.handle`, scritto e mai letto — rimosso).
// `Block` copre le grammatiche `UUID=`/`LABEL=`/`/dev/…` (chiave corta per
// `DISK_RESOLVE`); le future varianti (net/9P/…) aggiungono rami senza
// toccare i chiamanti.
pub enum Source {
    Block { key: String },
}

impl Source {
    /// Costruisce da una source normalizzata (`normalize_source`).
    pub fn parse(norm: &str) -> Option<Source> {
        resolve_key(norm).map(|key| Source::Block { key })
    }
}

/// Negozia il superblock per una sorgente (Fase 49, F3): prova i formati in
/// ordine e ritorna `(fstype, istanza)`. `None` = resolve fallito (il
/// chiamante NON cambia stato); `Some` con istanza inattiva = BPB illeggibile
/// (spec registrata inattiva, ritenta lazy — mai shadow ramfs).
pub fn negotiate(source: &Source) -> Option<(&'static str, MountedFs)> {
    match source {
        Source::Block { key } => {
            if key.is_empty() || key.len() > 16 {
                return None;
            }
            let handle = IpcDisk::new(0).resolve(key)?;
            // La traduzione LBA-nodo → LBA-fisico (base partizione) vive nel
            // driver (`nodes::locate`): LBA0 del nodo e' gia' partition-
            // relative, qui non serve alcun offset (ArcaFs lo tiene a 0).
            // ArcaFS prima (Fase 54, P5): match piu' stretto (magic +
            // versione + block-size + checksum: un BPB FAT non puo'
            // collidere). Istanza stub (volume riconosciuto, non leggibile
            // fino ad A1) — il mount riesce, le op rifiutano tipizzate.
            if let Some(id) = probe_arca(handle) {
                return Some((
                    "arcafs",
                    MountedFs::Arca(crate::arca::ArcaFs::stub(id.generation, id.uuid, 0)),
                ));
            }
            Some(("vfat", MountedFs::Fat(Fat32::mount(IpcDisk::new(handle)))))
        }
    }
}

/// Mount locale: binding target → sorgente + istanza.
pub struct FsMount {
    /// Identita' stabile del mount (Fase 49, F2): monotonica da
    /// `next_mount_id`, mai riusata (niente ABA). Gli fd tengono l'id, non
    /// l'indice nel `Vec`: `umount` (`remove`) non sposta piu' i riferimenti.
    pub id: u64,
    /// Target normalizzato senza slash ("fat", "mnt").
    pub target: String,
    /// Source originale (`UUID=xxxxxxxx`, mai lettere instabili) per
    /// diagnostica e re-apply.
    source: String,
    /// Opzioni mount (placeholder Strato 0: conservate, non interpretate —
    /// futuro: uid=/gid=/mode per i permessi FAT finti alla Linux).
    opts: String,
    /// Tipo effettivo negoziato (`"vfat"`/`"ramfs"`, domani `"arcafs"`):
    /// dal superblock, non dalla sintassi della source.
    pub fstype: &'static str,
    /// Filesystem montato.
    fs: MountedFs,
}

/// Spec statiche applicate a OGNI boot (fresco o restart): sostituiscono il
/// binding hardcodato con lo stesso codice dei mount dinamici (dogfood).
/// Montate per UUID stabile (Fase 16d): il boot non dipende piu' dalle
/// lettere `sdX`. Dinamici (R_MOUNT, 16b.2) si aggiungono alla tabella ma si
/// perdono al restart (stato runtime, come fd e handshake: i client
/// ristabiliscono).
///
/// **ADR-0058: il FAT esce dal boot.** Il sistema **non attacca** piu'
/// dischi FAT: il boot usa solo ArcaFS. La voce FAT qui sotto resta pero'
/// *iniettiva*: senza il disco il mount non si attiva (la spec e' lazy) e
/// non tocca nulla — serve ai run opt-in (suite FAT, confronto
/// prestazionale ArcaFS-vs-FAT) e tornerà con le chiavette USB (mount per
/// LABEL, ADR-0058).
pub const STATIC_MOUNTS: &[(&str, &str)] = &[
    ("UUID=4F4C4556", "fat"),
];

/// Normalizza un target ("//mnt//" → "mnt"). Rifiuta vuoti, `.`/`..`.
/// La radice (`""`) e' ammessa SOLO se esplicitamente richiesta (Fase 2: root
/// su volume ArcaFS scelto per UUID).
pub fn normalize_target(target: &str) -> Option<String> {
    let t = target.trim().trim_matches('/');
    if t.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    Some(String::from(t))
}

/// Normalizza una source. Tre forme (Fase 16d): `/dev/<nodo>` (nomi brevi
/// `sda`, by-path `disk/by-uuid/<HEX>` / `disk/by-label/<NOME>`), `UUID=<hex8>`
/// (seriale volume FAT, maiuscolo), `LABEL=<nome>` (match esatto, case
/// sensibile). Solo controllo sintattico: la chiave la risolve block via
/// DISK_RESOLVE (`Source::parse` + `negotiate`). Ritorna None fuori grammatica.
fn normalize_source(source: &str) -> Option<String> {
    let s = source.trim();
    if let Some(name) = s.strip_prefix("/dev/") {
        if name.is_empty() || name.contains("//") || name.len() > 32 {
            return None;
        }
        let name = name.trim_matches('/');
        if name.is_empty() {
            return None;
        }
        return Some(alloc::format!("/dev/{}", name));
    }
    if let Some(hex) = s.strip_prefix("UUID=") {
        // 8 hex = seriale FAT (32 bit); 16 hex = uuid ArcaFS (64 bit,
        // ADR-0058). Formati distinti: nessuna ambiguità.
        if (hex.len() != 8 && hex.len() != 16) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return Some(String::from(s));
    }
    if let Some(label) = s.strip_prefix("LABEL=") {
        if label.is_empty() || label.len() > 11 || label.contains('/') {
            return None;
        }
        return Some(String::from(s));
    }
    None
}

/// Riduce una source normalizzata alla chiave di resolve (Fase 16d):
/// `/dev/sda` → `sda`, `/dev/disk/by-uuid/<H>` → `<H>`,
/// `/dev/disk/by-label/<N>` → `<N>`, `UUID=<H>` → `<H>`, `LABEL=<N>` → `<N>`.
/// La semantica (`/dev` = namespace, driver = matching) resta una sola:
/// cardo possiede il layout, block il matching nome/UUID/label.
fn resolve_key(source: &str) -> Option<String> {
    if let Some(name) = source.strip_prefix("/dev/") {
        if let Some(tail) = name.strip_prefix("disk/by-uuid/") {
            return (!tail.is_empty() && !tail.contains('/')).then(|| String::from(tail));
        }
        if let Some(tail) = name.strip_prefix("disk/by-label/") {
            return (!tail.is_empty() && !tail.contains('/')).then(|| String::from(tail));
        }
        if name.is_empty() || name.contains('/') {
            return None;
        }
        return Some(String::from(name));
    }
    if let Some(hex) = source.strip_prefix("UUID=") {
        return Some(String::from(hex));
    }
    if let Some(label) = source.strip_prefix("LABEL=") {
        return Some(String::from(label));
    }
    None
}

/// Risolve una source in handle presso block (Fase 16c/16d: single
/// source of truth nel driver). Ritorna None a chiave sconosciuta o driver
/// irraggiungibile (bound, mai wedge): il chiamante non cambia stato.
/// Usata dagli open raw by-path (`handlers.rs`); i mount passano da
/// `Source::parse` + `negotiate`.
pub fn resolve_mount_source(source: &str) -> Option<u32> {
    let key = resolve_key(source)?;
    if key.is_empty() || key.len() > 16 {
        return None;
    }
    IpcDisk::new(0).resolve(&key)
}

/// Applica una spec (statica o dinamica): valida e registra/aggiorna sempre la
/// spec (idempotente sul target). Il replace conserva l'id (Fase 49, F2: gli
/// fd aperti restano validi); il push assegna `*next_id` monotonico.
/// Resolve fallito (nome sconosciuto o driver irraggiungibile): NESSUN cambio
/// di stato (come il parse fallito di prima) — la distinzione nome-ignoto vs
/// driver-down non serve: a driver caduto il client riprova (restart ~50 tick,
/// bound 500 dentro `resolve`); l'inattivita' lazy resta per BPB invalida e
/// drop d'epoca (`note_peer_death`). Ritorna true se il mount e' ATTIVO
/// (superblock valido subito), false altrimenti (spec inattiva registrata solo
/// a resolve riuscito ma superblock illeggibile: ritenta lazy, mai shadow
/// ramfs).
pub fn apply_mount_spec(
    mounts: &mut Vec<FsMount>,
    source: &str,
    target: &str,
    opts: &str,
    next_id: &mut u64,
) -> bool {
    let norm_target = match normalize_target(target) {
        Some(x) => x,
        None => return false,
    };
    // Sorgente sintetica `ramfs` (Fase 49, F4): istanza ramfs montabile via
    // R_MOUNT come qualunque altro FS — esercita `MountedFs::Local` per la
    // prima volta (tmpfs-like; niente resolve, sempre attiva).
    if source.trim() == "ramfs" {
        if let Some(m) = mounts.iter_mut().find(|m| m.target == norm_target) {
            m.source = String::from("ramfs");
            m.opts = String::from(opts);
            m.fstype = "ramfs";
            m.fs = MountedFs::Local(Box::new(crate::ramfs::RamFs::new()));
        } else {
            let id = *next_id;
            *next_id = next_id.wrapping_add(1);
            mounts.push(FsMount {
                id,
                target: norm_target,
                source: String::from("ramfs"),
                opts: String::from(opts),
                fstype: "ramfs",
                fs: MountedFs::Local(Box::new(crate::ramfs::RamFs::new())),
            });
        }
        return true;
    }
    let norm_source = match normalize_source(source) {
        Some(x) => x,
        None => return false,
    };
    // Resolve una sola volta qui (vale per spec nuove e sostituite): a
    // fallimento la tabella resta intatta (mai distruggere un buon mount con
    // una source sbagliata, mai registrare nomi ignoti).
    let src = match Source::parse(&norm_source) {
        Some(s) => s,
        None => return false,
    };
    let (fstype, fs) = match negotiate(&src) {
        Some(v) => v,
        None => return false,
    };
    let active = matches!(&fs, MountedFs::Fat(Some(_)) | MountedFs::Local(_) | MountedFs::Arca(_));
    if let Some(m) = mounts.iter_mut().find(|m| m.target == norm_target) {
        m.source = norm_source;
        m.opts = String::from(opts);
        m.fstype = fstype;
        m.fs = fs;
        return active;
    }
    let id = *next_id;
    *next_id = next_id.wrapping_add(1);
    mounts.push(FsMount {
        id,
        target: norm_target,
        source: norm_source,
        opts: String::from(opts),
        fstype,
        fs,
    });
    active
}

/// Indice nel `Vec` dal mount-id (Fase 49, F2): l'unico punto che traduce
/// id → posizione; lo shift di `remove` resta interno e invisibile agli fd.
pub fn by_id(mounts: &[FsMount], id: u64) -> Option<usize> {
    mounts.iter().position(|m| m.id == id)
}

/// Istanza dal mount-id (Fase 49, F2).
pub fn by_id_mut(mounts: &mut Vec<FsMount>, id: u64) -> Option<&mut FsMount> {
    mounts.iter_mut().find(|m| m.id == id)
}

/// Riattiva un mount inattivo (Fase 16c): re-resolve del nome presso block
/// (gli handle possono cambiare dopo un restart del driver) + remount.
/// Fast path: mount gia' attivo → true senza IPC. Ritorna true se attivo.
/// A remount riuscito bumpa `gen` (l'istanza parser e' nuova: le cache
/// FileInfo per-fd vanno rifatte).
pub fn reactivate_mount(mounts: &mut Vec<FsMount>, mi: usize, fgen: &mut u64) -> bool {
    // I mount provider (`Local` Fase 49 / `Arca` Fase 54) non hanno epoca da
    // invalidare: sempre attivi (lo stub Arca non dipende dal disco a runtime).
    if mounts.get(mi).map_or(false, |m| m.is_provider()) {
        return true;
    }
    if mounts.get(mi).map_or(false, |m| m.is_active()) {
        return true;
    }
    let name = match mounts.get(mi) {
        Some(m) => m.source.clone(),
        None => return false,
    };
    let src = match Source::parse(&name) {
        Some(s) => s,
        None => return false,
    };
    let (fstype, fs) = match negotiate(&src) {
        Some(v) => v,
        None => return false,
    };
    match mounts.get_mut(mi) {
        Some(m) => {
            m.fstype = fstype;
            m.fs = fs;
            let ok = m.is_active();
            if ok {
                *fgen = fgen.wrapping_add(1);
            }
            ok
        }
        None => false,
    }
}

/// Come `reactivate_mount` ma per mount-id (Fase 49, F2): gli handler con un
/// fd tengono l'id, mai l'indice.
pub fn reactivate_mount_by_id(mounts: &mut Vec<FsMount>, id: u64, fgen: &mut u64) -> bool {
    match by_id(mounts, id) {
        Some(mi) => reactivate_mount(mounts, mi, fgen),
        None => false,
    }
}

impl FsMount {
    /// Mount root su volume ArcaFS (Fase A): target `""` catch-all — tutto
    /// cio' che non matcha un target specifico cade qui. `normalize_target`
    /// rifiuta `""` apposta: la root si crea solo qui a boot (mai via
    /// R_MOUNT) e non si smonta (umount cerca per target normalizzato).
    pub fn root_arca(generation: u64, uuid: u64, id: u64) -> Self {
        Self {
            id,
            target: String::new(),
            source: String::from("arca-root"),
            opts: String::new(),
            fstype: "arcafs",
            fs: MountedFs::Arca(crate::arca::ArcaFs::stub(generation, uuid, 0)),
        }
    }

    /// Istanza FAT se montata e attiva (None se altra variante o inattiva).
    /// Le future varianti aggiungono i loro accessor qui; gli handler che
    /// servono FAT-specifico (create gia' assorbito in `open` dalla Fase 49;
    /// restano cache per-fd e lseek) usano questo + `fat_mut`.
    pub fn fat(&self) -> Option<&Fat32<IpcDisk>> {
        match &self.fs {
            MountedFs::Fat(opt) => opt.as_ref(),
            MountedFs::Local(_) | MountedFs::Arca(_) => None,
        }
    }

    /// Come `fat` ma mutabile (Fase 49, F5: `open` con OPEN_CREATE/OPEN_TRUNC via
    /// trait sul concreto).
    pub fn fat_mut(&mut self) -> Option<&mut Fat32<IpcDisk>> {
        match &mut self.fs {
            MountedFs::Fat(opt) => opt.as_mut(),
            MountedFs::Local(_) | MountedFs::Arca(_) => None,
        }
    }

    /// Istanza filesystem locale (tramite LocalFsDyn). Per FAT: Fat32; per
    /// ramfs montata e future varianti: dispatch diretto. Ritorna None se il
    /// mount e' inattivo o non ha un provider locale. (Fase 48: wiring FAT
    /// via trait; Fase 49: handle `AnyHandle` by-value, niente Box per-op.)
    pub fn local_dyn(&mut self) -> Option<&mut dyn crate::provider::LocalFsDyn> {        match &mut self.fs {
            MountedFs::Fat(Some(f)) => Some(f), // Fat32<B> implements LocalFsDyn
            MountedFs::Local(dyn_handle) => Some(&mut **dyn_handle),
            // ArcaFS (P5 stub: op tipizzate, mai panic) e' un provider
            // dinamico come gli altri: Fase 54 estende `MountedFs::Arca`
            // (variante propria per il dispatch nativo `R_OBJ_*` futuro).
            MountedFs::Arca(a) => Some(a),
            _ => None,
        }
    }

    /// Vista POSIX ArcaFS col motore globale (56.3): lega l'istanza del mount
    /// al `DiskEngine` passato SOLO se l'uuid combacia. `None` = motore
    /// assente o altro volume (il chiamante cade sullo stub loud: mai dati
    /// di un altro volume sullo stesso motore, un solo proprietario).
    pub fn arca_with<'a>(
        &'a mut self,
        eng: Option<&'a mut crate::btree_drv::DiskEngine>,
    ) -> Option<crate::arca::ArcaWith<'a>> {
        let e = eng?;
        if crate::btree_drv::volume_uuid(e) == 0 {
            return None;
        }
        match &mut self.fs {
            MountedFs::Arca(a) if a.uuid == crate::btree_drv::volume_uuid(e) => {
                Some(crate::arca::ArcaWith::bind(a, e))
            }
            _ => None,
        }
    }

    /// Invalida il client disco alla morte del peer (solo variante Fat con
    /// mount attivo; le future varianti con client propri fanno lo stesso).
    /// Se eravamo connessi (cambio d'epoca) droppa anche l'istanza: gli handle
    /// possono cambiare dopo un restart del driver (Fase 16c) e un handle
    /// stale leggerebbe il disco sbagliato in silenzio — il prossimo accesso
    /// re-risolve per nome e rimonta (fail-loud, mai shadow ramfs).
    /// Ritorna true se l'istanza e' stata droppata (il chiamante bumpa la
    /// generazione delle cache FileInfo).
    pub fn note_peer_death(&mut self, dead_chan: u64) -> bool {
        if let MountedFs::Fat(Some(f)) = &self.fs {
            if f.disk().note_peer_death(dead_chan) {
                self.fs = MountedFs::Fat(None);
                return true;
            }
        }
        false
    }

    /// true se il mount e' attivo (istanza viva).
    pub fn is_active(&self) -> bool {
        match &self.fs {
            MountedFs::Fat(opt) => opt.is_some(),
            // `Local` sempre vivo; `Arca` P5: istanza stub sempre presente
            // (il volume esiste ma non e' leggibile fino ad A1).
            MountedFs::Local(_) | MountedFs::Arca(_) => true,
        }
    }

    /// true se il mount e' un provider dinamico (`Local` Fase 49 / `Arca`
    /// Fase 54): dispatch via `local_dyn`, niente epoche disco.
    pub fn is_provider(&self) -> bool {
        matches!(&self.fs, MountedFs::Local(_) | MountedFs::Arca(_))
    }
}

/// Risolve un path nel mount col prefix piu' lungo. Attiva lazy se il mount e'
/// inattivo (re-resolve per nome + remount via `reactivate_mount`; i mount
/// `Local` sono sempre attivi). Ritorna (mount-id, rel): l'id e' stabile
/// oltre `umount`/`remove` altrui (Fase 49, F2), mai un indice.
pub fn resolve_fsmount<'a>(mounts: &mut Vec<FsMount>, path: &'a str, fgen: &mut u64) -> Option<(u64, &'a str)> {
    let t = path.trim_start_matches('/');
    // Fase A: la root catch-all non copre mai `/dev` (namespace dei driver
    // nella tabella legacy: resolve_mount/synth hanno precedenza come prima
    // del catch-all). Un mount specifico sotto /dev resta raggiungibile.
    let dev_tree = t == "dev" || t.starts_with("dev/");
    let mut best: Option<(usize, &str)> = None;
    for (i, m) in mounts.iter().enumerate() {
        let rel = if m.target.is_empty() {
            // Mount root (Fase A): catch-all — tutto cio' che non matcha un
            // target specifico cade qui con rel = path trimmato (`/` → "").
            // Il longest-prefix sotto preferisce sempre i mount specifici.
            if dev_tree {
                continue;
            }
            t
        } else if t == m.target {
            ""
        } else if t.len() > m.target.len()
            && t.as_bytes().get(m.target.len()) == Some(&b'/')
            && t.starts_with(m.target.as_str())
        {
            &t[m.target.len() + 1..]
        } else {
            continue;
        };
        if best.map_or(true, |(_, r)| rel.len() < r.len()) {
            best = Some((i, rel));
        }
    }
    let (i, rel) = best?;
    if !reactivate_mount(mounts, i, fgen) {
        return None;
    }
    Some((mounts.get(i)?.id, rel))
}

// ── Helper conversione ─────────────────────────────────────────────

/// Converte `Result<u64, u64>` in valore IPC (Fase 40, errori tipizzati):
/// `Ok(v)` → `v`, `Err(code)` → la sentinella del rifiuto.
#[inline]
pub fn to_reply_res(val: Result<u64, u64>) -> u64 {
    val.unwrap_or_else(|e| e)
}
