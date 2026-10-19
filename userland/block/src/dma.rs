//! Motore DMA Bus-Master PIIX (Fase 38.1c, attesa event-driven in 38.2).
//!
//! Trasferimenti `READ/WRITE DMA EXT` su staging contigua (1 pagina da
//! `SYS_DMA_ALLOC`: PRD a offset 0, dati a offset 64). Protocollo `DISK_*`
//! INVARIATO e cardo intoccato: il motore e' un'alternativa interna al PIO
//! per le stesse `(handle, lba, n)` — ogni errore degrada a PIO per-op.
//!
//! Attesa completamento = **event-driven** (38.2): `start_dma` arma il comando
//! e il server dorme in `recv` finche' la notify IRQ (ora recapitata: 38.0e)
//! segnala la fine; `finish_dma` chiude e copia. CPU libera per ~1,2 ms a
//! transfer invece di bruciarla in poll. Reso possibile dalla guardia 38.2a in
//! `pop_msg`: le notify kernel (canale 0) non toccano piu' la reply implicita,
//! quindi il `recv` tra richiesta e reply e' sicuro. Le risposte async FS
//! (`req_id < 0`) non la toccavano gia'.
//! Drain delle notify a testa-loop invariato: i re-fire level-triggered (EOI
//! prima del clear) lasciano stale in coda, e senza drain riempiono la coda
//! facendo scartare le send sync di cardo in silenzio (hang, provato in 38.1c).
//!
//! RISCHIO RESIDUO (accettato e documentato, 38.2): il `recv` non ha timeout —
//! un device che accetta il comando e non alza MAI intr appenderebbe (il poll
//! 38.1c degradava a PIO). Coperture: pre-flight (`wait_idle` + START ok),
//! re-fire level-triggered che auto-guarisce le notify perse per coda piena,
//! abort su EXIT del richiedente. Mai osservato su QEMU.
//!
//! Chiusura (sempre, anche a errore): STOP → lettura ATA status (spegne l'IRQ
//! del drive — PRIMA del clear BM, ordine del level-triggered) → clear
//! INTR/ERROR → check. Lo STOP+clear a init serve anche dopo kill+restart
//! (t32): la BAR/BM sopravvivono al processo.
//!
//! Il timeout su device impallato degrada a PIO (che fallisce a sua volta →
//! ERR all'utente, mai wedge): QEMU non impalla.

use super::*;
use civis::pio as io;

/// Offset registri Bus-Master dentro il canale (primario +0, secondario +8).
const BM_CMD_OFF: u16 = 0;
const BM_STATUS_OFF: u16 = 2;
const BM_PRD_OFF: u16 = 4;
/// Bit registro command: START/STOP + direzione (1 = read disco→mem).
const BM_CMD_START: u8 = 0x01;
const BM_CMD_RW: u8 = 0x08;
/// Bit registro status: ACTIVE (ro) + ERROR/INTR (write-1-to-clear).
const BM_ST_ACTIVE: u8 = 0x01;
const BM_ST_ERROR: u8 = 0x02;
const BM_ST_INTR: u8 = 0x04;
/// Bound poll pre-wait (stesso del PIO in `block.rs`: solo un freno).
const DMA_TIMEOUT: u32 = 2_000_000;
/// Staging: PRD (8 entry × 8 B) a offset 0, dati a offset 64 (allineato).
pub const PRD_MAX_ENTRIES: usize = 8;
const DATA_OFF: u64 = 64;
/// Log contatori ogni N trasferimenti (come la cache: throttled, mai spam).
const STAT_EVERY: u64 = 512;

/// Scrive le entry PRD per `(phys, len)` in `staging[0..64]`, splittando ai
/// confini 64K (il device non attraversa il confine in una entry). Layout
/// PIIX: `[base:4 LE][count:2 LE][0x00][EOT]`, EOT = 0x80 sull'ultima.
/// Ritorna il numero di entry o `None` (len 0 / oltre 7 settori / oltre il
/// 4G (PIIX a 32 bit) / oltre cap 8 — tutti fallback PIO dal chiamante).
fn build_prd(staging: *mut u8, phys: u64, len: usize) -> Option<usize> {
    if len == 0 || len > 7 * 512 {
        return None;
    }
    if phys + len as u64 > 0x1_0000_0000 {
        return None;
    }
    let mut entries = 0usize;
    let mut off = 0u64;
    let total = len as u64;
    while off < total {
        if entries >= PRD_MAX_ENTRIES {
            return None;
        }
        let base = phys + off;
        let to_boundary = 0x10000 - (base & 0xFFFF);
        let mut run = total - off;
        if run > to_boundary {
            run = to_boundary;
        }
        unsafe {
            let e = staging.add(entries * 8);
            core::ptr::write_unaligned(e as *mut u32, base as u32);
            core::ptr::write_unaligned(e.add(4) as *mut u16, run as u16);
            *e.add(6) = 0;
            *e.add(7) = 0; // EOT sotto, alzato sull'ultima
        }
        entries += 1;
        off += run;
    }
    unsafe {
        *staging.add((entries - 1) * 8 + 7) = 0x80;
    }
    Some(entries)
}

/// Motore DMA: BMIBA negoziata (38.0d) + staging contigua (38.1a) + modi per
/// disco (38.1b, parallelo a `disks` in `server.rs`: `None` = PIO).
pub struct DmaEngine {
    bmiba: u16,
    staging_va: u64,
    staging_phys: u64,
    dma_ok: u64,
    dma_fb: u64,
    /// Notify IRQ drenate a testa-loop (38.0e: ~1/transfer + re-fire).
    irq_drained: u64,
    /// Attese event-driven chiuse via IRQ (38.2) e via fast-path pre-check.
    ev_wait: u64,
    ev_fast: u64,
    /// Abort per morte richiedente (rari: stampa immediata, mai throttled).
    ev_abort: u64,
    /// Ultimo `ticks_used` proprio osservato (vedi `note`).
    cpu_last: u64,
}

impl DmaEngine {
    /// Inizializza il motore: `bmiba` da 38.0d (`None` = niente DMA),
    /// `modes` = modi negoziati per disco. Alloca 1 pagina staging e AZZERA
    /// il BM (STOP + clear su entrambi i canali: dopo kill+restart (t32) lo
    /// stato sopravvive al processo). `None` = PIO puro (data-plane intatto).
    pub fn init(bmiba: Option<u16>, modes: &[Option<u8>]) -> Option<DmaEngine> {
        let bmiba = bmiba?;
        if !modes.iter().any(|m| m.is_some()) {
            println!("[block] DMA: nessun disco con modo — resto in PIO");
            return None;
        }
        let phys = match civis::dma_alloc(1) {
            Ok(p) => p,
            Err(_) => {
                println!("[block] DMA: staging alloc fallita — resto in PIO");
                return None;
            }
        };
        for chan in [0u16, 8u16] {
            unsafe {
                let c = io::inb(bmiba + chan + BM_CMD_OFF);
                io::outb(bmiba + chan + BM_CMD_OFF, c & !BM_CMD_START);
                io::outb(bmiba + chan + BM_STATUS_OFF, BM_ST_ERROR | BM_ST_INTR);
            }
        }
        println!(
            "[block] DMA engine: bmiba={:#x} staging phys={:#x} (PRD+dati, 1 pagina)",
            bmiba, phys
        );
        Some(DmaEngine {
            bmiba,
            staging_va: civis::USER_DMA_VA,
            staging_phys: phys,
            dma_ok: 0,
            dma_fb: 0,
            irq_drained: 0,
            ev_wait: 0,
            ev_fast: 0,
            ev_abort: 0,
            cpu_last: 0,
        })
    }

    /// Contatori (prova d'uso reale in 38.3: `dma_ok > 0`, `dma_fb == 0`).
    /// `cpu` = tick CPU consumati da block (via `ps_info` su se stesso,
    /// 1 syscall ogni 512 xfers): la prova della CPU liberata dall'event-driven
    /// (38.2: poll bruciava ~1,2 ms/op a priorita' Normal, l'attesa dorme).
    fn note(&mut self, ok: bool) {
        if ok {
            self.dma_ok += 1;
        } else {
            self.dma_fb += 1;
        }
        let t = self.dma_ok + self.dma_fb;
        if t % STAT_EVERY == 0 {
            let cpu = civis::ps_info(civis::getpid() as u32).map_or(0, |e| e.ticks);
            let dcpu = cpu.saturating_sub(self.cpu_last);
            self.cpu_last = cpu;
            println!(
                "[block] DMA xfers: ok={} fb={} irq_drained={} ev_wait={} ev_fast={} ev_abort={} cpu={} (+{})",
                self.dma_ok,
                self.dma_fb,
                self.irq_drained,
                self.ev_wait,
                self.ev_fast,
                self.ev_abort,
                cpu,
                dcpu,
            );
        }
    }

    /// Conta una notify IRQ drenata a testa-loop (38.0e: ~1/transfer + re-fire
    /// level-triggered; il drain resta load-bearing contro il riempimento coda).
    pub fn note_irq_drained(&mut self) {
        self.irq_drained += 1;
    }

    /// Chiusura event-driven via IRQ (38.2): la notify e' arrivata e verificata.
    pub fn note_ev_wait(&mut self) {
        self.ev_wait += 1;
    }

    /// Chiusura via fast-path pre-check (INTR gia' settato prima del block).
    pub fn note_ev_fast(&mut self) {
        self.ev_fast += 1;
    }

    /// Abort: richiedente morto durante l'attesa (STOP/clear fatti, mai reply).
    /// Raro (morte cardo sotto carico): stampa subito, mai throttled.
    pub fn note_ev_abort(&mut self) {
        self.ev_abort += 1;
        println!("[block] DMA wait abort (richiedente morto): totale {}", self.ev_abort);
    }

    fn bm_cmd(&self, chan: u16) -> u16 {
        self.bmiba + chan + BM_CMD_OFF
    }

    fn bm_status(&self, chan: u16) -> u16 {
        self.bmiba + chan + BM_STATUS_OFF
    }

    /// True se il BM segnala fine (INTR) o errore (ERROR). ACTIVE da solo =
    /// ancora in corsa (non basta: a fine op il drive alza INTR). Pubblico per
    /// il wait event-driven del server (38.2): verifica a ogni wakeup.
    pub fn is_done(&self, chan: u16) -> bool {
        let st = unsafe { io::inb(self.bm_status(chan)) };
        st & (BM_ST_INTR | BM_ST_ERROR) != 0
    }

    /// Attende che il BM sia fermo (ACTIVE spento), bound come il PIO.
    fn wait_idle(&self, chan: u16) -> bool {
        for _ in 0..DMA_TIMEOUT {
            let st = unsafe { io::inb(self.bm_status(chan)) };
            if st & BM_ST_ACTIVE == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Ferma il BM sul canale (STOP; non tocca INTR/ERROR: li chiude `finish`).
    fn stop(&self, chan: u16) {
        unsafe {
            let c = io::inb(self.bm_cmd(chan));
            io::outb(self.bm_cmd(chan), c & !BM_CMD_START);
        }
    }

    /// Chiusura post-wait (sempre): STOP → lettura ATA status (spegne l'IRQ
    /// del drive — PRIMA del clear BM, ordine del level-triggered) → clear
    /// INTR/ERROR → check. `false` = errore ATA o BM (fallback PIO).
    fn finish(&self, disk: &super::block::AtaDisk, chan: u16) -> bool {
        // Cattura ERROR prima del clear (dopo leggerebbe 0).
        let bs = unsafe { io::inb(self.bm_status(chan)) };
        self.stop(chan);
        let ata = disk.task_status();
        unsafe {
            io::outb(self.bm_status(chan), BM_ST_ERROR | BM_ST_INTR);
        }
        if bs & BM_ST_ERROR != 0 {
            return false;
        }
        ata & 0x01 == 0
    }

    /// Abort d'emergenza (38.2, richiedente morto durante l'attesa): STOP +
    /// clear INTR/ERROR senza leggere l'ATA status (nessuno consumera' il
    /// risultato). Il canale torna idle per l'op successiva.
    pub fn abort(&self, chan: u16) {
        self.stop(chan);
        unsafe {
            io::outb(self.bm_status(chan), BM_ST_ERROR | BM_ST_INTR);
        }
    }

    /// Arma un trasferimento da `n` settori (1..=7) a `lba` fisico via DMA
    /// (38.2, prima meta' dello split di `transfer`): valida, costruisce il
    /// PRD, copia lo staging per le write, programma BM + taskfile e da' START.
    /// Ritorna false = fallback PIO dal chiamante (stesso contratto del PIO).
    /// Dopo `true` il chiamante DEVE chiudere con `finish_dma` (o `abort` se il
    /// richiedente muore): il comando e' in volo sul device.
    pub fn start_dma(
        &mut self,
        disk: &super::block::AtaDisk,
        chan: u16,
        lba: u64,
        n: usize,
        buf: &mut [u8],
        write: bool,
    ) -> bool {
        if n == 0 || n > 7 || buf.len() < n * 512 || !disk.lba48 {
            self.note(false);
            return false;
        }
        let data_phys = self.staging_phys + DATA_OFF;
        let staging = self.staging_va as *mut u8;
        if build_prd(staging, data_phys, n * 512).is_none() {
            self.note(false);
            return false;
        }
        if write {
            unsafe {
                core::ptr::copy_nonoverlapping(buf.as_ptr(), staging.add(DATA_OFF as usize), n * 512);
            }
        }
        if !self.wait_idle(chan) {
            self.note(false);
            return false;
        }
        unsafe {
            io::outl(self.bmiba + chan + BM_PRD_OFF, self.staging_phys as u32);
            io::outb(self.bm_status(chan), BM_ST_ERROR | BM_ST_INTR);
        }
        if !disk.start_dma_ext(lba, n as u8, write) {
            self.note(false);
            return false;
        }
        unsafe {
            let c = io::inb(self.bm_cmd(chan));
            io::outb(
                self.bm_cmd(chan),
                (c & !BM_CMD_START) | BM_CMD_START | if write { 0 } else { BM_CMD_RW },
            );
        }
        true
    }

    /// Chiude un trasferimento armato con `start_dma` (38.2, seconda meta'):
    /// `finish` (STOP → ATA status → clear → check) + flush per le write +
    /// copia staging→buf per le read. Stesso `note(ok)` di `transfer`.
    pub fn finish_dma(
        &mut self,
        disk: &super::block::AtaDisk,
        chan: u16,
        buf: &mut [u8],
        n: usize,
        write: bool,
    ) -> bool {
        let staging = self.staging_va as *mut u8;
        let ok = self.finish(disk, chan);
        if ok && write && !disk.flush_write_cache() {
            self.note(false);
            return false;
        }
        if ok && !write {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    staging.add(DATA_OFF as usize),
                    buf.as_mut_ptr(),
                    n * 512,
                );
            }
        }
        self.note(ok);
        ok
    }
}
