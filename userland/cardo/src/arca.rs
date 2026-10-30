//! ArcaFS vista POSIX (56.3): namespace emergente + set transient in RAM.
//!
//! Mappatura: bucket `ns` di `btree_drv` (chiavi = path relativi opachi per
//! il motore, es. `bin/interpres.bin`). Le directory sono EMERGENTI (esistono ⟺
//! chiavi col prefisso) + set transient in RAM per le `mkdir` esplicite:
//! `mkdir` non scrive mai su disco (niente commit); il set si perde a
//! restart/remount (le vuote spariscono, le piene riemergono dalle chiavi).
//! `rmdir` di mai-esistita = errore (mai `Ok` silenzioso).
//!
//! Il motore e' quello globale (`disk` in `server.rs`, un solo proprietario
//! per volume): i metodi `ns_*` lo prendono esplicito. `ArcaWith` lo lega al
//! mount per il dispatch `LocalFsDyn` dagli handler (uuid combaciante,
//! altrimenti `None` e si cade sullo stub loud — mai dati altrui).
//! Lo stub `LocalFs for ArcaFs` resta come rete di sicurezza (motore
//! assente/mismatch: errori tipizzati, init ripiega su FAT come prima).

use super::*;
use crate::provider::{EntrySink, LocalFs, LocalFsDyn, Meta, StatVfs};
use crate::btree_drv::DiskEngine;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// Handle ArcaFS: path copiato (come `RamHandle`: niente aliasing per
/// prefisso, mai troncamento silenzioso — oltre `MAX_PATH` rifiuto).
#[derive(Clone, Copy, PartialEq)]
pub struct ArcaHandle {
    path: [u8; crate::MAX_PATH],
    len: usize,
}

impl ArcaHandle {
    fn new(path: &str) -> Option<Self> {
        if path.len() > crate::MAX_PATH {
            return None;
        }
        let mut h = Self { path: [0u8; crate::MAX_PATH], len: path.len() };
        h.path[..path.len()].copy_from_slice(path.as_bytes());
        Some(h)
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.path[..self.len]).unwrap_or("")
    }
}

/// Istanza ArcaFS: identita' dal mount + set transient delle dir esplicite.
pub struct ArcaFs {
    /// Generation del superblock montato (diagnostica, come uuid).
    #[allow(dead_code)] // identita' diagnostica del mount (letto via VolumeStore)
    pub generation: u64,
    /// UUID volume (identita' globale con `object_id`).
    pub uuid: u64,
    /// Offset LBA della partizione (0 = whole-disk; traduzione nel driver).
    #[allow(dead_code)] // traduzione LBA delegata al driver (sempre 0 oggi)
    pub partition_offset: u64,
    /// Dir create per `mkdir` (path → mtime creazione): SOLO RAM, mai disco.
    /// Perso a restart/remount (le vuote spariscono, le piene riemergono).
    ram_dirs: BTreeMap<String, u64>,
}

impl ArcaFs {
    pub fn stub(generation: u64, uuid: u64, partition_offset: u64) -> Self {
        Self { generation, uuid, partition_offset, ram_dirs: BTreeMap::new() }
    }

    /// Prefissi propri di `rel` (`a/b/c` → `["a", "a/b"]`): heap, `mkdir`
    /// e' fredda (mai nel percorso dati caldo).
    fn ancestors(rel: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut start = 0usize;
        for (i, b) in rel.as_bytes().iter().enumerate() {
            if *b == b'/' {
                out.push(String::from(&rel[start..i]));
                start = i + 1;
            }
        }
        out
    }

    /// Testa di `rest` (`b/c` → (`b`, Some(`c`)); `b` → (`b`, None)).
    fn split_head(rest: &str) -> (&str, Option<&str>) {
        match rest.find('/') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        }
    }

    /// Classificazione di un path: file, dir (RAM e/o emergente), assente.
    /// La scansione e' O(n) sul namespace (full-scan dichiarata in
    /// `scan_prefix`): per un FS di boot va bene, la range-scan e' futura.
    #[inline(never)]
    fn classify(&mut self, eng: &DiskEngine, rel: &str) -> NsKind {
        if rel.is_empty() {
            return NsKind::Dir { ram_mtime: None };
        }
        // Link prima dei file (il marker decide, mai la base: la base di
        // un link non esiste come chiave).
        if let Some(lk) = link_key(rel) {
            if crate::btree_drv::ns_stat(eng, &lk).is_some() {
                return NsKind::Link;
            }
        }
        if crate::btree_drv::ns_stat(eng, rel.as_bytes()).is_some() {
            return NsKind::File;
        }
        let ram_mtime = self.ram_dirs.get(rel).copied();
        let mut pref = Vec::with_capacity(rel.len() + 1);
        pref.extend_from_slice(rel.as_bytes());
        pref.push(b'/');
        let emergent = crate::btree_drv::ns_scan(eng, &pref)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if ram_mtime.is_some() || emergent {
            NsKind::Dir { ram_mtime }
        } else {
            NsKind::Missing
        }
    }

    /// Figli immediati di `rel` (nomi ordinati): file (resto senza `/`) +
    /// dir (resto con `/`, testa) + dir RAM senza chiavi. Deterministico
    /// come la `BTreeMap` di ramfs.
    #[inline(never)]
    fn children(&mut self, eng: &DiskEngine, rel: &str) -> Option<(Vec<String>, Vec<String>)> {
        let mut pref: Vec<u8> = Vec::new();
        if !rel.is_empty() {
            pref.extend_from_slice(rel.as_bytes());
            pref.push(b'/');
        }
        let keys = crate::btree_drv::ns_scan(eng, &pref)?;
        let mut files: Vec<String> = Vec::new();
        let mut dirs: Vec<String> = Vec::new();
        for k in keys.iter() {
            // Marker symlink: nascosti, la base appare come nome (il tipo
            // lo dice stat, readdir elenca soli nomi come ramfs).
            let k: &[u8] = match unmark(k) {
                Some(base) => base,
                None => k.as_slice(),
            };
            let rem = k.strip_prefix(pref.as_slice())?;
            let rem = core::str::from_utf8(rem).ok()?;
            let (head, rest) = Self::split_head(rem);
            if rest.is_none() {
                if !files.iter().any(|f| f == head) {
                    files.push(String::from(head));
                }
            } else if !dirs.iter().any(|d| d == head) {
                dirs.push(String::from(head));
            }
        }
        for p in self.ram_dirs.keys() {
            let suff = if rel.is_empty() {
                p.as_str()
            } else {
                match p.strip_prefix(rel).and_then(|s| s.strip_prefix('/')) {
                    Some(s) => s,
                    None => continue,
                }
            };
            if suff.is_empty() {
                continue;
            }
            let (head, _) = Self::split_head(suff);
            if !head.is_empty()
                && !dirs.iter().any(|d| d == head)
                && !files.iter().any(|f| f == head)
            {
                dirs.push(String::from(head));
            }
        }
        files.sort();
        dirs.sort();
        Some((files, dirs))
    }

    /// mtime dir = max(mtime figli su disco, mtime RAM se presente). Costo
    /// O(figli): per un FS di boot va bene (nota di scaling per il futuro).
    #[inline(never)]
    fn dir_mtime(&mut self, eng: &DiskEngine, rel: &str, ram_mtime: Option<u64>) -> u64 {
        let mut mt = ram_mtime.unwrap_or(0);
        let mut pref: Vec<u8> = Vec::new();
        if !rel.is_empty() {
            pref.extend_from_slice(rel.as_bytes());
            pref.push(b'/');
        }
        if let Some(keys) = crate::btree_drv::ns_scan(eng, &pref) {
            // Le chiavi tornano intere (niente prefisso da ricucire).
            for k in keys.iter() {
                if let Some((_, m)) = crate::btree_drv::ns_stat(eng, k) {
                    if m > mt {
                        mt = m;
                    }
                }
            }
        }
        mt
    }

    /// Apertura con motore esplicito (semantica `RamFs::open`: OPEN_CREATE crea,
    /// OPEN_TRUNC azzera, dir = ISDIR, antenato file = NOTDIR).
    #[inline(never)]
    pub fn ns_open(
        &mut self,
        eng: &mut DiskEngine,
        rel: &str,
        flags: u32,
    ) -> Result<ArcaHandle, u64> {
        if rel.is_empty() {
            return Err(crate::ERR_NOTFOUND);
        }
        for a in Self::ancestors(rel) {
            if crate::btree_drv::ns_stat(eng, a.as_bytes()).is_some() {
                return Err(crate::ERR_NOTDIR);
            }
        }
        let creat = flags & civis::OPEN_CREATE != 0;
        let trunc = flags & civis::OPEN_TRUNC != 0;
        match self.classify(eng, rel) {
            NsKind::File => {
                if trunc {
                    crate::btree_drv::ns_put(eng, rel.as_bytes(), 0, &[])?;
                }
                ArcaHandle::new(rel).ok_or(crate::ERR_INVALID)
            }
            NsKind::Dir { .. } => Err(crate::ERR_ISDIR),
            // Symlink: l'handler segue prima di aprire (max 8 hop);
            // diretto = loud (mai open del blob-target come file).
            NsKind::Link => Err(crate::ERR_INVALID),
            NsKind::Missing => {
                if !creat {
                    return Err(crate::ERR_NOTFOUND);
                }
                crate::btree_drv::ns_put(eng, rel.as_bytes(), 0, &[])?;
                ArcaHandle::new(rel).ok_or(crate::ERR_INVALID)
            }
        }
    }

    /// Lettura con motore esplicito (oltre EOF = `Ok(0)`, mai panic).
    #[inline(never)]
    pub fn ns_read(
        &mut self,
        eng: &mut DiskEngine,
        h: ArcaHandle,
        off: usize,
        buf: &mut [u8],
    ) -> Result<usize, u64> {
        // S2.5f/F0: range (mai blob intero per slice).
        let (len, data) = crate::btree_drv::ns_get_range(eng, h.as_str().as_bytes(), off, buf.len())
            .ok_or(crate::ERR_NOTFOUND)?;
        if off >= len as usize {
            return Ok(0);
        }
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    /// Scrittura con motore esplicito (W1: UN solo `put_chunk` per op —
    /// il patch e' interno al motore, mai merge in RAM qui; la size viene
    /// da `stat`, mai rilettura integrale del blob).
    #[inline(never)]
    pub fn ns_write(
        &mut self,
        eng: &mut DiskEngine,
        h: ArcaHandle,
        off: usize,
        buf: &[u8],
        append: bool,
    ) -> Result<usize, u64> {
        if buf.is_empty() {
            return Ok(0);
        }
        let (size, _) =
            crate::btree_drv::ns_stat(eng, h.as_str().as_bytes()).ok_or(crate::ERR_NOTFOUND)?;
        let size = size as usize;
        let at = if append { size } else { off };
        if at != 0 || buf.len() >= size {
            // Append / oltre-EOF / overlap interno / rewrite totale: un solo
            // put (0 con buf coprente = versione fresca senza base; >0 =
            // patch interna al motore).
            crate::btree_drv::ns_put(eng, h.as_str().as_bytes(), at, buf)?;
        } else {
            // Prefix-overwrite parziale (at == 0, buf piu' corto del file):
            // put_chunk(0) da solo troncherebbe — merge esplicito, unico caso
            // che rilegge il blob.
            let mut merged =
                crate::btree_drv::ns_get(eng, h.as_str().as_bytes()).ok_or(crate::ERR_NOTFOUND)?;
            merged[..buf.len()].copy_from_slice(buf);
            crate::btree_drv::ns_put(eng, h.as_str().as_bytes(), 0, &merged)?;
        }
        Ok(buf.len())
    }

    /// Readdir con motore esplicito (file = NOTFOUND come ramfs, assente =
    /// NOTFOUND, dir = figli ordinati file+dir).
    #[inline(never)]
    pub fn ns_readdir(
        &mut self,
        eng: &DiskEngine,
        rel: &str,
        out: &mut dyn EntrySink,
    ) -> Result<usize, u64> {
        match self.classify(eng, rel) {
            NsKind::Dir { .. } => {}
            _ => return Err(crate::ERR_NOTFOUND),
        }
        let (files, dirs) = self.children(eng, rel).ok_or(crate::ERR)?;
        for f in files.iter() {
            out.emit(f);
        }
        for d in dirs.iter() {
            out.emit(d);
        }
        Ok(files.len() + dirs.len())
    }

    /// Stat con motore esplicito (root sintetica come gli handler).
    #[inline(never)]
    pub fn ns_stat(&mut self, eng: &DiskEngine, rel: &str) -> Result<Meta, u64> {
        if rel.is_empty() {
            return Ok(Meta { size: 0, kind: 1, readonly: false, mtime: 0 });
        }
        match self.classify(eng, rel) {
            NsKind::File => {
                let (size, mtime) =
                    crate::btree_drv::ns_stat(eng, rel.as_bytes()).ok_or(crate::ERR_NOTFOUND)?;
                Ok(Meta { size, kind: 0, readonly: false, mtime })
            }
            NsKind::Dir { ram_mtime } => {
                let mtime = self.dir_mtime(eng, rel, ram_mtime);
                Ok(Meta { size: 0, kind: 1, readonly: false, mtime })
            }
            // Symlink: stat = lstat (size/mtime del marker, kind 3).
            NsKind::Link => {
                let lk = link_key(rel).ok_or(crate::ERR_INVALID)?;
                let (size, mtime) =
                    crate::btree_drv::ns_stat(eng, &lk).ok_or(crate::ERR_NOTFOUND)?;
                Ok(Meta { size, kind: 3, readonly: false, mtime })
            }
            NsKind::Missing => Err(crate::ERR_NOTFOUND),
        }
    }

    /// Mkdir con motore esplicito: SOLO RAM (mai disco, mai commit).
    /// Esistente = EXISTS (parita' ramfs, t54); antenato file = NOTDIR.
    #[inline(never)]
    pub fn ns_mkdir(&mut self, eng: &DiskEngine, rel: &str) -> Result<(), u64> {
        if rel.is_empty() {
            return Err(crate::ERR_INVALID);
        }
        // Parita' ramfs/POSIX (t54): path esistente (file, dir RAM o
        // emergente) = EXISTS; solo i mai-esistiti si creano. "Idempotente"
        // (ADR-0042) = antenati auto-creati senza errore e mai effetti su
        // disco — il risultato per path esistente resta EXISTS.
        if !matches!(self.classify(eng, rel), NsKind::Missing) {
            return Err(crate::ERR_EXISTS);
        }
        for a in Self::ancestors(rel) {
            if crate::btree_drv::ns_stat(eng, a.as_bytes()).is_some() {
                return Err(crate::ERR_NOTDIR);
            }
        }
        let now = crate::wall::wall_secs();
        for a in Self::ancestors(rel) {
            self.ram_dirs.entry(a).or_insert(now);
        }
        self.ram_dirs.entry(String::from(rel)).or_insert(now);
        Ok(())
    }

    /// Symlink con motore esplicito (S1.1): crea il marker col target.
    /// Esistente (file/dir/link/emergente) = EXISTS; bound = INVALID.
    #[inline(never)]
    pub fn ns_symlink(&mut self, eng: &mut DiskEngine, link: &str, target: &str) -> Result<(), u64> {
        if link.is_empty() || target.is_empty() {
            return Err(crate::ERR_INVALID);
        }
        if target.len() > 1024 {
            return Err(crate::ERR_INVALID);
        }
        let lk = link_key(link).ok_or(crate::ERR_INVALID)?;
        if !matches!(self.classify(eng, link), NsKind::Missing) {
            return Err(crate::ERR_EXISTS);
        }
        crate::btree_drv::ns_put_sync(eng, &lk, 0, target.as_bytes())?;
        Ok(())
    }

    /// Readlink con motore esplicito: target del marker o errore (file/dir
    /// = INVALID, mai target inventati; assente = NOTFOUND).
    #[inline(never)]
    pub fn ns_readlink(&mut self, eng: &DiskEngine, rel: &str) -> Result<Vec<u8>, u64> {
        if rel.is_empty() {
            return Err(crate::ERR_INVALID);
        }
        let lk = link_key(rel).ok_or(crate::ERR_INVALID)?;
        crate::btree_drv::ns_get(eng, &lk).ok_or_else(|| {
            if crate::btree_drv::ns_stat(eng, rel.as_bytes()).is_some() {
                crate::ERR_INVALID
            } else {
                crate::ERR_NOTFOUND
            }
        })
    }

    /// Remove con motore esplicito: file = delete+commit; dir con figli =
    /// NOTFOUND (come ramfs); dir RAM vuota = tolta dal set; mai-esistita =
    /// NOTFOUND (mai `Ok` silenzioso). Link = cancella il marker (mai il
    /// target).
    #[inline(never)]
    pub fn ns_remove(&mut self, eng: &mut DiskEngine, rel: &str) -> Result<(), u64> {
        if rel.is_empty() {
            return Err(crate::ERR_INVALID);
        }
        if let Some(lk) = link_key(rel) {
            if crate::btree_drv::ns_stat(eng, &lk).is_some() {
                crate::btree_drv::ns_delete(eng, &lk)?;
                return Ok(());
            }
        }
        if crate::btree_drv::ns_stat(eng, rel.as_bytes()).is_some() {
            crate::btree_drv::ns_delete(eng, rel.as_bytes())?;
            return Ok(());
        }
        let mut pref = Vec::with_capacity(rel.len() + 1);
        pref.extend_from_slice(rel.as_bytes());
        pref.push(b'/');
        let nonempty = crate::btree_drv::ns_scan(eng, &pref).map(|v| !v.is_empty()).unwrap_or(false);
        if nonempty {
            return Err(crate::ERR_NOTFOUND);
        }
        match self.ram_dirs.remove(rel) {
            Some(_) => Ok(()),
            None => Err(crate::ERR_NOTFOUND),
        }
    }

    /// Rename con motore esplicito (S1.1): file = move chiave a commit
    /// singolo (stesso uuid/storia, replace atomico); dir = move di TUTTE
    /// le chiavi col prefisso + voci ram a UN commit. Regole: file→dir =
    /// ISDIR, dir→file = NOTDIR, dir→non-vuota = EXISTS, assente = NOTFOUND.
    /// Mai oltre il bound chiavi (INVALID, mai troncamenti).
    #[inline(never)]
    pub fn ns_rename(&mut self, eng: &mut DiskEngine, old: &str, new: &str) -> Result<(), u64> {
        if old.is_empty() || new.is_empty() {
            return Err(crate::ERR_INVALID);
        }
        if old.len() > civis::OBJ_KEY_MAX || new.len() > civis::OBJ_KEY_MAX {
            return Err(crate::ERR_INVALID);
        }
        if old == new {
            // No-op identitaria: esiste davvero (file, dir o link)?
            let is_link = link_key(old)
                .map(|lk| crate::btree_drv::ns_stat(eng, &lk).is_some())
                .unwrap_or(false);
            if crate::btree_drv::ns_stat(eng, old.as_bytes()).is_some()
                || self.ram_dirs.contains_key(old)
                || is_link
            {
                return Ok(());
            }
            return Err(crate::ERR_NOTFOUND);
        }
        let old_is_file = crate::btree_drv::ns_stat(eng, old.as_bytes()).is_some();
        let mut old_pref = Vec::with_capacity(old.len() + 1);
        old_pref.extend_from_slice(old.as_bytes());
        old_pref.push(b'/');
        let old_has_kids =
            crate::btree_drv::ns_scan(eng, &old_pref).map(|v| !v.is_empty()).unwrap_or(false)
                || self.ram_dirs.keys().any(|k| k.len() > old.len() && k.starts_with(old) && k.as_bytes().get(old.len()) == Some(&b'/'));
        let new_is_file = crate::btree_drv::ns_stat(eng, new.as_bytes()).is_some();
        let mut new_pref = Vec::with_capacity(new.len() + 1);
        new_pref.extend_from_slice(new.as_bytes());
        new_pref.push(b'/');
        let new_has_kids =
            crate::btree_drv::ns_scan(eng, &new_pref).map(|v| !v.is_empty()).unwrap_or(false)
                || self.ram_dirs.keys().any(|k| k.len() > new.len() && k.starts_with(new) && k.as_bytes().get(new.len()) == Some(&b'/'));
        let new_is_ramdir = self.ram_dirs.contains_key(new);
        // Il marker conta come esistenza (la base di un link non esiste
        // mai come chiave: senza questo il missing scatta prima del ramo
        // link sotto).
        let old_is_link = link_key(old)
            .map(|lk| crate::btree_drv::ns_stat(eng, &lk).is_some())
            .unwrap_or(false);
        if !old_is_file && !old_has_kids && !self.ram_dirs.contains_key(old) && !old_is_link {
            return Err(crate::ERR_NOTFOUND);
        }
        if old_is_file && (new_has_kids || new_is_ramdir) {
            return Err(crate::ERR_ISDIR);
        }
        if !old_is_file && new_is_file {
            return Err(crate::ERR_NOTDIR);
        }
        if !old_is_file && (new_has_kids) {
            return Err(crate::ERR_EXISTS);
        }
        // Link: si sposta IL MARKER (mai follow, mai il target). Dst file
        // o link = replace; dst dir = ISDIR (come i file).
        if let Some(old_lk) = link_key(old) {
            if crate::btree_drv::ns_stat(eng, &old_lk).is_some() {
                if new_has_kids || new_is_ramdir {
                    return Err(crate::ERR_ISDIR);
                }
                if new_is_file {
                    crate::btree_drv::ns_delete(eng, new.as_bytes())?;
                } else if let Some(new_lk) = link_key(new) {
                    if crate::btree_drv::ns_stat(eng, &new_lk).is_some() {
                        crate::btree_drv::ns_delete(eng, &new_lk)?;
                    }
                }
                let new_lk = link_key(new).ok_or(crate::ERR_INVALID)?;
                match eng.rename_key(crate::btree_drv::NS_BUCKET, &old_lk, &new_lk) {
                    Some(_) => {}
                    None => return Err(crate::ERR),
                }
                return crate::btree_drv::commit_or(eng, 0).map(|_| ());
            }
        }
        if old_is_file {
            // File→dir (ram o emergente, anche vuota in senso chiavi) =
            // ISDIR come ramfs (una dst prefisso-di-chiavi E' una dir).
            if new_has_kids {
                return Err(crate::ERR_ISDIR);
            }
            crate::btree_drv::ns_rename(eng, old.as_bytes(), new.as_bytes())?;
            return Ok(());
        }
        // Dir: sposta ogni chiave col prefisso + voci ram, UN commit.
        let mut keys = crate::btree_drv::ns_scan(eng, &old_pref).unwrap_or_default();
        keys.sort();
        for k in keys.iter() {
            let suffix = k.get(old_pref.len()..).ok_or(crate::ERR)?;
            let mut nk = Vec::with_capacity(new_pref.len() + suffix.len());
            nk.extend_from_slice(&new_pref);
            nk.extend_from_slice(suffix);
            if nk.len() > civis::OBJ_KEY_MAX {
                return Err(crate::ERR_INVALID);
            }
            match eng.rename_key(crate::btree_drv::NS_BUCKET, k, &nk) {
                Some(_) => {}
                None => return Err(crate::ERR),
            }
        }
        // Voci ram con prefisso (+ la dir stessa se nel set).
        let mut moves: Vec<(String, u64)> = Vec::new();
        self.ram_dirs.retain(|k, v| {
            if *k == *old || (k.len() > old.len() && k.starts_with(old) && k.as_bytes().get(old.len()) == Some(&b'/')) {
                moves.push((k.clone(), *v));
                false
            } else {
                true
            }
        });
        for (k, v) in moves.into_iter() {
            let suffix = k.get(old.len()..).unwrap_or("");
            let mut nk = String::with_capacity(new.len() + suffix.len());
            nk.push_str(new);
            nk.push_str(suffix);
            self.ram_dirs.insert(nk, v);
        }
        // Antenati del dst come mkdir (dir emergenti visibili subito).
        let now = crate::wall::wall_secs();
        for a in Self::ancestors(new) {
            self.ram_dirs.entry(a).or_insert(now);
        }
        crate::btree_drv::commit_or(eng, 0).map(|_| ())
    }

    /// Statvfs con motore esplicito (blocchi 65536; liberi illimitati come
    /// ramfs — sensore vero con quota/taglio, mai numero inventato).
    #[inline(never)]
    pub fn ns_statvfs(&mut self, eng: &DiskEngine) -> Result<StatVfs, u64> {
        let (high_water, _, _) = eng.store.vol().stats();
        Ok(StatVfs {
            bsize: arcafs::format::ARCA_BLOCK_SIZE as u64,
            blocks: high_water,
            bfree: u64::MAX,
            bavail: u64::MAX,
        })
    }
}

/// Classificazione path del namespace (vista POSIX).
enum NsKind {
    Missing,
    File,
    Dir { ram_mtime: Option<u64> },
    /// Symlink S1.1 (solo marker, mai chiave base).
    Link,
}

/// Suffisso delle chiavi-marker dei symlink (S1.1): il link `a/b` vive SOLO
/// come chiave `a/b\x00symlink` col target come valore (niente doppioni
/// base/marker, niente formato valori nuovo). NUL 0x00 non compare nei path
/// POSIX: i marker non collidono mai con file veri e si filtrano con
/// `strip_suffix` in un punto solo.
const LINK_SUFFIX: &[u8] = b"\x00symlink";

/// Chiave marker di un path (None oltre bound: mai troncamenti).
fn link_key(rel: &str) -> Option<Vec<u8>> {
    let mut k = Vec::with_capacity(rel.len() + LINK_SUFFIX.len());
    k.extend_from_slice(rel.as_bytes());
    k.extend_from_slice(LINK_SUFFIX);
    if k.len() > civis::OBJ_KEY_MAX {
        return None;
    }
    Some(k)
}

/// Base di una chiave marker (None se non e' un marker).
fn unmark(key: &[u8]) -> Option<&[u8]> {
    key.strip_suffix(LINK_SUFFIX)
}

/// Mount Arca + motore globale legati per un'op (56.3): uuid combaciante o
/// niente (mai dati di un altro volume sullo stesso motore).
pub struct ArcaWith<'e> {
    a: &'e mut ArcaFs,
    eng: &'e mut DiskEngine,
}

impl<'e> ArcaWith<'e> {
    /// Lega mount + motore (solo da `mount::arca_with`, dopo il check uuid).
    pub fn bind(a: &'e mut ArcaFs, eng: &'e mut DiskEngine) -> Self {
        Self { a, eng }
    }

    fn open(&mut self, rel: &str, flags: u32) -> Result<ArcaHandle, u64> {
        self.a.ns_open(self.eng, rel, flags)
    }
    fn read(&mut self, h: ArcaHandle, off: usize, buf: &mut [u8]) -> Result<usize, u64> {
        self.a.ns_read(self.eng, h, off, buf)
    }
    fn write(
        &mut self,
        h: ArcaHandle,
        off: usize,
        buf: &[u8],
        append: bool,
    ) -> Result<usize, u64> {
        self.a.ns_write(self.eng, h, off, buf, append)
    }
    fn readdir(&mut self, rel: &str, out: &mut dyn EntrySink) -> Result<usize, u64> {
        self.a.ns_readdir(self.eng, rel, out)
    }
    fn stat(&mut self, rel: &str) -> Result<Meta, u64> {
        self.a.ns_stat(self.eng, rel)
    }
    fn mkdir(&mut self, rel: &str) -> Result<(), u64> {
        self.a.ns_mkdir(self.eng, rel)
    }
    fn remove(&mut self, rel: &str) -> Result<(), u64> {
        self.a.ns_remove(self.eng, rel)
    }
    fn rename(&mut self, old: &str, new: &str) -> Result<(), u64> {
        self.a.ns_rename(self.eng, old, new)
    }
    fn symlink(&mut self, link: &str, target: &str) -> Result<(), u64> {
        self.a.ns_symlink(self.eng, link, target)
    }
    fn readlink(&mut self, rel: &str) -> Result<String, u64> {
        self.a.ns_readlink(self.eng, rel).and_then(|v| {
            String::from_utf8(v).map_err(|_| crate::ERR_INVALID)
        })
    }
    /// chmod S1.1 su Arca: accetta no-op (niente xattr pre-A4: la mode non
    /// si conserva; ramfs la tiene, enforcement zero ovunque fino ad A4).
    /// Esistente (file/dir/link/emergente) = Ok (i build non devono
    /// fallire), mai-esistita = NOTFOUND.
    fn chmod(&mut self, rel: &str, _mode: u32) -> Result<(), u64> {
        match self.a.classify(self.eng, rel) {
            NsKind::Missing => Err(crate::ERR_NOTFOUND),
            _ => Ok(()),
        }
    }
    fn statvfs(&mut self) -> Result<StatVfs, u64> {
        self.a.ns_statvfs(self.eng)
    }
}

impl LocalFsDyn for ArcaWith<'_> {
    fn open_dyn(&mut self, rel: &str, flags: u32) -> Result<crate::provider::AnyHandle, u64> {
        self.open(rel, flags).map(|h| crate::provider::AnyHandle::Arca(h))
    }
    fn read_dyn(
        &mut self,
        h: crate::provider::AnyHandle,
        off: usize,
        buf: &mut [u8],
    ) -> Result<usize, u64> {
        match h {
            crate::provider::AnyHandle::Arca(ah) => self.read(ah, off, buf),
            _ => Err(crate::ERR_INVALID),
        }
    }
    fn write_dyn(
        &mut self,
        h: crate::provider::AnyHandle,
        off: usize,
        buf: &[u8],
        append: bool,
    ) -> Result<usize, u64> {
        match h {
            crate::provider::AnyHandle::Arca(ah) => self.write(ah, off, buf, append),
            _ => Err(crate::ERR_INVALID),
        }
    }
    fn readdir_dyn(&mut self, rel: &str, out: &mut dyn EntrySink) -> Result<usize, u64> {
        self.readdir(rel, out)
    }
    fn stat_dyn(&mut self, rel: &str) -> Result<Meta, u64> {
        self.stat(rel)
    }
    fn mkdir_dyn(&mut self, rel: &str) -> Result<(), u64> {
        self.mkdir(rel)
    }
    fn remove_dyn(&mut self, rel: &str) -> Result<(), u64> {
        self.remove(rel)
    }
    fn rename_dyn(&mut self, old: &str, new: &str) -> Result<(), u64> {
        self.rename(old, new)
    }
    fn symlink_dyn(&mut self, link: &str, target: &str) -> Result<(), u64> {
        self.symlink(link, target)
    }
    fn readlink_dyn(&mut self, rel: &str) -> Result<String, u64> {
        self.readlink(rel)
    }
    fn chmod_dyn(&mut self, rel: &str, mode: u32) -> Result<(), u64> {
        self.chmod(rel, mode)
    }
    fn statvfs_dyn(&mut self, _rel: &str) -> Result<StatVfs, u64> {
        self.statvfs()
    }
}

impl LocalFs for ArcaFs {
    type Handle = ArcaHandle;

    /// Rete di sicurezza (motore assente/mismatch: gli handler usano
    /// `ArcaWith` e non arrivano mai qui — errori tipizzati come prima).
    fn open(&mut self, _rel: &str, _flags: u32) -> Result<Self::Handle, u64> {
        Err(crate::ERR_NOTFOUND)
    }
    fn read(&mut self, _h: Self::Handle, _off: usize, _buf: &mut [u8]) -> Result<usize, u64> {
        Err(crate::ERR_NOTFOUND)
    }
    fn write(&mut self, _h: Self::Handle, _off: usize, _buf: &[u8], _append: bool) -> Result<usize, u64> {
        Err(crate::ERR_READONLY)
    }
    fn readdir(&mut self, _rel: &str, _out: &mut dyn EntrySink) -> Result<usize, u64> {
        Err(crate::ERR_NOTFOUND)
    }
    fn stat(&mut self, _rel: &str) -> Result<Meta, u64> {
        Err(crate::ERR_NOTFOUND)
    }
    fn mkdir(&mut self, _rel: &str) -> Result<(), u64> {
        Err(crate::ERR_READONLY)
    }
    fn remove(&mut self, _rel: &str) -> Result<(), u64> {
        Err(crate::ERR_READONLY)
    }
    fn rename(&mut self, _old: &str, _new: &str) -> Result<(), u64> {
        Err(crate::ERR_READONLY)
    }
    fn symlink(&mut self, _link: &str, _target: &str) -> Result<(), u64> {
        Err(crate::ERR_READONLY)
    }
    fn readlink(&mut self, _rel: &str) -> Result<String, u64> {
        Err(crate::ERR_NOTFOUND)
    }
    fn chmod(&mut self, _rel: &str, _mode: u32) -> Result<(), u64> {
        Err(crate::ERR_READONLY)
    }
    fn statvfs(&mut self, _rel: &str) -> Result<StatVfs, u64> {
        Err(crate::ERR_NOTFOUND)
    }
}

impl LocalFsDyn for ArcaFs {
    fn open_dyn(&mut self, rel: &str, flags: u32) -> Result<crate::provider::AnyHandle, u64> {
        // Sempre `Err` (stub): il `map` non scatta mai, niente handle costruito.
        <Self as LocalFs>::open(self, rel, flags).map(crate::provider::AnyHandle::Arca)
    }
    fn read_dyn(
        &mut self,
        h: crate::provider::AnyHandle,
        off: usize,
        buf: &mut [u8],
    ) -> Result<usize, u64> {
        match h {
            crate::provider::AnyHandle::Arca(ah) => <Self as LocalFs>::read(self, ah, off, buf),
            _ => Err(crate::ERR_INVALID),
        }
    }
    fn write_dyn(
        &mut self,
        h: crate::provider::AnyHandle,
        off: usize,
        buf: &[u8],
        append: bool,
    ) -> Result<usize, u64> {
        match h {
            crate::provider::AnyHandle::Arca(ah) => <Self as LocalFs>::write(self, ah, off, buf, append),
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
    fn rename_dyn(&mut self, old: &str, new: &str) -> Result<(), u64> {
        <Self as LocalFs>::rename(self, old, new)
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
    fn statvfs_dyn(&mut self, rel: &str) -> Result<StatVfs, u64> {
        <Self as LocalFs>::statvfs(self, rel)
    }
}
