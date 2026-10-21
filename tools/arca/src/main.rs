//! `arca` host tool (Fase 54, P5): per ora solo `create` — scrive un
//! superblock ArcaFS con offset automatico: whole-disk → LBA0, partizione →
//! `start_lba` dalla tabella MBR/GPT del disco.
//! Offset/magic/checksum sono la single source `syscall-numbers`: questo
//! tool non li duplica (il round-trip create→mount→stat e' il test che li
//! tiene d'accordo). Le op guest (`list`/`get`/...) vivono in
//! `userland/tools/arca`; il tool completo arriva in A7.
//!
//! Uso: `arca create <path> [--uuid HEX] [--size-mib N]`.
//! `<path>` puo' essere un file immagine (LBA0) o un device/partizione:
//! `/dev/sdX` → whole-disk (LBA0), `/dev/sdXn` → partizione, il tool legge
//! la tabella MBR/GPT del disco per trovare `start_lba` e scrive li'.

use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::exit;

use arcafs as af;
use arcafs::format::{ARCA_BLOCK_SECTORS, ARCA_BLOCK_SIZE};

fn usage() -> ! {
    eprintln!("uso: arca create <path> [--uuid HEX16] [--size-mib N] [--label L] [--mountpoint M]  (default 1 MiB, uuid auto)");
    eprintln!("     arca uuid <img> <HEX16> [--lba N]  (patch uuid superblock a LBA N, default 0)");
    eprintln!("     arca set <img> [--uuid HEX16] [--label L] [--mountpoint M] [--lba N]  (patch identita' volume)");
    eprintln!("     arca seed <img> [--lba N] [ns:chiave=host]... [sys:chiave=host]...  (solo volumi freschi)");
    exit(2);
}

/// Inizializza un volume vuoto su `f` a `base` settori (0 = whole-disk):
/// header-estensione + nodo root al blocco 1 + ROOT nel superblock.
/// Fallisce loud (exit 1) su qualunque errore IO.
fn init_empty_volume(f: &mut std::fs::File, base: u64, uuid: u64) {
    use std::io::{Seek, SeekFrom, Write};
    // Root vuota al blocco 1 (relativo a `base`).
    let mut root = [0u8; ARCA_BLOCK_SIZE as usize];
    af::format::node_fill(&mut root, af::format::ARCA_NODE_TYPE_RAW, 0, &[0u8; af::format::ARCA_NODE_PAYLOAD_LEN]);
    let root_off = (base + ARCA_BLOCK_SECTORS as u64) * 512;
    if f.seek(SeekFrom::Start(root_off)).is_err() || f.write_all(&root).is_err() {
        eprintln!("scrittura root fallita");
        exit(1);
    }
    // Header-estensione a settori base+2..base+6.
    let xh = af::format::HeaderExt {
        free_head: 0,
        high_water: 2,
        next_id: 1,
        next_snap: 1,
        flags: 0,
    };
    let enc = af::format::xhdr_encode(&xh);
    let xh_off = base * 512 + af::format::ARCA_XHDROFF as u64;
    if f.seek(SeekFrom::Start(xh_off)).is_err() || f.write_all(&enc).is_err() {
        eprintln!("scrittura header-estensione fallita");
        exit(1);
    }
    // ROOT = 1 nel superblock (checksum ricalcolato).
    let mut sec0 = [0u8; 512];
    if f.seek(SeekFrom::Start(base * 512)).is_err()
        || std::io::Read::read_exact(f, &mut sec0).is_err()
        || af::format::superblock_set_root(&mut sec0, 1).is_none()
    {
        eprintln!("ROOT nel superblock fallito");
        exit(1);
    }
    if f.seek(SeekFrom::Start(base * 512)).is_err() || f.write_all(&sec0).is_err() {
        eprintln!("scrittura superblock con ROOT fallita");
        exit(1);
    }
    // Shadow sincronizzato (stessa generazione al format: crash-safe per
    // costruzione, il flip entra con il commit 56.2b).
    if f.seek(SeekFrom::Start((base + 1) * 512)).is_err() || f.write_all(&sec0).is_err() {
        eprintln!("scrittura shadow fallita");
        exit(1);
    }
    println!(
        "arca: volume inizializzato (root=1, uuid={:016X})",
        uuid,
    );
}

/// Genera un UUID volume a 64 bit dall'OS RNG (`/dev/urandom`, 8 byte).
/// Fallback senza entropia: `(nanos xor pid)` — mai collisione pratica su
/// volumi creati a mano, ma il path primario resta l'OS (documentato qui,
/// non nascosto). Zero dipendenze esterne (repo offline, niente registry).
fn auto_uuid() -> u64 {
    if let Ok(mut f) = OpenOptions::new().read(true).open("/dev/urandom") {
        let mut b = [0u8; 8];
        if std::io::Read::read_exact(&mut f, &mut b).is_ok() {
            let v = u64::from_le_bytes(b);
            if v != 0 {
                return v;
            }
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    (nanos ^ (std::process::id() as u64).wrapping_mul(0x100000001B3)).max(1)
}

/// Estrae il nome del disco base da un path di partizione.
/// `/dev/sda1` → `sda`, `/dev/nvme0n1p2` → `nvme0n1`, `/dev/sdb` → `sdb`.
fn base_disk_name(path: &str) -> Option<&str> {
    let name = path.trim_start_matches("/dev/");
    // MBR/GPT naming: sd[a-z][1-9] o nvmeNnM[p][1-9]
    if name.starts_with("sd") && name.len() > 2 {
        let rest = &name[2..];
        if let Some(idx) = rest.bytes().position(|b| b.is_ascii_digit()) {
            return Some(&name[..2 + idx]);
        }
    }
    // NVMe: nvmeNnM o nvmeNnMpM
    if name.starts_with("nvme") {
        let mut parts = name.split('p');
        if let Some(base) = parts.next() {
            for p in parts {
                if p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty() {
                    return Some(base);
                }
            }
            // No partition number → whole nvme device
            return Some(name);
        }
    }
    None
}

/// Legge il primo settore (512 B) del disco per rilevare MBR/GPT.
fn read_mbr_gpt(disk_path: &str, buf: &mut [u8; 512]) -> bool {
    let mut f = match OpenOptions::new().read(true).open(disk_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    match f.read_exact(buf) {
        Ok(_) => true,
        Err(_) => false,
    }
}

/// Cerca una partizione attiva nel MBR (registro 446–511).
/// Ritorna `Some((start_lba, size))` della prima entry con type != 0.
/// BUG STORICO (Fase 55): lo start era ricalcolato a mano su 3 byte stile
/// CHS invece che u32 LE a off+8 — ora single source col guest
/// (`parse_mbr` in block/part.rs).
fn find_mbr_partition(buf: &[u8; 512]) -> Option<(u64, u64)> {
    // MBR partition table starts at offset 446 (0x1BE), 16 bytes per entry
    for i in 0..4 {
        let off = 446 + i * 16;
        let ptype = buf[off + 4];
        if ptype == 0 {
            continue; // entry vuota
        }
        let start = u32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]) as u64;
        let size = u32::from_le_bytes([
            buf[off + 12],
            buf[off + 13],
            buf[off + 14],
            buf[off + 15],
        ]) as u64;
        if start > 0 && size > 0 {
            return Some((start, size));
        }
    }
    None
}

/// Trova l'offset LBA della partizione nel path dato.
fn find_partition_offset(path: &str) -> Option<(u64, u64)> {
    let disk_name = base_disk_name(path)?;
    // Costruisce il path del disco base: /dev/sda1 → /dev/sda
    let disk_path = format!("/dev/{}", disk_name);

    let mut buf = [0u8; 512];
    if !read_mbr_gpt(&disk_path, &mut buf) {
        eprintln!("arca: impossibile leggere {} (prova come file immagine)", disk_path);
        return None;
    }

    // Prova MBR prima
    if let Some((start, size)) = find_mbr_partition(&buf) {
        println!(
            "arca: partizione {} su {}: MBR start={} settori={}",
            path, disk_path, start, size
        );
        return Some((start, size));
    }

    // Prova GPT: tipo prima voce (byte 450, NON 446 che e' il boot flag) =
    // 0xEE → protective MBR. Header a LBA1, offset UEFI reali (stessi del
    // guest `parse_gpt_header`: magic 0, current 24, first usable 40,
    // last usable 48, entry LBA 72, count 80, size 84).
    if buf[450] == 0xEE {
        let mut f = match OpenOptions::new().read(true).open(&disk_path) {
            Ok(f) => f,
            Err(_) => return None,
        };
        let mut hdr = [0u8; 512];
        if f.seek(SeekFrom::Start(512)).is_err() || f.read_exact(&mut hdr).is_err() {
            return None;
        }
        if hdr[0..8] != *b"EFI PART" {
            return None;
        }
        let u64le = |o: usize| u64::from_le_bytes([
            hdr[o], hdr[o+1], hdr[o+2], hdr[o+3], hdr[o+4], hdr[o+5], hdr[o+6], hdr[o+7],
        ]);
        if u64le(24) != 1 {
            return None;
        }
        let first_usable = u64le(40);
        let last_usable = u64le(48);
        let entries_lba = u64le(72);
        let num_entries = u32::from_le_bytes([hdr[80], hdr[81], hdr[82], hdr[83]]) as u64;
        let entry_size = u32::from_le_bytes([hdr[84], hdr[85], hdr[86], hdr[87]]);
        if entry_size != 128 || num_entries == 0 {
            return None;
        }
        // Legge l'array entry (128 B l'una, anche a cavallo di settore).
        let total = (num_entries.min(128) * 128) as usize;
        let arr_off = entries_lba * 512;
        let arr_end = arr_off + total as u64;
        let disk_len = f.seek(SeekFrom::End(0)).unwrap_or(0);
        if arr_end > disk_len {
            return None;
        }
        let mut arr = vec![0u8; total];
        if f.seek(SeekFrom::Start(arr_off)).is_err() || f.read_exact(&mut arr).is_err() {
            return None;
        }
        for i in 0..num_entries.min(128) {
            let off = (i * 128) as usize;
            let e = &arr[off..off + 128];
            if e[0..16] == [0u8; 16] {
                continue; // entry vuota
            }
            // Entry UEFI: first LBA a +32, last LBA a +40 (u64 LE).
            let start = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
            if start == 0 || start > last || start < first_usable || last > last_usable {
                continue;
            }
            let size = last - start + 1;
            println!(
                "arca: partizione {} su {}: GPT start={} settori={}",
                path, disk_path, start, size
            );
            return Some((start, size));
        }
    }

    eprintln!("arca: nessuna partizione trovata su {}", disk_path);
    None
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }
    match args[1].as_str() {
        "create" => cmd_create(&args),
        "uuid" => cmd_uuid(&args),
        "set" => cmd_set(&args),
        "seed" => cmd_seed(&args),
        _ => usage(),
    }
}

/// Patch del superblock: uuid e/o label e/o mountpoint, checksum ricalcolato.
/// Fallisce loud su qualunque errore (mai immagini mute).
fn patch_superblock(
    path: &str,
    lba: u64,
    uuid: Option<u64>,
    label: Option<&str>,
    mountpoint: Option<&str>,
) {
    let mut f = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("set {}: {}", path, e);
            exit(1);
        }
    };
    let mut sec = [0u8; 512];
    let ok = f.seek(SeekFrom::Start(lba * 512)).is_ok()
        && std::io::Read::read_exact(&mut f, &mut sec).is_ok()
        && af::format::superblock_verify(&sec).is_some();
    if !ok {
        eprintln!("set {}: superblock ACFS assente a LBA {}", path, lba);
        exit(1);
    }
    if let Some(u) = uuid {
        sec[af::format::ARCA_OFF_UUID..af::format::ARCA_OFF_UUID + 8]
            .copy_from_slice(&u.to_le_bytes());
    }
    // `superblock_set_identity` ricalcola il checksum: se i campi testuali
    // non cambiano, lo ricalcoliamo a mano (stessa formula).
    if label.is_some() || mountpoint.is_some() {
        if af::format::superblock_set_identity(&mut sec, label, mountpoint).is_none() {
            eprintln!("set {}: superblock non valido", path);
            exit(1);
        }
    } else {
        let check = af::image_hash(&sec[..af::format::ARCA_OFF_CHECK]);
        sec[af::format::ARCA_OFF_CHECK..af::format::ARCA_OFF_CHECK + 8]
            .copy_from_slice(&check.to_le_bytes());
    }
    let ok = f.seek(SeekFrom::Start(lba * 512)).is_ok() && f.write_all(&sec).is_ok();
    if !ok {
        eprintln!("set {}: scrittura fallita", path);
        exit(1);
    }
    let id = af::format::superblock_identity(&sec);
    match id {
        Some(i) => println!(
            "arca: {} LBA{} uuid={:016X} label='{}' mountpoint='{}'",
            path,
            lba,
            i.uuid,
            i.label_str(),
            i.mountpoint_str()
        ),
        None => println!("arca: {} LBA{} patchato", path, lba),
    }
}

/// Patch dell'UUID nel superblock (Fase 2: volumi derivati con uuid
/// distinti — la root si sceglie per UUID, mai per lettera/scan).
/// `arca uuid <img> <HEX16> [--lba N]`: riscrive uuid + checksum FNV.
/// Fallisce loud su qualunque errore (mai immagini mute).
fn cmd_uuid(args: &[String]) {
    if args.len() < 4 {
        usage();
    }
    let path = &args[2];
    let uuid = match u64::from_str_radix(&args[3], 16) {
        Ok(v) if v != 0 => v,
        _ => {
            eprintln!("uuid: atteso esadecimale non-zero (es. 4152434100000002)");
            exit(2);
        }
    };
    let mut lba: u64 = 0;
    let mut i = 4;
    while i < args.len() {
        match args[i].as_str() {
            "--lba" => {
                i += 1;
                lba = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(v) => v,
                    None => {
                        eprintln!("--lba: atteso intero >= 0");
                        exit(2);
                    }
                };
            }
            other => {
                eprintln!("argomento ignoto: {}", other);
                usage();
            }
        }
        i += 1;
    }
    let mut f = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("uuid {}: {}", path, e);
            exit(1);
        }
    };
    drop(f);
    patch_superblock(path, lba, Some(uuid), None, None);
}

/// `arca set <img> [--uuid HEX16] [--label L] [--mountpoint M] [--lba N]`:
/// patch dell'identita' del volume (v4): il nome (label) e il posto dove
/// vuole essere montato (mountpoint, path RELATIVO alla root).
fn cmd_set(args: &[String]) {
    if args.len() < 3 {
        usage();
    }
    let path = &args[2];
    let mut uuid: Option<u64> = None;
    let mut label: Option<String> = None;
    let mut mountpoint: Option<String> = None;
    let mut lba: u64 = 0;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--uuid" => {
                i += 1;
                uuid = match args.get(i).and_then(|s| u64::from_str_radix(s, 16).ok()) {
                    Some(v) if v != 0 => Some(v),
                    _ => {
                        eprintln!("--uuid: atteso esadecimale non-zero (es. 4152434100000002)");
                        exit(2);
                    }
                };
            }
            "--label" => {
                i += 1;
                label = match args.get(i) {
                    Some(v) => Some(v.clone()),
                    None => {
                        eprintln!("--label: atteso un testo");
                        exit(2);
                    }
                };
            }
            "--mountpoint" => {
                i += 1;
                mountpoint = match args.get(i) {
                    Some(v) => Some(v.clone()),
                    None => {
                        eprintln!("--mountpoint: atteso un path (relativo, es. tests)");
                        exit(2);
                    }
                };
            }
            "--lba" => {
                i += 1;
                lba = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(v) => v,
                    None => {
                        eprintln!("--lba: atteso intero >= 0");
                        exit(2);
                    }
                };
            }
            other => {
                eprintln!("argomento ignoto: {}", other);
                usage();
            }
        }
        i += 1;
    }
    if uuid.is_none() && label.is_none() && mountpoint.is_none() {
        eprintln!("set: serve almeno --uuid, --label o --mountpoint");
        exit(2);
    }
    patch_superblock(path, lba, uuid, label.as_deref(), mountpoint.as_deref());
}

/// Store su file per il seed (Fase 2): specchio host di `VolumeStore`
/// (guest) — stessa semantica alloc/freelist/live/nodi, I/O su file.
/// Solo volumi freschi (freelist vuota): alloc = high_water++.
struct FileStore {
    f: std::cell::RefCell<std::fs::File>,
    base: u64,
    xh: af::format::HeaderExt,
    live: std::collections::BTreeSet<u64>,
}

impl FileStore {
    fn open(path: &str, base_sectors: u64) -> Option<Self> {
        let f = OpenOptions::new().read(true).write(true).open(path).ok()?;
        let base = base_sectors.checked_mul(512)?;
        let mut raw = vec![0u8; af::format::ARCA_XHDRLEN];
        {
            let mut fh = f.try_clone().ok()?;
            use std::io::{Seek, SeekFrom};
            fh.seek(SeekFrom::Start(base + af::format::ARCA_XHDROFF as u64)).ok()?;
            std::io::Read::read_exact(&mut fh, &mut raw).ok()?;
        }
        let xh = af::format::xhdr_decode(&raw)?;
        Some(Self { f: std::cell::RefCell::new(f), base, xh, live: std::collections::BTreeSet::new() })
    }

    fn blk_off(&self, n: u64) -> Option<u64> {
        n.checked_mul(af::format::ARCA_BLOCK_SIZE as u64)?.checked_add(self.base)
    }

    fn read_block(&self, n: u64) -> Option<[u8; ARCA_BLOCK_SIZE as usize]> {
        let off = self.blk_off(n)?;
        let mut b = [0u8; ARCA_BLOCK_SIZE as usize];
        let mut fh = self.f.borrow_mut();
        use std::io::{Seek, SeekFrom};
        fh.seek(SeekFrom::Start(off)).ok()?;
        std::io::Read::read_exact(&mut *fh, &mut b).ok()?;
        Some(b)
    }

    fn write_block(&self, n: u64, b: &[u8; ARCA_BLOCK_SIZE as usize]) -> bool {
        let off = match self.blk_off(n) {
            Some(o) => o,
            None => return false,
        };
        let mut fh = self.f.borrow_mut();
        use std::io::{Seek, SeekFrom, Write};
        fh.seek(SeekFrom::Start(off)).is_ok() && fh.write_all(b).is_ok()
    }

    fn store_xh(&self) -> bool {
        let enc = af::format::xhdr_encode(&self.xh);
        let off = self.base + af::format::ARCA_XHDROFF as u64;
        let mut fh = self.f.borrow_mut();
        use std::io::{Seek, SeekFrom, Write};
        fh.seek(SeekFrom::Start(off)).is_ok() && fh.write_all(&enc).is_ok()
    }
}

impl af::btree::BlockStore for FileStore {
    fn read_node(&self, blk: u64, out: &mut [u8; af::format::ARCA_NODE_PAYLOAD_LEN]) -> Option<(u8, u64)> {
        if blk == 0 {
            return None;
        }
        let b = self.read_block(blk)?;
        let (ty, gen) = af::format::node_verify(&b)?;
        out.copy_from_slice(&b[af::format::ARCA_NODE_PAYLOAD..af::format::ARCA_NODE_PAYLOAD + af::format::ARCA_NODE_PAYLOAD_LEN]);
        Some((ty, gen))
    }

    fn write_node(&mut self, blk: u64, ty: u8, gen: u64, payload: &[u8; af::format::ARCA_NODE_PAYLOAD_LEN]) -> bool {
        if blk == 0 || !self.live.contains(&blk) {
            return false;
        }
        let mut b = [0u8; ARCA_BLOCK_SIZE as usize];
        af::format::node_fill(&mut b, ty, gen, payload);
        self.write_block(blk, &b)
    }

    fn alloc(&mut self) -> Option<u64> {
        if self.xh.free_head != 0 {
            eprintln!("seed solo su volumi freschi (freelist non vuota)");
            return None;
        }
        let n = self.xh.high_water;
        if n == 0 {
            return None;
        }
        self.xh.high_water += 1;
        if !self.live.insert(n) {
            return None;
        }
        if !self.store_xh() {
            self.live.remove(&n);
            return None;
        }
        Some(n)
    }

    fn free(&mut self, n: u64) -> bool {
        if n == 0 || !self.live.remove(&n) {
            return false;
        }
        let mut blk = [0u8; ARCA_BLOCK_SIZE as usize];
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
}

/// Commit host (Fase 2): specchio di `btree_drv::commit` (guest) —
/// header-ext + shadow + flip gen+1 + clear DIRTY. Stesso ordine, stesse
/// garanzie (COW + root-last: a crash pre-flip la vecchia generazione e'
/// intatta). Lo store vive dentro l'engine (niente doppi handle).
fn host_commit(eng: &mut af::btree::BTree<FileStore>) -> bool {
    let (nid, nsn, rp, rs, rr, meta) = (
        eng.next_id,
        eng.next_snap,
        eng.root_primary,
        eng.root_secondary,
        eng.root_refcount,
        eng.meta_root,
    );
    if nid == 0 || nsn == 0 {
        return false;
    }
    let base = eng.store.base;
    eng.store.xh.next_id = nid;
    eng.store.xh.next_snap = nsn;
    eng.store.xh.flags |= af::format::ARCA_XH_DIRTY;
    if !eng.store.store_xh() {
        return false;
    }
    let mut sec = [0u8; 512];
    {
        let mut fh = eng.store.f.borrow_mut();
        use std::io::{Seek, SeekFrom};
        if fh.seek(SeekFrom::Start(base)).is_err()
            || std::io::Read::read_exact(&mut *fh, &mut sec).is_err()
        {
            return false;
        }
    }
    let (gen, _, _, _, _) = match af::format::superblock_roots(&sec) {
        Some(g) => g,
        None => return false,
    };
    let gen1 = match gen.checked_add(1) {
        Some(g) => g,
        None => return false,
    };
    if af::format::superblock_set_roots(&mut sec, gen1, rp, rs, rr, meta).is_none() {
        return false;
    }
    {
        let mut fh = eng.store.f.borrow_mut();
        use std::io::{Seek, SeekFrom, Write};
        if fh.seek(SeekFrom::Start(base + 512)).is_err() || fh.write_all(&sec).is_err() {
            return false;
        }
        if fh.seek(SeekFrom::Start(base)).is_err() || fh.write_all(&sec).is_err() {
            return false;
        }
    }
    eng.store.xh.flags &= !af::format::ARCA_XH_DIRTY;
    eng.store.store_xh()
}

/// Seed di un volume fresco (Fase 2): `arca seed <img> [--lba N]`
/// `ns:chiave=host`... `sys:chiave=host`... Mette i file di boot
/// (`/bin`, `/test`, fixture) nel volume PRIMA del boot: fonte unica
/// riproducibile (niente copia runtime). Solo volumi freschi (radici a
/// zero): le derivate ereditano via copia blocchi. Fallisce loud.
fn cmd_seed(args: &[String]) {
    if args.len() < 3 {
        usage();
    }
    let path = &args[2];
    let mut lba: u64 = 0;
    let mut entries: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--lba" => {
                i += 1;
                lba = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(v) => v,
                    None => {
                        eprintln!("--lba: atteso intero >= 0");
                        exit(2);
                    }
                };
            }
            other => {
                // D1: bucket arbitrario `<nome>:chiave=host` (ns/sys/vela/usr…):
                // nome ASCII alfanumerico entro OBJ_BUCKET_MAX (16), come il
                // guest (`bounds_invalid` rifiuta oltre bound — mai discrepanze
                // host/guest sui nomi).
                let (bk, rest) = match other.split_once(':') {
                    Some((b, r))
                        if !b.is_empty()
                            && b.len() <= 16
                            && b.bytes().all(|c| c.is_ascii_alphanumeric()) =>
                    {
                        (b, r)
                    }
                    _ => {
                        eprintln!("voce ignota (atteso <bucket>:chiave=host): {}", other);
                        exit(2);
                    }
                };
                let (key, host) = match rest.split_once('=') {
                    Some((k, h)) if !k.is_empty() && !h.is_empty() => (k, h),
                    _ => {
                        eprintln!("voce malformata (atteso <bucket>:chiave=host): {}", other);
                        exit(2);
                    }
                };
                entries.push((bk.as_bytes().to_vec(), key.as_bytes().to_vec(), host.as_bytes().to_vec()));
            }
        }
        i += 1;
    }
    if entries.is_empty() {
        eprintln!("seed: nessuna voce (ns:chiave=host ...)");
        exit(2);
    }
    let store = match FileStore::open(path, lba) {
        Some(s) => s,
        None => {
            eprintln!("seed {}: volume illeggibile", path);
            exit(1);
        }
    };
    let mut sec = [0u8; 512];
    {
        let mut fh = store.f.borrow_mut();
        use std::io::{Seek, SeekFrom};
        if fh.seek(SeekFrom::Start(store.base)).is_err()
            || std::io::Read::read_exact(&mut *fh, &mut sec).is_err()
        {
            eprintln!("seed {}: superblock illeggibile", path);
            exit(1);
        }
    }
    let (gen, _uuid, _rp, rs, _rr) = match af::format::superblock_roots(&sec) {
        Some(g) => g,
        None => {
            eprintln!("seed {}: superblock invalido", path);
            exit(1);
        }
    };
    if rs != 0 {
        eprintln!("seed {}: solo volumi freschi (secondary root != 0: gia' inizializzato)", path);
        exit(1);
    }
    let mut eng = match af::btree::BTree::new(store, gen) {
        Some(e) => e,
        None => {
            eprintln!("seed {}: init indice fallito", path);
            exit(1);
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut total = 0usize;
    for (bucket, key, host) in entries.iter() {
        let data = match std::fs::read(
            core::str::from_utf8(host).unwrap_or(""),
        ) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("seed: lettura {}: {}", String::from_utf8_lossy(host), e);
                exit(1);
            }
        };
        total += data.len();
        if eng.put_chunk(bucket, key, 0, &data, now, 8).is_none() {
            eprintln!("seed: put {} fallito", String::from_utf8_lossy(key));
            exit(1);
        }
    }
    if !host_commit(&mut eng) {
        eprintln!("seed {}: commit fallito", path);
        exit(1);
    }
    println!("arca: seed {} ({} voci, {} B, gen {})", path, entries.len(), total, gen + 1);
}

fn cmd_create(args: &[String]) {
    if args.len() < 3 {
        usage();
    }
    let path = &args[2];
    // UUID auto-generato (Fase 56.2a): l'unicita' del volume non puo'
    // dipendere dall'utente (default 1 = collisioni garantite tra volumi).
    // `--uuid` resta solo come override esplicito (test deterministici).
    let mut uuid: u64 = auto_uuid();
    let mut size_mib: u64 = 1;
    // v4 (ADR-0058): il volume si presenta — `label` (il nome) e
    // `mountpoint` (dove vuole essere montato, path RELATIVO alla root).
    // Default: il mountpoint segue la label ("TESTS" -> "tests"); vuoto =
    // il volume non dichiara niente e non viene auto-montato.
    let mut label = String::new();
    let mut mountpoint: Option<String> = None;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--uuid" => {
                i += 1;
                uuid = match args.get(i).and_then(|s| u64::from_str_radix(s, 16).ok()) {
                    Some(v) => v,
                    None => {
                        eprintln!("--uuid: atteso esadecimale (es. 4152434100000001)");
                        exit(2);
                    }
                };
            }
            "--label" => {
                i += 1;
                label = match args.get(i) {
                    Some(v) => v.clone(),
                    None => {
                        eprintln!("--label: atteso un testo");
                        exit(2);
                    }
                };
            }
            "--mountpoint" => {
                i += 1;
                mountpoint = match args.get(i) {
                    Some(v) => Some(v.clone()),
                    None => {
                        eprintln!("--mountpoint: atteso un path (relativo, es. tests)");
                        exit(2);
                    }
                };
            }
            "--size-mib" => {
                i += 1;
                size_mib = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(v) if v > 0 => v,
                    _ => {
                        eprintln!("--size-mib: atteso intero > 0");
                        exit(2);
                    }
                };
            }
            other => {
                eprintln!("argomento ignoto: {}", other);
                usage();
            }
        }
        i += 1;
    }
    // Mountpoint di default = la label (minuscola): "TESTS" -> "tests".
    let mount = match mountpoint {
        Some(m) => m,
        None => label.to_lowercase(),
    };

    let sb = af::format::superblock_build(uuid, 1, &label, &mount);

    // Determina se e' una partizione o whole-disk
    let is_partition = path.trim_start_matches("/dev/")
        .as_bytes()
        .last()
        .map_or(false, |b| b.is_ascii_digit());

    if is_partition {
        // Partizione: trova start_lba dalla tabella del disco
        match find_partition_offset(path) {
            Some((start, size)) => {
                let mut f = match OpenOptions::new().read(true).write(true).open(path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("create {}: {}", path, e);
                        exit(1);
                    }
                };
                // Scrivi superblock a start_lba * 512 + shadow a start+1
                // (partition-relative LBA0/LBA1, arcafs.md §16.4).
                let offset = start * 512;
                let shadow = (start + 1) * 512;
                let ok = f.seek(SeekFrom::Start(offset)).is_ok()
                    && f.write_all(&sb).is_ok()
                    && f.seek(SeekFrom::Start(shadow)).is_ok()
                    && f.write_all(&sb).is_ok();
                if !ok {
                    eprintln!("scrittura superblock a offset {} fallita", offset);
                    exit(1);
                }
                init_empty_volume(&mut f, start, uuid);
                println!(
                    "arca: creato {} (partizione start={} size={} MiB), uuid={:016X}, gen=1, block_size={}",
                    path,
                    start,
                    size * 512 / 1024 / 1024,
                    uuid,
                    af::format::ARCA_BLOCK_SIZE
                );
            }
            None => {
                eprintln!("arca: impossibile determinare offset partizione per {}", path);
                exit(1);
            }
        }
    } else {
        // Whole-disk o file immagine: LBA0
        let size = size_mib * 1024 * 1024;
        let mut f = match OpenOptions::new().create(true).read(true).write(true).truncate(true).open(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("create {}: {}", path, e);
                exit(1);
            }
        };
        if f.write_all(&sb).is_err() || f.seek(SeekFrom::Start(512)).is_err() || f.write_all(&sb).is_err() {
            eprintln!("scrittura superblock fallita");
            exit(1);
        }
        // Resto dell'immagine a zero (sparse): porta la dimensione a `size`.
        if f.set_len(size).is_err() {
            eprintln!("set_len {} fallita", size);
            exit(1);
        }
        init_empty_volume(&mut f, 0, uuid);
        println!(
            "arca: creato {} ({} MiB), uuid={:016X}, gen=1, block_size={}",
            path,
            size_mib,
            uuid,
            af::format::ARCA_BLOCK_SIZE
        );
    }
    
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    /// Seed + reload come il bind guest (superblock → roots → load →
    /// get/stat/scan): prova che il volume seedato si apre dal disco.
    #[test]
    fn seed_roundtrip_load() {
        let dir = std::env::temp_dir().join("arca-seed-test.img");
        let _ = std::fs::remove_file(&dir);
        let path = dir.to_str().unwrap().to_string();
        // Crea + seed via API interna (stesso codice dei sottocomandi).
        {
            let mut f = OpenOptions::new().create(true).read(true).write(true).truncate(true).open(&path).unwrap();
            let uuid = 0x41524341000000A1u64;
            let sb = af::format::superblock_build(uuid, 1, "TEST", "tests");
            f.write_all(&sb).unwrap();
            f.seek(SeekFrom::Start(512)).unwrap();
            f.write_all(&sb).unwrap();
            f.set_len(8 * 1024 * 1024).unwrap();
            init_empty_volume(&mut f, 0, uuid);
        }
        let store = FileStore::open(&path, 0).expect("open store");
        let mut sec = [0u8; 512];
        {
            let mut fh = store.f.borrow_mut();
            use std::io::{Seek, SeekFrom};
            fh.seek(SeekFrom::Start(0)).unwrap();
            std::io::Read::read_exact(&mut *fh, &mut sec).unwrap();
        }
        let (gen, uuid, rp, rs, rr) = af::format::superblock_roots(&sec).expect("roots");
        assert_eq!(uuid, 0x41524341000000A1u64);
        assert_eq!(rs, 0, "fresco: secondary a zero");
        let mut eng = af::btree::BTree::new(store, gen).expect("new");
        assert!(eng.put_chunk(b"ns", b"bin/x", 0, b"payload\xe7\xa8\xae", 7, 8).is_some());
        assert!(host_commit(&mut eng));
        // Reload dal disco come il bind guest.
        let store2 = FileStore::open(&path, 0).expect("reopen");
        let mut sec2 = [0u8; 512];
        {
            let mut fh = store2.f.borrow_mut();
            use std::io::{Seek, SeekFrom};
            fh.seek(SeekFrom::Start(0)).unwrap();
            std::io::Read::read_exact(&mut *fh, &mut sec2).unwrap();
        }
        let (gen2, _, rp2, rs2, rr2) = af::format::superblock_roots(&sec2).expect("roots2");
        assert_eq!(gen2, gen + 1);
        let eng2 = af::btree::BTree::new_with_roots(store2, gen2, rp2, rs2, rr2).expect("load");
        assert_eq!(eng2.get(b"ns", b"bin/x"), Some(b"payload\xe7\xa8\xae".to_vec()));
        let (_, size, _, mtime) = eng2.stat(b"ns", b"bin/x").expect("stat");
        assert_eq!((size, mtime), (10, 7));
        let keys = eng2.scan_prefix(b"ns", b"bin/").expect("scan");
        assert_eq!(keys, vec![b"bin/x".to_vec()]);
        let _ = std::fs::remove_file(&path);
        let _ = (rp, rr);
    }
}
