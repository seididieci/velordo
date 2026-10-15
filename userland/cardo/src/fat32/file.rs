use super::*;

impl<B: BlockSource> Fat32<B> {
    /// Legge fino a `count` byte del file a partire da `offset`, copiandoli in
    /// `out`. Ritorna i byte letti (puo' essere < count a fine file).
    /// 24.1 — legge SOLO i settori coperti da [offset, offset+to_read): il
    /// walk dei cluster saltati costa solo FAT (memo), mai dati. Prima si
    /// leggeva ogni cluster intero (spc settori) anche per 25 byte.
    pub fn read_file(&mut self, info: &FileInfo, offset: usize, count: usize, out: &mut [u8]) -> usize {
        let csize = self.cluster_bytes();
        let size = info.size as usize;
        if offset >= size {
            return 0;
        }
        let to_read = count.min(size - offset);
        let spc = self.spc as usize;
        let first_ci = offset / csize;
        let last_ci = (offset + to_read - 1) / csize;
        // S2.5d: cluster di partenza dal memo (O(1)); senza, ogni slice
        // ricamminerebbe da 0 (O(n^2) sul file). Build pigra con cap
        // anti-loop (mai oltre size+1: ciclo o size bugiarda = corrotta).
        let mut cluster = match self.chain_memo.get(&info.first_cluster) {
            Some(chain) => match chain.get(first_ci) {
                Some(&c) => c,
                None => return 0, // catena piu' corta di size: corrotta
            },
            None => {
                let chain = match self.build_chain(info.first_cluster, size, csize) {
                    Some(ch) => ch,
                    None => return 0,
                };
                let c = match chain.get(first_ci) {
                    Some(&c) => c,
                    None => return 0,
                };
                self.chain_memo.insert(info.first_cluster, chain);
                c
            }
        };
        let mut done = 0usize;
        // 24.2 — un solo run per cluster a chunk da ≤8 settori in buffer
        // stack (niente Vec temporanei: vedi read_dir). `spc` resta qualunque
        // (potenza di 2 da BPB), il chunking interno regge spc > 8.
        let mut run = [0u8; 8 * 512];
        for ci in first_ci..=last_ci {
            let ci_start = ci * csize;
            let ci_end = ((ci + 1) * csize).min(size);
            let s0 = if ci == first_ci { (offset - ci_start) / 512 } else { 0 };
            let s1 = if ci == last_ci {
                (offset + to_read - ci_start + 511) / 512
            } else {
                spc
            };
            let lba0 = self.data_start as u64 + (cluster - 2) as u64 * spc as u64;
            let mut s = s0;
            while s < s1 {
                let k = (s1 - s).min(8);
                if !self.disk.read_sectors(lba0 + s as u64, k, &mut run[..k * 512]) {
                    return done;
                }
                for si in s..s + k {
                    let g0 = ci_start + si * 512;
                    let g1 = (g0 + 512).min(ci_end);
                    let r0 = g0.max(offset);
                    let r1 = g1.min(offset + to_read);
                    if r1 > r0 {
                        let base = (si - s) * 512;
                        out[done..done + r1 - r0]
                            .copy_from_slice(&run[base + (r0 - g0)..base + (r1 - g0)]);
                        done += r1 - r0;
                    }
                }
                s += k;
            }
            if ci != last_ci {
                match self.next_cluster(cluster) {
                    Some(next) => cluster = next,
                    None => return done, // catena corta: corrotta, stop
                }
            }
        }
        done
    }

    /// Costruisce la cluster-chain completa da `first` (S2.5d, per il memo
    /// di `read_file`). Cap a size+1 (mai oltre: ciclo o size bugiarda =
    /// `None`, come la catena corta). `first < 2` = cluster riservati.
    fn build_chain(&self, first: u32, size: usize, csize: usize) -> Option<alloc::vec::Vec<u32>> {
        if first < 2 || size == 0 {
            return None;
        }
        let max = size.div_ceil(csize) + 1;
        let mut chain = alloc::vec::Vec::new();
        let mut c = first;
        loop {
            chain.push(c);
            if chain.len() > max {
                return None;
            }
            match self.next_cluster(c) {
                Some(n) => c = n,
                None => break,
            }
        }
        if chain.is_empty() {
            return None;
        }
        Some(chain)
    }
    /// Scrive fino a `data.len()` byte del file a partire da `offset`
    /// (Fase 20.2: SOLO overwrite entro `size`, mai crescita — oltre EOF si
    /// ferma e ritorna i byte scritti). Read-modify-write a settori: ogni
    /// settore toccato viene letto, rattoppato e riscritto (write-through,
    /// niente cache). Ritorna i byte scritti (0 = niente da fare/errore).
    pub fn write_file(&self, info: &FileInfo, offset: usize, data: &[u8]) -> usize {
        let csize = self.cluster_bytes();
        let size = info.size as usize;
        if offset >= size || data.is_empty() || info.first_cluster < 2 {
            return 0;
        }
        let to_write = data.len().min(size - offset);
        // Walk fino al cluster che contiene `offset`.
        let mut cluster = info.first_cluster;
        let mut file_pos = 0usize;
        while file_pos + csize <= offset {
            match self.next_cluster(cluster) {
                Some(next) => cluster = next,
                None => return 0, // catena piu' corta di size: corrotta
            }
            file_pos += csize;
        }
        let mut done = 0usize;
        // 24.2 — read-modify-write per run di cluster a chunk da ≤8 settori
        // in buffer stack (niente Vec: vedi read_dir). Un solo PIO + flush
        // per chunk invece di N coppie singolo-settore + N flush.
        let mut run = [0u8; 8 * 512];
        while done < to_write {
            // Span di settori toccati in questo cluster.
            let rel0 = offset + done - file_pos;
            let rel1 = (offset + to_write - file_pos).min(csize);
            let s0 = rel0 / 512;
            let s1 = (rel1 + 511) / 512;
            let lba0 = self.data_start as u64
                + ((cluster - 2) as u64) * (self.spc as u64);
            // Rattoppa dal payload a chunk (il rattoppo segue i chunk letti).
            let mut p = done;
            let mut s = s0;
            while s < s1 {
                let k = (s1 - s).min(8);
                if !self.disk.read_sectors(lba0 + s as u64, k, &mut run[..k * 512]) {
                    return done;
                }
                let g0 = s * 512; // inizio chunk, relativo al cluster
                while p < done + (rel1 - rel0).min(to_write - done)
                    && offset + p - file_pos < g0 + k * 512
                {
                    let g = offset + p - file_pos - g0; // offset nel chunk
                    let chunk = (k * 512 - g).min(to_write - p);
                    run[g..g + chunk].copy_from_slice(&data[p..p + chunk]);
                    p += chunk;
                }
                if !self.disk.write_sectors(lba0 + s as u64, k, &run[..k * 512]) {
                    return done;
                }
                s += k;
            }
            done = p;
            // Sforato nel cluster successivo: avanza la catena.
            if offset + done - file_pos >= csize {
                file_pos += csize;
                if offset + done < size {
                    match self.next_cluster(cluster) {
                        Some(next) => cluster = next,
                        None => return done, // catena corta: corrotta, stop
                    }
                }
            }
        }
        done
    }

    /// Azzera un intero cluster (sicurezza: niente stale leggibile dopo grow).
    /// 24.2 — scritture multi a chunk da ≤8 in buffer stack (1 flush a
    /// chunk), niente Vec.
    fn zero_cluster(&self, c: u32) -> bool {
        let zero = [0u8; 8 * 512];
        let spc = self.spc as usize;
        let lba0 = self.data_start as u64 + ((c - 2) as u64) * (self.spc as u64);
        let mut s = 0usize;
        while s < spc {
            let k = (spc - s).min(8);
            if !self.disk.write_sectors(lba0 + s as u64, k, &zero[..k * 512]) {
                return false;
            }
            s += k;
        }
        true
    }
    /// Azzera il range [a, b) del file (catena da `first`, size logica `end`):
    /// read-modify-write per run di cluster a chunk stack (24.2, come
    /// `write_file`). Usato per la coda [old_size, new_end).
    fn zero_range(&self, first: u32, a: usize, b: usize) -> bool {
        if b <= a || first < 2 {
            return true;
        }
        let csize = self.cluster_bytes();
        let mut cluster = first;
        let mut file_pos = 0usize;
        while file_pos + csize <= a {
            match self.next_cluster(cluster) {
                Some(next) => cluster = next,
                None => return false,
            }
            file_pos += csize;
        }
        let mut run = [0u8; 8 * 512];
        let mut pos = a;
        while pos < b {
            let rel0 = pos - file_pos;
            let rel1 = (b - file_pos).min(csize);
            let s0 = rel0 / 512;
            let s1 = (rel1 + 511) / 512;
            let lba0 = self.data_start as u64
                + ((cluster - 2) as u64) * (self.spc as u64);
            let mut s = s0;
            while s < s1 {
                let k = (s1 - s).min(8);
                if !self.disk.read_sectors(lba0 + s as u64, k, &mut run[..k * 512]) {
                    return false;
                }
                let z0 = rel0.max(s * 512) - s * 512;
                let z1 = rel1.min((s + k) * 512) - s * 512;
                run[z0..z1].fill(0);
                if !self.disk.write_sectors(lba0 + s as u64, k, &run[..k * 512]) {
                    return false;
                }
                s += k;
            }
            pos += rel1 - rel0;
            if pos - file_pos >= csize && pos < b {
                file_pos += csize;
                match self.next_cluster(cluster) {
                    Some(next) => cluster = next,
                    None => return false,
                }
            }
        }
        true
    }
    /// (LBA assoluto, offset) dell'entry da 32 B in directory: walk della
    /// catena dir per `entry_off`. L'entry puo' cavalcare due settori (off >
    /// 480): il chiamante gestisce entrambi.
    fn dir_entry_pos(&self, dir_cluster: u32, entry_off: usize) -> Option<(u64, usize)> {
        if dir_cluster < 2 {
            return None;
        }
        let csize = self.cluster_bytes();
        let mut cluster = dir_cluster;
        let mut skip = entry_off / csize;
        while skip > 0 {
            match self.next_cluster(cluster) {
                Some(next) => cluster = next,
                None => return None,
            }
            skip -= 1;
        }
        let in_cl = entry_off % csize;
        let lba = self.data_start as u64
            + ((cluster - 2) as u64) * (self.spc as u64)
            + (in_cl / 512) as u64;
        Some((lba, in_cl % 512))
    }
    /// Aggiorna size (+ first_cluster se cambiato) nella dir-entry. Gestisce
    /// lo straddle su due settori. Size scritta PER ULTIMA (crash-safe).
    fn patch_entry(&self, dir_cluster: u32, entry_off: usize, first: u32, size: u32) -> bool {
        let (lba, off) = match self.dir_entry_pos(dir_cluster, entry_off) {
            Some(p) => p,
            None => return false,
        };
        let mut sec = [0u8; 512];
        if !self.disk.read_sector(lba, &mut sec) {
            return false;
        }
        // first_cluster: byte 26-27 (lo) + 20-21 (hi); size: byte 28-31.
        let patches: [(usize, u8); 8] = [
            (26, (first & 0xFF) as u8),
            (27, ((first >> 8) & 0xFF) as u8),
            (20, ((first >> 16) & 0xFF) as u8),
            (21, ((first >> 24) & 0xFF) as u8),
            (28, (size & 0xFF) as u8),
            (29, ((size >> 8) & 0xFF) as u8),
            (30, ((size >> 16) & 0xFF) as u8),
            (31, ((size >> 24) & 0xFF) as u8),
        ];
        for (i, v) in patches {
            let pos = off + i;
            if pos < 512 {
                sec[pos] = v;
            } else {
                // Straddle: secondo settore (pos - 512).
                let mut sec2 = [0u8; 512];
                if !self.disk.read_sector(lba + 1, &mut sec2) {
                    return false;
                }
                sec2[pos - 512] = v;
                if !self.disk.write_sector(lba + 1, &sec2) {
                    return false;
                }
            }
        }
        self.disk.write_sector(lba, &sec)
    }
    /// Aggiorna WrtTime/WrtDate (byte 22-25) all'ora corrente (Fase 50, P1
    /// orologio). Best-effort come `patch_entry` (false a errore IO): i dati
    /// sono gia' a posto, il timbro non trasforma mai una write riuscita in
    /// fallita. Straddle gestito come `patch_entry`. A servizio Time assente
    /// `wall_secs()` e' 0 → clamp a 1980-01-01 (convenzione DOS per ignoto).
    fn stamp_mtime(&self, dir_cluster: u32, entry_off: usize) -> bool {
        let (wdate, wtime) = civis::time::epoch_to_dos(crate::wall::wall_secs());
        let (lba, off) = match self.dir_entry_pos(dir_cluster, entry_off) {
            Some(p) => p,
            None => return false,
        };
        let mut sec = [0u8; 512];
        if !self.disk.read_sector(lba, &mut sec) {
            return false;
        }
        // WrtTime a 22-23 + WrtDate a 24-25 (LE16).
        let bytes: [(usize, u8); 4] = [
            (22, (wtime & 0xFF) as u8),
            (23, ((wtime >> 8) & 0xFF) as u8),
            (24, (wdate & 0xFF) as u8),
            (25, ((wdate >> 8) & 0xFF) as u8),
        ];
        for (i, v) in bytes {
            let pos = off + i;
            if pos < 512 {
                sec[pos] = v;
            } else {
                // Straddle: secondo settore (pos - 512).
                let mut sec2 = [0u8; 512];
                if !self.disk.read_sector(lba + 1, &mut sec2) {
                    return false;
                }
                sec2[pos - 512] = v;
                if !self.disk.write_sector(lba + 1, &sec2) {
                    return false;
                }
            }
        }
        self.disk.write_sector(lba, &sec)
    }
    /// Tronca il file a size 0 (Fase 40, OPEN_TRUNC): libera la catena cluster
    /// (ogni entry torna a 0 in tutte le copie FAT, come `free` — niente leak
    /// a ogni `>` redirect) e azzera first_cluster+size nella dir-entry PER
    /// ULTIMA (i dati restano sui settori ma irraggiungibili, come un unlink).
    /// FSInfo bumpata dei cluster liberati. Ritorna false su catena corrotta
    /// o errore IO (il chiamante tratta come rifiuto: mai truncate parziale
    /// dichiarato riuscito — a walk interrotto la size resta invariata).
    pub fn truncate(&self, info: &FileInfo) -> bool {
        if info.is_dir {
            return false;
        }
        let mut freed = 0i64;
        let mut c = info.first_cluster;
        // Bound come chain_tail (capacita' FAT): mai loop infiniti su catene
        // circolari corrotte (a interruzione: false, size invariata).
        let max = self.fat_size as usize * 128;
        let mut steps = 0usize;
        while c >= 2 {
            steps += 1;
            if steps > max {
                return false;
            }
            let next = self.next_cluster(c);
            if !self.set_fat_entry(c, 0) {
                return false;
            }
            freed += 1;
            match next {
                Some(n) => c = n,
                None => break,
            }
        }
        if !self.patch_entry(info.dir_cluster, info.entry_off, 0, 0) {
            return false;
        }
        // Troncamento = modifica: timbra (best-effort, mai fallire per questo).
        let _ = self.stamp_mtime(info.dir_cluster, info.entry_off);
        let _ = self.fsinfo_bump(freed, 2);
        true
    }
    /// Scrive con crescita (Fase 20.3): se `offset+len` supera `size`, alloca
    /// i cluster mancanti (linkati subito), azzera la coda [size, new_end) e
    /// aggiorna la dir-entry. Ritorna i byte scritti; la size cresce solo di
    /// quanto e' atterrato davvero (mai oltre). Fallimento allocazione →
    /// degrado a overwrite entro size (come 20.2).
    pub fn write_grow(&mut self, info: &FileInfo, offset: usize, data: &[u8]) -> usize {
        // S2.5d: qualunque scrittura invalida il memo (overwrite non cambia
        // la catena ma append/alloc si': drop incondizionato, rebuild pigro).
        self.chain_memo.remove(&info.first_cluster);
        let size = info.size as usize;
        if data.is_empty() || info.is_dir {
            return 0;
        }
        if offset + data.len() <= size {
            let n = self.write_file(info, offset, data);
            if n > 0 {
                let _ = self.stamp_mtime(info.dir_cluster, info.entry_off);
            }
            return n;
        }
        let csize = self.cluster_bytes();
        let (mut have, mut last) = match self.chain_tail(info.first_cluster) {
            Some(t) => t,
            None => return self.write_file(info, offset, data), // corrotta: degrado
        };
        let mut first = info.first_cluster;
        let mut new_end = offset + data.len();
        let need = new_end.div_ceil(csize);
        let mut allocated = 0u32;
        while have < need {
            match self.alloc_one() {
                Some(c) => {
                    if have == 0 {
                        first = c;
                    } else if !self.set_fat_entry(last, c) {
                        // Link fallito: cluster orfano (fsck-fixabile), degrado.
                        break;
                    }
                    last = c;
                    have += 1;
                    allocated += 1;
                }
                None => break, // disco pieno: degrado a quanto c'e'
            }
        }
        // Azzera la coda [size, new_end) sulla catena estesa (sicurezza).
        let cap_end = have * csize;
        if cap_end < new_end {
            // Allocazione corta: si scrive solo fin dove c'e' spazio.
            new_end = cap_end;
        }
        if new_end <= offset {
            return 0;
        }
        if !self.zero_range(first, size.min(new_end), new_end) {
            // Zero fallito: degrado a overwrite entro la vecchia size.
            let grown = self.write_file(info, offset, data);
            if grown > 0 {
                let _ = self.stamp_mtime(info.dir_cluster, info.entry_off);
            }
            let _ = self.fsinfo_bump(-(allocated as i64), last.saturating_add(1));
            return grown;
        }
        // Scrivi i dati (bound = new_end via info ombra).
        let shadow = FileInfo {
            first_cluster: first,
            size: new_end as u32,
            is_dir: false,
            mtime: info.mtime,
            dir_cluster: info.dir_cluster,
            entry_off: info.entry_off,
        };
        let done = self.write_file(&shadow, offset, &data[..(new_end - offset).min(data.len())]);
        let final_size = (offset + done).max(size.min(new_end));
        // Dir-entry PER ULTIMA + FSInfo (best-effort: i dati sono gia' a posto).
        let _ = self.patch_entry(info.dir_cluster, info.entry_off, first, final_size as u32);
        if done > 0 {
            let _ = self.stamp_mtime(info.dir_cluster, info.entry_off);
        }
        let _ = self.fsinfo_bump(-(allocated as i64), last.saturating_add(1));
        done
    }

    /// Crea un file vuoto (Fase 20.4): entry 8.3 maiuscola (no LFN) nella
    /// directory padre, primo cluster 0 + size 0. Ritorna false se il nome
    /// non e' 8.3 valido, il padre manca/non e' dir, esiste gia', o la
    /// directory e' piena e non si allarga (errore IO). `mkdir` su FAT resta
    /// fuori scope (attr sempre 0x20 = file).
    pub fn create_file(&self, path: &str) -> bool {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return false;
        }
        let (parent, leaf) = match path.rsplit_once('/') {
            Some((p, l)) => (p, l),
            None => ("", path),
        };
        // Nome 8.3 maiuscolo (stessa normalizzazione del match di find).
        let norm = Self::normalize_component(leaf);
        let (name, ext) = match norm.split_once('.') {
            Some((n, e)) => (n, e),
            None => (norm.as_str(), ""),
        };
        if name.is_empty() || name.len() > 8 || ext.len() > 3 {
            return false;
        }
        // Caratteri FAT consentiti (maiusc + cifre + simboli std, niente LFN).
        for &b in name.as_bytes().iter().chain(ext.as_bytes().iter()) {
            let ok = b.is_ascii_uppercase()
                || b.is_ascii_digit()
                || b"$%'-_@~`!(){}^#&".contains(&b);
            if !ok {
                return false;
            }
        }
        // Cluster iniziale della padre (root se vuota).
        let mut dir_cluster = self.root_cluster;
        if !parent.is_empty() {
            match self.find(parent) {
                Some(info) if info.is_dir && info.first_cluster >= 2 => {
                    dir_cluster = info.first_cluster;
                }
                _ => return false,
            }
        }
        // Esiste gia'? (find sulla padre + match nome: evita duplicati.)
        if self.find(path).is_some() {
            return false;
        }
        // Slot libero: primo byte 0x00 (fine, riusabile) o 0xE5 (cancellata)
        // a confine 32 B, scorrendo TUTTA la catena. Se piena, un cluster
        // nuovo azzerato in coda (slot = suo offset 0).
        let csize = self.cluster_bytes();
        let mut cluster = dir_cluster;
        let mut base_off = 0usize; // byte stream della dir a inizio cluster
        let slot_off: usize = loop {
            let lba0 = self.data_start as u64 + ((cluster - 2) as u64) * (self.spc as u64);
            let mut sec = [0u8; 512];
            let mut found: Option<usize> = None;
            'scan: for s in 0..self.spc as u64 {
                if !self.disk.read_sector(lba0 + s, &mut sec) {
                    return false;
                }
                let mut i = 0usize;
                while i + 32 <= 512 {
                    let first = sec[i];
                    if first == 0x00 || first == 0xE5 {
                        found = Some(base_off + (s as usize) * 512 + i);
                        break 'scan;
                    }
                    i += 32;
                }
            }
            if let Some(off) = found {
                break off;
            }
            match self.next_cluster(cluster) {
                Some(next) => {
                    cluster = next;
                    base_off += csize;
                }
                None => {
                    // Catena piena: nuovo cluster azzerato in coda.
                    let c = match self.alloc_one() {
                        Some(c) => c,
                        None => return false, // disco pieno
                    };
                    if !self.zero_cluster(c) || !self.set_fat_entry(cluster, c) {
                        return false;
                    }
                    let _ = self.fsinfo_bump(-1, c + 1);
                    break base_off + csize; // offset 0 del nuovo cluster
                }
            }
        };
        // Entry 32 B: nome 11 maiusc + spazi, attr archivio, cluster 0, size 0.
        let mut raw = [b' '; 11];
        raw[..name.len()].copy_from_slice(&name.as_bytes()[..name.len().min(8)]);
        raw[8..8 + ext.len()].copy_from_slice(&ext.as_bytes()[..ext.len().min(3)]);
        let (lba, off) = match self.dir_entry_pos(dir_cluster, slot_off) {
            Some(p) => p,
            None => return false,
        };
        let mut sec = [0u8; 512];
        if !self.disk.read_sector(lba, &mut sec) {
            return false;
        }
        // Slot a cavallo di due settori: quasi impossibile (slot trovati a
        // confine 32 B dentro un settore da 512 = 16 slot esatti), ma gestito.
        let mut spill: Option<[u8; 512]> = None;
        if off + 32 > 512 {
            let mut s2 = [0u8; 512];
            if !self.disk.read_sector(lba + 1, &mut s2) {
                return false;
            }
            spill = Some(s2);
        }
        let mut put = |i: usize, v: u8| {
            if off + i < 512 {
                sec[off + i] = v;
            } else if let Some(ref mut s2) = spill {
                s2[off + i - 512] = v;
            }
        };
        for i in 0..11 {
            put(i, raw[i]);
        }
        put(11, 0x20); // archivio
        for i in 12..22 {
            put(i, 0); // Crt*/LstAcc: sconosciuti (come prima)
        }
        // WrtTime/WrtDate all'ora corrente (Fase 50): mai entry senza tempo.
        let (wdate, wtime) = civis::time::epoch_to_dos(crate::wall::wall_secs());
        put(22, (wtime & 0xFF) as u8);
        put(23, ((wtime >> 8) & 0xFF) as u8);
        put(24, (wdate & 0xFF) as u8);
        put(25, ((wdate >> 8) & 0xFF) as u8);
        for i in 26..32 {
            put(i, 0); // cluster 0 + size 0
        }
        if !self.disk.write_sector(lba, &sec) {
            return false;
        }
        if let Some(s2) = spill {
            if !self.disk.write_sector(lba + 1, &s2) {
                return false;
            }
        }
        true
    }
}
