//! Client disco via IPC verso `block` (Fase 16, resolve Fase 16c).
//!
//! Implementa `BlockSource` per il parser FAT32 sopra il protocollo `DISK_*`
//! su canale diretto (service_lookup(Disk)): `HELLO` per handshake (i fisici
//! viaggiano nella reply: w0 = req_phys, w1 = resp_phys), poi `READ` settoriali
//! con `send` sincrona e frame risposta letto dalla finestra mappata.
//!
//! Risoluzione nomi (16c): block e' la single source of truth della mappa
//! nome→handle. `resolve(name)` scrive un frame `[namelen:8][name]` nel
//! DISK_REQ ring (mappato a `DISK_REQ_VA`, stessa VA di block: page table
//! per-processo, nessun conflitto) e manda `DISK_RESOLVE`; l'handle torna in
//! w0 di reply (ERR = sconosciuto). cardo non indovina piu' nulla dal nome.
//!
//! Riconnessione (init-restart): il canale e' invalidato alla morte di
//! block (`note_peer_death` su EXIT_NOTIFY, o send fallita) e il prossimo
//! read/resolve rifa' lookup + HELLO + remap (bound, mai wedge). Il remap
//! riallinea entrambi i ring: il contenuto appartiene all'epoca morta e l'op
//! e' ritentata dal chiamante. `block` non richiama mai `cardo`: le send
//! sincrone non creano cicli (stesso argomento dei relay verso devfs/console).
//!
//! Single-threaded per costruzione (cardo e' monolitico): `Cell` basta, mai
//! rientranza (la send blocca senza eseguire altro codice).

use core::cell::Cell;

use crate::fat32::BlockSource;
use civis::println;
/// Tag DISK_* (single source in `syscall-numbers`, Fase 16c): handshake,
/// validazione nodo, lettura settoriale, resolve nome→handle di proprieta'
/// del driver.
use civis::{DISK_HELLO, DISK_OPEN, DISK_READ, DISK_RESOLVE, DISK_WRITE};
/// Topologia P2 (Fase 51): LIST/INFO (single source in `syscall-numbers`).
use civis::{DISK_INFO, DISK_LIST, DISK_FLUSH};

/// Finestra del request ring di block (stessa VA del server: ogni processo
/// ha le proprie page table, nessun conflitto). cardo e' l'unico writer.
const DISK_REQ_VA: u64 = 0x0000_4000_0024_0000;
/// Finestra del response ring di block (stessa VA del server: ogni processo
/// ha le proprie page table, nessun conflitto).
const DISK_RESP_VA: u64 = 0x0000_4000_0025_0000;
/// Bound nomi di resolve (deve combaciare con `DISK_MAX_NAME` di block).
const DISK_MAX_NAME: usize = 16;
// Geometria ring + errore IPC (A1): single source in `civis`.
use civis::{ERR, RING_DATA_CAP, RING_HEAD, RING_TAIL};
/// Bound attesa block a boot/restart (~5 s, come `wait_ready` di init).
const HELLO_BOUND_TICKS: i64 = 500;
/// Bound drenaggio ring cardo→block per op (~1 s): oltre = guasto vero,
/// fail loud (mai wedge del FS). Sotto: backpressure normale sotto burst
/// (a velocita' W2 il block non drena sempre in tempo: senza retry
/// l'op falliva transitoria — write Err osservato 1/128 append).
const DISK_RING_BOUND_TICKS: i64 = 100;

    /// Throttle breve: un quanto di spin (il peer drena in us-ms; il bound
    /// lo conta il chiamante in tick).
    fn ring_wait() {
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }
    }
/// 24.2 — settori max per IPC DISK (bound del ring: 8 + 7*512 = 3592 nella
/// request, 16 + 7*512 = 3600 nella response, entrambi < 4087).
const DISK_MAX_SECTORS: usize = 7;

/// Dettaglio disco da DISK_INFO (Fase 51, P2): settori/flags (layout in
/// `syscall-numbers`) + stringhe IDENTIFY a lunghezza esplicita.
pub struct IpcDiskInfo {
    pub sectors: u64,
    pub flags: u64,
    pub model: [u8; 40],
    pub model_len: usize,
    pub serial: [u8; 20],
    pub serial_len: usize,
}

pub struct IpcDisk {
    /// Handle nodo di mount codificato (disco<<16|sub): 0 = sda whole-disk.
    handle: u32,
    /// Canale diretto verso block (None = da riconnettere).
    chan: Cell<Option<u64>>,
    /// Nodo validato con DISK_OPEN sulla connessione corrente (Fase 21: prima
    /// si faceva OPEN a OGNI settore — 2 round-trip per settore invece di 1.
    /// L'OPEN ora e' una tantum per connessione: la tabella nodi del driver e'
    /// statica a driver vivo, e a morte driver il canale cade (qui sotto) e
    /// la riconnessione rivalida. Mai stale silenzioso.)
    open_ok: Cell<bool>,
}

impl IpcDisk {
    pub fn new(handle: u32) -> Self {
        Self { handle, chan: Cell::new(None), open_ok: Cell::new(false) }
    }

    /// Dimentica la connessione (canale caduto o epoca cambiata): il prossimo
    /// uso riconnette e rivalida (OPEN) da zero.
    fn drop_conn(&self) {
        self.chan.set(None);
        self.open_ok.set(false);
    }

    /// Segnala la morte di un peer (EXIT_NOTIFY): se e' block, invalida il
    /// canale — il prossimo read/resolve riconnette. Ritorna true se eravamo
    /// connessi (cambio d'epoca: il chiamante cardo droppa le istanze FAT
    /// attive, gli handle possono cambiare — re-resolve per nome al prossimo
    /// accesso, Fase 16c).
    pub fn note_peer_death(&self, dead_chan: u64) -> bool {
        if self.chan.get() == Some(dead_chan) {
            self.drop_conn();
            true
        } else {
            false
        }
    }

    /// Legge un response frame DISK dalla finestra mappata (consumer SPSC:
    /// si legge a `tail`, il producer avanza `head`) e ne copia `expect` byte
    /// in `out`. 24.2 — l'header porta la lunghezza (`len == expect`, prima
    /// era fissa a 512): resync difensivo come prima a mismatch.
    unsafe fn frame_read(out: &mut [u8], expect: usize) -> bool {
        if out.len() < expect {
            return false;
        }
        unsafe {
            let head = core::ptr::read_volatile((DISK_RESP_VA + RING_HEAD as u64) as *const u32);
            let tail = core::ptr::read_volatile((DISK_RESP_VA + RING_TAIL as u64) as *const u32);
            if head == tail {
                return false;
            }
            let base = DISK_RESP_VA as *const u8;
            let t = tail as usize;
            let mut hdr = [0u8; 16];
            for i in 0..16 {
                hdr[i] = core::ptr::read_volatile(base.add((t + i) % RING_DATA_CAP));
            }
            let res = u64::from_le_bytes(hdr[0..8].try_into().unwrap_or([0xFF; 8]));
            if res != expect as u64 {
                core::ptr::write_volatile(
                    (DISK_RESP_VA + RING_TAIL as u64) as *mut u32,
                    head,
                );
                return false;
            }
            for i in 0..expect {
                out[i] = core::ptr::read_volatile(base.add((t + 16 + i) % RING_DATA_CAP));
            }
            let new_tail = (t + 16 + expect) % RING_DATA_CAP;
            core::ptr::write_volatile((DISK_RESP_VA + RING_TAIL as u64) as *mut u32, new_tail as u32);
            true
        }
    }

    /// Connessione (lookup + HELLO + map di ENTRAMBI i ring, bound, mai wedge).
    /// Fast path: una Cell-lettura. NON valida il nodo (serve a `resolve`,
    /// che l'handle non ce l'ha ancora). Epoca fresca: azzera il DISK_REQ
    /// (unico writer: niente in volo) e riallinea il DISK_RESP (tail=head).
    fn connect(&self) -> Option<u64> {
        if let Some(c) = self.chan.get() {
            return Some(c);
        }
        let t0 = civis::get_ticks();
        loop {
            if let Ok(c) = civis::service_lookup(civis::Service::Block) {
                let cu = c as u64;
                let ok = match civis::send(cu, DISK_HELLO, 0, 0) {
                    Ok(rep) if rep.w0 != ERR => {
                        if civis::map_physical(rep.w0, DISK_REQ_VA, 1).is_err()
                            || civis::map_physical(rep.w1, DISK_RESP_VA, 1).is_err()
                        {
                            false
                        } else {
                            // Epoca fresca: scarta l'epoca morta (l'op e' ritentata).
                            unsafe {
                                core::ptr::write_volatile(
                                    (DISK_REQ_VA + RING_HEAD as u64) as *mut u32,
                                    0,
                                );
                                core::ptr::write_volatile(
                                    (DISK_REQ_VA + RING_TAIL as u64) as *mut u32,
                                    0,
                                );
                                let head = core::ptr::read_volatile(
                                    (DISK_RESP_VA + RING_HEAD as u64) as *const u32,
                                );
                                core::ptr::write_volatile(
                                    (DISK_RESP_VA + RING_TAIL as u64) as *mut u32,
                                    head,
                                );
                            }
                            true
                        }
                    }
                    _ => false,
                };
                if ok {
                    println!("[cardo] block connesso (chan {})", cu);
                    self.chan.set(Some(cu));
                    return Some(cu);
                }
                // Trovato ma HELLO fallito (restart in corso?): riprova.
            }
            if civis::get_ticks() - t0 > HELLO_BOUND_TICKS {
                return None;
            }
            for _ in 0..100_000 {
                core::hint::spin_loop();
            }
        }
    }

    /// Assicura connessione + nodo validato (lookup + HELLO + OPEN, bound).
    /// Fast path: canale cachato e nodo gia' validato (una Cell-lettura: niente
    /// IPC). L'OPEN e' deterministico (`locate` su tabelle statiche): un solo
    /// tentativo per connessione — fallisce solo a handle stale (re-resolve
    /// del chiamante) o morte del driver durante la send (una riconnessione e
    /// un retry, come i read).
    fn ensure(&self) -> Option<u64> {
        let cu = self.connect()?;
        if self.open_ok.get() {
            return Some(cu);
        }
        match civis::send(cu, DISK_OPEN, self.handle as u64, 0) {
            Ok(rep) if rep.w0 != ERR => {
                self.open_ok.set(true);
                Some(cu)
            }
            Ok(_) => None, // handle stale: il chiamante re-risolve per nome
            Err(_) => {
                self.drop_conn();
                let cu = self.connect()?;
                match civis::send(cu, DISK_OPEN, self.handle as u64, 0) {
                    Ok(rep) if rep.w0 != ERR => {
                        self.open_ok.set(true);
                        Some(cu)
                    }
                    _ => None,
                }
            }
        }
    }

    /// Un tentativo di lettura multi (nessun retry qui: lo fa il chiamante).
    /// 24.2 — frame di richiesta `[count:8]` (1..=7), risposta con `n*512`
    /// byte: 1 IPC invece di n. Ring pieno = backpressure (retry throttled
    /// fino al bound, poi loud); reply ERR = fallimento vero.
    fn try_read_multi(&self, chan: u64, lba: u64, n: usize, out: &mut [u8]) -> bool {
        if n == 0 || n > DISK_MAX_SECTORS || out.len() < n * 512 {
            return false;
        }
        let t0 = civis::get_ticks();
        while !unsafe { Self::req_write_count(n) } {
            if civis::get_ticks() - t0 > DISK_RING_BOUND_TICKS {
                return false;
            }
            ring_wait();
        }
        match civis::send(chan, DISK_READ, self.handle as u64, lba) {
            Ok(rep) => {
                if rep.w0 == ERR {
                    return false;
                }
                let ok = unsafe { Self::frame_read(out, n * 512) };
                if !ok {
                }
                ok
            }
            Err(_) => {
                // block morto durante la send: invalida, il chiamante ritenta.
                self.drop_conn();
                false
            }
        }
    }

    /// Scrive un frame di richiesta read `[count:8]` nel DISK_REQ ring
    /// (24.2). Ritorna false se non c'e' spazio (disciplina sync + reset a
    /// ogni connessione: non dovrebbe mai accadere).
    unsafe fn req_write_count(n: usize) -> bool {
        unsafe {
            let head = core::ptr::read_volatile((DISK_REQ_VA + RING_HEAD as u64) as *const u32);
            let tail = core::ptr::read_volatile((DISK_REQ_VA + RING_TAIL as u64) as *const u32);
            let used = (head.wrapping_sub(tail)) % RING_DATA_CAP as u32;
            if RING_DATA_CAP as u32 - used < (8 + 1) as u32 {
                return false;
            }
            let dst = DISK_REQ_VA as *mut u8;
            let count_b = (n as u64).to_le_bytes();
            for (i, byte) in count_b.iter().enumerate() {
                core::ptr::write_volatile(dst.add(((head as usize) + i) % RING_DATA_CAP), *byte);
            }
            let new_head = ((head as usize) + 8) % RING_DATA_CAP;
            core::ptr::write_volatile((DISK_REQ_VA + RING_HEAD as u64) as *mut u32, new_head as u32);
            true
        }
    }

    /// Scrive un frame di write `[count:8][settori]` nel DISK_REQ ring
    /// (24.2, generalizza il vecchio `[512:8][settore]`). Ritorna false se
    /// non c'e' spazio o count fuori bound.
    unsafe fn req_write_sectors(n: usize, data: &[u8]) -> bool {
        if n == 0 || n > DISK_MAX_SECTORS || data.len() < n * 512 {
            return false;
        }
        unsafe {
            let head = core::ptr::read_volatile((DISK_REQ_VA + RING_HEAD as u64) as *const u32);
            let tail = core::ptr::read_volatile((DISK_REQ_VA + RING_TAIL as u64) as *const u32);
            let used = (head.wrapping_sub(tail)) % RING_DATA_CAP as u32;
            if RING_DATA_CAP as u32 - used < (8 + n * 512 + 1) as u32 {
                return false;
            }
            let dst = DISK_REQ_VA as *mut u8;
            let count_b = (n as u64).to_le_bytes();
            for (i, byte) in count_b.iter().enumerate() {
                core::ptr::write_volatile(dst.add(((head as usize) + i) % RING_DATA_CAP), *byte);
            }
            for (i, byte) in data[..n * 512].iter().enumerate() {
                core::ptr::write_volatile(dst.add(((head as usize) + 8 + i) % RING_DATA_CAP), *byte);
            }
            let new_head = ((head as usize) + 8 + n * 512) % RING_DATA_CAP;
            core::ptr::write_volatile((DISK_REQ_VA + RING_HEAD as u64) as *mut u32, new_head as u32);
            true
        }
    }

    /// Scrittura multi (24.2, retry throttled qui): ring pieno = backpressure
    /// (il peer su canale vivo drena sempre: riprova fino al bound, poi
    /// loud). Reply ERR = fallimento vero (niente retry). Send fallita =
    /// driver morto: invalida.
    fn try_write_multi(&self, chan: u64, lba: u64, n: usize, data: &[u8]) -> bool {
        let t0 = civis::get_ticks();
        while !unsafe { Self::req_write_sectors(n, data) } {
            if civis::get_ticks() - t0 > DISK_RING_BOUND_TICKS {
                return false;
            }
            ring_wait();
        }
        match civis::send(chan, DISK_WRITE, self.handle as u64, lba) {
            Ok(rep) => rep.w0 != ERR,
            Err(_) => {
                self.drop_conn();
                false
            }
        }
    }

    /// Scrive un frame di resolve `[namelen:8][name]` nel DISK_REQ ring.
    /// Ritorna false se non c'e' spazio (disciplina sync + reset a ogni
    /// connessione: non dovrebbe mai accadere; il chiamante fallisce loud).
    unsafe fn req_write_name(name: &str) -> bool {
        let bytes = name.as_bytes();
        let frame_len = 8 + bytes.len();
        unsafe {
            let head = core::ptr::read_volatile((DISK_REQ_VA + RING_HEAD as u64) as *const u32);
            let tail = core::ptr::read_volatile((DISK_REQ_VA + RING_TAIL as u64) as *const u32);
            let used = (head.wrapping_sub(tail)) % RING_DATA_CAP as u32;
            if RING_DATA_CAP as u32 - used < frame_len as u32 + 1 {
                return false;
            }
            let dst = DISK_REQ_VA as *mut u8;
            let len_b = (bytes.len() as u64).to_le_bytes();
            for (i, byte) in len_b.iter().enumerate() {
                core::ptr::write_volatile(dst.add(((head as usize) + i) % RING_DATA_CAP), *byte);
            }
            for (i, byte) in bytes.iter().enumerate() {
                core::ptr::write_volatile(dst.add(((head as usize) + 8 + i) % RING_DATA_CAP), *byte);
            }
            let new_head = ((head as usize) + frame_len) % RING_DATA_CAP;
            core::ptr::write_volatile((DISK_REQ_VA + RING_HEAD as u64) as *mut u32, new_head as u32);
            true
        }
    }

    /// Un tentativo di resolve (nessun retry qui: lo fa il chiamante).
    /// Reply w0 = handle (0 e' valido: sda whole-disk), ERR = nome sconosciuto
    /// (canale intatto: niente retry). Send fallita = driver morto: invalida.
    fn try_resolve(&self, chan: u64, name: &str) -> Option<u32> {
        if !unsafe { Self::req_write_name(name) } {
            return None;
        }
        match civis::send(chan, DISK_RESOLVE, 0, 0) {
            Ok(rep) => {
                if rep.w0 == ERR {
                    None
                } else {
                    Some(rep.w0 as u32)
                }
            }
            Err(_) => {
                self.drop_conn();
                None
            }
        }
    }

    /// Barriera write-cache del drive (Fase 52, P3): FLUSH CACHE sul disco
    /// del proprio handle (vale la parte disco, sub ignorata). Niente OPEN
    /// né frame (info di connessione, non di nodo). Stessa disciplina di
    /// `resolve`: un retry solo a canale caduto, mai su risposta ERR.
    pub fn flush_cache(&self) -> bool {
        let chan = match self.connect() {
            Some(c) => c,
            None => return false,
        };
        if self.try_flush(chan, self.handle) {
            return true;
        }
        if self.chan.get().is_some() {
            return false;
        }
        let chan = match self.connect() {
            Some(c) => c,
            None => return false,
        };
        self.try_flush(chan, self.handle)
    }

    /// Un tentativo di FLUSH (nessun retry qui: lo fa il chiamante).
    fn try_flush(&self, chan: u64, handle: u32) -> bool {
        match civis::send(chan, DISK_FLUSH, handle as u64, 0) {
            Ok(rep) => rep.w0 != ERR,
            Err(_) => {
                self.drop_conn();
                false
            }
        }
    }

    /// Dettaglio disco (Fase 51, P2): settori/flags + modello/seriale per
    /// l'handle `handle` (vale la parte disco, sub ignorata). Niente OPEN
    /// (info di connessione, non di nodo). Stessa disciplina di `resolve`:
    /// un retry solo a canale caduto, mai su risposta ERR.
    pub fn info(&self, handle: u32) -> Option<IpcDiskInfo> {
        let chan = self.connect()?;
        if let Some(i) = self.try_info(chan, handle) {
            return Some(i);
        }
        if self.chan.get().is_some() {
            return None;
        }
        let chan = self.connect()?;
        self.try_info(chan, handle)
    }

    /// Un tentativo di INFO (nessun retry qui: lo fa il chiamante).
    /// Frame RESP fisso 76 B `[model_len:8][model:40][serial_len:8]`
    /// `[serial:20]`; settori/flags in w0/w1 di reply.
    fn try_info(&self, chan: u64, handle: u32) -> Option<IpcDiskInfo> {
        let rep = match civis::send(chan, DISK_INFO, handle as u64, 0) {
            Ok(r) => r,
            Err(_) => {
                self.drop_conn();
                return None;
            }
        };
        if rep.w0 == ERR {
            return None;
        }
        let mut buf = [0u8; 76];
        if !unsafe { Self::frame_read(&mut buf, 76) } {
            return None;
        }
        let ml = u64::from_le_bytes(buf[..8].try_into().unwrap_or([0xFF; 8])) as usize;
        let sl = u64::from_le_bytes(buf[48..56].try_into().unwrap_or([0xFF; 8])) as usize;
        if ml > 40 || sl > 20 {
            return None;
        }
        let mut model = [0u8; 40];
        model[..ml].copy_from_slice(&buf[8..8 + ml]);
        let mut serial = [0u8; 20];
        serial[..sl].copy_from_slice(&buf[56..56 + sl]);
        Some(IpcDiskInfo {
            sectors: rep.w0,
            flags: rep.w1,
            model,
            model_len: ml,
            serial,
            serial_len: sl,
        })
    }

    /// Topologia dischi (Fase 51, P2): `(settori, flags)` per disco
    /// (sda=0, ...). Bound 16 come il server (oltre: il count in reply resta
    /// vero ma il frame porta solo i primi 16 — qui si ritorna il frame).
    pub fn list(&self) -> Option<alloc::vec::Vec<(u64, u64)>> {
        let chan = self.connect()?;
        if let Some(v) = self.try_list(chan) {
            return Some(v);
        }
        if self.chan.get().is_some() {
            return None;
        }
        let chan = self.connect()?;
        self.try_list(chan)
    }

    /// Un tentativo di LIST (nessun retry qui: lo fa il chiamante).
    /// Reply w0 = count; frame RESP con entry 16 B `[sectors:8][flags:8]`.
    fn try_list(&self, chan: u64) -> Option<alloc::vec::Vec<(u64, u64)>> {
        let rep = match civis::send(chan, DISK_LIST, 0, 0) {
            Ok(r) => r,
            Err(_) => {
                self.drop_conn();
                return None;
            }
        };
        if rep.w0 == ERR {
            return None;
        }
        let n = (rep.w0 as usize).min(16);
        let mut buf = [0u8; 16 * 16];
        if !unsafe { Self::frame_read(&mut buf, n * 16) } {
            return None;
        }
        let mut out = alloc::vec::Vec::new();
        for k in 0..n {
            let s = u64::from_le_bytes(buf[k * 16..k * 16 + 8].try_into().unwrap_or([0; 8]));
            let f = u64::from_le_bytes(buf[k * 16 + 8..k * 16 + 16].try_into().unwrap_or([0; 8]));
            out.push((s, f));
        }
        Some(out)
    }

    /// Risolve un nome nodo corto ("sda", "sda1") in handle presso block
    /// (Fase 16c: single source of truth nel driver). Bound, mai wedge.
    /// Solo se il canale e' caduto (non su nome sconosciuto): riconnetti e
    /// ritenta UNA volta. Usata dai mount (l'istanza e' usa-e-getta: l'handle
    /// per l'I/O vive poi nel `FsMount` + `IpcDisk` dedicati).
    pub fn resolve(&self, name: &str) -> Option<u32> {
        if name.is_empty() || name.len() > DISK_MAX_NAME {
            return None;
        }
        let chan = self.connect()?;
        if let Some(h) = self.try_resolve(chan, name) {
            return Some(h);
        }
        // Nome sconosciuto (canale intatto): errore legittimo, niente retry.
        if self.chan.get().is_some() {
            return None;
        }
        let chan = self.connect()?;
        self.try_resolve(chan, name)
    }
}

impl BlockSource for IpcDisk {
    fn read_sector(&self, lba: u64, buf: &mut [u8; 512]) -> bool {
        self.read_sectors(lba, 1, buf)
    }

    /// Scrive un settore via DISK_WRITE (Fase 20): stessa disciplina del read
    /// (un retry solo a canale caduto, mai su errore IO vero).
    fn write_sector(&self, lba: u64, data: &[u8; 512]) -> bool {
        self.write_sectors(lba, 1, data)
    }

    /// 24.2 — run di `n` settori in chunk da ≤7 IPC (stessa disciplina del
    /// singolo: un retry solo a canale caduto). `out` lungo almeno `n*512`.
    fn read_sectors(&self, lba: u64, n: usize, out: &mut [u8]) -> bool {
        if out.len() < n * 512 {
            return false;
        }
        let mut done = 0usize;
        while done < n {
            let k = (n - done).min(DISK_MAX_SECTORS);
            let chan = match self.ensure() {
                Some(c) => c,
                None => return false,
            };
            let ok = self.try_read_multi(chan, lba + done as u64, k, &mut out[done * 512..(done + k) * 512]);
            if ok {
                done += k;
                continue;
            }
            // Solo se il canale e' caduto (non su errore IO vero): riconnetti
            // e ritenta UNA volta. La send fallita ha gia' invalidato il canale.
            if self.chan.get().is_some() {
                return false;
            }
            let chan = match self.ensure() {
                Some(c) => c,
                None => return false,
            };
            if !self.try_read_multi(chan, lba + done as u64, k, &mut out[done * 512..(done + k) * 512]) {
                return false;
            }
            done += k;
        }
        true
    }

    /// 24.2 — come `read_sectors` in scrittura (1 comando PIO + 1 flush per
    /// chunk nel driver).
    fn write_sectors(&self, lba: u64, n: usize, data: &[u8]) -> bool {
        if data.len() < n * 512 {
            return false;
        }
        let mut done = 0usize;
        while done < n {
            let k = (n - done).min(DISK_MAX_SECTORS);
            let chan = match self.ensure() {
                Some(c) => c,
                None => return false,
            };
            let ok = self.try_write_multi(chan, lba + done as u64, k, &data[done * 512..(done + k) * 512]);
            if ok {
                done += k;
                continue;
            }
            if self.chan.get().is_some() {
                return false;
            }
            let chan = match self.ensure() {
                Some(c) => c,
                None => return false,
            };
            if !self.try_write_multi(chan, lba + done as u64, k, &data[done * 512..(done + k) * 512]) {
                return false;
            }
            done += k;
        }
        true
    }
}
