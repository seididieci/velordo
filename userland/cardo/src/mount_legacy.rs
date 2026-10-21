use super::*;

// ── Mount table dinamica ───────────────────────────────────────────

/// Risoluzione di un path: filesystem locale o server remoto.
#[derive(Clone, Copy, PartialEq)]
pub enum FsKind {
    Ram,
    Fat,
    /// Mount `Local` (Fase 49, F4: ramfs montata, domani ArcaFS): dispatch
    /// via `local_dyn` + handle `AnyHandle` in ftable, mai path-based reopen.
    Local,
}

/// Un mount point registrato da un driver via FS_REGISTER.
pub struct Mount {
    pub prefix: alloc::string::String,
    /// Canale del driver verso cardo (ADR-0008): cardo inoltra le DEV_* su
    /// QUESTO canale (il driver lo ha aperto con service_lookup(Fs)).
    pub driver_chan: u64,
}

/// Cerca il mount point più lungo che matcha il path (longest prefix match).
/// Ritorna (driver_chan, path relativo al mount).
pub fn resolve_mount<'a>(path: &'a str, mounts: &[Mount]) -> Option<(u64, &'a str)> {
    let t = path.trim_start_matches('/');
    let mut best: Option<(u64, &'a str)> = None;
    for m in mounts {
        let prefix = m.prefix.trim_start_matches('/');
        if t == prefix {
            let rel = "";
            if best.as_ref().map_or(true, |(_, r)| r.len() > rel.len()) {
                best = Some((m.driver_chan, rel));
            }
        } else if t.len() > prefix.len()
            && t.as_bytes().get(prefix.len()) == Some(&b'/')
            && t.starts_with(prefix)
        {
            let rel = &t[prefix.len() + 1..];
            if best.as_ref().map_or(true, |(_, r)| r.len() > rel.len()) {
                best = Some((m.driver_chan, rel));
            }
        }
    }
    best
}

/// Figli immediati di `path` tra i prefix registrati (Fase 16d, discovery).
/// I prefix (`/dev/null`, `/dev/disk/by-uuid/<H>`, …) implicano le directory
/// che li contengono: `readdir("/dev")` → ["console", "disk", "input", …].
/// Root INCLUSA (Fase 18.1-ter: union con dedupe nel chiamante, mai shadow
/// del ramfs): `readdir("/")` → ["dev", …]. Ritorna None se nessun prefix sta
/// sotto `path`. Nessun IPC: la Mount table basta (single source gia' qui).
/// Nomi presi in prestito dai prefix (mai heap: vivono nella Mount table oltre
/// la richiesta); il contenitore e' scratch (vita = iterazione corrente).
pub fn synth_children<'a>(mounts: &'a [Mount], path: &str) -> Option<StrList<'a>> {
    let t = path.trim_matches('/');
    let mut out = StrList::with_capacity(mounts.len())?;
    for m in mounts {
        let p = m.prefix.trim_start_matches('/');
        if t.is_empty() {
            // Root: primo componente di ogni prefix ("dev" da "/dev/null").
            let child = p.split('/').next().unwrap_or("");
            if !child.is_empty() && !out.contains(child) {
                out.push(child);
            }
            continue;
        }
        if p.len() <= t.len() {
            continue;
        }
        if p.starts_with(t) && p.as_bytes().get(t.len()) == Some(&b'/') {
            let rest = &p[t.len() + 1..];
            let child = rest.split('/').next().unwrap_or("");
            if !child.is_empty() && !out.contains(child) {
                out.push(child);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        out.sort();
        Some(out)
    }
}

/// Lista scratch di `&str` (backing civis, vita = iterazione corrente del
/// loop). Capacita' esatta a monte (ogni mount contribuisce al massimo un
/// figlio: il dedupe rende i push ≤ cap): `push` oltre cap e' no-op difensivo
/// (mai heap di fallback — i bound sono strutturali, come i ring).
/// Il contenitore e' un raw pointer (non un borrow `'static`): `as_slice`
/// restituisce un borrow legato a `&self`, che il compilatore traccia
/// nell'iterazione — meglio di un `&'static` che mentirebbe oltre il reset.
pub struct StrList<'a> {
    ptr: *mut &'a str,
    cap: usize,
    len: usize,
}

impl<'a> StrList<'a> {
    fn with_capacity(cap: usize) -> Option<Self> {
        // `'s = 'a`: il borrow del contenitore vive quanto i contenuti.
        let buf = civis::scratch::alloc_slice::<'a, &'a str>(cap)?;
        Some(Self { ptr: buf.as_mut_ptr(), cap, len: 0 })
    }

    fn push(&mut self, s: &'a str) {
        if self.len < self.cap {
            unsafe {
                *self.ptr.add(self.len) = s;
            }
            self.len += 1;
        }
    }

    fn contains(&self, s: &str) -> bool {
        self.as_slice().iter().any(|e| *e == s)
    }

    fn sort(&mut self) {
        unsafe {
            core::slice::from_raw_parts_mut(self.ptr, self.len).sort();
        }
    }

    fn as_slice(&self) -> &[&'a str] {
        unsafe { core::slice::from_raw_parts(self.ptr as *const _, self.len) }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Figli immediati di `path` tra i target dei mount locali (Fase 18.1-ter,
/// speculare a `synth_children`): i target (`fat`, `mnt`, …) sono mount point
/// e compaiono nei listing (`ls /` → ["fat", …]). Include gli inattivi: il
/// mount point esiste, l'accesso fallisce lazy come oggi. None se nessun
/// target sta sotto `path`. Come `synth_children`: nomi in prestito, scratch.
fn fsmount_children<'a>(mounts: &'a [mount::FsMount], path: &str) -> Option<StrList<'a>> {
    let t = path.trim_matches('/');
    let mut out = StrList::with_capacity(mounts.len())?;
    for m in mounts {
        let p = m.target.as_str();
        let rest = if t.is_empty() {
            Some(p)
        } else if p.len() > t.len()
            && p.starts_with(t)
            && p.as_bytes().get(t.len()) == Some(&b'/')
        {
            Some(&p[t.len() + 1..])
        } else {
            None
        };
        if let Some(rest) = rest {
            let child = rest.split('/').next().unwrap_or("");
            if !child.is_empty() && !out.contains(child) {
                out.push(child);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        out.sort();
        Some(out)
    }
}

/// Union di entry locali con i figli dei mount (Fase 18.1-ter): i mount point
/// (`fat`, `dev`, …) compaiono nei listing senza mai coprire le entry locali
/// (dedupe a parita' di nome + sort). Rispecchia `handle_open` (driver → FAT
/// → ramfs): a parita' di nome l'entry e' una sola, mai ambigua.
pub fn union_mount_children(
    mut entries: Vec<String>,
    mounts: &[Mount],
    mounts_fat: &[mount::FsMount],
    path: &str,
) -> Vec<String> {
    // Extra in prestito dalle tabelle (scratch): solo i nomi dei mount point
    // restano owned (pochi, solo nei listing che contengono mount — es. `ls /`).
    if let Some(extra) = synth_children(mounts, path) {
        for e in extra.as_slice() {
            if !entries.iter().any(|x| x.as_str() == *e) {
                entries.push(String::from(*e));
            }
        }
    }
    if let Some(extra) = fsmount_children(mounts_fat, path) {
        for e in extra.as_slice() {
            if !entries.iter().any(|x| x.as_str() == *e) {
                entries.push(String::from(*e));
            }
        }
    }
    entries.sort();
    entries
}

/// Risolve un path in FsKind (ramfs di default).
/// Per i path remoti (/dev/*), ritorna None (usa resolve_mount).
/// Per i path sotto un mount noto, ritorna Fat o Local a seconda della
/// variante (usa resolve_fsmount per id+rel nel percorso fd).
pub fn resolve_local(mounts_fat: &[mount::FsMount], path: &str) -> Option<FsKind> {
    let t = path.trim_start_matches('/');
    if t.starts_with("dev/") || t == "dev" {
        return None; // gestito da resolve_mount
    }
    // Longest-prefix come `resolve_fsmount`, ma senza attivazione (puro):
    // la variante distingue Fat da Local.
    let mut best: Option<&mount::FsMount> = None;
    for m in mounts_fat {
        // Mount root (Fase A): target `""` catch-all — matcha tutto (il
        // longest-prefix sotto preferisce sempre i mount specifici).
        let hit = m.target.is_empty()
            || t == m.target
            || (t.len() > m.target.len()
                && t.as_bytes().get(m.target.len()) == Some(&b'/')
                && t.starts_with(m.target.as_str()));
        if hit && best.map_or(true, |b| m.target.len() > b.target.len()) {
            best = Some(m);
        }
    }
    match best {
        Some(m) if m.is_provider() => Some(FsKind::Local),
        Some(_) => Some(FsKind::Fat),
        None => Some(FsKind::Ram),
    }
}

/// Converte device name in tipo vela (w0 di DEV_OPEN).
pub fn dev_type(name: &str) -> Option<u64> {
    match name {
        "null" => Some(DEV_NULL),
        "zero" => Some(DEV_ZERO),
        "keyboard" => Some(DEV_KEYBOARD),
        "console" => Some(DEV_CONSOLE),
        "kbd" => Some(DEV_KBD),
        _ => None,
    }
}

/// Parsa un nome nodo disco Linux ("sda".."sdp", "sda1"..) in handle codificato
/// (disco<<16|sub, 0 = whole-disk). SOLO per gli open raw `/dev/sdX` (rel
/// vuota): il mount (Fase 16c) risolve l'handle presso block via
/// DISK_RESOLVE invece di indovinarlo qui. Ritorna None se non e' un nome disco.
pub fn disk_handle(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("sd")?;
    let mut chars = rest.chars();
    let letter = chars.next()?;
    if !('a'..='p').contains(&letter) {
        return None;
    }
    let disk = (letter as u32) - ('a' as u32);
    let tail: String = chars.collect();
    let sub = if tail.is_empty() {
        0
    } else {
        let n: u32 = tail.parse().ok()?;
        if n == 0 || n > 64 {
            return None;
        }
        n
    };
    Some((disk << 16) | sub)
}
