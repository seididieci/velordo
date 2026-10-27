use super::abi::*;
use super::{cwd, ring, session};
use super::{State, Sys};
use super::error::Error;

// ── FS wrappers ───────────────────────────────────────────────────

/// `open(path, flags)`: apre un file tramite il fs server.
/// Ritorna il fd (>=0) o l'errore nativo (Fase 39).
#[inline]
pub fn open<S: Sys>(st: &'static State<S>, path: &str, flags: u32) -> Result<i64, Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, _, _) = fs_notify(st, R_OPEN, rp.len() as u64, flags as u64, rp.as_ref())?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|v| v as i64)
}

/// Throttle tra retry su pipe (Fase 42): solo spin puri (IF=1, mai `get_ticks`
/// che maschera gli interrupt e affama il timer — lezione scheduler). Ogni
/// retry e' comunque un round-trip FS in cui il client dorme bloccato in
/// `send`: niente dilution (il quanto va agli altri, t30).
fn pipe_throttle() {
    for _ in 0..200_000 {
        core::hint::spin_loop();
    }
}

/// Invia un frame request (`tag`, `w0`, `w1`, `payload`) e notifica cardo:
/// `fs_gate`, scrittura request ring, `FS_NOTIFY`, retry con riscrittura del
/// frame (re-handshake). Ritorna `(result, w1, payload_len)` della reply o
/// `Err(NotReady)` se cardo non e' raggiungibile.
///
/// NON consuma il frame di risposta: spetta al chiamante
/// `resp_ring_consume(16)` (nessun payload) o `resp_ring_read_payload`
/// (con payload), poi interpretare `result` via `fs_reply_check`.
#[inline(always)]
fn fs_notify<S: Sys>(
    st: &'static State<S>,
    tag: u32,
    w0: u64,
    w1: u64,
    payload: &[u8],
) -> Result<(u64, u64, usize), Error> {
    session::fs_gate(st)?;
    if !ring::req_ring_write(tag, w0, w1, payload) {
        return Err(Error::RingFull);
    }
    match session::fs_notify_result(st, tag as u64, || {
        ring::req_ring_write(tag, w0, w1, payload)
    }) {
        Some(r) => Ok(r),
        None => Err(Error::NotReady),
    }
}

/// `read_fs(fd, dst, max_count)`: legge fino a `max_count` byte dal file.
/// I dati viaggiano nel response ring; se `max_count` supera la capacita' di
/// un singolo frame, la lettura viene spezzata in piu' round trip (Fase 10.2).
/// Ritorna i byte letti (0 = EOF); un errore dopo progressi parziali ritorna
/// `Ok(got)` (semantica POSIX: conta cio' che c'e'), solo a zero progressi e'
/// `Err` (Fase 39).
/// Fase 42 (pipe): `Empty` (pipe vuota, writer vivi) si riprova throttled QUI
/// (read bloccante: i file non emettono mai Empty, quindi per loro nulla
/// cambia). La morte del writer risolve sempre via purge server-side (EOF):
/// l'unico stallo possibile e' un writer vivo che non scrive mai — come la
/// read bloccante POSIX.
pub fn read_fs<S: Sys>(st: &'static State<S>, fd: i64, dst: &mut [u8], max_count: usize) -> Result<usize, Error> {
    session::fs_gate(st)?;
    let mut got = 0usize;
    while got < max_count {
        let want = (max_count - got).min(ring::RING_MAX_PAYLOAD);
        if !ring::req_ring_write(R_READ, fd as u64, want as u64, &[]) {
            return if got > 0 { Ok(got) } else { Err(Error::RingFull) };
        }
        let n = match session::fs_notify_result(st, FS_NOTIFY, || {
            ring::req_ring_write(R_READ, fd as u64, want as u64, &[])
        }) {
            Some((result, _, payload_len)) => {
                // Pipe vuota (writer vivi): read bloccante — throttled e si
                // riprova lo STESSO chunk (solo le pipe emettono Empty).
                if result == ERR_EMPTY {
                    ring::resp_ring_consume(16);
                    pipe_throttle();
                    continue;
                }
                if session::fs_reply_check(result).is_err() {
                    ring::resp_ring_consume(16);
                    if got > 0 {
                        return Ok(got);
                    }
                    return Err(Error::Failed);
                }
                let avail = (result as usize).min(payload_len).min(max_count - got);
                if avail > 0 {
                    ring::resp_ring_read_payload(&mut dst[got..got + avail], avail);
                } else {
                    ring::resp_ring_consume(16);
                }
                let _ = result;
                avail
            }
            None => {
                if got > 0 {
                    return Ok(got);
                }
                return Err(Error::NotReady);
            }
        };
        if n == 0 {
            break; // EOF
        }
        got += n;
        if n < want {
            break; // read corto (EOF o file piu' corto)
        }
    }
    Ok(got)
}

/// `write_fs(fd, src, count)`: scrive `count` byte sul file (dal request ring).
/// I dati viaggiano nel request ring; se `count` supera la capacita' di un
/// singolo frame, la scrittura viene spezzata in piu' round trip (Fase 10.2).
/// Ritorna i byte scritti; errore dopo progressi parziali = `Ok(done)` (come
/// `read_fs`, Fase 39).
/// Fase 42 (pipe): `Empty` (pipe piena, lettori vivi) si riprova throttled
/// QUI (write bloccante) e il parziale si completa col resto (i file non
/// emettono mai Empty e il loro parziale resta terminale come prima: ogni
/// iterazione o avanza `done` o esce — mai loop infinito).
pub fn write_fs<S: Sys>(st: &'static State<S>, fd: i64, src: &[u8], count: usize) -> Result<usize, Error> {
    session::fs_gate(st)?;
    let mut done = 0usize;
    while done < count {
        let want = (count - done).min(ring::RING_MAX_PAYLOAD);
        if !ring::req_ring_write(R_WRITE, fd as u64, want as u64, &src[done..done + want]) {
            return if done > 0 { Ok(done) } else { Err(Error::RingFull) };
        }
        let n = match session::fs_notify_result(st, FS_NOTIFY, || {
            ring::req_ring_write(R_WRITE, fd as u64, want as u64, &src[done..done + want])
        }) {
            Some((result, _, _)) => {
                // Pipe piena (lettori vivi): write bloccante — throttled e si
                // riprova lo STESSO chunk (solo le pipe emettono Empty).
                if result == ERR_EMPTY {
                    ring::resp_ring_consume(16);
                    pipe_throttle();
                    continue;
                }
                ring::resp_ring_consume(16);
                let r = match session::fs_reply_check(result) {
                    Ok(v) => v,
                    Err(e) => {
                        if done > 0 {
                            return Ok(done);
                        }
                        return Err(e);
                    }
                };
                (r as usize).min(want)
            }
            None => {
                if done > 0 {
                    return Ok(done);
                }
                return Err(Error::NotReady);
            }
        };
        if n == 0 {
            break;
        }
        done += n;
        // Parziale: si completa col resto (pipe piena drenata in concorrenza;
        // i file restano terminali di fatto: il prossimo giro accetta 0 e si
        // esce qui sopra — mai loop senza progressi).
    }
    Ok(done)
}

/// `close(fd)`: chiude un file descriptor.
#[inline]
pub fn close<S: Sys>(st: &'static State<S>, fd: i64) -> Result<(), Error> {
    let (result, _, _) = fs_notify(st, R_CLOSE, fd as u64, 0, &[])?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// `readdir(path, entries_buf, buf_len)`: legge le entry di una directory.
/// Le entry vengono scritte dal server nel response ring nel formato
/// "name\0name\0...\0\0"; le copiamo in `entries_buf`. Ritorna il numero di
/// entry (Fase 39).
#[inline]
pub fn readdir<S: Sys>(st: &'static State<S>, path: &str, entries_buf: &mut [u8], buf_len: usize) -> Result<usize, Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, _, payload_len) = fs_notify(st, R_READDIR, rp.len() as u64, 0, rp.as_ref())?;
    // Disciplina ring (Fase 17): il response frame si consuma SEMPRE (anche a
    // diniego), POI si interpreta — mai early-return prima.
    let count = session::fs_reply_check(result);
    if count.is_ok() && payload_len > 0 {
        ring::resp_ring_read_payload(entries_buf, payload_len.min(buf_len));
    } else {
        ring::resp_ring_consume(16);
    }
    count.map(|c| c as usize)
}

/// `mkdir(path)`: crea una directory tramite il fs server (Fase 39).
#[inline]
pub fn mkdir<S: Sys>(st: &'static State<S>, path: &str) -> Result<(), Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, _, _) = fs_notify(st, R_MKDIR, rp.len() as u64, 0, rp.as_ref())?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// `remove(path)`: cancella un file o una directory VUOTA (Fase 18.2).
/// Solo ramfs: FAT read-only e device remoti rifiutano (Fase 39).
#[inline]
pub fn remove<S: Sys>(st: &'static State<S>, path: &str) -> Result<(), Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, _, _) = fs_notify(st, R_DELETE, rp.len() as u64, 0, rp.as_ref())?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// S1.1 — rinomina `old` in `new` sullo STESSO mount (payload `old\0new\0`).
/// Cross-mount/remoti/FAT = errore (mai silent-copy); fd aperti sul vecchio
/// nome vanno stale (NotFound al prossimo uso). Errori tipizzati dal server
/// (NotFound/IsDir/NotDir/Exists/ReadOnly/Invalid).
pub fn rename<S: Sys>(st: &'static State<S>, old: &str, new: &str) -> Result<(), Error> {
    let old = cwd::resolve::<S>(old)?;
    let new = cwd::resolve::<S>(new)?;
    let mut p = alloc::vec::Vec::with_capacity(old.len() + new.len() + 2);
    p.extend_from_slice(&old);
    p.push(0);
    p.extend_from_slice(&new);
    p.push(0);
    let (result, _, _) = fs_notify(st, R_RENAME, p.len() as u64, 0, &p)?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// S1.1 — `symlink(target, link)`: crea un link simbolico (payload
/// `link\0target\0`). Il target non e' risolto (dangling ammessi);
/// FAT/remoti = errore. Errori tipizzati (Exists/NotFound/Invalid/ReadOnly).
pub fn symlink<S: Sys>(st: &'static State<S>, link: &str, target: &str) -> Result<(), Error> {
    let link = cwd::resolve::<S>(link)?;
    let mut p = alloc::vec::Vec::with_capacity(link.len() + target.len() + 2);
    p.extend_from_slice(&link);
    p.push(0);
    p.extend_from_slice(target.as_bytes());
    p.push(0);
    let (result, _, _) = fs_notify(st, R_SYMLINK, p.len() as u64, 0, &p)?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// S1.1 — `readlink(path)`: legge il target del link (reply w0 = len +
/// frame). Non segue (come readlink POSIX); su non-link = errore.
pub fn readlink<S: Sys>(st: &'static State<S>, path: &str) -> Result<alloc::vec::Vec<u8>, Error> {
    session::fs_gate(st)?;
    let rp = cwd::resolve::<S>(path)?;
    if !ring::req_ring_write(R_READLINK, rp.len() as u64, 0, &rp) {
        return Err(Error::RingFull);
    }
    match session::fs_notify_result(st, FS_NOTIFY, || {
        ring::req_ring_write(R_READLINK, rp.len() as u64, 0, &rp)
    }) {
        Some((result, w1, payload_len)) => {
            let total = match session::fs_reply_check(result) {
                Ok(v) => v as usize,
                Err(e) => {
                    ring::resp_ring_consume(16);
                    return Err(e);
                }
            };
            let _ = w1;
            // Consuma SEMPRE l'intero frame (16 + payload_len): parziali
            // disallineerebbero il ring (stessa disciplina obj_get).
            let mut buf = alloc::vec::Vec::with_capacity(payload_len);
            buf.resize(payload_len, 0);
            ring::resp_ring_read_payload(&mut buf, payload_len);
            buf.truncate(total.min(payload_len));
            Ok(buf)
        }
        None => Err(Error::NotReady),
    }
}

/// S1.1 — `chmod(path, mode)`: projection (mode 12 bit in w1), mai
/// enforcement fino ad A4. Accetta su esistente (i build non falliscono),
/// NOTFOUND su mancante. FAT/remoti accettano/rifiutano come rename.
pub fn chmod<S: Sys>(st: &'static State<S>, path: &str, mode: u32) -> Result<(), Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, _, _) = fs_notify(st, R_CHMOD, rp.len() as u64, mode as u64, rp.as_ref())?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// Fase 19.2 — metadati di un path (zero kernel: frame R_STAT a cardo, nessun
/// fd coinvolto). `size` = byte del file (0 per dir/device); `kind` = tipo
/// (STAT_FILE/DIR/DEVICE); `readonly` = bit 7 (FAT sempre, ramfs mai, device
/// mai affermato senza interrogare il driver). Fase 50: `mtime` = secondi
/// epoch (UTC) dal provider, 0 = sconosciuto (sintetici, mai inventato).
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub size: u64,
    pub kind: u64,
    pub readonly: bool,
    pub mtime: u64,
}

impl Stat {
    pub fn is_file(&self) -> bool {
        self.kind & 0x3 == STAT_FILE
    }
    pub fn is_dir(&self) -> bool {
        self.kind & 0x3 == STAT_DIR
    }
    pub fn is_device(&self) -> bool {
        self.kind & 0x3 == STAT_DEVICE
    }
    pub fn is_symlink(&self) -> bool {
        self.kind & 0x3 == STAT_SYMLINK
    }
}

/// `stat(path, out)`: metadati senza aprire (Fase 39; `NotFound` tipizzato in
/// Fase 40, oggi `Failed`).
#[inline]
pub fn stat<S: Sys>(st: &'static State<S>, path: &str, out: &mut Stat) -> Result<(), Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, w1, _) = fs_notify(st, R_STAT, rp.len() as u64, 0, rp.as_ref())?;
    // Risposta self-written `[size:8][kind:8][mtime:8]` (Fase 50):
    // result=size, w1=kind, payload=mtime (LE64). Disciplina obj_get
    // (test-6): prima si interpreta il result, il payload si legge solo
    // a successo; a errore si consuma il deny-frame da 16 e si ritorna
    // il codice tipizzato (mai overshoot nel frame dopo).
    let size = match session::fs_reply_check(result) {
        Ok(v) => v,
        Err(e) => {
            ring::resp_ring_consume(16);
            return Err(e);
        }
    };
    let mut mt = [0u8; 8];
    ring::resp_ring_read_payload(&mut mt, 8);
    out.size = size;
    out.kind = w1 & 0x3;
    out.readonly = w1 & STAT_READONLY != 0;
    out.mtime = u64::from_le_bytes(mt);
    Ok(())
}

/// Fase 51 (P2 vocabolario disco) — descrittore disco da `R_DISK_INFO`
/// (relay cardo verso DISK_*). `flags` con layout single-source in
/// `syscall-numbers`; stringhe IDENTIFY a lunghezza esplicita (max 40+20).
#[derive(Clone, Copy, Debug)]
pub struct DiskDesc {
    pub sectors: u64,
    pub flags: u64,
    pub model: [u8; 40],
    pub model_len: usize,
    pub serial: [u8; 20],
    pub serial_len: usize,
}

impl DiskDesc {
    /// LBA48 (word 83.10) vs LBA28.
    pub fn lba48(&self) -> bool {
        self.flags & 1 != 0
    }
    /// TRIM capability rilevata (word 169.0, mai usata dal sistema).
    pub fn trim(&self) -> bool {
        self.flags & (1 << 1) != 0
    }
    /// Modo UDMA negoziato (None = PIO).
    pub fn udma(&self) -> Option<u8> {
        match (self.flags >> 8) & 0xF {
            0xF => None,
            m => Some(m as u8),
        }
    }
    /// Rotation rate word 217 (1 = SSD, 0 = non riportato, else RPM).
    pub fn rotation(&self) -> u16 {
        ((self.flags >> 16) & 0xFFFF) as u16
    }
    /// true se SSD/non-rotazionale; false = HDD o ignoto.
    pub fn is_ssd(&self) -> bool {
        self.rotation() == 1
    }
    /// Settore logico / fisico in byte (word 106+117-118).
    pub fn sec_logical(&self) -> u16 {
        ((self.flags >> 32) & 0xFFFF) as u16
    }
    pub fn sec_physical(&self) -> u16 {
        ((self.flags >> 48) & 0xFFFF) as u16
    }
    /// Modello/seriale come str (IDENTIFY e' ASCII: mai panico qui).
    pub fn model_str(&self) -> &str {
        core::str::from_utf8(&self.model[..self.model_len]).unwrap_or("?")
    }
    pub fn serial_str(&self) -> &str {
        core::str::from_utf8(&self.serial[..self.serial_len]).unwrap_or("?")
    }
}

/// `disk_list()`: topologia dischi (sda=0, ...) come `(settori, flags)` —
/// relay `R_DISK_LIST` (expect 0), risposta self-written con entry 16 B.
/// Usato dai test (t32) e da `arca list` (P5).
#[inline]
pub fn disk_list<S: Sys>(st: &'static State<S>) -> Result<alloc::vec::Vec<(u64, u64)>, Error> {
    let (result, _, _) = fs_notify(st, R_DISK_LIST, 0, 0, &[])?;
    // Risposta `[count:8][0:8][entry...]`: result=count, payload N×16 B.
    // Disciplina obj_get (test-6): a errore si consuma il deny-frame.
    let n = match session::fs_reply_check(result) {
        Ok(v) => (v as usize).min(16),
        Err(e) => {
            ring::resp_ring_consume(16);
            return Err(e);
        }
    };
    let mut buf = [0u8; 16 * 16];
    ring::resp_ring_read_payload(&mut buf, n * 16);
    let mut out = alloc::vec::Vec::new();
    for k in 0..n {
        let s = u64::from_le_bytes(buf[k * 16..k * 16 + 8].try_into().unwrap_or([0; 8]));
        let f = u64::from_le_bytes(buf[k * 16 + 8..k * 16 + 16].try_into().unwrap_or([0; 8]));
        out.push((s, f));
    }
    Ok(out)
}

/// `disk_info(idx)`: dettaglio disco `idx` (sda=0, ...) — relay `R_DISK_INFO`
/// (w0 = indice, niente payload), risposta self-written con settori/flags +
/// frame fisso 76 B (il client sa sempre cosa leggere).
#[inline]
pub fn disk_info<S: Sys>(st: &'static State<S>, idx: u32) -> Result<DiskDesc, Error> {
    let (result, w1, _) = fs_notify(st, R_DISK_INFO, idx as u64, 0, &[])?;
    let sectors = match session::fs_reply_check(result) {
        Ok(v) => v,
        Err(e) => {
            ring::resp_ring_consume(16);
            return Err(e);
        }
    };
    let mut buf = [0u8; 76];
    ring::resp_ring_read_payload(&mut buf, 76);
    let ml = u64::from_le_bytes(buf[..8].try_into().unwrap_or([0xFF; 8])) as usize;
    let sl = u64::from_le_bytes(buf[48..56].try_into().unwrap_or([0xFF; 8])) as usize;
    if ml > 40 || sl > 20 {
        return Err(Error::Failed);
    }
    let mut model = [0u8; 40];
    model[..ml].copy_from_slice(&buf[8..8 + ml]);
    let mut serial = [0u8; 20];
    serial[..sl].copy_from_slice(&buf[56..56 + sl]);
    Ok(DiskDesc { sectors, flags: w1, model, model_len: ml, serial, serial_len: sl })
}

/// Fase 52 (P3 durabilita') — `disk_sync(mode)`: imposta l'aspettativa di
/// durabilita' del canale (`SYNC_NONE/GROUP/PERWRITE`) e ritorna il modo
/// precedente (pattern umask). `GROUP` esegue anche la barriera subito
/// (FLUSH dei mount FAT). `Err` a modo ignoto (stato invariato) o canale
/// senza diritto SYNC.
#[inline]
pub fn disk_sync<S: Sys>(st: &'static State<S>, mode: u32) -> Result<u64, Error> {
    let (result, _, _) = fs_notify(st, R_SYNC, mode as u64, 0, &[])?;
    // Solo risultato (modo precedente): consuma l'header 16 B come ogni
    // op senza payload (senza, la coda RESP slitta e l'op successiva
    // legge questo risultato stale — osservato in t32).
    ring::resp_ring_consume(16);
    session::fs_reply_check(result)
}

/// Fase 52 — spazio del mount di `path` (statvfs, zero kernel come `stat`).
/// `bsize` = byte per blocco; `blocks`/`bfree`/`bavail` in blocchi;
/// `bfree == bavail == u64::MAX` = illimitato (ramfs memory-backed).
#[derive(Clone, Copy, Debug)]
pub struct StatVfs {
    pub bsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
}

/// `statvfs(path)`: spazio senza aprire — relay `R_STATVFS` (payload path),
/// risposta self-written `[0:8][0:8]` + 32 B. `Err` su device/sintetici
/// (nessun device da contabilizzare) e mount inattivi.
#[inline]
pub fn statvfs<S: Sys>(st: &'static State<S>, path: &str, out: &mut StatVfs) -> Result<(), Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, _, _) = fs_notify(st, R_STATVFS, rp.len() as u64, 0, rp.as_ref())?;
    // Disciplina obj_get (test-6): check-first, payload solo a Ok.
    session::fs_reply_check(result).map_err(|e| {
        ring::resp_ring_consume(16);
        e
    })?;
    let mut buf = [0u8; 32];
    ring::resp_ring_read_payload(&mut buf, 32);
    out.bsize = u64::from_le_bytes(buf[..8].try_into().unwrap_or([0; 8]));
    out.blocks = u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8]));
    out.bfree = u64::from_le_bytes(buf[16..24].try_into().unwrap_or([0; 8]));
    out.bavail = u64::from_le_bytes(buf[24..32].try_into().unwrap_or([0; 8]));
    Ok(())
}

/// Fase 54 (P5 integrita') — `get_hash(path, out)`: BLAKE2s-256 del contenuto
/// via `R_GET_HASH` (compute-on-query: cardo rilegge e hasha, nessuno stato).
/// `out` = 32 byte. `Err` per device/path senza contenuto o mount inattivi.
#[inline]
pub fn get_hash<S: Sys>(st: &'static State<S>, path: &str, out: &mut [u8; 32]) -> Result<(), Error> {
    let rp = cwd::resolve::<S>(path)?;
    let (result, _, _) = fs_notify(st, R_GET_HASH, rp.len() as u64, 0, rp.as_ref())?;
    // Disciplina obj_get (test-6): check-first, payload solo a Ok.
    session::fs_reply_check(result).map_err(|e| {
        ring::resp_ring_consume(16);
        e
    })?;
    ring::resp_ring_read_payload(out, 32);
    Ok(())
}

/// Scrive un frame "source\0target\0" e lo notifica (helper di `mount`).
fn mount_frame(source: &str, target: &str) -> bool {
    // Path lunghi al massimo MAX_PATH (256) l'uno + 2 NUL.
    let total = source.len() + 1 + target.len() + 1;
    if total > ring::RING_MAX_PAYLOAD || total > 514 {
        return false;
    }
    let mut buf = [0u8; 520];
    buf[..source.len()].copy_from_slice(source.as_bytes());
    buf[source.len()] = 0;
    buf[source.len() + 1..source.len() + 1 + target.len()].copy_from_slice(target.as_bytes());
    buf[source.len() + 1 + target.len()] = 0;
    ring::req_ring_write(R_MOUNT, total as u64, 0, &buf[..total])
}

/// `mount(source, target)`: monta una sorgente a blocchi (es. "/dev/sda")
/// su un target (es. "/mnt", Fase 16b). Errori nativi in Fase 39 (dettaglio
/// in Fase 40).
#[inline]
pub fn mount<S: Sys>(st: &'static State<S>, source: &str, target: &str) -> Result<(), Error> {
    session::fs_gate(st)?;
    if !mount_frame(source, target) {
        return Err(Error::Invalid);
    }
    match session::fs_notify_result(st, FS_NOTIFY, || mount_frame(source, target)) {
        Some((result, _, _)) => {
            ring::resp_ring_consume(16);
            session::fs_reply_check(result).map(|_| ())
        }
        None => Err(Error::NotReady),
    }
}

/// `umount(target)`: smonta un target (Fase 16b). Rifiutato se ci sono fd
/// aperti sotto il target (Fase 39; `Busy` tipizzato in Fase 40).
#[inline]
pub fn umount<S: Sys>(st: &'static State<S>, target: &str) -> Result<(), Error> {
    let (result, _, _) = fs_notify(st, R_UMOUNT, target.len() as u64, 0, target.as_bytes())?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// `lseek(fd, off, whence)`: sposta l'offset di un fd LOCALE (Fase 40, P1).
/// `whence` = `SEEK_START`/`SEEK_CURRENT`/`SEEK_END`; `off` con segno
/// (negativo lecito verso SEEK_END/CURRENT, mai sotto zero). Solo Local:
/// su device remoti
/// il server risponde `Invalid` (l'offset vive in cardo). Ritorna il nuovo
/// offset. A rifiuto l'offset resta quello di prima (two-phase server-side).
#[inline]
pub fn lseek<S: Sys>(st: &'static State<S>, fd: i64, off: i64, whence: u64) -> Result<u64, Error> {
    let w = [whence as u8];
    let (result, _, _) = fs_notify(st, R_LSEEK, fd as u64, off as u64, &w)?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result)
}

/// `dup_grant(fd)`: registra un grant single-use per handoff al figlio
/// (Fase 40, modello B). Solo fd locali (Remote → `Invalid`). Ritorna il
/// nonce da passare al figlio (via memoria COW pre-fork, mai via IPC).
/// Il grant vive finche' il figlio lo riscuote, il parent lo cancella, o il
/// parent muore (purge server-side: mai grant orfani riusabili).
#[inline]
pub fn dup_grant<S: Sys>(st: &'static State<S>, fd: i64) -> Result<u64, Error> {
    let (result, _, _) = fs_notify(st, R_DUP_GRANT, fd as u64, 0, &[])?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result)
}

/// `dup_claim(nonce)`: riscuote un grant (Fase 40). Solo il FIGLIO del
/// registrante (doppia attestazione server-side: parentela + canale vivo).
/// Ritorna un fd indipendente sul PROPRIO canale, con offset copiato
/// (semantica handoff). Single-use: il grant viene consumato.
#[inline]
pub fn dup_claim<S: Sys>(st: &'static State<S>, nonce: u64) -> Result<i64, Error> {
    let n = nonce.to_le_bytes();
    let (result, _, _) = fs_notify(st, R_DUP_CLAIM, 0, 0, &n)?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|v| v as i64)
}

/// `dup_cancel(nonce)`: cancella un grant pendente (Fase 40, cleanup parent).
/// Best-effort idempotente server-side (sempre Ok, anche a nonce assente);
/// qui si mappa comunque la reply (morte server → `Err`, mai `Ok` bugiardo).
#[inline]
pub fn dup_cancel<S: Sys>(st: &'static State<S>, nonce: u64) -> Result<(), Error> {
    let n = nonce.to_le_bytes();
    let (result, _, _) = fs_notify(st, R_DUP_CANCEL, 0, 0, &n)?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// `pipe()` (Fase 42): crea una pipe nel fs server. Ritorna `(read_fd,
/// write_fd)`: due fd indipendenti sullo stesso buffer server-side.
/// Semantica bloccante nei wrapper (il server non dorme mai): `read_fs` su
/// vuota con writer aperti riprova throttled fino a dati/EOF; `write_fs`
/// oltre la capacita' completa col resto finche' i lettori drenano; senza
/// lettori → `Err(Closed)`. Gli fd si passano ai figli fork+exec con
/// `dup_grant`/`dup_claim` come i file (stessa capability).
#[inline]
pub fn pipe<S: Sys>(st: &'static State<S>) -> Result<(i64, i64), Error> {
    session::fs_gate(st)?;
    if !ring::req_ring_write(R_PIPE_CREATE, 0, 0, &[]) {
        return Err(Error::RingFull);
    }
    match session::fs_notify_result(st, FS_NOTIFY, || {
        ring::req_ring_write(R_PIPE_CREATE, 0, 0, &[])
    }) {
        // Risposta a due fd: result = lettura, w1 = scrittura (nessun
        // payload). Entrambi passano per `fs_reply_check`: una sentinella in
        // una delle due posizioni e' un rifiuto, mai un fd.
        Some((result, w1, _)) => {
            ring::resp_ring_consume(16);
            let r = session::fs_reply_check(result)?;
            let w = session::fs_reply_check(w1)?;
            Ok((r as i64, w as i64))
        }
        None => Err(Error::NotReady),
    }
}

/// `rights_drop(keep_mask, subtree)`: riduce i propri diritti sul canale
/// verso cardo (Fase 17, self-restriction only). Solo shrink: il server fa
/// AND con la mask corrente; il subtree puo' solo restringersi (widen =
/// `Err`, nessun cambio). `subtree=None` = solo-ops (Fase 39).
/// Irrevocabile per disegno (nessun GRANT: i canali non sono trasferibili).
#[inline]
pub fn rights_drop<S: Sys>(st: &'static State<S>, keep_mask: u32, subtree: Option<&str>) -> Result<(), Error> {
    let sub_bytes: &[u8] = match subtree {
        Some(s) => s.as_bytes(),
        None => &[],
    };
    if sub_bytes.len() > 256 {
        return Err(Error::Invalid);
    }
    let (result, _, _) = fs_notify(
        st,
        R_RIGHTS_DROP,
        keep_mask as u64,
        sub_bytes.len() as u64,
        sub_bytes,
    )?;
    ring::resp_ring_consume(16);
    session::fs_reply_check(result).map(|_| ())
}

/// `rights_get(buf)`: legge i propri diritti (Fase 17). Scrive il subtree
/// normalizzato + NUL in `buf` (root = solo NUL) e ritorna la mask ops
/// (0..=RIGHTS_ALL). Dimensionare `buf` ≥ 257 (Fase 39).
#[inline]
pub fn rights_get<S: Sys>(st: &'static State<S>, buf: &mut [u8]) -> Result<u32, Error> {
    if buf.is_empty() {
        return Err(Error::Invalid);
    }
    session::fs_gate(st)?;
    if !ring::req_ring_write(R_RIGHTS_GET, 0, 0, &[]) {
        return Err(Error::RingFull);
    }
    match session::fs_notify_result(st, FS_NOTIFY, || ring::req_ring_write(R_RIGHTS_GET, 0, 0, &[])) {
        Some((result, w1, payload_len)) => {
            // Stessa disciplina di `readdir`: consuma sempre, poi interpreta.
            let ops = session::fs_reply_check(result);
            // Leggi tutto il payload in uno stack buffer (subtree ≤ 256 dal
            // server): un solo consumo 16+len, mai disallineamenti.
            let mut tmp = [0u8; 256];
            let take = payload_len.min(256);
            if take > 0 {
                ring::resp_ring_read_payload(&mut tmp, take);
            } else {
                ring::resp_ring_consume(16);
            }
            let ops = ops?;
            let n = (w1 as usize).min(take).min(buf.len() - 1);
            buf[..n].copy_from_slice(&tmp[..n]);
            buf[n] = 0;
            Ok(ops as u32)
        }
        None => Err(Error::NotReady),
    }
}

/// Identità stabile di un volume FAT32 dal boot sector (Fase 16d):
/// `(seriale, label_raw_11B)`. Seriale solo con firma estesa `0x29` (layout
/// standard firma-a-66/volid-67-70, o variante mkfat firma-a-67/volid-68-71);
/// label sempre (11 byte raw, trim a carico del chiamante). `None` se il
/// settore non e' un BPB FAT valido (stessi check minimi di mount: 55AA,
/// bps 512, spc potenza di 2 non zero, almeno una FAT non vuota, root ≥ 2).
/// Usato sia dal parser (`cardo/fat32.rs`) che dallo sniff per-nodo del
/// driver (`block`): un nodo annuncia UUID/label sse monta davvero.
pub fn fat_bpb_identity(boot: &[u8; 512]) -> Option<(Option<u32>, [u8; 11])> {
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return None;
    }
    let bps = u16::from_le_bytes([boot[11], boot[12]]);
    let spc = boot[13];
    let num_fats = boot[16];
    let fat_size = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);
    let root = u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]]);
    if bps != 512 || spc == 0 || (spc & (spc - 1)) != 0 {
        return None;
    }
    if num_fats == 0 || fat_size == 0 || root < 2 {
        return None;
    }
    let vol_serial = if boot[66] == 0x29 {
        Some(u32::from_le_bytes([boot[67], boot[68], boot[69], boot[70]]))
    } else if boot[67] == 0x29 {
        Some(u32::from_le_bytes([boot[68], boot[69], boot[70], boot[71]]))
    } else {
        None
    };
    let mut vol_label = [0u8; 11];
    vol_label.copy_from_slice(&boot[71..82]);
    Some((vol_serial, vol_label))
}

/// `fs_register(prefix)`: un driver (devfs/console) registra il proprio prefix
/// di mount presso cardo (Fase 39: errore nativo; il chiamante puo' ritentare
/// se cardo non e' ancora pronto).
#[inline]
pub fn fs_register<S: Sys>(st: &'static State<S>, prefix: &[u8]) -> Result<(), Error> {
    fs_register_multi(st, &[prefix])
}

/// `fs_register_multi(prefixes)`: registra PIU' prefix con UNA SOLA IPC
/// sincrona (Fase 16d). Serve ai driver multi-nodo (devfs: `/dev/null` +
/// `/dev/zero`): due register sincroni consecutivi creerebbero un mount
/// forwardable dopo il primo, e se cardo in quel momento sta inoltrando una
/// richiesta al driver (single-threaded, `send` bloccante) si crea un
/// deadlock incrociato (driver→cardo register, cardo→driver forward).
/// Payload = prefix separati da NUL. `Ok` se TUTTI registrati (Fase 39).
pub fn fs_register_multi<S: Sys>(st: &'static State<S>, prefixes: &[&[u8]]) -> Result<(), Error> {
    session::fs_gate(st)?;
    let mut buf = [0u8; 520];
    let mut n = 0usize;
    for (i, p) in prefixes.iter().enumerate() {
        if i > 0 {
            if n + 1 > buf.len() {
                return Err(Error::Invalid);
            }
            buf[n] = 0;
            n += 1;
        }
        if n + p.len() > buf.len() {
            return Err(Error::Invalid);
        }
        buf[n..n + p.len()].copy_from_slice(p);
        n += p.len();
    }
    if n == 0 {
        return Err(Error::Invalid);
    }
    if !ring::req_ring_write(R_REGISTER, n as u64, 0, &buf[..n]) {
        return Err(Error::RingFull);
    }
    match session::fs_notify_result(st, FS_REGISTER, || {
        ring::req_ring_write(R_REGISTER, n as u64, 0, &buf[..n])
    }) {
        Some((result, _, _)) => {
            ring::resp_ring_consume(16);
            session::fs_reply_check(result).map(|_| ())
        }
        None => Err(Error::NotReady),
    }
}

/// Legge un file intero in heap (bound 256 KiB = SPAWN_IMAGE_MAX kernel).
/// None su qualunque errore (open/read/close) o file vuoto. Chunk da
/// RING_MAX_PAYLOAD: un round-trip per chunk (ogni round-trip puo' attendere
/// un quanto sotto carico: dimezzarli dimezza il tempo di load).
/// (A4: prima identico in init/usertests/usertest-client come
/// `load_file`/`load_bin`; la variante init accettava anche il file vuoto,
/// qui rifiutato — un .bin vuoto non e' mai valido e falliva loud comunque).
pub fn load_file<S: Sys>(st: &'static State<S>, path: &str) -> Option<alloc::vec::Vec<u8>> {
    let fd = open(st, path, 0).ok()?;
    let mut data = alloc::vec::Vec::new();
    let mut chunk = [0u8; ring::RING_MAX_PAYLOAD];
    loop {
        if data.len() >= 256 * 1024 {
            let _ = close(st, fd);
            return None; // troppo grosso: mai un binario valido
        }
        let n = match read_fs(st, fd, &mut chunk, ring::RING_MAX_PAYLOAD) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
    }
    let _ = close(st, fd);
    if data.is_empty() {
        return None;
    }
    Some(data)
}
