use super::*;
use crate::provider::LocalFs;
use crate::provider::LocalFsDyn;

// ── Handler (Option<u64> internamente) ─────────────────────────────

/// Vista POSIX ArcaFS col motore globale (56.3): se il mount `mid` e' un
/// volume Arca e l'uuid combacia col motore, lega i due per l'op. `None` =
/// altro provider, motore assente o altro volume (il chiamante usa il path
/// generico/stub loud — mai dati di un altro volume sullo stesso motore).
fn arca_ns<'m>(
    mounts_fat: &'m mut Vec<mount::FsMount>,
    mid: u64,
    disk: &'m mut Option<btree_drv::DiskEngine>,
) -> Option<crate::arca::ArcaWith<'m>> {
    mount::by_id_mut(mounts_fat, mid)?.arca_with(disk.as_mut())
}

#[inline(never)]
pub fn handle_open(
    fs: &mut ramfs::RamFs,
    ftable: &mut ftable::FileTable,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    chan: u64,
    flags: u64,
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    let flags = flags as u32;
    let creat = flags & civis::OPEN_CREATE != 0;
    let trunc = flags & civis::OPEN_TRUNC != 0;
    let append = flags & civis::OPEN_APPEND != 0;

    // S1.1: segue i symlink (max 8 hop) prima del routing — tranne i path
    // remoti (driver opachi, mai link). Il probe e' None per i non-link
    // (mai errore spurio: solo il routing fallito resta Err).
    let owned;
    let path = if mount_legacy::resolve_mount(path, mounts).is_none() {
        owned = follow_links(fs, mounts_fat, mounts, path, fgen, disk)?;
        owned.as_str()
    } else {
        path
    };

    // Cerca nei mount point registrati (vela, console, block, futuri driver).
    if let Some((driver_chan, rel)) = mount_legacy::resolve_mount(path, mounts) {
        // (Ramo device invariato: gli errori dei driver restano opachi —
        // nessun dominio attribuibile senza interrogarli.)
        if rel.is_empty() {
            let prefix = path.trim_start_matches('/');
            if let Some(name) = prefix.strip_prefix("dev/") {
                if name.starts_with("disk/by-") {
                    let h = mount::resolve_mount_source(&alloc::format!("/dev/{}", name)).ok_or(ERR)?;
                    let reply = civis::send(driver_chan, DEV_OPEN, h as u64, 0).map_err(|_| ERR)?;
                    if reply.w0 == ERR {
                        return Err(ERR);
                    }
                    return Ok(ftable.open_remote(chan, driver_chan, reply.w0 as u32));
                }
                if let Some(h) = mount_legacy::disk_handle(name) {
                    let reply = civis::send(driver_chan, DEV_OPEN, h as u64, 0).map_err(|_| ERR)?;
                    if reply.w0 == ERR {
                        return Err(ERR);
                    }
                    return Ok(ftable.open_remote(chan, driver_chan, reply.w0 as u32));
                }
                if let Some(dev) = name.rsplit('/').next().and_then(mount_legacy::dev_type) {
                    let reply = civis::send(driver_chan, DEV_OPEN, dev, 0).map_err(|_| ERR)?;
                    if reply.w0 == ERR {
                        return Err(ERR);
                    }
                    return Ok(ftable.open_remote(chan, driver_chan, reply.w0 as u32));
                }
            }
            return Err(ERR_NOTFOUND);
        }
        let device_type = mount_legacy::dev_type(rel).ok_or(ERR_NOTFOUND)?;
        let reply = civis::send(driver_chan, DEV_OPEN, device_type, 0).map_err(|_| ERR)?;
        let remote_fd = reply.w0 as u32;
        return Ok(ftable.open_remote(chan, driver_chan, remote_fd));
    }

    // Filesystem locali: prima i mount (FAT con attivazione lazy, Local
    // sempre attivi), poi ramfs radice.
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        // Mount `Local` (Fase 49, F4): open via `open_dyn`, handle AnyHandle
        // nell'fd (mai reopen per-path, mai cache da invalidare). Sonda senza
        // tenere borrow oltre l'op (le varianti sono esclusive).
        let is_local = matches!(
            mount::by_id(mounts_fat, mid).and_then(|i| mounts_fat.get(i)),
            Some(m) if m.is_provider()
        );
        if is_local {
            // 56.3 — ArcaFS col motore globale (uuid combaciante): se il
            // motore manca o e' un altro volume si cade sullo stub loud.
            if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
                // Fase A: niente map_err — gli errori tipizzati (IsDir,
                // NotDir, NotFound) devono arrivare al client intatti: la
                // root e' ArcaFS e i test (t54) distinguono IsDir/NotFound.
                let h = w.open_dyn(rel, flags)?;
                return Ok(ftable.open_local(chan, rel, mid, h, append));
            }
            let h = mount::by_id_mut(mounts_fat, mid)
                .ok_or(ERR)?
                .local_dyn()
                .ok_or(ERR)?
                .open_dyn(rel, flags)
                .map_err(|_| ERR_NOTFOUND)?;
            return Ok(ftable.open_local(chan, rel, mid, h, append));
        }
        // 49.5 — open FAT via trait `LocalFs` sul concreto (F5): `Fat32::open`
        // assorbe OPEN_CREATE (crea), OPEN_TRUNC (tronca) e rifiuta le dir (ISDIR)
        // come `RamFs::open` — niente piu' create_file/truncate/find fuori
        // trait. La cache per-fd resta in ftable (path-based storage, come U1).
        let info = mount::by_id_mut(mounts_fat, mid)
            .ok_or(ERR)?
            .fat_mut()
            .ok_or(ERR)?
            .open(rel, flags)
            .map_err(|_| ERR_NOTFOUND)?;
        // OPEN_CREATE/OPEN_TRUNC possono aver mutato il volume: invalida le cache.
        if creat || trunc {
            *fgen = fgen.wrapping_add(1);
        }
        return Ok(ftable.open_fat(chan, rel, mid, info, *fgen, append));
    }
    match mount_legacy::resolve_local(mounts_fat, path).ok_or(ERR_NOTFOUND)? {
        // Mount noto ma inattivo, o Local non risolto sopra (difensivo: i
        // Local attivi passano sempre da resolve_fsmount): errore, mai shadow.
        mount_legacy::FsKind::Fat | mount_legacy::FsKind::Local => Err(ERR),
        mount_legacy::FsKind::Ram => {
            // 47.1 — open via trait `LocalFs` (U1): la trait gestisce OPEN_CREATE,
            // validazione file/dir e OPEN_TRUNC internamente; il RamHandle restituito
            // non si memorizza in ftable (U1 mantiene path-based storage).
            let _ = crate::provider::LocalFs::open(fs, path, flags as u32)?;
            Ok(ftable.open(chan, path, mount_legacy::FsKind::Ram, None, append))
        }
    }
}

#[inline(never)]
pub fn handle_read(
    fs: &mut ramfs::RamFs,
    ftable: &mut ftable::FileTable,
    pipes: &mut pipes::PipeTable,
    mounts_fat: &mut Vec<mount::FsMount>,
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    fd: u32,
    count: usize,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if count > 4096 {
        return Err(ERR_INVALID);
    }

    // File remoto: inoltro al driver. cardo inietta entrambi i ring del
    // client nel driver (`map_in`): il driver legge dalla request ring e
    // scrive nella response ring → zero copie (Fase 10.2).
    if let Some((driver_chan, remote_fd)) = ftable.get_remote(chan, fd) {
        let (req_phys, resp_phys) = match rings.get(&chan) {
            Some(&r) => r,
            None => return Err(ERR),
        };
        civis::map_in(driver_chan, req_phys, civis::CLI_REQ_VA, 1).map_err(|_| ERR)?;
        civis::map_in(driver_chan, resp_phys, civis::CLI_RESP_VA, 1).map_err(|_| ERR)?;
        let reply = civis::send(driver_chan, DEV_READ, remote_fd as u64, count as u64).map_err(|_| ERR)?;
        return Ok(reply.w0);
    }
    // Estremita' di pipe in lettura (Fase 42): mai dalla tail condivisa, mai
    // blocco — vedi handle_pipe_read (Empty vs EOF distinti per costruzione).
    if let Some((pipe_id, write)) = ftable.get_pipe(chan, fd) {
        if write {
            return Err(ERR_INVALID); // read sul lato scrittura
        }
        return handle_pipe_read(pipes, rings, chan, pipe_id, count);
    }
    if ftable.get(chan, fd).is_none() {
        return Err(ERR_INVALID);
    }

    let (path, kind, offset, mnt) = ftable.get(chan, fd).ok_or(ERR_INVALID)?;

    // 24.2 — buffer di risposta sullo stack (count ≤ 4096 per il check in
    // testa): niente `to_vec()`/`Vec` temporanei per-op (la free-list
    // dell'heap di cardo cresceva di ~1 blocco a op FAT → scansioni O(n)
    // su tutte le op successive; vedi read_dir in fat32.rs).
    let mut buf_stack = [0u8; 4096];
    let data: &[u8] = match kind {
        mount_legacy::FsKind::Ram => {
            // 47.2 — read via trait `LocalFs` (U1): open per ottenere handle,
            // poi read attraverso la trait. Flags=0 = sola lettura (no create/truncate).
            let handle = crate::provider::LocalFs::open(fs, path, 0)?;
            let n = crate::provider::LocalFs::read(fs, handle, offset, &mut buf_stack[..count])?;
            &buf_stack[..n]
        }
        mount_legacy::FsKind::Local => {
            // 49.4 — read via `AnyHandle` dell'fd (Fase 49, F4): aperto una
            // volta con `open_dyn`, mai reopen per-path. 56.3: ArcaFS col
            // motore globale (uuid combaciante), altrimenti path generico.
            let h = ftable.get_dyn_handle(chan, fd).ok_or(ERR_INVALID)?;
            let mid = mnt.ok_or(ERR)?;
            let n = if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
                w.read_dyn(h, offset, &mut buf_stack[..count])?
            } else {
                let d = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
                d.read_dyn(h, offset, &mut buf_stack[..count])?
            };
            &buf_stack[..n]
        }
        mount_legacy::FsKind::Fat => {
            // 49.1 — read via trait `LocalFsDyn` con `AnyHandle` by-value
            // (Fase 49, F1): niente piu' `*const ()`. La cache FileInfo per-fd
            // (Fase 21) resta la sorgente dell'handle: un reopen per path
            // farebbe un find a OGNI read (~8x sui load da disco, regressione
            // misurata nei restart t27/t28/t32). L'handle e' copiato sullo
            // stack (FileInfo: Copy, niente heap per-op, regola Fase 24).
            let mid = mnt.ok_or(ERR)?;
            // Il mount puo' essere caduto inattivo alla morte di block
            // (drop d'epoca in `note_peer_death`): riattiva per id qui, come
            // `resolve_fsmount` fa per open/readdir (fail-loud, mai shadow).
            if !mount::reactivate_mount_by_id(mounts_fat, mid, fgen) {
                return Err(ERR);
            }
            let g = *fgen;
            let fat_c = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.fat().ok_or(ERR)?;
            let info = ftable::fd_fat_info(ftable, fat_c, chan, fd, g).ok_or(ERR_NOTFOUND)?;
            if info.is_dir {
                return Err(ERR_ISDIR);
            }
            let fat = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
            let n = crate::provider::LocalFsDyn::read_dyn(
                fat,
                crate::provider::AnyHandle::Fat(info),
                offset,
                &mut buf_stack[..count],
            )?;
            &buf_stack[..n]
        }
    };

    let bytes_read = data.len();
    // Frame SEMPRE, anche vuoto a EOF (Fase 18.2-bis): senza, il client non
    // trova risposta e riporta -1 invece di 0. Stesso contratto dei driver
    // (console scrive sempre il frame). Short (< count) o vuoto (0) = EOF.
    if let Some(&(_, _)) = rings.get(&chan) {
        rings::map_client_resp_ring(rings, chan);
        rings::resp_ring_write(bytes_read as u64, 0, &data);
    }
    ftable.set_offset(chan, fd, offset + bytes_read);
    Ok(bytes_read as u64)
}

/// Read da estremita' pipe (Fase 42): self-written come le read locali
/// (il dispatch non riscrive per R_READ). Tre esiti, mai blocco:
/// dati → frame con payload; vuota con writer aperti → ERR_EMPTY (il client
/// riprova throttled: 0 significherebbe EOF e troncherebbe la pipeline);
/// vuota con writer chiusi → frame vuoto + 0 (EOF vero, contratto 18.2-bis).
#[inline(never)]
fn handle_pipe_read(
    pipes: &mut pipes::PipeTable,
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    pipe_id: u32,
    count: usize,
) -> Result<u64, u64> {
    let mut buf_stack = [0u8; 4096];
    let n = count.min(4096);
    let (got, eof) = match pipes.read(pipe_id, &mut buf_stack[..n]) {
        Some(v) => v,
        None => return Err(ERR), // id ignoto: inconsistenza interna
    };
    // Pipe vuota con lettori vivi: il deny-frame lo scrive il dispatch
    // (contratto "un solo frame per op", test-6).
    if got == 0 && !eof {
        return Err(ERR_EMPTY);
    }
    if let Some(&(_, _)) = rings.get(&chan) {
        rings::map_client_resp_ring(rings, chan);
        rings::resp_ring_write(got as u64, 0, &buf_stack[..got]);
    }
    Ok(got as u64)
}

/// Crea una pipe (Fase 42): buffer + due fd (lettura, scrittura) sul canale
/// del chiamante. Ritorna (read_fd, write_fd): il dispatch li mette in
/// result e w1 del response frame.
#[inline(never)]
pub fn handle_pipe_create(
    ftable: &mut ftable::FileTable,
    pipes: &mut pipes::PipeTable,
    chan: u64,
    hint: usize,
) -> Result<(u64, u64), u64> {
    let id = pipes.create(hint);
    let r = ftable.open_pipe(chan, id, false);
    let w = ftable.open_pipe(chan, id, true);
    Ok((r, w))
}

/// File remoto: inoltro al driver. Il payload del WRITE RESTA nel request ring
/// del client (zero copy): cardo inietta entrambi i ring del client nel driver
/// (`map_in`), il driver legge i dati direttamente dal request ring e avanza la
/// tail (SPSC). La chiamata NON deve consumare il frame nel request ring.
/// Ritorna i byte accettati dal driver (reply.w0).
#[inline(never)]
pub fn handle_write_remote(
    ftable: &ftable::FileTable,
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    fd: u32,
    count: usize,
) -> Result<u64, u64> {
    let (driver_chan, remote_fd) = match ftable.get_remote(chan, fd) {
        Some(r) => r,
        None => return Err(ERR_INVALID),
    };
    let (req_phys, resp_phys) = match rings.get(&chan) {
        Some(&r) => r,
        None => return Err(ERR),
    };
    civis::map_in(driver_chan, req_phys, civis::CLI_REQ_VA, 1).map_err(|_| ERR)?;
    civis::map_in(driver_chan, resp_phys, civis::CLI_RESP_VA, 1).map_err(|_| ERR)?;
    let reply = civis::send(driver_chan, DEV_WRITE, remote_fd as u64, count as u64).map_err(|_| ERR)?;
    Ok(reply.w0)
}

/// Write locale (ramfs): il frame e' gia' stato consumato e il payload e' in
/// Scrive `count` byte di `payload` sul fd (ramfs o FAT-overwrite). FAT32 e'
/// scrivibile dalla Fase 20 (write-through, niente cache): solo overwrite
/// entro la size esistente (20.2) — la crescita/creazione arrivano dopo.
#[inline(never)]
pub fn handle_write_local(
    fs: &mut ramfs::RamFs,
    ftable: &mut ftable::FileTable,
    pipes: &mut pipes::PipeTable,
    mounts_fat: &mut Vec<mount::FsMount>,
    chan: u64,
    fd: u32,
    count: usize,
    payload: &[u8],
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    // Estremita' di pipe in scrittura (Fase 42): append/offset ignorati (le
    // pipe non hanno offset); oltre la capacita' = parziale (il client
    // rimanda); senza lettori = ERR_CLOSED (SIGPIPE senza segnali).
    if let Some((pipe_id, write)) = ftable.get_pipe(chan, fd) {
        if !write {
            return Err(ERR_INVALID); // write sul lato lettura
        }
        if !pipes.has_readers(pipe_id) {
            return Err(ERR_CLOSED);
        }
        let want = count.min(payload.len());
        match pipes.write(pipe_id, &payload[..want]) {
            // Zero accettati a lettori vivi (piena): NON 0 (il client lo
            // leggerebbe come "fatto") ma ERR_EMPTY — il client riprova
            // throttled finche' il lettore drena (wrapping di write_fs).
            Some(0) => return Err(ERR_EMPTY),
            Some(n) => return Ok(n as u64),
            None => return Err(ERR),
        }
    }
    let (path, kind, offset, mnt) = ftable.get(chan, fd).ok_or(ERR_INVALID)?;
    let append = ftable.is_append(chan, fd);
    if kind == mount_legacy::FsKind::Local {
        // Scrittura su mount `Local` (Fase 49, F4): handle dell'fd, mai
        // reopen; niente generazioni (il provider e' authoritative). 56.3:
        // ArcaFS col motore globale (uuid combaciante), altrimenti generico.
        let h = ftable.get_dyn_handle(chan, fd).ok_or(ERR_INVALID)?;
        let mid = mnt.ok_or(ERR)?;
        let n = if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
            w.write_dyn(h, offset, &payload[..count.min(payload.len())], append)?
        } else {
            let d = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
            d.write_dyn(h, offset, &payload[..count.min(payload.len())], append)?
        };
        // OPEN_APPEND non usa `offset`: il nuovo offset e' la size dopo la
        // scrittura (via stat fresca, mai stale oltre l'op).
        let new_off = if append {
            let rel_owned: alloc::string::String = alloc::string::String::from(path);
            if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
                w.stat_dyn(&rel_owned).map(|m| m.size as usize).unwrap_or(offset + n as usize)
            } else {
                let d = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
                d.stat_dyn(&rel_owned).map(|m| m.size as usize).unwrap_or(offset + n as usize)
            }
        } else {
            offset + n as usize
        };
        ftable.set_offset(chan, fd, new_off);
        return Ok(n as u64);
    }
    if kind == mount_legacy::FsKind::Fat {
        // Scrittura FAT (Fase 20): overwrite + crescita con allocazione
        // (write-through, niente cache FileInfo: la scrittura puo' cambiare
        // size/first_cluster, quindi dopo si bumpa la generazione e si
        // riaggiorna la cache con un find fresco — un find per write, rumore
        // contro le centinaia di round-trip DISK della scrittura stessa).
        let mi = mnt.ok_or(ERR)?;
        if !mount::reactivate_mount_by_id(mounts_fat, mi, fgen) {
            return Err(ERR);
        }
        let g = *fgen;
        // `path` presta da ftable: clonato una volta (come il vecchio handler)
        // per poter prendere ftable in mut per la cache FileInfo.
        let rel_path: String = alloc::string::String::from(path);
        // Cache FileInfo per-fd come la read (evita un find per write). L'handle
        // passato alla trait e' il FileInfo cachato (copia sullo stack, F1).
        let fat_c = mount::by_id_mut(mounts_fat, mi).ok_or(ERR)?.fat().ok_or(ERR)?;
        let info = ftable::fd_fat_info(ftable, fat_c, chan, fd, g).ok_or(ERR_NOTFOUND)?;
        if info.is_dir {
            return Err(ERR_ISDIR);
        }
        let fat = mount::by_id_mut(mounts_fat, mi).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
        let n = crate::provider::LocalFsDyn::write_dyn(
            fat,
            crate::provider::AnyHandle::Fat(info),
            offset,
            &payload[..count.min(payload.len())],
            append,
        )?;
        *fgen = fgen.wrapping_add(1);
        let g2 = *fgen;
        // Rileggi l'entry dopo la mutazione (size/first_cluster possono aver
        // cambiato valore): la cache resta valida alla nuova generazione.
        let fat_c = mount::by_id_mut(mounts_fat, mi).ok_or(ERR)?.fat().ok_or(ERR)?;
        let fresh = fat_c.find(&rel_path);
        // OPEN_APPEND non usa `offset` del fd: il nuovo offset e' la size dopo la
        // scrittura. Altrimenti offset + n (contratto ramfs).
        let new_off = if append {
            fresh.map(|i| i.size as usize).unwrap_or(offset + n as usize)
        } else {
            offset + n as usize
        };
        ftable.refresh_fat_info(chan, fd, fresh, g2);
        ftable.set_offset(chan, fd, new_off);
        return Ok(n as u64);
    }

    // 47.3 — write via trait `LocalFs` (U1): open con OPEN_CREATE per creare file
    // inesistenti, poi write attraverso la trait (gestisce resize + copy).
    let handle = crate::provider::LocalFs::open(fs, path, civis::OPEN_CREATE)?;
    let n = crate::provider::LocalFs::write(fs, handle, offset, payload, append)?;
    ftable.set_offset(chan, fd, offset + n as usize);
    Ok(n as u64)
}

#[inline(never)]
pub fn handle_close(
    ftable: &mut ftable::FileTable,
    pipes: &mut pipes::PipeTable,
    chan: u64,
    fd: u32,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    // Estremita' pipe: rimuovi l'fd e decrementa il conteggio (l'ultima
    // close libera il buffer, mai leak a pipeline finite).
    if let Some((pipe_id, write)) = ftable.get_pipe(chan, fd) {
        ftable.close(chan, fd);
        pipes.end_closed(pipe_id, write);
        return Ok(0);
    }
    // File remoto: chiudi anche sul server.
    if let Some((driver_chan, remote_fd)) = ftable.get_remote(chan, fd) {
        let _ = civis::send(driver_chan, DEV_CLOSE, remote_fd as u64, 0);
    }
    // Solo i file locali possono aver sporcato l'epoca (ramfs/FAT non
    // toccano il motore; pipe/remote escluse sopra): la loro close la
    // commita (W4 commit batching — le read-only non sporcano mai, quindi
    // la loro close resta no-op). Errore di commit onesto (stile EIO).
    let flush = matches!(
        ftable.get(chan, fd),
        Some((_, mount_legacy::FsKind::Local, _, _))
    );
    if ftable.close(chan, fd) {
        if flush {
            if let Some(d) = disk {
                btree_drv::epoch_flush(d)?;
            }
        }
        Ok(0)
    } else {
        Err(ERR_INVALID)
    }
}

#[inline(never)]
pub fn handle_readdir(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    // Directory remota (device): inoltro al driver, che scrive le entry nella
    // response ring del client (mappata li' da map_in).
    if let Some((driver_chan, _rel)) = mount_legacy::resolve_mount(path, mounts) {
        let (req_phys, resp_phys) = rings.get(&chan).ok_or(ERR)?;
        civis::map_in(driver_chan, *req_phys, civis::CLI_REQ_VA, 1).map_err(|_| ERR)?;
        civis::map_in(driver_chan, *resp_phys, civis::CLI_RESP_VA, 1).map_err(|_| ERR)?;
        let reply = civis::send(driver_chan, DEV_READDIR, 0, 0).map_err(|_| ERR)?;
        return Ok(reply.w0);
    }

    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        // 48.5 — readdir via trait `LocalFsDyn`: la trait gestisce il path relativo al mount.
        // 56.3: ArcaFS col motore globale (uuid combaciante), altrimenti generico.
        let mut entries: Vec<String> = Vec::new();
        {
            struct CollectSink<'a>(&'a mut Vec<String>);
            impl crate::provider::EntrySink for CollectSink<'_> {
                #[inline(never)]
                fn emit(&mut self, name: &str) {
                    self.0.push(alloc::string::String::from(name));
                }
            }
            if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
                let _ = w.readdir_dyn(rel, &mut CollectSink(&mut entries))?;
            } else {
                let fat = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
                let _ = crate::provider::LocalFsDyn::readdir_dyn(fat, rel, &mut CollectSink(&mut entries))?;
            }
        }
        // Mount annidati sotto dir FAT (edge raro, gratis col design union).
        let entries = mount_legacy::union_mount_children(entries, mounts, mounts_fat, path);
        let mut buf = Vec::new();
        for entry in &entries {
            buf.extend_from_slice(entry.as_bytes());
            buf.push(0);
        }
        buf.push(0);
        if let Some(&(_, _)) = rings.get(&chan) {
            rings::map_client_resp_ring(rings, chan);
            rings::resp_ring_write(entries.len() as u64, 0, &buf);
        }
        return Ok(entries.len() as u64);
    }

    // Listing sintetizzato dai prefix registrati (Fase 16d, discovery):
    // se `path` e' directory padre di prefix noti (es. "/dev",
    // "/dev/disk/by-uuid") elenca i figli immediati. Solo dove ramfs/fat non
    // hanno la dir (mai shadow, mai cambi ai listing esistenti). Nota:
    // `resolve_local` esclude i path /dev/* (None) prima ancora di guardare
    // ramfs — la sintesi copre anche quelli.
    //
    // Fase 18.1-ter: i mount point si mergiano SEMPRE (union con dedupe, mai
    // shadow): `ls /` mostra ramfs + `fat` + `dev`. Solo nomi, mai contenuti:
    // il check subtree Fase 17 sul path richiesto resta prima del dispatch.
    // Directory esistente ma vuota resta OK (exists): solo "sconosciuto E
    // senza mount" e' errore.
    let (exists, base): (bool, Vec<String>) = match mount_legacy::resolve_local(mounts_fat, path) {
        Some(mount_legacy::FsKind::Ram) => {
            // 47.4 — readdir via trait `LocalFs` (U1): sink inline per raccogliere entry.
            // Ok(0) su dir vuota = esiste ma senza figli; Err = inesistente.
            let mut entries: Vec<String> = Vec::new();
            struct CollectSink<'a>(&'a mut Vec<String>);
            impl crate::provider::EntrySink for CollectSink<'_> {
                #[inline(never)]
                fn emit(&mut self, name: &str) { self.0.push(alloc::string::String::from(name)); }
            }
            let ok = crate::provider::LocalFs::readdir(fs, path, &mut CollectSink(&mut entries)).is_ok();
            (ok, entries)
        }
        // Mount noto ma inattivo, o Local non risolto sopra (difensivo: i
        // Local attivi passano sempre da resolve_fsmount): errore, mai shadow.
        Some(mount_legacy::FsKind::Fat) | Some(mount_legacy::FsKind::Local) => {
            return Err(ERR_NOTFOUND)
        }
        None => (false, Vec::new()),
    };
    let entries = mount_legacy::union_mount_children(base, mounts, mounts_fat, path);
    if !exists && entries.is_empty() {
        // Distingue "e' un file" (NotDir) da "non esiste" (NotFound): la
        // union sopra e' invariata (mai shadow), si raffina solo l'errore.
        match fs.find(path) {
            Some(ramfs::FsNode::File { .. }) => return Err(ERR_NOTDIR),
            _ => return Err(ERR_NOTFOUND),
        }
    }

    let mut buf = Vec::new();
    for entry in &entries {
        buf.extend_from_slice(entry.as_bytes());
        buf.push(0);
    }
    buf.push(0);
    // Scrivi le entry nella response ring del client.
    if let Some(&(_, _)) = rings.get(&chan) {
        rings::map_client_resp_ring(rings, chan);
        rings::resp_ring_write(entries.len() as u64, 0, &buf);
    }
    Ok(entries.len() as u64)
}

/// Scrive il response frame di R_STAT (`[size:8][kind:8][mtime:8]`, Fase 50)
/// e ritorna 0 per il reply IPC (self-written: il dispatch non riscrive).
/// `mtime` = secondi epoch dal provider (`Meta`, mai fabbricato qui); 0 =
/// sconosciuto (sintetici root/device/padri, senza dir-entry).
#[inline(never)]
pub fn stat_reply(rings: &BTreeMap<u64, (u64, u64)>, chan: u64, size: u64, kind: u64, mtime: u64) -> u64 {
    if rings.get(&chan).is_some() {
        rings::map_client_resp_ring(rings, chan);
        rings::resp_ring_write(size, kind, &mtime.to_le_bytes());
    }
    0
}

/// Codifica il campo `kind` di R_STAT da `Meta` (Fase 48): bit
/// STAT_FILE/DIR + STAT_READONLY dal provider. `civis::stat` decodifica
/// `w1 & STAT_READONLY`; senza questa propagazione `Meta.readonly` sarebbe
/// ignorato (era il caso prima del wiring FAT).
#[inline(never)]
fn stat_kind(meta: &crate::provider::Meta) -> u64 {
    // S1.1: passa i 2 bit di tipo intatti (file/dir/device/symlink=3) +
    // bit readonly. Prima collassava tutto a file/dir (il symlink spariva).
    let base = (meta.kind & 0x3) as u64;
    if meta.readonly {
        base | civis::STAT_READONLY
    } else {
        base
    }
}

/// R_STAT: metadati del path (Fase 19.2, zero kernel). Self-written come
/// read/readdir (frame `[size:8][kind:8][mtime:8]`, vedi `stat_reply`); None =
/// inesistente. Precedenza come open (mai shadow): device esatti → FAT (con
/// attivazione lazy) → ramfs → padri sintetizzati 16d → None. Mount FAT noto
/// ma inattivo = errore (stesso contratto di open/readdir). kind in
/// `syscall-numbers` (STAT_FILE/DIR/DEVICE + STAT_READONLY): ramfs da' len
/// reale (dir = 0, mai readonly), FAT size dalla dir entry (scrivibile dalla
/// Fase 20: mai readonly), device size 0 readonly 0 (sconosciuto senza
/// interrogare il driver: i prefix registrati sono foglie, qui mai contattati).
#[inline(never)]
pub fn handle_stat(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    // Root ramfs: esiste sempre.
    if path == "/" {
        return Ok(stat_reply(rings, chan, 0, civis::STAT_DIR, 0));
    }
    // Device registrati: foglie (rel non vuota = path sotto un device: None,
    // come open che rifiuta i dev_type sconosciuti).
    if let Some((_driver_chan, rel)) = mount_legacy::resolve_mount(path, mounts) {
        if rel.is_empty() {
            return Ok(stat_reply(rings, chan, 0, civis::STAT_DEVICE, 0));
        }
        return Err(ERR_NOTFOUND);
    }
    // FAT con attivazione lazy; mount noto ma inattivo = errore, mai shadow.
    // (find fresco a ogni stat: niente fd, niente cache — i metadati non
    // devono mai essere stale.)
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        // 48.5 — stat via trait `LocalFsDyn`: la trait gestisce il path relativo al mount.
        // 56.3: ArcaFS col motore globale (uuid combaciante), altrimenti generico.
        if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
            if rel.is_empty() {
                return Ok(stat_reply(rings, chan, 0, civis::STAT_DIR, 0));
            }
            let meta = w.stat_dyn(rel)?;
            return Ok(stat_reply(rings, chan, meta.size, stat_kind(&meta), meta.mtime));
        }
        let fat = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
        if rel.is_empty() {
            return Ok(stat_reply(rings, chan, 0, civis::STAT_DIR, 0));
        }
        let meta = fat.stat_dyn(rel)?;
        return Ok(stat_reply(rings, chan, meta.size, stat_kind(&meta), meta.mtime));
    }
    match mount_legacy::resolve_local(mounts_fat, path) {
        // Mount noto ma inattivo, o Local non risolto sopra (difensivo):
        // errore, mai shadow ramfs.
        Some(mount_legacy::FsKind::Fat) | Some(mount_legacy::FsKind::Local) => Err(ERR),
        Some(mount_legacy::FsKind::Ram) => {
            // 47.4 — stat via trait `LocalFs` (U1): metadati diretti dalla trait.
            let meta = crate::provider::LocalFs::stat(fs, path)?;
            Ok(stat_reply(rings, chan, meta.size, stat_kind(&meta), meta.mtime))
        }
        // /dev/* senza prefix noto: solo sintesi (sotto).
        None => mount_legacy::synth_children(mounts, path)
            .map(|_| stat_reply(rings, chan, 0, civis::STAT_DIR, 0))
            .ok_or(ERR_NOTFOUND),
    }
}

/// R_DISK_LIST (Fase 51, P2): topologia dischi via `IpcDisk::list`.
/// Self-written come R_STAT (`[count:8][0:8][entry...]`, entry 16 B):
/// il dispatch non riscrive. `topo` e' la connessione topologia di cardo
/// (riconnessione automatica a morte driver, come i mount).
#[inline(never)]
pub fn handle_disk_list(
    topo: &ipc_disk::IpcDisk,
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
) -> Result<u64, u64> {
    let entries = topo.list().ok_or(ERR)?;
    let n = entries.len().min(16);
    let mut payload = [0u8; 256];
    for (k, (s, f)) in entries.iter().take(16).enumerate() {
        payload[k * 16..k * 16 + 8].copy_from_slice(&s.to_le_bytes());
        payload[k * 16 + 8..k * 16 + 16].copy_from_slice(&f.to_le_bytes());
    }
    if rings.get(&chan).is_some() {
        rings::map_client_resp_ring(rings, chan);
        rings::resp_ring_write(n as u64, 0, &payload[..n * 16]);
    }
    Ok(n as u64)
}

/// R_DISK_INFO (Fase 51): dettaglio disco `idx` (sda=0, ...) via
/// `IpcDisk::info`. Self-written (`[sectors:8][flags:8]` + frame fisso 76 B
/// come DISK_INFO). Indice oltre i dischi → ERR (mai frame parziale).
#[inline(never)]
pub fn handle_disk_info(
    topo: &ipc_disk::IpcDisk,
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    idx: u32,
) -> Result<u64, u64> {
    let info = topo.info((idx as u32) << 16).ok_or(ERR)?;
    let mut payload = [0u8; 76];
    payload[..8].copy_from_slice(&(info.model_len as u64).to_le_bytes());
    payload[8..8 + info.model_len].copy_from_slice(&info.model[..info.model_len]);
    payload[48..56].copy_from_slice(&(info.serial_len as u64).to_le_bytes());
    payload[56..56 + info.serial_len].copy_from_slice(&info.serial[..info.serial_len]);
    if rings.get(&chan).is_some() {
        rings::map_client_resp_ring(rings, chan);
        rings::resp_ring_write(info.sectors, info.flags, &payload);
    }
    Ok(info.sectors)
}

/// Legge tutto il file via trait `read` a chunk 4K e ne calcola BLAKE2s-256
/// (Fase 54, opzione A compute-on-query: nessuno stato, nessuno store).
/// `open`+`read`+`close` (close no-op) sul provider risolto; EOF = fine.
/// Buffer in heap (W3: 4K stack qui + 4K nel chiamante + dispatch sfondavano
/// i 16 KiB — firewall + niente array KiB per-valore, regola §18).
#[inline(never)]
fn hash_of_provider(
    d: &mut dyn crate::provider::LocalFsDyn,
    rel: &str,
) -> Result<[u8; 32], u64> {
    let h = d.open_dyn(rel, 0)?;
    let mut hasher = blake2s::Hasher::new();
    // `new_zeroed` azzera in heap (mai `[0u8; 4096]` letterale: 4K transienti
    // sullo stack prima del move — vedi `format::boxed_node`).
    let mut buf: alloc::boxed::Box<[u8; 4096]> =
        unsafe { alloc::boxed::Box::new_zeroed().assume_init() };
    let mut off = 0usize;
    loop {
        let n = d.read_dyn(h, off, &mut buf[..])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        off += n;
    }
    Ok(hasher.finalize())
}

/// R_GET_HASH (Fase 54, P5 integrita'): BLAKE2s-256 del contenuto del path,
/// self-written `[0:8][0:8]` + 32 B (come R_STAT, niente fd). Solo mount
/// `Local` (ramfs/FAT/Arca via trait `read`); device/remoti/sintetici e
/// mount inattivi → ERR (nessun contenuto da hashare). Precedenza come stat.
#[inline(never)]
pub fn handle_get_hash(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    use crate::provider::LocalFs;
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    let reply_hash = |rings: &BTreeMap<u64, (u64, u64)>, digest: &[u8; 32]| -> u64 {
        if rings.get(&chan).is_some() {
            rings::map_client_resp_ring(rings, chan);
            rings::resp_ring_write(0, 0, digest);
        }
        0
    };
    // Device registrati: nessun contenuto (foglie; sotto-device = None).
    if mount_legacy::resolve_mount(path, mounts).is_some() {
        return Err(ERR_INVALID);
    }
    // FAT/Local/Arca con attivazione lazy (stesso contratto di stat).
    // 56.3: ArcaFS col motore globale (uuid combaciante), altrimenti generico.
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
            let digest = hash_of_provider(&mut w, rel)?;
            return Ok(reply_hash(rings, &digest));
        }
        let d = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
        let digest = hash_of_provider(d, rel)?;
        return Ok(reply_hash(rings, &digest));
    }
    match mount_legacy::resolve_local(mounts_fat, path) {
        Some(mount_legacy::FsKind::Fat) | Some(mount_legacy::FsKind::Local) => Err(ERR),
        Some(mount_legacy::FsKind::Ram) => {
            // ramfs radice: hash via trait `LocalFs` diretta (open+read loop).
            // Buffer in heap come sopra (stessa catena: dispatch → qui).
            let h = LocalFs::open(fs, path, 0)?;
            let mut hasher = blake2s::Hasher::new();
            let mut buf: alloc::boxed::Box<[u8; 4096]> =
                unsafe { alloc::boxed::Box::new_zeroed().assume_init() };
            let mut off = 0usize;
            loop {
                let n = LocalFs::read(fs, h, off, &mut buf[..])?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                off += n;
            }
            let digest = hasher.finalize();
            Ok(reply_hash(rings, &digest))
        }
        // Sintetici (`/dev`): nessun contenuto.
        None => Err(ERR_INVALID),
    }
}

/// R_SYNC (Fase 52, P3 durabilita'): imposta l'aspettativa del canale e
/// ritorna il modo precedente (pattern umask, testabile). `GROUP` esegue
/// anche la barriera subito: FLUSH di tutti i mount FAT attivi (ramfs
/// volatile per disegno, mai promessa). Two-phase: modo invalidato o
/// barriera fallita → `Err`, stato invariato (mai stabilita' presunta).
/// Default per-canale `SYNC_NONE` (nessuna pretesa registrata).
#[inline(never)]
pub fn handle_sync(
    mounts_fat: &mut Vec<mount::FsMount>,
    sync_expect: &mut BTreeMap<u64, u32>,
    chan: u64,
    mode: u32,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if mode != civis::SYNC_NONE && mode != civis::SYNC_GROUP && mode != civis::SYNC_PERWRITE {
        return Err(ERR_INVALID);
    }
    let prev = sync_expect.get(&chan).copied().unwrap_or(civis::SYNC_NONE);
    if mode == civis::SYNC_GROUP {
        // Barriera sui mount FAT attivi (quelli inattivi non hanno connessioni
        // con pendenze: write-through, niente dirty da spingere altrove).
        for m in mounts_fat.iter() {
            let ok = match m.fat() {
                Some(f) => f.disk().flush_cache(),
                // Mount `Local` (ramfs tmpfs): niente da flusare, per disegno.
                None => true,
            };
            if !ok {
                return Err(ERR);
            }
        }
        // 56.3/W4 — barriera ArcaFS = flush epoca esplicita (commit se
        // dirty, no-op se pulita; loud a IO fallito). E' la SYNC esplicita
        // del contratto crash (ADR-0052): chi vuole durevolezza subito la
        // chiede qui invece di aspettare soglia/eta'/close. Senza motore
        // legato niente da committare (init ripiega come prima).
        if let Some(d) = disk {
            if btree_drv::commit_or(d, 0).is_err() {
                return Err(ERR);
            }
        }
    }
    sync_expect.insert(chan, mode);
    Ok(prev as u64)
}

/// R_STATVFS (Fase 52): spazio del mount del path (precedenza come open,
/// senza shadow). Self-written (`[0:8][0:8]` + 32 B): il dispatch non
/// riscrive. Device e padri sintetizzati → ERR_INVALID (nessun device da
/// contabilizzare); mount noto ma inattivo → ERR (stesso contratto di open).
#[inline(never)]
pub fn handle_statvfs(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    rings: &BTreeMap<u64, (u64, u64)>,
    chan: u64,
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    use crate::provider::LocalFs;
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    let reply_vfs = |rings: &BTreeMap<u64, (u64, u64)>, v: &crate::provider::StatVfs| -> u64 {
        if rings.get(&chan).is_some() {
            rings::map_client_resp_ring(rings, chan);
            let mut payload = [0u8; 32];
            payload[..8].copy_from_slice(&v.bsize.to_le_bytes());
            payload[8..16].copy_from_slice(&v.blocks.to_le_bytes());
            payload[16..24].copy_from_slice(&v.bfree.to_le_bytes());
            payload[24..32].copy_from_slice(&v.bavail.to_le_bytes());
            rings::resp_ring_write(0, 0, &payload);
        }
        0
    };
    // Fase A: niente shortcut — `/` si risolve via mount come gli altri
    // path (root ArcaFS: bsize 65536; la ramfs resta solo sotto /tmp).
    // Device: foglie senza blocchi (mai contabilizzati).
    if mount_legacy::resolve_mount(path, mounts).is_some() {
        return Err(ERR_INVALID);
    }
    // FAT/Local con attivazione lazy (find fresco a ogni chiamata, come stat).
    // 56.3: ArcaFS col motore globale (uuid combaciante), altrimenti generico.
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
            let v = w.statvfs_dyn(rel)?;
            return Ok(reply_vfs(rings, &v));
        }
        let fat = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
        let v = fat.statvfs_dyn(rel)?;
        return Ok(reply_vfs(rings, &v));
    }
    match mount_legacy::resolve_local(mounts_fat, path) {
        Some(mount_legacy::FsKind::Fat) | Some(mount_legacy::FsKind::Local) => Err(ERR),
        Some(mount_legacy::FsKind::Ram) => {
            let v = LocalFs::statvfs(fs, path)?;
            Ok(reply_vfs(rings, &v))
        }
        // Sintetizzati (`/dev`): esistono come nomi, non come spazio.
        None => Err(ERR_INVALID),
    }
}

#[inline(never)]
pub fn handle_mkdir(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    // Mount `Local` (Fase 49, F4): mkdir via trait sul mount. 56.3: ArcaFS
    // col motore globale (uuid combaciante: SOLO RAM, mai disco).
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        let m = mount::by_id_mut(mounts_fat, mid).ok_or(ERR_NOTFOUND)?;
        if m.is_provider() {
            if let Some(mut w) = m.arca_with(disk.as_mut()) {
                w.mkdir_dyn(rel)?;
                return Ok(0);
            }
            let d = m.local_dyn().ok_or(ERR)?;
            d.mkdir_dyn(rel)?;
            return Ok(0);
        }
        return Err(ERR); // FAT: niente mkdir.
    }
    // mkdir solo su ramfs radice (i mount FAT/remoti non hanno mkdir).
    match mount_legacy::resolve_local(mounts_fat, path).ok_or(ERR_NOTFOUND)? {
        mount_legacy::FsKind::Ram => {
            // 47.5 — mkdir via trait `LocalFs` (U1): la trait gestisce Exists vs NotFound.
            crate::provider::LocalFs::mkdir(fs, path)?;
            Ok(0)
        }
        _ => Err(ERR),
    }
}

/// Cancella un file o una directory VUOTA (Fase 18.2, `R_DELETE`).
/// Ramfs radice e mount `Local` (via trait); su FAT manca l'unlink (e'
/// scrivibile dalla Fase 20, ma non cancellabile) e i device remoti non sono
/// file cancellabili (e un mount point non si rimuove: si smonta).
/// Ritorna Some(0) o None.
#[inline(never)]
pub fn handle_delete(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    // Mai dentro driver remoti…
    if mount_legacy::resolve_mount(path, mounts).is_some() {
        return Err(ERR_INVALID);
    }
    // …mount `Local` via trait (Fase 49, F4). 56.3: ArcaFS col motore
    // globale (uuid combaciante), altrimenti generico…
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        let m = mount::by_id_mut(mounts_fat, mid).ok_or(ERR_NOTFOUND)?;
        if m.is_provider() {
            if let Some(mut w) = m.arca_with(disk.as_mut()) {
                w.remove_dyn(rel)?;
                return Ok(0);
            }
            let d = m.local_dyn().ok_or(ERR)?;
            d.remove_dyn(rel)?;
            return Ok(0);
        }
        return Err(ERR_READONLY); // FAT: unlink non implementato.
    }
    // …e mai su mount FAT inattivo: solo ramfs radice.
    match mount_legacy::resolve_local(mounts_fat, path).ok_or(ERR_NOTFOUND)? {
        mount_legacy::FsKind::Ram => {
            // 47.5 — delete via trait `LocalFs` (U1): la trait gestisce NotFound vs EmptyDir.
            crate::provider::LocalFs::remove(fs, path)?;
            Ok(0)
        }
        _ => Err(ERR_READONLY),
    }
}

/// Rinomina `old` in `new` sullo STESSO mount (S1.1, `R_RENAME`).
/// Cross-mount = `ERR_INVALID` loud (niente EXDEV tipizzato, mai
/// silent-copy); mount remoti/FAT = rifiuto (FAT: namespace immutabile).
/// Provider `Local`/Arca via trait (rel mount-relative), ramfs radice via
/// trait (path intero). Ritorna Ok(0).
#[inline(never)]
pub fn handle_rename(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    old: &str,
    new: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if old.is_empty() || new.is_empty() || old.len() > MAX_PATH || new.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    // Mai dentro driver remoti (nessun dei due capi).
    if mount_legacy::resolve_mount(old, mounts).is_some()
        || mount_legacy::resolve_mount(new, mounts).is_some()
    {
        return Err(ERR_INVALID);
    }
    // Mount `Local`/Arca via trait: entrambi i capi sullo STESSO mount
    // (stesso mid) o rifiuto cross-mount.
    let ro = mount::resolve_fsmount(mounts_fat, old, fgen);
    let rn = mount::resolve_fsmount(mounts_fat, new, fgen);
    match (ro, rn) {
        (Some((mid_o, rel_o)), Some((mid_n, rel_n))) => {
            if mid_o != mid_n {
                return Err(ERR_INVALID); // cross-mount
            }
            let m = mount::by_id_mut(mounts_fat, mid_o).ok_or(ERR_NOTFOUND)?;
            if m.is_provider() {
                if let Some(mut w) = m.arca_with(disk.as_mut()) {
                    w.rename_dyn(&rel_o, &rel_n)?;
                    return Ok(0);
                }
                let d = m.local_dyn().ok_or(ERR)?;
                d.rename_dyn(&rel_o, &rel_n)?;
                return Ok(0);
            }
            return Err(ERR_READONLY); // FAT: namespace immutabile
        }
        (Some(_), None) | (None, Some(_)) => return Err(ERR_INVALID), // a cavallo
        (None, None) => {}
    }
    // Ramfs radice (legacy): entrambi i capi Ram o rifiuto.
    let ko = mount_legacy::resolve_local(mounts_fat, old).ok_or(ERR_NOTFOUND)?;
    let kn = mount_legacy::resolve_local(mounts_fat, new).ok_or(ERR_NOTFOUND)?;
    match (ko, kn) {
        (mount_legacy::FsKind::Ram, mount_legacy::FsKind::Ram) => {
            crate::provider::LocalFs::rename(fs, old, new)?;
            Ok(0)
        }
        _ => Err(ERR_INVALID),
    }
}

/// Legge il target di un link SENZA seguire (S1.1): stesso routing degli
/// open locali (provider via trait, ramfs radice, remoti/FAT = errore).
/// `probe` = uso interno del follow: qualunque esito non-link (file, dir,
/// assente, FAT, driver) diventa `Ok(None)` — solo gli errori di routing
/// restano Err. `probe = false` (readlink esplicita): file/dir = INVALID
/// (mai target inventati), assente = NOTFOUND, FAT/remoti = errore.
#[inline(never)]
fn path_readlink(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
    probe: bool,
) -> Result<Option<String>, u64> {
    // Driver opachi e FAT (niente link): probe = None, esplicita = errore.
    if mount_legacy::resolve_mount(path, mounts).is_some() {
        return if probe { Ok(None) } else { Err(ERR_INVALID) };
    }
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        let m = mount::by_id_mut(mounts_fat, mid).ok_or(ERR_NOTFOUND)?;
        if m.is_provider() {
            let r = if let Some(mut w) = m.arca_with(disk.as_mut()) {
                w.readlink_dyn(rel)
            } else if let Some(mut d) = m.local_dyn() {
                d.readlink_dyn(rel)
            } else {
                return Err(ERR);
            };
            return match r {
                Ok(t) => Ok(Some(t)),
                Err(_) if probe => Ok(None),
                Err(e) => Err(e),
            };
        }
        return if probe { Ok(None) } else { Err(ERR_READONLY) };
    }
    match mount_legacy::resolve_local(mounts_fat, path).ok_or(ERR_NOTFOUND)? {
        mount_legacy::FsKind::Ram => match crate::provider::LocalFs::readlink(fs, path) {
            Ok(t) => Ok(Some(t)),
            Err(_) if probe => Ok(None),
            Err(e) => Err(e),
        },
        _ => Err(ERR_INVALID),
    }
}

/// Stat lstat di un path con lo stesso routing di `path_readlink` (S1.1):
/// serve a `handle_readlink` per distinguere file/dir esistenti (INVALID)
/// dai path mancanti (NOTFOUND). Remoti = INVALID (driver opachi).
#[inline(never)]
fn path_stat(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
    out: &mut crate::provider::Meta,
) -> Result<(), u64> {
    if mount_legacy::resolve_mount(path, mounts).is_some() {
        return Err(ERR_INVALID);
    }
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        let m = mount::by_id_mut(mounts_fat, mid).ok_or(ERR_NOTFOUND)?;
        if m.is_provider() {
            if let Some(mut w) = m.arca_with(disk.as_mut()) {
                *out = w.stat_dyn(rel)?;
            } else if let Some(mut d) = m.local_dyn() {
                *out = d.stat_dyn(rel)?;
            } else {
                return Err(ERR);
            }
            return Ok(());
        }
        // FAT: stat via trait (file/dir con metadati veri).
        *out = mount::by_id_mut(mounts_fat, mid)
            .ok_or(ERR)?
            .fat_mut()
            .ok_or(ERR)?
            .stat(rel)
            .map_err(|_| ERR_NOTFOUND)?;
        return Ok(());
    }
    match mount_legacy::resolve_local(mounts_fat, path).ok_or(ERR_NOTFOUND)? {
        mount_legacy::FsKind::Ram => {
            *out = crate::provider::LocalFs::stat(fs, path)?;
            Ok(())
        }
        _ => Err(ERR_INVALID),
    }
}

/// Risolve i symlink di `path` (S1.1, follow POSIX all'open/stat): fino a 8
/// hop, target relativi contro la dir del link. Ritorna il path risolto
/// (sempre assoluto, allocato) o l'errore: loop oltre 8 = INVALID (ELOOP
/// senza numero dedicato, mai hang), link a vuoto = NOTFOUND dal livello
/// sotto. Usato da open (segue) — stat NON segue (lstat).
#[inline(never)]
fn follow_links(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<String, u64> {
    let mut cur = String::from(path);
    for _ in 0..8 {
        match path_readlink(fs, mounts_fat, mounts, &cur, fgen, disk, true)? {
            Some(target) => {
                if target.starts_with('/') {
                    cur = target;
                } else {
                    // Relativo alla dir del link (mai oltre la root: i `..`
                    // restano testuali, la normalizzazione e' del chiamante
                    // come per gli altri path).
                    match cur.rsplit_once('/') {
                        Some((dir, _)) if !dir.is_empty() => {
                            cur = String::from(dir);
                            cur.push('/');
                            cur.push_str(&target);
                        }
                        _ => {
                            cur = String::from("/");
                            cur.push_str(&target);
                        }
                    }
                }
            }
            None => return Ok(cur),
        }
    }
    Err(ERR_INVALID) // oltre 8 hop: loop
}

/// Crea un symlink (S1.1, `R_SYMLINK`): solo mount Local/Arca/ramfs (stessa
/// struttura di handle_rename senza il secondo capo: il target non si
/// risolve mai qui). FAT/remoti = rifiuto. Ritorna Ok(0).
#[inline(never)]
pub fn handle_symlink(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    link: &str,
    target: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if link.is_empty() || target.is_empty() || link.len() > MAX_PATH || target.len() > 1024 {
        return Err(ERR_INVALID);
    }
    if mount_legacy::resolve_mount(link, mounts).is_some() {
        return Err(ERR_INVALID);
    }
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, link, fgen) {
        let m = mount::by_id_mut(mounts_fat, mid).ok_or(ERR_NOTFOUND)?;
        if m.is_provider() {
            if let Some(mut w) = m.arca_with(disk.as_mut()) {
                w.symlink_dyn(rel, target)?;
                return Ok(0);
            }
            let d = m.local_dyn().ok_or(ERR)?;
            d.symlink_dyn(rel, target)?;
            return Ok(0);
        }
        return Err(ERR_READONLY);
    }
    match mount_legacy::resolve_local(mounts_fat, link).ok_or(ERR_NOTFOUND)? {
        mount_legacy::FsKind::Ram => {
            crate::provider::LocalFs::symlink(fs, link, target)?;
            Ok(0)
        }
        _ => Err(ERR_INVALID),
    }
}

/// Legge un symlink (S1.1, `R_READLINK`): ritorna il target (il chiamante
/// scrive il frame). Stesso routing di readlink interno.
#[inline(never)]
pub fn handle_readlink(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    path: &str,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<String, u64> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    match path_readlink(fs, mounts_fat, mounts, path, fgen, disk, false)? {
        Some(t) => Ok(t),
        // Non-link: se esiste e' file/dir (INVALID, mai target inventati),
        // se manca NOTFOUND — distinguiamo con una stat leggera (lstat:
        // non segue, il link stesso e' gia' escluso dal ramo sopra).
        None => {
            let mut st = crate::provider::Meta { size: 0, kind: 0, readonly: false, mtime: 0 };
            match path_stat(fs, mounts_fat, mounts, path, fgen, disk, &mut st) {
                Ok(()) => Err(ERR_INVALID),
                Err(_) => Err(ERR_NOTFOUND),
            }
        }
    }
}

/// chmod S1.1 (`R_CHMOD`): projection, mai enforcement. Stesso routing dei
/// symlink (provider/ram; FAT accetta no-op; remoti INVALID). Ritorna Ok(0).
#[inline(never)]
pub fn handle_chmod(
    fs: &mut ramfs::RamFs,
    mounts_fat: &mut Vec<mount::FsMount>,
    mounts: &[mount_legacy::Mount],
    path: &str,
    mode: u32,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ERR_INVALID);
    }
    if mount_legacy::resolve_mount(path, mounts).is_some() {
        return Err(ERR_INVALID);
    }
    if let Some((mid, rel)) = mount::resolve_fsmount(mounts_fat, path, fgen) {
        let m = mount::by_id_mut(mounts_fat, mid).ok_or(ERR_NOTFOUND)?;
        if m.is_provider() {
            if let Some(mut w) = m.arca_with(disk.as_mut()) {
                w.chmod_dyn(rel, mode)?;
                return Ok(0);
            }
            let d = m.local_dyn().ok_or(ERR)?;
            d.chmod_dyn(rel, mode)?;
            return Ok(0);
        }
        // FAT: accetta no-op su esistente (i build non falliscono).
        let fat = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?;
        let fm = fat.fat_mut().ok_or(ERR)?;
        fm.chmod_dyn(rel, mode)?;
        return Ok(0);
    }
    match mount_legacy::resolve_local(mounts_fat, path).ok_or(ERR_NOTFOUND)? {
        mount_legacy::FsKind::Ram => {
            crate::provider::LocalFs::chmod(fs, path, mode)?;
            Ok(0)
        }
        _ => Err(ERR_INVALID),
    }
}

/// Monta una sorgente sul target (Fase 16b, payload "source\0target\0").
/// Ritorna Ok(0) se il mount e' ATTIVO, Err tipizzato altrimenti: a resolve
/// fallito (sorgente/target invalidi, nome ignoto, driver irraggiungibile)
/// nessun cambio di stato; a BPB illeggibile la spec resta registrata
/// INATTIVA e ritenta lazy (mai shadow ramfs).
#[inline(never)]
pub fn handle_mount(
    mounts: &mut Vec<mount::FsMount>,
    payload: &str,
    fgen: &mut u64,
    next_id: &mut u64,
) -> Result<u64, u64> {
    let mut parts = payload.split('\0');
    let source = parts.next().ok_or(ERR_INVALID)?;
    let target = parts.next().ok_or(ERR_INVALID)?;
    if source.is_empty() || target.is_empty() {
        return Err(ERR_INVALID);
    }
    if mount::apply_mount_spec(mounts, source, target, "", next_id) {
        // La tabella e' cambiata (spec nuova/sostituita): invalida le cache.
        *fgen = fgen.wrapping_add(1);
        Ok(0)
    } else {
        // apply fallisce a resolve sorgente (nome ignoto/driver morto) o a
        // target invalido: NotFound nel primo caso. Senza visibilita' interna,
        // NotFound e' il rifiuto piu' onesto (il client tipico ha sbagliato la
        // sorgente; il target malformato e' gia' filtrato sopra).
        Err(ERR_NOTFOUND)
    }
}

/// Smonta un target (Fase 16b). Rifiutato se ci sono fd aperti sotto il mount
/// (EBUSY); la radice ramfs non e' smontabile (non e' in tabella).
/// A rimozione riuscita bumpa `gen`. Gli fd tengono mount-id (Fase 49, F2):
/// orfani di un umount riuscito danno errore al prossimo uso invece di
/// aliasare il vicino shiftato.
#[inline(never)]
pub fn handle_umount(
    mounts: &mut Vec<mount::FsMount>,
    ftable: &ftable::FileTable,
    target: &str,
    fgen: &mut u64,
) -> Result<u64, u64> {
    let norm = mount::normalize_target(target).ok_or(ERR_INVALID)?;
    let mid = mounts.iter().find(|m| m.target == norm).map(|m| m.id).ok_or(ERR_NOTFOUND)?;
    if ftable.has_mount_users(mid) {
        return Err(ERR_BUSY);
    }
    let idx = mount::by_id(mounts, mid).ok_or(ERR_NOTFOUND)?;
    mounts.remove(idx);
    *fgen = fgen.wrapping_add(1);
    Ok(0)
}

/// Sposta l'offset di un fd LOCALE (Fase 40, R_LSEEK): `off` con segno,
/// `whence` = SEEK_START/CUR/END. Solo Local (Remote → ERR_INVALID: l'offset
/// vive in cardo, i driver non lo conoscono). Ritorna il nuovo offset.
/// Two-phase: valida tutto PRIMA di `set_offset` (a rifiuto l'offset resta
/// quello di prima, mai stato intermedio).
#[inline(never)]
pub fn handle_lseek(
    fs: &ramfs::RamFs,
    ftable: &mut ftable::FileTable,
    mounts_fat: &mut Vec<mount::FsMount>,
    chan: u64,
    fd: u32,
    off: i64,
    whence: u64,
    fgen: &mut u64,
    disk: &mut Option<btree_drv::DiskEngine>,
) -> Result<u64, u64> {
    // `get` ritorna Some solo per i Local (Remote e fd ignoti → Invalid:
    // niente EBADF nel nativo; l'offset vive in cardo, i driver non lo
    // conoscono).
    let (path, kind, cur, mnt) = ftable.get(chan, fd).ok_or(ERR_INVALID)?;
    let base: i64 = match whence {
        civis::SEEK_START => 0,
        civis::SEEK_CURRENT => cur as i64,
        civis::SEEK_END => {
            let size = match kind {
                mount_legacy::FsKind::Ram => match fs.find(path).ok_or(ERR_NOTFOUND)? {
                    ramfs::FsNode::File { data, .. } => data.len() as i64,
                    // Dir: size 0 (lseek lecito, le read restano IsDir).
                    _ => 0,
                },
                mount_legacy::FsKind::Local => {
                    // Size fresca via stat (Fase 49, F4): niente cache.
                    // 56.3: ArcaFS col motore globale (uuid combaciante).
                    let mid = mnt.ok_or(ERR)?;
                    let rel_owned: String = alloc::string::String::from(path);
                    if let Some(mut w) = arca_ns(mounts_fat, mid, disk) {
                        match w.stat_dyn(&rel_owned).map_err(|_| ERR_NOTFOUND)? {
                            meta if meta.kind == 1 => 0,
                            meta => meta.size as i64,
                        }
                    } else {
                        let d = mount::by_id_mut(mounts_fat, mid).ok_or(ERR)?.local_dyn().ok_or(ERR)?;
                        match d.stat_dyn(&rel_owned).map_err(|_| ERR_NOTFOUND)? {
                            meta if meta.kind == 1 => 0,
                            meta => meta.size as i64,
                        }
                    }
                }
                mount_legacy::FsKind::Fat => {
                    let mid = mnt.ok_or(ERR)?;
                    if !mount::reactivate_mount_by_id(mounts_fat, mid, fgen) {
                        return Err(ERR);
                    }
                    let g = *fgen;
                    let fat = mount::by_id(mounts_fat, mid)
                        .and_then(|i| mounts_fat.get(i))
                        .ok_or(ERR)?
                        .fat()
                        .ok_or(ERR)?;
                    match ftable::fd_fat_info(ftable, fat, chan, fd, g).ok_or(ERR_NOTFOUND)? {
                        info if info.is_dir => 0,
                        info => info.size as i64,
                    }
                }
            };
            size
        }
        _ => return Err(ERR_INVALID),
    };
    let new = base.checked_add(off).ok_or(ERR_INVALID)?;
    if new < 0 {
        return Err(ERR_INVALID);
    }
    // Oltre EOF lecito (le read tornano 0, le write crescono): solo >= 0.
    ftable.set_offset(chan, fd, new as usize);
    Ok(new as u64)
}

// ── Object store nativo ArcaFS (56.2b: backend unico su disco) ───────
// Gli handler `R_OBJ_*`/`R_SNAP_*` vivono in `btree_drv` (B+tree COW +
// commit per-op). Il backend in-RAM 56.1 e' rimosso (transitorio per
// dichiarazione; oracolo nei test host `arcafs`): niente doppio backend,
// niente flag di routing nel dispatch.

// ── Volume on-disk: admin debug formato/allocatore (Fase 56.2a) ────
// UN solo tag (`R_ARCA_DEBUG`, sub-op in `arcafs::proto`): scaffold per il
// gate su volume di scratch; gating di policy in A7. L'unico handler RAW
// vive in `btree_drv` (stesso `VolumeStore` pre/post bind: niente doppi
// handle, niente divergenze); il dispatch sta in `server.rs`.


