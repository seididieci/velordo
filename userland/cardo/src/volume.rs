//! Volume ArcaFS on-disk (Fase 56.2a: formato + allocatore + nodi opachi).
//!
//! Blocco = 3584 B = 7 settori (1 op `DISK_*` esatta), partition-relative:
//! blocco N = settori N*7..N*7+6. Blocco 0 = superblock (LBA0) + shadow
//! (LBA1) + header-estensione (settori 2-6). Il blocco 0 non si alloca mai.
//! Layout e checksum vivono in `arcafs::format` (condivisi col tool host):
//! qui solo I/O + freelist + guardia live.
//!
//! Memoria: niente cache nodi in 56.2a (ogni op = I/O diretto); `live' e'
//! una guardia RAM anti-double-free, non uno store (lo store in-RAM e'
//! eliminato: il motore parla solo blocchi).

use super::*;
use crate::fat32::BlockSource;
use alloc::collections::BTreeSet;
use arcafs::format::{
    self, HeaderExt, ARCA_BLOCK_SECTORS, ARCA_NODE_PAYLOAD_LEN, ARCA_XHDROFF, ARCA_XHDRLEN,
};

/// Byte di un blocco (ARCA_BLOCK_SIZE = 3584).
pub const BLOCK_BYTES: usize = 3584;

#[inline(never)]
fn block_lba(n: u64) -> Option<u64> {
    n.checked_mul(ARCA_BLOCK_SECTORS as u64)
}

/// Volume aperto: handle nodo + header-estensione + guardia allocazioni.
pub struct ArcaVolume {
    disk: IpcDisk,
    xh: HeaderExt,
    live: BTreeSet<u64>,
    /// UUID dal superblock (56.3: la vista POSIX serve solo il volume del
    /// motore globale — mount di altri volumi = errore loud, mai dati altrui).
    uuid: u64,
}

impl ArcaVolume {
    /// UUID del volume (identita' stabile, dal superblock letto in `open`).
    pub fn uuid(&self) -> u64 {
        self.uuid
    }

    /// Generation dal superblock (diagnostica per lo stub del mount root,
    /// Fase A): una lettura settore 0, una tantum al boot.
    #[inline(never)]
    pub fn generation(&self) -> Option<u64> {
        let mut sec = [0u8; 512];
        if !self.disk.read_sector(0, &mut sec) {
            return None;
        }
        format::superblock_roots(&sec).map(|(generation, _, _, _, _)| generation)
    }

    /// Legge un blocco intero. None su errore IO/overflow.
    #[inline(never)]
    fn read_block(&self, n: u64, out: &mut [u8; BLOCK_BYTES]) -> bool {
        match block_lba(n) {
            Some(lba) => self.disk.read_sectors(lba, ARCA_BLOCK_SECTORS, &mut out[..]),
            None => false,
        }
    }

    /// Scrive un blocco intero. False su errore IO/overflow.
    #[inline(never)]
    fn write_block(&self, n: u64, data: &[u8; BLOCK_BYTES]) -> bool {
        match block_lba(n) {
            Some(lba) => self.disk.write_sectors(lba, ARCA_BLOCK_SECTORS, &data[..]),
            None => false,
        }
    }

    /// Legge i 5 settori dell'header-estensione (blocco 0 da ARCA_XHDROFF).
    #[inline(never)]
    fn read_xh_raw(&self, out: &mut [u8; ARCA_XHDRLEN]) -> bool {
        let mut sec = [0u8; 512];
        let mut done = 0usize;
        let mut lba = (ARCA_XHDROFF / 512) as u64;
        while done < ARCA_XHDRLEN {
            if !self.disk.read_sector(lba, &mut sec) {
                return false;
            }
            let take = (ARCA_XHDRLEN - done).min(512);
            out[done..done + take].copy_from_slice(&sec[..take]);
            done += take;
            lba += 1;
        }
        true
    }

    /// Scrive l'header-estensione (write-through: chiamata a ogni alloc/free).
    /// La coda dell'ultimo settore resta a zero (riservata).
    #[inline(never)]
    fn write_xh_raw(&self, xh: &[u8; ARCA_XHDRLEN]) -> bool {
        let mut sec = [0u8; 512];
        let mut done = 0usize;
        let mut lba = (ARCA_XHDROFF / 512) as u64;
        while done < ARCA_XHDRLEN {
            let take = (ARCA_XHDRLEN - done).min(512);
            sec.fill(0);
            sec[..take].copy_from_slice(&xh[done..done + take]);
            if !self.disk.write_sector(lba, &sec) {
                return false;
            }
            done += take;
            lba += 1;
        }
        true
    }

    /// Scrive l'header-estensione corrente (dopo alloc/free).
    #[inline(never)]
    fn store_xh(&self) -> bool {
        self.write_xh_raw(&format::xhdr_encode(&self.xh))
    }

    /// Apre un volume formattato: superblock valido + header-estensione
    /// valida. La guardia `live' parte vuota (i blocchi allocati prima di
    /// questa apertura sono noti solo alla freelist).
    #[inline(never)]
    pub fn open(handle: u32) -> Option<Self> {
        let disk = IpcDisk::new(handle);
        let mut sec = [0u8; 512];
        println!("[cardo] ArcaVolume::open handle={} read_sector(0)...", handle);
        if !disk.read_sector(0, &mut sec) {
            println!("[cardo] ArcaVolume::open handle={} read_sector FAILED", handle);
            return None;
        }
        println!("[cardo] ArcaVolume::open handle={} superblock_verify...", handle);
        let (_, uuid) = format::superblock_verify(&sec)?;
        let v = Self {
            disk,
            xh: HeaderExt { free_head: 0, high_water: 0, next_id: 0, next_snap: 0, flags: 0 },
            live: BTreeSet::new(),
            uuid,
        };
        let mut raw = [0u8; ARCA_XHDRLEN];
        if !v.read_xh_raw(&mut raw) {
            println!("[cardo] ArcaVolume::open handle={} read_xh_raw FAILED", handle);
            return None;
        }
        println!("[cardo] ArcaVolume::open handle={} xhdr_decode...", handle);
        let xh = format::xhdr_decode(&raw)?;
        println!("[cardo] ArcaVolume::open handle={} SUCCESS uuid={:016X}", handle, uuid);
        Some(Self { xh, ..v })
    }

    /// Formatta: header-ext vergine + nodo root vuoto al blocco 1 + ROOT nel
    /// superblock (checksum ricalcolato). Il superblock deve gia' esistere
    /// (`arca create`); qui si riempiono solo i campi A2. high_water = 2.
    /// (56.2c: recovery; per ora solo bootstrap dei volumi di test.)
    #[inline(never)]
    pub fn format(handle: u32) -> Option<Self> {
        let disk = IpcDisk::new(handle);
        let mut sec = [0u8; 512];
        if !disk.read_sector(0, &mut sec) {
            return None;
        }
        // L'uuid e' nel superblock scritto da `arca create` (invariato
        // dal format: si riusa, mai rigenerato qui).
        let uuid = format::superblock_verify(&sec).map(|(_, u)| u).unwrap_or(0);
        let mut v = Self {
            disk,
            xh: HeaderExt { free_head: 0, high_water: 2, next_id: 1, next_snap: 1, flags: 0 },
            live: BTreeSet::new(),
            uuid,
        };
        format::superblock_set_root(&mut sec, 1)?;
        if !v.disk.write_sector(0, &sec) {
            return None;
        }
        if !v.store_xh() {
            return None;
        }
        // Buffer heap + payload statico (mai 7K stack — regola §18; la
        // funzione e' oggi unreachable in guest ma resta sicura).
        const ZERO_PAYLOAD: [u8; ARCA_NODE_PAYLOAD_LEN] = [0; ARCA_NODE_PAYLOAD_LEN];
        let mut root = format::boxed_block();
        format::node_fill(&mut root, format::ARCA_NODE_TYPE_RAW, 0, &ZERO_PAYLOAD);
        if !v.write_block(1, &root) {
            return None;
        }
        v.live.insert(1);
        Some(v)
    }

    /// Legge il primo settore di un blocco (i puntatori freelist vivono nei
    /// primi 8 B: mai 3.5K di stack per leggere un u64 — regola §18).
    #[inline(never)]
    fn read_first_sector(&self, n: u64, out: &mut [u8; 512]) -> bool {
        match block_lba(n) {
            Some(lba) => self.disk.read_sector(lba, out),
            None => false,
        }
    }

    /// Bound walk freelist: la catena non puo' superare i blocchi al di sotto
    /// di `high_water` (+16 di margine); oltre = corruzione, stop loud invece
    /// di hang su catene cicliche (lezione: guardie `1 << 20` con IO dentro
    /// appenderebbero il boot su disco danneggiato).
    #[inline(never)]
    fn walk_cap(&self) -> usize {
        (self.xh.high_water.min(u32::MAX as u64) as usize).saturating_add(16).max(32)
    }

    /// Blocchi nella freelist (walk con bound). Usata dalla GC per
    /// distinguere liberi da orfani.
    #[inline(never)]
    pub fn freelist_blocks(&self) -> Vec<u64> {
        let mut out = Vec::new();
        let mut cur = self.xh.free_head;
        let mut guard = self.walk_cap();
        let mut sec = [0u8; 512];
        while cur != 0 && guard > 0 {
            guard -= 1;
            if !self.read_first_sector(cur, &mut sec) {
                break;
            }
            out.push(cur);
            cur = u64::from_le_bytes(sec[..8].try_into().unwrap_or([0; 8]));
        }
        out
    }

    /// La guardia live conosce `n` (allocato da questo handle)? La GC non
    /// tocca mai i blocchi live (leftover RAW mai sganciati: leak sicuro,
    /// mai double-push in freelist che corromperebbe catena e allocator).
    #[inline(never)]
    pub fn is_live(&self, n: u64) -> bool {
        self.live.contains(&n)
    }

    /// Spinge una lista di orfani in freelist (GC): per ognuno scrive il
    /// next-pointer nel primo settore e avanza la testa; UN solo store_xh
    /// alla fine. La lista deve essere dedupata e senza 0 (il chiamante
    /// garantisce: vedi `gc_collect`). Niente guardia live qui — il chiamante
    /// ha gia' escluso i live (fresh handle dopo restart: guardia vuota).
    /// A IO fallito: stato disco intatto (freelist persistita intoccata),
    /// i blocchi toccati erano orfani irraggiungibili.
    #[inline(never)]
    pub fn gc_push_free_list(&mut self, orphans: &[u64]) -> bool {
        for &n in orphans {
            if n == 0 {
                return false;
            }
            let mut sec = [0u8; 512];
            sec[..8].copy_from_slice(&self.xh.free_head.to_le_bytes());
            if !self.write_first_sector(n, &sec) {
                return false;
            }
            self.xh.free_head = n;
        }
        self.store_xh()
    }

    /// Riscrive il primo settore di un blocco (solo unlink freelist: i
    /// settori 1-6 di un blocco libero sono spazzatura senza lettori — il
    /// payload conta solo dopo realloc, che riscrive sempre tutto).
    #[inline(never)]
    fn write_first_sector(&self, n: u64, data: &[u8; 512]) -> bool {
        match block_lba(n) {
            Some(lba) => self.disk.write_sector(lba, data),
            None => false,
        }
    }

    /// Alloca un blocco: pop dalla freelist o high_water++. Mai il blocco 0.
    /// Scrive header-ext (write-through) e marca live (guardia double-alloc).
    #[inline(never)]
    pub fn alloc(&mut self) -> Option<u64> {
        let n = if self.xh.free_head != 0 {
            let head = self.xh.free_head;
            let mut sec = [0u8; 512];
            if !self.read_first_sector(head, &mut sec) {
                return None;
            }
            self.xh.free_head = u64::from_le_bytes(sec[..8].try_into().ok()?);
            head
        } else {
            let n = self.xh.high_water;
            if n == 0 {
                return None; // high_water 0 = blocco 0, mai allocabile
            }
            self.xh.high_water += 1;
            n
        };
        if n == 0 || !self.live.insert(n) {
            return None; // blocco 0 o double-alloc (guardia RAM)
        }
        if !self.store_xh() {
            self.live.remove(&n);
            return None;
        }
        Some(n)
    }

    /// Alloca fino a `max` blocchi preferendo la continuazione di `hint`
    /// (affinita' ultimo-run W2: gli extent del file restano compatti).
    /// Tenta `hint+1, hint+2, ...` via `alloc_specific`, al primo buco passa
    /// ad `alloc` normale per il resto. Mai blocco 0; vuoto a disco pieno.
    /// I run risultanti possono essere corti (il motore raggruppa comunque
    /// i consecutivi: la dinamica vera emerge qui, niente tabella).
    pub fn alloc_run(&mut self, hint: u64, max: usize) -> Vec<u64> {
        let mut out = Vec::with_capacity(max.min(512));
        if hint != 0 {
            let mut n = hint.checked_add(1).unwrap_or(0);
            while out.len() < max && n != 0 {
                if self.alloc_specific(n) {
                    out.push(n);
                    n = n.checked_add(1).unwrap_or(0);
                } else {
                    break;
                }
            }
        }
        while out.len() < max {
            match self.alloc() {
                Some(b) if b != 0 => out.push(b),
                _ => break,
            }
        }
        out
    }

    /// Alloca uno SPECIFICO blocco (56.2b: secondary root fissa al blocco 2).
    /// Lo sgancia dalla freelist se presente, altrimenti lo prende solo se e'
    /// la cima (`high_water`, caso volume fresco). Blocco 0, live o oltre la
    /// cima → false. Scrive header-ext come `alloc`. Walk a settori (mai
    /// 3.5K stack — regola §18; vedi `write_first_sector` per la patch).
    #[inline(never)]
    pub fn alloc_specific(&mut self, n: u64) -> bool {
        if n == 0 || !self.live.insert(n) {
            return false; // blocco 0 o double-alloc (guardia RAM)
        }
        // Fast path: n e' la testa.
        if self.xh.free_head == n {
            let mut sec = [0u8; 512];
            if !self.read_first_sector(n, &mut sec) {
                self.live.remove(&n);
                return false;
            }
            self.xh.free_head = u64::from_le_bytes(sec[..8].try_into().unwrap_or([0; 8]));
            if !self.store_xh() {
                self.live.remove(&n);
                return false;
            }
            return true;
        }
        // Walk: cerca il prev il cui next e' n (letture da 1 settore,
        // bound anti-loop: vedi `walk_cap`).
        let mut prev = self.xh.free_head;
        let mut guard = self.walk_cap();
        let mut sec = [0u8; 512];
        while prev != 0 && guard > 0 {
            guard -= 1;
            if !self.read_first_sector(prev, &mut sec) {
                self.live.remove(&n);
                return false;
            }
            let next = u64::from_le_bytes(sec[..8].try_into().unwrap_or([0; 8]));
            if next == n {
                let mut ns = [0u8; 512];
                if !self.read_first_sector(n, &mut ns) {
                    self.live.remove(&n);
                    return false;
                }
                sec[..8].copy_from_slice(&ns[..8]);
                if !self.write_first_sector(prev, &sec) {
                    self.live.remove(&n);
                    return false;
                }
                if !self.store_xh() {
                    self.live.remove(&n);
                    return false;
                }
                return true;
            }
            prev = next;
        }
        // Non in freelist: solo cima fresca.
        if n == self.xh.high_water && self.xh.high_water != 0 {
            self.xh.high_water += 1;
            if !self.store_xh() {
                self.live.remove(&n);
                return false;
            }
            return true;
        }
        self.live.remove(&n);
        false
    }

    /// Libera un blocco: push in freelist (next nei primi 8 B) + unmark live.
    /// Blocco 0, mai-allocato o double-free → false (guardia RAM, mai IO).
    /// Buffer intero sull'heap (mai 3.5K stack — regola §18).
    #[inline(never)]
    pub fn free(&mut self, n: u64) -> bool {
        if n == 0 || !self.live.remove(&n) {
            return false;
        }
        let mut blk = format::boxed_block();
        blk[..8].copy_from_slice(&self.xh.free_head.to_le_bytes());
        if !self.write_block(n, &blk) {
            self.live.insert(n);
            return false;
        }
        self.xh.free_head = n;
        if !self.store_xh() {
            return false;
        }
        true
    }

    /// Scrive un nodo opaco (header magic+type+gen + payload + checksum).
    /// Blocco 0 protetto; il blocco deve essere live (allocato qui).
    /// Buffer intero sull'heap (regola §18: il chiamante btree tiene gia'
    /// il payload in `Box`, qui si aggiunge solo header + checksum).
    #[inline(never)]
    pub fn write_node(&mut self, n: u64, ty: u8, generation: u64, payload: &[u8; ARCA_NODE_PAYLOAD_LEN]) -> bool {
        if n == 0 || !self.live.contains(&n) {
            return false;
        }
        let mut blk = format::boxed_block();
        format::node_fill(&mut blk, ty, generation, payload);
        self.write_block(n, &blk)
    }

    /// Legge e verifica un nodo: magic + checksum. Il payload va in `out`.
    /// Blocco 0 protetto; type/gen riportati per il chiamante (56.2b).
    /// Lettura heap + copia payload (niente array 3.5K stack — regola §18);
    /// a verifica fallita `out` resta sporco e si ritorna None (contratto
    /// invariato: il chiamante non usa `out` a errore).
    #[inline(never)]
    pub fn read_node(&self, n: u64, out: &mut [u8; ARCA_NODE_PAYLOAD_LEN]) -> Option<(u8, u64)> {
        if n == 0 {
            return None;
        }
        let mut blk = format::boxed_block();
        if !self.read_block(n, &mut blk) {
            return None;
        }
        let (ty, generation) = format::node_verify(&blk)?;
        out.copy_from_slice(&blk[arcafs::format::ARCA_NODE_PAYLOAD..arcafs::format::ARCA_NODE_PAYLOAD + ARCA_NODE_PAYLOAD_LEN]);
        Some((ty, generation))
    }

    /// (high_water, live_count, free_head) per il debug STAT.
    #[inline(never)]
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.xh.high_water, self.live.len() as u64, self.xh.free_head)
    }

    /// Settore raw partition-relative (56.2b commit: superblock LBA0/shadow
    /// LBA1 + flip di generazione). Niente checksum qui: il chiamante usa
    /// `arcafs::format` (stesso formato del tool host, mai duplicato).
    #[inline(never)]
    pub fn read_raw_sector(&self, lba: u64, out: &mut [u8; 512]) -> bool {
        self.disk.read_sector(lba, out)
    }

    /// Scrive un settore raw partition-relative (vedi sopra).
    #[inline(never)]
    pub fn write_raw_sector(&self, lba: u64, data: &[u8; 512]) -> bool {
        self.disk.write_sector(lba, data)
    }

    /// Contatori id persistenti dall'header-ext (fonte dopo kill/restart).
    #[inline(never)]
    pub fn ids(&self) -> (u64, u64) {
        (self.xh.next_id, self.xh.next_snap)
    }

    /// Sincronizza i contatori id e persiste l'header-ext (fine commit).
    /// Rifiuta 0 (id mai validi, F2): niente stati degeneri su disco.
    #[inline(never)]
    pub fn sync_ids(&mut self, next_id: u64, next_snap: u64) -> bool {
        if next_id == 0 || next_snap == 0 {
            return false;
        }
        self.xh.next_id = next_id;
        self.xh.next_snap = next_snap;
        self.store_xh()
    }

    /// Scrive contatori + flags in UN colpo (commit 56.2b: un solo settore
    /// per l'header-ext a commit, non tre). Rifiuta id 0 (F2).
    #[inline(never)]
    pub fn store_meta(&mut self, next_id: u64, next_snap: u64, flags: u64) -> bool {
        if next_id == 0 || next_snap == 0 {
            return false;
        }
        self.xh.next_id = next_id;
        self.xh.next_snap = next_snap;
        self.xh.flags = flags;
        self.store_xh()
    }

    /// Flags header-ext (bit DIRTY per il commit).
    #[inline(never)]
    pub fn xh_flags(&self) -> u64 {
        self.xh.flags
    }

    /// Imposta i flags header-ext e li persiste.
    #[inline(never)]
    pub fn set_xh_flags(&mut self, flags: u64) -> bool {
        self.xh.flags = flags;
        self.store_xh()
    }
}
