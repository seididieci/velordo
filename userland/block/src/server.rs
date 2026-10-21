use super::*;

civis::entry!(real_main);

/// Esito dell'attesa event-driven di un DMA armato (38.2).
enum DmaWait {
    /// INTR/ERROR osservato e verificato: chiudere con `finish_dma`.
    Done,
    /// Richiedente morto durante l'attesa: STOP/clear fatti, MAI reply.
    Aborted,
}

/// Attende in `recv` il completamento di un DMA armato con `start_dma` (38.2,
/// event-driven): la CPU e' libera invece di bruciare il poll (~1,2 ms a
/// transfer). Sicuro grazie alla guardia 38.2a (le notify kernel a canale 0
/// non toccano la reply implicita) e alle risposte async FS (mai reply state
/// per costruzione, Fase 13). `requester` = pid del richiedente (da
/// `peer_pid` sul canale della richiesta): serve l'EXIT-abort.
/// Ritorna `Done` al primo wakeup con `is_done` vero (fast-path pre-check
/// incluso: IRQ gia' arrivata o INTR gia' settato), `Aborted` se il
/// richiedente muore (EXIT_NOTIFY con w1 == requester): STOP/clear fatti,
/// niente reply (peer morto, convenzione driver).
/// EXIT altrui = reset fsreg + continua, SENZA perdere la reply (38.2e: la
/// guardia `pop_msg` non tocca la reply implicita per gli EXIT — rispondere a
/// un morto e' impossibile per disegno, nessun server lo fa).
/// Altra richiesta sincrona durante il pending = impossibile (l'unico
/// richiedente DISK/DEV e' cardo, bloccato sulla reply): si ignora.
fn wait_dma(
    eng: &mut dma::DmaEngine,
    chan: u16,
    requester: u64,
    fsreg: &mut fs_reg::FsReg,
    reg_prefixes: &[String],
) -> DmaWait {
    if eng.is_done(chan) {
        eng.note_ev_fast();
        return DmaWait::Done;
    }
    loop {
        let msg = match civis::recv() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if msg.tag == civis::IRQ_NOTIFY_DISK {
            if eng.is_done(chan) {
                eng.note_ev_wait();
                return DmaWait::Done;
            }
            eng.note_irq_drained(); // stale/re-fire: coda pulita, mai reply
            continue;
        }
        if fsreg.collect_if_mine(msg.req_id, reg_prefixes) {
            continue; // async FS: reply state intatto
        }
        if msg.tag == civis::EXIT_NOTIFY {
            if msg.w1 == requester {
                eng.abort(chan);
                eng.note_ev_abort();
                return DmaWait::Aborted;
            }
            fsreg.reset(); // morte altrui: re-handshake, reply intatta (38.2e)
            continue;
        }
    }
}

fn real_main(_sp: u64) -> ! {
    println!("[block] starting, pid={}", civis::getpid());

    // 1. Rilevamento (solo HW, niente FS coinvolto).
    let mut infos = Vec::new();
    let atapi = detect::detect(&mut infos);
    let mut disks = Vec::new();
    for (i, info) in infos.iter().enumerate() {
        let letter = (b'a' + i as u8) as char;
        let model = core::str::from_utf8(&info.model[..info.model_len]).unwrap_or("?");
        println!(
            "[block] sd{}: {} settori, {} {} ({})",
            letter,
            info.sectors,
            if info.lba48 { "LBA48" } else { "LBA28" },
            if info.cmd == 0x1F0 { "primary" } else { "secondary" },
            if info.drive == 0 { "master" } else { "slave" }
        );
        println!("[block] sd{}: modello '{}'", letter, model);
        let serial = core::str::from_utf8(&info.serial[..info.serial_len]).unwrap_or("?");
        println!("[block] sd{}: seriale '{}'", letter, serial);
        // Topologia P2 (Fase 51): capability + geometria per S1/S2 di ArcaFS.
        // `alloc::format!` qui (non nel println diretto): il match con
        // `rpm` lega un valore da interpolare.
        let rot = match info.rotation {
            0 => alloc::format!("rotazione ignota"),
            1 => alloc::format!("SSD"),
            rpm => alloc::format!("{} RPM", rpm),
        };
        println!(
            "[block] sd{}: TRIM={}, settore {}/{}B, {}",
            letter,
            if info.trim { "si" } else { "no" },
            info.sec_logical,
            info.sec_physical,
            rot,
        );
        disks.push(block::AtaDisk::open(info.cmd, info.drive, info.lba48));
    }
    if atapi > 0 {
        println!("[block] {} device ATAPI skippati (PACKET futuro)", atapi);
    }
    if disks.is_empty() {
        println!("[block] nessun disco ATA: solo registrazione servizio");
    }

    // 1b. PCI Bus-Master (Fase 38.0d): trova il PIIX3-IDE, programma la BAR4 a
    // `BM_BASE` e abilita I/O Space + Bus Master. Qualunque esito avverso
    // (assente, BAR fuori finestra, readback diversa) = resto in PIO: il
    // data-plane sotto e' invariato (il DMA vero arriva in 38.1).
    let bmiba: Option<u16> = match civis::pci::find_piix3_ide() {
        Some(dev) => match civis::pci::enable_bus_master(dev) {
            Some(b) => {
                println!(
                    "[block] BMIBA={:#x} (irqline={}), DMA negoziato — data-plane ancora PIO fino a 38.1",
                    b,
                    civis::pci::irq_line(dev)
                );
                Some(b)
            }
            None => {
                println!("[block] BAR4 fuori finestra/non verificata: resto in PIO");
                None
            }
        },
        None => {
            println!("[block] PIIX3-IDE non trovato su PCI: resto in PIO");
            None
        }
    };

    // 1c. Modi DMA (Fase 38.1b): `SET FEATURES` per disco, SOLO log (nessun
    // trasferimento ancora — 38.1c). Qualunque rifiuto = PIO: il data-plane
    // sotto e' invariato (il modo non tocca i comandi PIO). I modi restano in
    // `dma_modes` per il motore DMA (38.1c).
    let mut dma_modes: Vec<Option<u8>> = Vec::new();
    for (i, disk) in disks.iter().enumerate() {
        let letter = (b'a' + i as u8) as char;
        let mode = disk.set_dma_mode(infos[i].udma_modes);
        // Modo negoziato in `DiskInfo` (Fase 51): single source per DISK_INFO.
        infos[i].udma_neg = mode;
        match mode {
            Some(m) => println!(
                "[block] sd{}: UDMA mode {} negoziato — data-plane ancora PIO fino a 38.1c",
                letter, m
            ),
            None => println!(
                "[block] sd{}: niente UDMA (word88={:#x}, word63={:#x}): resto in PIO",
                letter, infos[i].udma_modes, infos[i].mdma_modes
            ),
        }
        dma_modes.push(mode);
    }

    // 1d. Motore DMA (38.1c): staging contigua + STOP/clear BM (sicuro anche
    // dopo kill+restart: lo stato BM sopravvive al processo). `None` = PIO
    // puro (data-plane intatto: il routing sotto tenta DMA solo se `Some`).
    let mut dma_eng = dma::DmaEngine::init(bmiba, &dma_modes);

    // 2. Nodi: whole-disk + partizioni (MBR/GPT, Fase 16 + Fase 55).
    // Handle = disco<<16|sub, allocato QUI (Fase 16c): la tabella `nodes' e'
    // la single source of truth nome→handle; cardo lo chiede con DISK_RESOLVE.
    let mut nodes: Vec<nodes::Node> = Vec::new();
    let mut disk_sectors: Vec<u64> = Vec::new();
    let mut parts: Vec<Vec<nodes::PartLoc>> = Vec::new();
    for (i, disk) in disks.iter().enumerate() {
        let letter = (b'a' + i as u8) as char;
        disk_sectors.push(infos[i].sectors);
        // Identità del whole-disk dallo stesso settore 0 (Fase 16d).
        let (wd_uuid, wd_label) = nodes::sniff_identity(disk, 0);
        let wd_arca = nodes::sniff_arca_uuid(disk, 0);
        nodes.push(nodes::Node {
            name: alloc::format!("sd{}", letter),
            handle: (i as u32) << 16,
            vol_uuid: wd_uuid,
            vol_arca_uuid: wd_arca,
            vol_label: wd_label,
        });
        let mut disk_parts: Vec<nodes::PartLoc> = Vec::new();
        let mut sec0 = [0u8; 512];
        if disk.read_sector(0, &mut sec0) {
            match part::parse_partitions(disk, &sec0) {
                part::PartitionResult::Mbr(parsed) => {
                    for (p, part) in parsed.iter().enumerate() {
                        println!(
                            "[block] sd{}{}: tipo MBR {:#04x}, start {}, settori {}",
                            letter,
                            p + 1,
                            part.ptype,
                            part.start,
                            part.sectors
                        );
                        // Identità della partizione dal suo boot sector (Fase 16d:
                        // un settore in più per partizione, solo a boot).
                        let (pu, pl) = nodes::sniff_identity(disk, part.start as u64);
                        let pa = nodes::sniff_arca_uuid(disk, part.start as u64);
                        nodes.push(nodes::Node {
                            name: alloc::format!("sd{}{}", letter, p + 1),
                            handle: ((i as u32) << 16) | (p as u32 + 1),
                            vol_uuid: pu,
                            vol_arca_uuid: pa,
                            vol_label: pl,
                        });
                        disk_parts.push(nodes::PartLoc { start: part.start as u64, sectors: part.sectors as u64 });
                    }
                }
                part::PartitionResult::Gpt(parsed) => {
                    for (p, part) in parsed.iter().enumerate() {
                        println!(
                            "[block] sd{}{}: GPT start {}, settori {}",
                            letter,
                            p + 1,
                            part.start,
                            part.sectors
                        );
                        // Identità della partizione dal suo boot sector (Fase 16d).
                        let (pu, pl) = nodes::sniff_identity(disk, part.start);
                        let pa = nodes::sniff_arca_uuid(disk, part.start);
                        nodes.push(nodes::Node {
                            name: alloc::format!("sd{}{}", letter, p + 1),
                            handle: ((i as u32) << 16) | (p as u32 + 1),
                            vol_uuid: pu,
                            vol_arca_uuid: pa,
                            vol_label: pl,
                        });
                        disk_parts.push(nodes::PartLoc { start: part.start, sectors: part.sectors });
                    }
                }
            }
        }
        parts.push(disk_parts);
    }

    // Prefix da registrare presso cardo (Fase 16d): il nodo + gli alias
    // stabili che ha (`/dev/disk/by-uuid/<HEX8>`, `/dev/disk/by-label/<NOME>`).
    // La FsReg li consuma in ordine; cardo li tratta come prefix qualunque
    // (open esatto + listing sintetizzato dalla Mount table, B4).
    let mut reg_prefixes: Vec<String> = Vec::new();
    for n in nodes.iter() {
        reg_prefixes.push(alloc::format!("/dev/{}", n.name));
        if let Some(u) = n.vol_uuid {
            reg_prefixes.push(alloc::format!("/dev/disk/by-uuid/{:08X}", u));
        }
        // ArcaFS (ADR-0058): uuid a 64 bit = 16 hex (formato distinto dal
        // seriale FAT a 8 hex: nessuna ambiguità fra i due).
        if let Some(u) = n.vol_arca_uuid {
            reg_prefixes.push(alloc::format!("/dev/disk/by-uuid/{:016X}", u));
        }
        if let Some(l) = &n.vol_label {
            reg_prefixes.push(alloc::format!("/dev/disk/by-label/{}", l));
        }
        // Riga identità per-nodo (Fase 16d): umana + asserzione host-side
        // del reorder (test-uuid-reorder.py cerca `uuid=<U2>` sulla lettera).
        let mut idline = alloc::format!("[block] {}: handle={:#x}", n.name, n.handle);
        if let Some(u) = n.vol_uuid {
            idline.push_str(&alloc::format!(" uuid={:08X}", u));
        }
        if let Some(u) = n.vol_arca_uuid {
            idline.push_str(&alloc::format!(" uuid={:016X}", u));
        }
        if let Some(l) = &n.vol_label {
            idline.push_str(&alloc::format!(" label='{}'", l));
        }
        println!("{}", idline);
    }

    // 3. Ring FS + DISK dedicati (allocazione raw, MAI via civis::fs_init che e'
    // sincrono): FS per BUF_REG/REGISTER async, DISK per il data-plane con
    // cardo. Retry throttled: senza, niente registrazione ne' data-plane.
    // Reset head=tail: le pagine devono partire allineate.
    let (fs_req_phys, fs_resp_phys) = loop {
        if let Some(pair) = civis::ring_alloc_raw() {
            break pair;
        }
        for _ in 0..1_000_000 {
            core::hint::spin_loop();
        }
    };
    let (disk_req_phys, disk_resp_phys) = loop {
        if let Some(pair) = civis::ring_alloc_raw() {
            break pair;
        }
        for _ in 0..1_000_000 {
            core::hint::spin_loop();
        }
    };
    if civis::map_physical(fs_req_phys, FS_REQ_VA, 1).is_err()
        || civis::map_physical(fs_resp_phys, FS_RESP_VA, 1).is_err()
        || civis::map_physical(disk_req_phys, DISK_REQ_VA, 1).is_err()
        || civis::map_physical(disk_resp_phys, DISK_RESP_VA, 1).is_err()
    {
        println!("[block] map ring fallita, exit");
        civis::exit(1);
    }
    rings::fs_rings_reset();
    unsafe {
        core::ptr::write_volatile((DISK_REQ_VA + RING_HEAD as u64) as *mut u32, 0);
        core::ptr::write_volatile((DISK_REQ_VA + RING_TAIL as u64) as *mut u32, 0);
        core::ptr::write_volatile((DISK_RESP_VA + RING_HEAD as u64) as *mut u32, 0);
        core::ptr::write_volatile((DISK_RESP_VA + RING_TAIL as u64) as *mut u32, 0);
    }

    // 4. Servizio Disk per nome (ADR-0008): cardo lo risolve per il
    // data-plane, init per la supervisione, il kernel non instrada IRQ.
    // (La BMIBA negoziata sopra e' in `bmiba`, il motore DMA in `dma` sotto.)
    if civis::service_register(civis::Service::Block).is_ok() {
        println!("[block] registered as service Disk");
    }

    // 5. READY al parent SUBITO (come console): block parte PRIMA di cardo
    // (16.3) e l'ACK non puo' aspettare il mount (deadlock: il mount aspetta
    // Fs che parte dopo). Fire-and-forget in `civis` (A3), retry bounded, mai hang.
    civis::signal_ready(1);

    // 6. Registrazione FS via SM async (mai sync: vedi doc in testa). DISK e
    // DEV funzionano anche a registrazione incompleta: cardo monta appena
    // HELLO risponde, senza aspettare i prefix.
    let mut fsreg = fs_reg::FsReg::new(fs_req_phys, fs_resp_phys);

    // fd DEV_* (raw sequenziale) → (handle nodo, posizione in byte).
    let mut fds: BTreeMap<u32, (u32, u64)> = BTreeMap::new();
    let mut next_fd: u32 = 1;

    loop {
        // Invio nella stessa chiamata (lezione tty): prima di dormire in recv
        // bisogna aver notificato, altrimenti nessuno ci sveglia.
        fsreg.step(&reg_prefixes);
        let msg = match civis::recv() {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Reply async FS (BUF_REG/REGISTER): consuma per primo, prima di ogni
        // dispatch (req_id > 0 solo per le risposte, mai per le richieste).
        if fsreg.collect_if_mine(msg.req_id, &reg_prefixes) {
            continue;
        }

        // cardo morto e rinato: reset SM (re-handshake + re-register). I ring
        // DISK persistono (pagine proprie): cardo rifa' HELLO da solo. Niente
        // send sincrone qui: solo reset di stato. Mai reply (peer morto).
        if msg.tag == civis::EXIT_NOTIFY {
            fsreg.reset();
            continue;
        }

        // 38.2 — drain notify IRQ stale (bridge interrupt→IPC, canale 0 senza
        // peer, come IRQ_NOTIFY_KBD per kbd): i re-fire level-triggered (EOI
        // dell'handler prima del clear nel nostro `finish`) lasciano stale in
        // coda tra un'op e l'altra — si scartano QUI, prima di qualunque reply
        // (sicuro: `reply_chan` e' None e il prossimo `recv` lo riscrive; e dal
        // 38.2a le notify non lo toccano comunque). Senza drain si accumulano
        // e la coda piena fa scartare le send sync di cardo in silenzio (hang
        // permanente, provato in 38.1c). MAI reply — non c'e' nessuno ad
        // aspettarla.
        if msg.tag == civis::IRQ_NOTIFY_DISK {
            if let Some(eng) = dma_eng.as_mut() {
                eng.note_irq_drained();
            }
            continue;
        }

        // ── Data-plane DISK_* (canale diretto cardo) ──
        if msg.tag == DISK_HELLO {
            // Fisici nei registri di reply (tag 0, mai !0 = ERR): niente frame.
            let _ = civis::reply(0, disk_req_phys, disk_resp_phys);
            continue;
        }
        if msg.tag == DISK_OPEN {
            if nodes::locate(msg.w0 as u32, &disk_sectors, &parts).is_some() {
                let _ = civis::reply(0, 0, 0);
            } else {
                let _ = civis::reply(0, ERR, 0);
            }
            continue;
        }
        if msg.tag == DISK_READ {
            // 24.2 — richiesta multi: frame `[count:8]`, risposta con
            // count*512 byte in UN frame (1 IPC invece di count).
            let handle = msg.w0 as u32;
            let lba = msg.w1;
            let mut buf = [0u8; nodes::DISK_MAX_SECTORS * 512];
            let count = match rings::disk_req_read_count() {
                Some(n) => n,
                None => {
                    let _ = civis::reply(0, ERR, 0);
                    continue;
                }
            };
            // 38.2 — DMA event-driven: start, attesa in `recv` (CPU libera
            // invece del poll), finish. Fallback PIO a qualunque `false`
            // (stesso contratto). Senza `peer_pid` attribuibile niente attesa
            // (EXIT-abort impossibile): PIO diretto, mai recv nel mezzo.
            // Abort (richiedente morto) = mai reply, mai PIO: nessuno legge.
            let mut dma_done = false;
            if let Some(eng) = dma_eng.as_mut() {
                if let Some((di, base, sectors)) = nodes::locate(handle, &disk_sectors, &parts) {
                    let end_ok =
                        lba.checked_add(count as u64).map_or(false, |e| e <= sectors);
                    if end_ok && dma_modes.get(di).copied().flatten().is_some() {
                        let disk = &disks[di];
                        let chan = disk.bm_chan_off();
                        if let Ok(req) = civis::peer_pid(msg.channel) {
                            let nbytes = count * 512;
                            if eng.start_dma(
                                disk,
                                chan,
                                base + lba,
                                count,
                                &mut buf[..nbytes],
                                false,
                            ) {
                                match wait_dma(
                                    &mut *eng,
                                    chan,
                                    req as u64,
                                    &mut fsreg,
                                    &reg_prefixes,
                                ) {
                                    DmaWait::Done => {
                                        if eng.finish_dma(
                                            disk,
                                            chan,
                                            &mut buf[..nbytes],
                                            count,
                                            false,
                                        ) {
                                            // Coerenza cache (56.2c): il DMA
                                            // salta `node_read_multi`, quindi
                                            // la cache va popolata qui o una
                                            // DEV_READ successiva servirebbe
                                            // stale (osservato: gen ferma).
                                            for j in 0..count {
                                                crate::cache::insert_from(
                                                    di,
                                                    base + lba + j as u64,
                                                    &buf[j * 512..(j + 1) * 512],
                                                );
                                            }
                                            unsafe {
                                                rings::disk_resp_write(
                                                    nbytes as u64,
                                                    0,
                                                    &buf[..nbytes],
                                                )
                                            };
                                            let _ = civis::reply(0, 0, 0);
                                            dma_done = true;
                                        }
                                    }
                                    DmaWait::Aborted => continue,
                                }
                            }
                        }
                    }
                }
            }
            if dma_done {
                continue;
            }
            // PIO (invariato).
            if nodes::node_read_multi(
                &disks,
                &disk_sectors,
                &parts,
                handle,
                lba,
                &mut buf[..count * 512],
            ) {
                unsafe { rings::disk_resp_write((count * 512) as u64, 0, &buf[..count * 512]) };
                let _ = civis::reply(0, 0, 0);
            } else {
                let _ = civis::reply(0, ERR, 0);
            }
            continue;
        }
        if msg.tag == DISK_CLOSE {
            let _ = civis::reply(0, 0, 0);
            continue;
        }
        if msg.tag == DISK_WRITE {
            // 24.2 — frame `[count:8][count*512 byte]`, 1 comando PIO + 1
            // flush per l'intero run (prima: comando+flush a settore).
            // Handle in w0, lba in w1. Frame consumato sempre, anche a
            // handle/lba invalidi (stesso contratto dei ring FS).
            let handle = msg.w0 as u32;
            let lba = msg.w1;
            let mut buf = [0u8; nodes::DISK_MAX_SECTORS * 512];
            let count = match rings::disk_req_read_multi(&mut buf) {
                Some(n) => n,
                None => {
                    let _ = civis::reply(0, ERR, 0);
                    continue;
                }
            };
            // 38.2 — DMA event-driven (come il READ sopra): start, attesa in
            // `recv`, finish. Frame gia' consumato in ogni caso; fallback PIO
            // a qualunque `false`; abort = mai reply, mai PIO.
            let mut dma_done = false;
            if let Some(eng) = dma_eng.as_mut() {
                if let Some((di, base, sectors)) = nodes::locate(handle, &disk_sectors, &parts) {
                    let end_ok =
                        lba.checked_add(count as u64).map_or(false, |e| e <= sectors);
                    if end_ok && dma_modes.get(di).copied().flatten().is_some() {
                        let disk = &disks[di];
                        let chan = disk.bm_chan_off();
                        if let Ok(req) = civis::peer_pid(msg.channel) {
                            let nbytes = count * 512;
                            if eng.start_dma(
                                disk,
                                chan,
                                base + lba,
                                count,
                                &mut buf[..nbytes],
                                true,
                            ) {
                                match wait_dma(
                                    &mut *eng,
                                    chan,
                                    req as u64,
                                    &mut fsreg,
                                    &reg_prefixes,
                                ) {
                                    DmaWait::Done => {
                                        if eng.finish_dma(
                                            disk, chan, &mut buf[..nbytes], count, true,
                                        ) {
                                            // Coerenza cache (56.2c): come il
                                            // read DMA, il write DMA salta
                                            // `node_write_multi`: senza fill
                                            // una DEV_READ cached vedrebbe il
                                            // vecchio contenuto.
                                            for j in 0..count {
                                                crate::cache::insert_from(
                                                    di,
                                                    base + lba + j as u64,
                                                    &buf[j * 512..(j + 1) * 512],
                                                );
                                            }
                                            let _ = civis::reply(0, 0, 0);
                                            dma_done = true;
                                        }
                                    }
                                    DmaWait::Aborted => continue,
                                }
                            }
                        }
                    }
                }
            }
            if dma_done {
                continue;
            }
            // PIO (invariato).
            let ok = nodes::node_write_multi(
                &disks,
                &disk_sectors,
                &parts,
                handle,
                lba,
                &buf[..count * 512],
            );
            if ok {
                let _ = civis::reply(0, 0, 0);
            } else {
                let _ = civis::reply(0, ERR, 0);
            }
            continue;
        }
        if msg.tag == DISK_RESOLVE {
            // Single source of truth nome→handle (Fase 16c, identità 16d):
            // la chiave ("sda", UUID hex, label) arriva nel frame DISK_REQ,
            // l'handle torna in w0. Sconosciuto/malformato → ERR, mai frame.
            let result = match rings::disk_req_read_name() {
                Some(key) => nodes::resolve_node(&nodes, &key).map(|n| n.handle as u64),
                None => None,
            };
            let _ = civis::reply(0, result.unwrap_or(ERR), 0);
            continue;
        }
        if msg.tag == DISK_LIST {
            // Topologia dischi (Fase 51, P2): reply w0 = count + 1 frame
            // RESP con entry fisse 16 B `[sectors:8][flags:8]` (sda=0, ...).
            // Bound 16 dischi (stack, mai heap nel per-op): oltre si tronca
            // (QEMU ne ha 2; il count in w0 resta quello vero).
            let n = infos.len();
            let mut payload = [0u8; 16 * 16];
            let mut k = 0usize;
            for info in infos.iter().take(16) {
                payload[k * 16..k * 16 + 8].copy_from_slice(&info.sectors.to_le_bytes());
                payload[k * 16 + 8..k * 16 + 16].copy_from_slice(&info.topo_flags().to_le_bytes());
                k += 1;
            }
            // Header `result` = byte payload (convenzione READ, letta dal
            // client per dimensionare la lettura); il count vero in w0.
            unsafe { rings::disk_resp_write((k * 16) as u64, 0, &payload[..k * 16]) };
            let _ = civis::reply(0, n as u64, 0);
            continue;
        }
        if msg.tag == DISK_INFO {
            // Dettaglio disco (Fase 51): w0 = handle (vale la parte disco,
            // sub ignorata), niente frame REQ; reply w0 = settori, w1 = flags
            // + 1 frame `[model_len:8][model][serial_len:8][serial]`.
            let di = (msg.w0 as u32 >> 16) as usize;
            let info = match infos.get(di) {
                Some(i) => i,
                None => {
                    let _ = civis::reply(0, ERR, 0);
                    continue;
                }
            };
            // Frame a dimensione FISSA (76 B: il client non conosce le
            // lunghezze prima di leggere): `[model_len:8][model:40]`
            // + `[serial_len:8][serial:20]`, resto azzerato.
            let mut payload = [0u8; 76];
            payload[..8].copy_from_slice(&(info.model_len as u64).to_le_bytes());
            payload[8..8 + info.model_len].copy_from_slice(&info.model[..info.model_len]);
            payload[48..56].copy_from_slice(&(info.serial_len as u64).to_le_bytes());
            payload[56..56 + info.serial_len].copy_from_slice(&info.serial[..info.serial_len]);
            // Header `result` = byte payload (come sopra); settori/flags in w0/w1.
            unsafe { rings::disk_resp_write(76, info.topo_flags(), &payload) };
            let _ = civis::reply(0, info.sectors, info.topo_flags());
            continue;
        }
        if msg.tag == DISK_FLUSH {
            // Barriera write-cache del drive (Fase 52, P3): FLUSH CACHE sul
            // disco di `w0` (vale la parte disco, sub ignorata). Sincrono
            // puro, niente frame. Indice ignoto → ERR (mai flush altrui).
            let di = (msg.w0 as u32 >> 16) as usize;
            let ok = match disks.get(di) {
                Some(d) => d.flush_write_cache(),
                None => false,
            };
            let _ = civis::reply(0, if ok { 0 } else { ERR }, 0);
            continue;
        }

        // ── Relay DEV_* (open raw /dev/sdX dai client via cardo) ──
        // w0 di DEV_OPEN = handle codificato (disco<<16|sub, 0 = whole):
        // cardo-16.2 lo ricava parsando il nome Linux ("sda"→0, "sda1"→1),
        // senza bisogno della lista nodi. La posizione avanza a ogni READ.
        let result: Option<u64> = match msg.tag {
            DEV_OPEN => {
                let handle = msg.w0 as u32;
                if nodes::locate(handle, &disk_sectors, &parts).is_some() {
                    let fd = next_fd;
                    next_fd += 1;
                    fds.insert(fd, (handle, 0));
                    Some(fd as u64)
                } else {
                    None
                }
            }
            DEV_READ => {
                let (handle, pos) = match fds.get(&(msg.w0 as u32)) {
                    Some(&p) => p,
                    None => {
                        let _ = civis::reply(0, ERR, 0);
                        continue;
                    }
                };
                let node_sectors = match nodes::locate(handle, &disk_sectors, &parts) {
                    Some((_, _, s)) => s,
                    None => {
                        let _ = civis::reply(0, ERR, 0);
                        continue;
                    }
                };
                let avail = node_sectors * 512 - pos.min(node_sectors * 512);
                let want = (msg.w1 as u64).min(avail).min(4096);
                let nsec = (want / 512) as usize;
                if nsec == 0 {
                    // EOF o count < 512: frame vuoto + 0 (come /dev/null), mai
                    // wedge il client (lezione fix kbd/tty).
                    unsafe { resp_frame_write(CLI_RESP, &[]) };
                    Some(0)
                } else {
                    let mut buf = [0u8; 4096];
                    let mut ok = 0usize;
                    for s in 0..nsec {
                        let lba = pos / 512 + s as u64;
                        let mut sec = [0u8; 512];
                        if !nodes::node_read(&disks, &disk_sectors, &parts, handle, lba, &mut sec) {
                            break;
                        }
                        buf[s * 512..(s + 1) * 512].copy_from_slice(&sec);
                        ok += 1;
                    }
                    if ok == 0 {
                        None
                    } else {
                        let n = ok * 512;
                        unsafe { resp_frame_write(CLI_RESP, &buf[..n]) };
                        fds.insert(msg.w0 as u32, (handle, pos + n as u64));
                        Some(n as u64)
                    }
                }
            }
            DEV_WRITE => {
                // Read-only: consuma comunque il payload (tail!) e rifiuta.
                let count = msg.w1 as usize;
                unsafe { req_frame_consume(CLI_REQ, count) };
                None
            }
            DEV_CLOSE => {
                if fds.remove(&(msg.w0 as u32)).is_some() {
                    Some(0)
                } else {
                    None
                }
            }
            DEV_READDIR => {
                // Nomi delle partizioni figlie del nodo (o vuoto). Il chiamante
                // e' cardo su relay del prefix stesso: rel non disponibile qui,
                // quindi si elencano i figli di TUTTI i dischi? No: senza rel,
                // risposta vuota conservativa (t32 usa open/read diretti).
                unsafe { resp_frame_write(CLI_RESP, &[]) };
                Some(0)
            }
            _ => None,
        };
        // Idempotente: reply ERR senza frame (convenzione driver), come
        // vela/kbd — il client vede -1, mai wedge.
        let _ = civis::reply(0, result.unwrap_or(ERR), 0);
    }
}
