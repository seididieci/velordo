use super::*;
use crate::provider::{LocalFs, EntrySink, Meta, StatVfs, AnyHandle};

// ── ramfs ──────────────────────────────────────────────────────────

/// Handle per RamFs: path limitato a 64 byte (Copy + PartialEq).
#[derive(Clone, Copy, PartialEq)]
pub struct RamHandle {
    path: [u8; 64],
    len: usize,
}

impl RamHandle {
    /// Costruisce un handle dal path; `None` se oltre capacita' (Fase 49:
    /// mai troncamento silenzioso — due path con stesso prefisso 63B
    /// aliaserebbero lo stesso handle).
    fn new(path: &str) -> Option<Self> {
        let bytes = path.as_bytes();
        if bytes.len() > 64 {
            return None;
        }
        let len = bytes.len();
        let mut p = [0u8; 64];
        p[..len].copy_from_slice(&bytes[..len]);
        Some(Self { path: p, len })
    }

    fn as_str(&self) -> &str {
        // SAFETY: i bytes sono stati scritti da as_bytes() di un &str valido.
        unsafe { core::str::from_utf8_unchecked(&self.path[..self.len]) }
    }
}

#[derive(Clone)]
#[allow(dead_code)] // `mode`: placeholder Strato 0 (16b), enforcement futuro
pub enum FsNode {
    File { data: Vec<u8>, mode: u32, mtime: u64 },
    Dir { entries: BTreeMap<String, FsNode>, mode: u32, mtime: u64 },
    /// Symlink S1.1: target opaco (risolto all'open, mai qui). `mode` come
    /// i file (placeholder Strato 0, mai enforcement).
    Symlink { target: String, mode: u32, mtime: u64 },
}

/// Mode Unix di default (placeholder Strato 0, Fase 16b): conservati, MAI
/// enforcement (nessun uid nel sistema; i check R/W/X arrivano col login
/// boundary, futuro). FAT e' mappata fissa a mount (file 0o444, dir 0o555:
/// placeholder, l'enforcement non esiste; FAT e' scrivibile dalla Fase 20).
pub const MODE_FILE_DEF: u32 = 0o666;
pub const MODE_DIR_DEF: u32 = 0o777;
#[allow(dead_code)]
pub const MODE_FAT_FILE: u32 = 0o444;
#[allow(dead_code)]
pub const MODE_FAT_DIR: u32 = 0o555;

pub struct RamFs {
    root: BTreeMap<String, FsNode>,
}

impl RamFs {
    pub fn new() -> Self {
        Self { root: BTreeMap::new() }
    }

    /// Trova un nodo per path (es. "hello.txt" o "dir/file.txt").
    /// Ritorna il nodo finale (file o dir); i componenti intermedi devono
    /// essere directory (altrimenti None, come ENOTDIR).
    pub fn find(&self, path: &str) -> Option<&FsNode> {
        if path.is_empty() || path == "/" {
            return None;
        }
        // Componenti in scratch (mai heap: 1 alloc per lookup prima). Two-pass:
        // conta poi riempi — il path e' minuscolo, la doppia scansione e'
        // trascurabile contro una free-list round-trip.
        let t = path.trim_start_matches('/');
        let n = t.split('/').count();
        let parts_buf = civis::scratch::alloc_slice::<&str>(n)?;
        for (i, comp) in t.split('/').enumerate() {
            parts_buf[i] = comp;
        }
        let parts = &parts_buf[..n];
        let mut current_dir = &self.root;
        for (i, &part) in parts.iter().enumerate() {
            let node = current_dir.get(part)?;
            if i == parts.len() - 1 {
                return Some(node);
            }
            match node {
                FsNode::Dir { entries: d, .. } => current_dir = d,
                _ => return None,
            }
        }
        None
    }

    /// Trova o crea un nodo per path (crea le directory intermedie).
    pub fn find_or_create(&mut self, path: &str) -> Option<&mut FsNode> {
        // Componenti in scratch come `find` (i `String::from` sotto restano
        // heap: vivono nell'albero ramfs oltre la richiesta, mai scratch).
        let t = path.trim_start_matches('/');
        let n = t.split('/').count();
        let parts_buf = civis::scratch::alloc_slice::<&str>(n)?;
        for (i, comp) in t.split('/').enumerate() {
            parts_buf[i] = comp;
        }
        let parts = &parts_buf[..n];
        if parts.is_empty() || parts[0].is_empty() {
            return None;
        }

        let mut current = &mut self.root;
        let now = crate::wall::wall_secs();
        for (i, &part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                current.entry(String::from(part))
                    .or_insert_with(|| FsNode::File { data: Vec::new(), mode: MODE_FILE_DEF, mtime: now });
                return current.get_mut(part);
            }
            let entry = current.entry(String::from(part))
                .or_insert_with(|| FsNode::Dir { entries: BTreeMap::new(), mode: MODE_DIR_DEF, mtime: now });
            match entry {
                FsNode::Dir { entries: dir, .. } => current = dir,
                _ => return None,
            }
        }
        None
    }

    /// Byte dati contenuti nell'albero (Fase 52, P3: sensore statvfs).
    /// Solo payload file (niente overhead nodi: stima per difetto onesta,
    /// mai gonfiata — la quota futura misuri per eccesso altrove).
    pub fn used_bytes(&self) -> u64 {
        fn sum(dir: &BTreeMap<String, FsNode>, acc: &mut u64) {
            for node in dir.values() {
                match node {
                    FsNode::File { data, .. } => *acc += data.len() as u64,
                    FsNode::Symlink { target, .. } => *acc += target.len() as u64,
                    FsNode::Dir { entries, .. } => sum(entries, acc),
                }
            }
        }
        let mut acc = 0u64;
        sum(&self.root, &mut acc);
        acc
    }

    /// Lista le entry di una directory.
    pub fn readdir(&self, path: &str) -> Option<Vec<String>> {
        if path.is_empty() || path == "/" {
            return Some(self.root.keys().cloned().collect());
        }
        let node = self.find(path)?;
        match node {
            FsNode::Dir { entries, .. } => Some(entries.keys().cloned().collect()),
            _ => None,
        }
    }

    /// Cancella un file o una directory VUOTA (Fase 18.2, `R_DELETE`).
    /// Directory non vuote, root e path inesistenti → None. Non crea nulla.
    pub fn remove(&mut self, path: &str) -> Option<()> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if parts.is_empty() || parts[0].is_empty() {
            return None;
        }
        let mut current = &mut self.root;
        for (i, &part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                // File, o dir vuota: rimuovibile. Dir non vuota, root o
                // assente: rifiuto (niente `remove` sotto borrow attivo).
                let ok = match current.get(part) {
                    Some(FsNode::File { .. }) => true,
                    Some(FsNode::Symlink { .. }) => true,
                    Some(FsNode::Dir { entries, .. }) => entries.is_empty(),
                    _ => false,
                };
                if !ok {
                    return None;
                }
                current.remove(part);
                return Some(());
            }
            match current.get_mut(part) {
                Some(FsNode::Dir { entries: dir, .. }) => current = dir,
                _ => return None,
            }
        }
        None
    }
}

// ── Implementazione LocalFs per RamFs (U0, provider trait) ─────────

impl LocalFs for RamFs {
    type Handle = RamHandle;

    fn open(&mut self, rel: &str, flags: u32) -> Result<Self::Handle, u64> {
        let path = rel.trim_start_matches('/');
        if path.is_empty() {
            return Err(crate::ERR_NOTFOUND);
        }
        let creat = flags & civis::OPEN_CREATE != 0;
        let trunc = flags & civis::OPEN_TRUNC != 0;

        // OPEN_CREATE: crea il file se non esiste.
        if creat {
            if self.find(path).is_none() {
                // Crea il file (e le directory intermedie se necessario).
                let node = self.find_or_create(path);
                if node.is_none() {
                    return Err(crate::ERR_NOTFOUND);
                }
                match node.unwrap() {
                    FsNode::File { data, .. } => {
                        if trunc {
                            data.clear();
                        }
                    }
                    _ => return Err(crate::ERR_ISDIR),
                }
            } else if trunc {
                // Il file esiste gia': svuotalo.
                let node = self.find(path).unwrap();
                match node {
                    FsNode::File { .. } => {
                        // Devo usare find_or_create per avere &mut Vec<u8>.
                        let mut_node = self.find_or_create(path);
                        if let Some(FsNode::File { data, mtime, .. }) = mut_node {
                            data.clear();
                            *mtime = crate::wall::wall_secs();
                        }
                    }
                    _ => return Err(crate::ERR_ISDIR),
                }
            }
        } else if trunc {
            // OPEN_TRUNC senza OPEN_CREATE: il file deve esistere.
            let node = self.find(path).ok_or(crate::ERR_NOTFOUND)?;
            match node {
                FsNode::File { .. } => {
                    let mut_node = self.find_or_create(path);
                    if let Some(FsNode::File { data, mtime, .. }) = mut_node {
                        data.clear();
                        *mtime = crate::wall::wall_secs();
                    }
                }
                _ => return Err(crate::ERR_ISDIR),
            }
        }

        // Verifica che il nodo sia un file (non una directory).
        match self.find(path) {
            Some(FsNode::File { .. }) => Ok(RamHandle::new(path).ok_or(crate::ERR_INVALID)?),
            Some(FsNode::Dir { .. }) => Err(crate::ERR_ISDIR),
            // Symlink: l'handler segue prima di aprire; diretto = loud.
            Some(FsNode::Symlink { .. }) => Err(crate::ERR_INVALID),
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    fn read(&mut self, h: Self::Handle, off: usize, buf: &mut [u8]) -> Result<usize, u64> {
        let path = h.as_str();
        match self.find(path) {
            Some(FsNode::File { data, .. }) => {
                // Oltre EOF: ritorna 0 (mai panic per off >= len).
                if off >= data.len() {
                    return Ok(0);
                }
                let len = data.len() - off;
                let n = len.min(buf.len());
                buf[..n].copy_from_slice(&data[off..off + n]);
                Ok(n)
            }
            Some(FsNode::Dir { .. }) => Err(crate::ERR_ISDIR),
            Some(FsNode::Symlink { .. }) => Err(crate::ERR_INVALID),
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    fn write(&mut self, h: Self::Handle, off: usize, buf: &[u8], append: bool) -> Result<usize, u64> {
        let path = h.as_str();
        // Clone il path prima di mutare self (borrow checker).
        let path_owned: alloc::string::String = path.into();
        // Timbro mtime campionato una volta per op (Fase 50).
        let now = crate::wall::wall_secs();
        match self.find_or_create(&path_owned) {
            Some(FsNode::Symlink { .. }) => return Err(crate::ERR_INVALID),
            Some(FsNode::File { data, mtime, .. }) => {
                let n = if append {
                    let n = buf.len();
                    data.extend_from_slice(buf);
                    n
                } else {
                    // Write con offset: estende il vettore se necessario.
                    let end = off + buf.len();
                    if end > data.len() {
                        data.resize(end, 0);
                    }
                    data[off..off + buf.len()].copy_from_slice(buf);
                    buf.len()
                };
                *mtime = now;
                Ok(n)
            }
            Some(FsNode::Dir { .. }) => Err(crate::ERR_ISDIR),
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    fn readdir(&mut self, rel: &str, out: &mut dyn EntrySink) -> Result<usize, u64> {
        // Chiama RamFs::readdir esplicitamente per evitare collisione con LocalFs::readdir.
        match RamFs::readdir(self, rel) {
            Some(entries) => {
                for name in &entries {
                    out.emit(name);
                }
                Ok(entries.len())
            }
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    fn stat(&mut self, rel: &str) -> Result<Meta, u64> {
        match self.find(rel) {
            Some(FsNode::File { data, mtime, .. }) => Ok(Meta {
                size: data.len() as u64,
                kind: 0, // file
                readonly: false,
                mtime: *mtime,
            }),
            Some(FsNode::Dir { mtime, .. }) => Ok(Meta {
                size: 0,
                kind: 1, // dir
                readonly: false,
                mtime: *mtime,
            }),
            // Symlink: stat = lstat (il link stesso, mai il target: size =
            // target len, kind 3; l'open segue invece).
            Some(FsNode::Symlink { target, mtime, .. }) => Ok(Meta {
                size: target.len() as u64,
                kind: 3, // symlink (STAT_SYMLINK)
                readonly: false,
                mtime: *mtime,
            }),
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    fn mkdir(&mut self, rel: &str) -> Result<(), u64> {
        let path = rel.trim_start_matches('/');
        if path.is_empty() {
            return Err(crate::ERR_INVALID);
        }
        // Crea la directory (e le intermedie).
        match self.find(path) {
            Some(FsNode::Dir { .. }) => Err(crate::ERR_EXISTS), // gia' esistente.
            Some(FsNode::Symlink { .. }) => Err(crate::ERR_EXISTS),
            Some(FsNode::File { .. }) => Err(crate::ERR_NOTDIR),
            None => {
                // Crea ricorsivamente.
                let parts: Vec<&str> = path.split('/').collect();
                if parts.is_empty() || parts[0].is_empty() {
                    return Err(crate::ERR_INVALID);
                }
                let mut current = &mut self.root;
                let now = crate::wall::wall_secs();
                for (i, &part) in parts.iter().enumerate() {
                    if i == parts.len() - 1 {
                        // Ultima parte: crea la directory.
                        current.entry(String::from(part))
                            .or_insert_with(|| FsNode::Dir {
                                entries: BTreeMap::new(),
                                mode: MODE_DIR_DEF,
                                mtime: now,
                            });
                        return Ok(());
                    }
                    let entry = current.entry(String::from(part))
                        .or_insert_with(|| FsNode::Dir {
                            entries: BTreeMap::new(),
                            mode: MODE_DIR_DEF,
                            mtime: now,
                        });
                    match entry {
                        FsNode::Dir { entries: dir, .. } => current = dir,
                        _ => return Err(crate::ERR_NOTDIR),
                    }
                }
                Ok(())
            }
        }
    }

    fn remove(&mut self, rel: &str) -> Result<(), u64> {
        if self.remove(rel).is_some() {
            Ok(())
        } else {
            Err(crate::ERR_NOTFOUND)
        }
    }

    /// Symlink S1.1: crea il link (target opaco, bound chiave come rename).
    /// Esistente (qualunque tipo) = EXISTS; parent mancante = NOTFOUND.
    fn symlink(&mut self, link: &str, target: &str) -> Result<(), u64> {
        let now = crate::wall::wall_secs();
        let lp = link.trim_start_matches('/');
        let tp = target.trim_start_matches('/');
        if lp.is_empty() || tp.is_empty() || lp.len() > 255 || tp.len() > 1024 {
            return Err(crate::ERR_INVALID);
        }
        let (par, leaf) = match lp.rsplit_once('/') {
            Some((p, l)) if !l.is_empty() => (p, l),
            None => ("", lp),
            _ => return Err(crate::ERR_INVALID),
        };
        if self.find(lp).is_some() {
            return Err(crate::ERR_EXISTS);
        }
        // Naviga al parent (mai autocreate: parent mancante = NOTFOUND).
        let mut cur = &mut self.root;
        if !par.is_empty() {
            for part in par.split('/') {
                match cur.get_mut(part) {
                    Some(FsNode::Dir { entries, .. }) => cur = entries,
                    _ => return Err(crate::ERR_NOTFOUND),
                }
            }
        }
        cur.insert(
            String::from(leaf),
            FsNode::Symlink { target: String::from(tp), mode: MODE_FILE_DEF, mtime: now },
        );
        Ok(())
    }

    fn readlink(&mut self, rel: &str) -> Result<String, u64> {
        match self.find(rel.trim_start_matches('/')) {
            Some(FsNode::Symlink { target, .. }) => Ok(target.clone()),
            Some(_) => Err(crate::ERR_INVALID), // non-link: mai target inventati
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    /// chmod S1.1: scrive il mode sul nodo (12 bit), qualunque tipo
    /// (file/dir/link: la projection non distingue). Assente = NOTFOUND.
    fn chmod(&mut self, rel: &str, mode: u32) -> Result<(), u64> {
        // Esistenza vera prima (find_or_create creerebbe la foglia come
        // Dir vuota: chmod non deve creare mai).
        match self.find(rel.trim_start_matches('/')) {
            Some(_) => {}
            None => return Err(crate::ERR_NOTFOUND),
        }
        let node = self.find_or_create(rel.trim_start_matches('/')).ok_or(crate::ERR_NOTFOUND)?;
        match node {
            FsNode::File { mode: m, mtime, .. }
            | FsNode::Dir { mode: m, mtime, .. }
            | FsNode::Symlink { mode: m, mtime, .. } => {
                *m = mode & 0o7777;
                *mtime = crate::wall::wall_secs();
            }
        }
        Ok(())
    }

    /// Rename S1.1: sposta il nodo (stesso oggetto, subtree al seguito per
    /// le dir). Regole: file→dir = ISDIR, dir→file = NOTDIR, dir→non-vuota
    /// = EXISTS, assente = NOTFOUND. Niente autocreate dei parent (POSIX:
    /// parent dst mancante = NOTFOUND). Move di chiave BTreeMap: atomico
    /// per costruzione (niente stati intermedi osservabili).
    fn rename(&mut self, old_rel: &str, new_rel: &str) -> Result<(), u64> {
        fn split(path: &str) -> Option<(&str, &str)> {
            let p = path.trim_start_matches('/');
            if p.is_empty() {
                return None;
            }
            match p.rsplit_once('/') {
                Some(("", _)) => None, // "/x" senza parent → root
                Some((par, leaf)) if !leaf.is_empty() => Some((par, leaf)),
                None => Some(("", p)),
                _ => None,
            }
        }
        let (old_par, old_leaf) = split(old_rel).ok_or(crate::ERR_INVALID)?;
        let (new_par, new_leaf) = split(new_rel).ok_or(crate::ERR_INVALID)?;
        // Naviga a una dir esistente (mai autocreate qui).
        fn nav<'a>(
            root: &'a mut BTreeMap<String, FsNode>,
            par: &str,
        ) -> Option<&'a mut BTreeMap<String, FsNode>> {
            let mut cur = root;
            if par.is_empty() {
                return Some(cur);
            }
            for part in par.split('/') {
                match cur.get_mut(part) {
                    Some(FsNode::Dir { entries, .. }) => cur = entries,
                    _ => return None,
                }
            }
            Some(cur)
        }
        // Classifica src/dst prima di mutare (two-phase: a rifiuto niente si
        // muove).
        let src_kind = match nav(&mut self.root, old_par).and_then(|d| d.get(old_leaf)) {
            Some(FsNode::File { .. }) => 0,
            // Symlink: si rinomina IL LINK (mai follow, come POSIX).
            Some(FsNode::Symlink { .. }) => 0,
            Some(FsNode::Dir { entries, .. }) if entries.is_empty() => 1,
            Some(FsNode::Dir { .. }) => 2,
            None => return Err(crate::ERR_NOTFOUND),
        };
        if old_par == new_par && old_leaf == new_leaf {
            return Ok(()); // no-op identitaria (esiste: vedi sopra)
        }
        let dst_kind = match nav(&mut self.root, new_par).and_then(|d| d.get(new_leaf)) {
            Some(FsNode::File { .. }) => 0,
            Some(FsNode::Symlink { .. }) => 0,
            Some(FsNode::Dir { entries, .. }) if entries.is_empty() => 1,
            Some(FsNode::Dir { .. }) => 2,
            None => 3,
        };
        if dst_kind == 3 && new_par != old_par {
            // Parent dst distinto e navigazione fallita → parent mancante.
            // (nav ritorna None sia per parent mancante che... qui dst
            // assente E parent ok danno 3; parent mancante da' 3 uguale:
            // distinguiamo verificando il parent.)
            if nav(&mut self.root, new_par).is_none() {
                return Err(crate::ERR_NOTFOUND);
            }
        }
        // file→dir (anche vuota) = ISDIR; dir→file = NOTDIR;
        // dir→dir-non-vuota = EXISTS; replace solo file→file e
        // dir-vuota→dir-vuota; dir-non-vuota→libero/vuota = move subtree.
        match (src_kind, dst_kind) {
            (_, 3) => {}                       // dst libero: ok
            (0, 0) | (1, 1) | (2, 1) => {}     // replace omogenei
            (0, 1) | (0, 2) => return Err(crate::ERR_ISDIR),
            (1, 0) | (2, 0) => return Err(crate::ERR_NOTDIR),
            (1, 2) | (2, 2) => return Err(crate::ERR_EXISTS),
            _ => return Err(crate::ERR_INVALID),
        }
        // Move: out dal parent src, dentro il parent dst (replace se occupato
        // da file/dir-vuota — gia' validato sopra).
        let node = nav(&mut self.root, old_par)
            .and_then(|d| d.remove(old_leaf))
            .ok_or(crate::ERR_NOTFOUND)?;
        let dst = nav(&mut self.root, new_par).ok_or(crate::ERR_NOTFOUND)?;
        dst.insert(String::from(new_leaf), node);
        Ok(())
    }

    fn statvfs(&mut self, _rel: &str) -> Result<StatVfs, u64> {
        // ramfs memory-backed (Fase 52, P3): blocchi usati camminati,
        // libero/available illimitati (= u64::MAX: cresce con l'heap fino a
        // OOM — il sensore vero del tetto e' `SYS_MEMINFO`, mai questo).
        Ok(StatVfs {
            bsize: 512,
            blocks: self.used_bytes().div_ceil(512),
            bfree: u64::MAX,
            bavail: u64::MAX,
        })
    }
}

// ── Implementazione LocalFsDyn per RamFs (Fase 49: `MountedFs::Local`
// esercitato davvero) ─────────────────────────────────────────────
// Dispatch su `AnyHandle` discriminato, speculare a `Fat32`: il ramo
// sbagliato e' errore, mai reinterpretazione.
impl crate::provider::LocalFsDyn for RamFs {
    fn open_dyn(&mut self, rel: &str, flags: u32) -> Result<AnyHandle, u64> {
        <Self as LocalFs>::open(self, rel, flags).map(AnyHandle::Ram)
    }

    fn read_dyn(&mut self, h: AnyHandle, off: usize, buf: &mut [u8]) -> Result<usize, u64> {
        match h {
            AnyHandle::Ram(rh) => <Self as LocalFs>::read(self, rh, off, buf),
            _ => Err(crate::ERR_INVALID),
        }
    }

    fn write_dyn(&mut self, h: AnyHandle, off: usize, buf: &[u8], append: bool) -> Result<usize, u64> {
        match h {
            AnyHandle::Ram(rh) => <Self as LocalFs>::write(self, rh, off, buf, append),
            _ => Err(crate::ERR_INVALID),
        }
    }

    fn readdir_dyn(&mut self, rel: &str, out: &mut dyn EntrySink) -> Result<usize, u64> {
        <Self as LocalFs>::readdir(self, rel, out)
    }

    fn stat_dyn(&mut self, rel: &str) -> Result<Meta, u64> {
        <Self as LocalFs>::stat(self, rel)
    }

    fn mkdir_dyn(&mut self, rel: &str) -> Result<(), u64> {
        <Self as LocalFs>::mkdir(self, rel)
    }

    fn remove_dyn(&mut self, rel: &str) -> Result<(), u64> {
        <Self as LocalFs>::remove(self, rel)
    }

    fn symlink_dyn(&mut self, link: &str, target: &str) -> Result<(), u64> {
        <Self as LocalFs>::symlink(self, link, target)
    }

    fn readlink_dyn(&mut self, rel: &str) -> Result<String, u64> {
        <Self as LocalFs>::readlink(self, rel)
    }

    fn chmod_dyn(&mut self, rel: &str, mode: u32) -> Result<(), u64> {
        <Self as LocalFs>::chmod(self, rel, mode)
    }

    fn rename_dyn(&mut self, old: &str, new: &str) -> Result<(), u64> {
        <Self as LocalFs>::rename(self, old, new)
    }

    fn statvfs_dyn(&mut self, rel: &str) -> Result<StatVfs, u64> {
        <Self as LocalFs>::statvfs(self, rel)
    }
}
