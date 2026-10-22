use super::*;

/// Scrive `count` byte da `buf` sulla maniglia console `con`
/// (`CONSOLE_OUT`/`CONSOLE_ERR`, non fd POSIX). Ritorna i byte scritti,
/// oppure un valore negativo in caso di errore (maniglia sconosciuta).
#[inline]
pub fn write(con: u64, buf: *const u8, count: usize) -> i64 {
    unsafe { syscall4(SYS_WRITE, con, buf as u64, count as u64, 0) }
}

/// Id del processo corrente.
#[inline]
pub fn getpid() -> i64 {
    unsafe { syscall4(SYS_GETPID, 0, 0, 0, 0) }
}

/// Numero di tick PIT trascorsi dall'avvio (100 Hz).
#[inline]
pub fn get_ticks() -> i64 {
    unsafe { syscall4(SYS_GET_TICKS, 0, 0, 0, 0) }
}

/// `map_mmio(phys, virt, count)`: mappa `count` pagine di MMIO device (BAR)
/// UC nel VA del chiamante (L0b, virtio-gpu). Grant ristretto all'owner di
/// `Service::Gpu` e alla finestra `DEV_MMIO_BASE`/`USER_MMIO_VA` (ADR-0057).
#[inline]
pub fn map_mmio(phys: u64, virt: u64, count: usize) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_MAP_MMIO, phys, virt, count as u64, 0) };
    if r < 0 {
        return Err(Error::Denied);
    }
    Ok(())
}

/// Attende `n` tick con batch di spin puri tra due `get_ticks` (pattern
/// robusto scheduler, A4: prima identico in init/usertests). Una `get_ticks`
/// per iterazione maschera IF=0 a ogni syscall e brucia quanti che rallentano
/// gli handoff IPC altrui; 512 spin puri lasciano IF=1 quasi tutto il tempo.
pub fn spin_ticks(n: i64) {
    let t0 = get_ticks();
    while get_ticks() - t0 < n {
        for _ in 0..512 {
            core::hint::spin_loop();
        }
    }
}

/// `sbrk(inc)`: estende l'heap del processo di `inc` byte (arrotondati a
/// pagina dal kernel; nessuna pagina mappata subito, materializzazione lazy
/// al primo accesso). Ritorna il vecchio `heap_brk` (inizio della nuova
/// regione), oppure `Err` se l'estensione non e' possibile.
#[inline]
pub fn sbrk(inc: usize) -> Result<usize, Error> {
    let r = unsafe { syscall4(SYS_SBRK, inc as u64, 0, 0, 0) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as usize)
    }
}

/// Fase 28 — `mmap(hint, len)`: mappa anonima privata RW nel basso canonico
/// (zero-fill lazy come `sbrk`). `hint == 0` = scelta kernel (first-fit dal
/// basso); altrimenti e' un consiglio onorato solo se libero. Ritorna la base
/// (sempre < 2^63) o `Err`. Solo RW in 28 (il kernel rifiuta altri `prot`).
#[inline]
pub fn mmap(hint: usize, len: usize) -> Result<usize, Error> {
    let r = unsafe { syscall4(SYS_MMAP, hint as u64, len as u64, PROT_READ | PROT_WRITE, 0) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as usize)
    }
}

/// Fase 28 — `mmap_fixed(addr, len)`: come `mmap` ma piazza esattamente ad
/// `addr` (`MMAP_FIXED`) o fallisce, mai fallback. Utile per riuso
/// deterministico dopo `munmap`.
#[inline]
pub fn mmap_fixed(addr: usize, len: usize) -> Result<usize, Error> {
    let r = unsafe { syscall4(SYS_MMAP, addr as u64, len as u64, PROT_READ | PROT_WRITE, MMAP_FIXED) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as usize)
    }
}

/// Fase 28 — `munmap(addr, len)`: smappa VMA intere (parziali = `Err` senza
/// cambiare stato, niente split in 28).
#[inline]
pub fn munmap(addr: usize, len: usize) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_MUNMAP, addr as u64, len as u64, 0, 0) };
    if r < 0 {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}

/// Fase 29 — `mmap_prot(hint, len, prot)`: come `mmap` ma con `prot` esplicito
/// (`PROT_NONE`/`PROT_READ`/`PROT_READ|PROT_WRITE`; altri = `Err`). Le pagine
/// vengono materializzate al primo accesso con i flag del prot (RO = scrittura
/// → fault → kill del processo).
#[inline]
pub fn mmap_prot(hint: usize, len: usize, prot: u64) -> Result<usize, Error> {
    let r = unsafe { syscall4(SYS_MMAP, hint as u64, len as u64, prot, 0) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as usize)
    }
}

/// Fase 29 — `mprotect(addr, len, prot)`: cambia le protezioni di VMA intere
/// (copertura esatta come `munmap`; parziali = `Err` senza cambiare stato).
/// A `PROT_NONE` le pagine cadono (il riuso rimaterializza zero); RO↔RW flippa
/// il bit W. Il codice e' l'unico mapping eseguibile: niente PROT_EXEC.
#[inline]
pub fn mprotect(addr: usize, len: usize, prot: u64) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_MPROTECT, addr as u64, len as u64, prot, 0) };
    if r < 0 {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}

/// Fase 30 — `shm_create(len)`: crea una regione di memoria condivisa di
/// `len` byte (frame contigui azzerati, max 256 KiB) e ritorna l'id (>= 1).
/// L'id si passa a un altro processo via IPC, che la mappa con `shm_map`.
#[inline]
pub fn shm_create(len: usize) -> Result<u32, Error> {
    let r = unsafe { syscall4(SYS_SHM_CREATE, len as u64, 0, 0, 0) };
    if r <= 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as u32)
    }
}

/// Fase 30 — `shm_map(id, hint, prot)`: mappa la regione condivisa `id` nello
/// spazio del chiamante (prot `PROT_READ`/`PROT_READ|PROT_WRITE`); le pagine
/// sono le STESSE per tutti i mappatori (scritture visibili). `hint == 0` =
/// scelta kernel. Ritorna la base o `Err`.
#[inline]
pub fn shm_map(id: u32, hint: usize, prot: u64) -> Result<usize, Error> {
    let r = unsafe { syscall4(SYS_SHM_MAP, id as u64, hint as u64, prot, 0) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as usize)
    }
}

/// Fase 33 — `shm_map_cow(id, hint)`: mappa la regione condivisa `id` in
/// copy-on-write (sempre `PROT_READ`): le pagine sono gli STESSI frame per
/// tutti finche' nessuno scrive; al primo write il kernel materializza una
/// copia privata (il resto resta condiviso). `hint == 0` = scelta kernel.
/// Ritorna la base o `Err` (id inesistente, saturazione ref — mai in pratica).
#[inline]
pub fn shm_map_cow(id: u32, hint: usize) -> Result<usize, Error> {
    let r = unsafe { syscall4(SYS_SHM_MAP, id as u64, hint as u64, PROT_READ, MAP_COW) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as usize)
    }
}


/// Fase 38.1 — `dma_alloc(pages)`: alloca `pages` (1..=DMA_PAGES_MAX) frame
/// fisici contigui azzerati, li mappa RW/NX a `USER_DMA_VA` e ritorna il
/// fisico base (serve il phys per programmare PRD/BMIBA del device).
/// Single-slot: seconda alloc = `Err`. I frame cadono a teardown/exec.
#[inline]
pub fn dma_alloc(pages: usize) -> Result<u64, Error> {
    if pages == 0 || pages > DMA_PAGES_MAX {
        return Err(Error::Invalid);
    }
    let r = unsafe { syscall4(SYS_DMA_ALLOC, pages as u64, 0, 0, 0) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as u64)
    }
}

/// D4 — `dma_free_slot(slot)`: libera lo slot DMA del chiamante (il buffer
/// device si rialloca quando la risoluzione del display cambia). Gli slot
/// >= 1 (device buffer) sono dell'owner di `Service::Console`.
#[inline]
pub fn dma_free_slot(slot: u64) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_DMA_FREE, slot, 0, 0, 0) };
    if r < 0 {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}

/// D3 — `dma_alloc_slot(pages, slot)`: come `dma_alloc` ma con lo **slot**
/// scelto: `0` = staging (path storico), `DMA_SLOT_ANY` = primo libero,
/// `>= 1` = device buffer (grant riservato all'owner di `Service::Console`).
/// Ritorna `(phys, va)`: il device vuole il fisico, il processo scrive nella
/// VA.
#[inline]
pub fn dma_alloc_slot(pages: usize, slot: u64) -> Result<(u64, u64), Error> {
    if pages == 0 {
        return Err(Error::Invalid);
    }
    let r = unsafe {
        syscall4_out(SYS_DMA_ALLOC, pages as u64, slot, 0, 0)
    };
    if r.0 < 0 {
        return Err(Error::NoMemory);
    }
    Ok((r.0 as u64, r.2))
}

/// Termina il processo corrente con il codice `code`. Non ritorna.
#[inline]
pub fn exit(code: i64) -> ! {
    unsafe {
        syscall4(SYS_EXIT, code as u64, 0, 0, 0);
    }
    unsafe {
        core::arch::asm!("ud2", options(noreturn));
    }
}

/// Fase 14 — `kill(pid, code)`: chiede al kernel di terminare il processo
/// user `pid` con il codice `code` (cleanup differito + cascata sulla
/// discendenza + notifica `EXIT_NOTIFY` al parent). Fase 35 (hardening): solo
/// il parent (o init) puo' killare — uccidere un server supervisionato e'
/// operazione da supervisore (via init, `init_bounce`). `Ok` se il processo
/// e' stato terminato, `Err` se il pid non esiste / non e' killabile (init,
/// processi kernel, se stesso, non-figlio).
#[inline]
pub fn kill(pid: i64, code: i64) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_KILL, pid as u64, code as u64, 0, 0) };
    if r < 0 {
        Err(Error::Denied)
    } else {
        Ok(())
    }
}

/// Fase 44a (job control) — `suspend(pid)`: chiede al kernel di congelare il
/// processo user `pid` (meccanismo neutro, semantica POSIX in shell: Ctrl-Z).
/// Stessi gate di `kill` (solo parent/init). `Ok` se sospeso o gia' sospeso
/// (idempotente), `Err` se il pid non esiste / non e' sospendibile.
#[inline]
pub fn suspend(pid: i64) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_SUSPEND, pid as u64, 0, 0, 0) };
    if r < 0 {
        Err(Error::Denied)
    } else {
        Ok(())
    }
}

/// Fase 44a (job control) — `resume(pid)`: rimette in schedulazione un
/// processo sospeso (no-op ok se gia' running). Stessi gate di `suspend`.
#[inline]
pub fn resume(pid: i64) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_RESUME, pid as u64, 0, 0, 0) };
    if r < 0 {
        Err(Error::Denied)
    } else {
        Ok(())
    }
}

/// Fase 14 — `is_exit_notify(m)`: true se `m` e' la notifica kernel→parent
/// della morte di un figlio (`EXIT_NOTIFY`: w0 = exit code, w1 = pid del
/// figlio). I loop `recv` dei server/test devono ignorarla o gestirla.
#[inline]
pub fn is_exit_notify(m: &IpcMsg) -> bool {
    m.tag == EXIT_NOTIFY
}

/// Fase 19.1 — entry `ps`: snapshot di un processo (syscall 37, layout dei
/// campi in `syscall-numbers::SYS_PS_INFO`). `name` = byte del nome (max 16,
/// stop al primo NUL), `parent` = pid del padre (`None` per init/idle).
#[derive(Clone, Copy, Debug)]
pub struct PsEntry {
    pub pid: u32,
    pub name: [u8; 16],
    pub state: u8,
    pub prio: u8,
    pub parent: Option<u32>,
    pub ipc: u8,
    pub ticks: u64,
}

impl PsEntry {
    /// Sospeso via `SYS_SUSPEND` (Fase 44a, job control): `state == 2`.
    pub fn stopped(&self) -> bool {
        self.state == 2
    }
    /// Lunghezza del nome (stop al primo NUL).
    pub fn name_len(&self) -> usize {
        self.name.iter().position(|&b| b == 0).unwrap_or(16)
    }
    /// Nome come `&str` ("?" se non UTF-8, mai in pratica: nomi statici).
    pub fn name_str(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len()]).unwrap_or("?")
    }
}

/// `ps_info(pid)`: snapshot del processo `pid`, `None` se lo slot e' vuoto o
/// il processo e' terminato (come `ps` salta i PID morti).
pub fn ps_info(pid: u32) -> Option<PsEntry> {
    let (rax, rdi, rsi, rdx, r10) =
        unsafe { syscall4_out(SYS_PS_INFO, pid as u64, 0, 0, 0) };
    if rax != 0 {
        return None;
    }
    let mut name = [0u8; 16];
    name[..8].copy_from_slice(&rdi.to_le_bytes());
    name[8..].copy_from_slice(&rsi.to_le_bytes());
    let parent_raw = ((rdx >> 16) & 0xFF) as u32;
    Some(PsEntry {
        pid,
        name,
        state: (rdx & 0xFF) as u8,
        prio: ((rdx >> 8) & 0xFF) as u8,
        parent: if parent_raw == 0 { None } else { Some(parent_raw - 1) },
        ipc: ((rdx >> 24) & 0xFF) as u8,
        ticks: r10,
    })
}

/// Scrive una stringa su console (`CONSOLE_OUT`) bypassando il line buffer.
/// Usare solo per dati binari/raw; per output di testo usare `print!`/`println!`.
#[inline]
pub fn write_stdout(buf: *const u8, count: usize) -> i64 {
    write(CONSOLE_OUT, buf, count)
}

/// Scrive byte su console (`CONSOLE_OUT`) bypassando il line buffer.
/// Compat: usato dai programmi userspace esistenti.
#[inline]
pub fn print_string(s: &[u8]) -> i64 {
    write(CONSOLE_OUT, s.as_ptr(), s.len())
}

/// `map_in(chan, phys, virt, count)`: mappa `count` pagine fisiche a partire
/// da `phys` all'indirizzo virtuale `virt` nello spazio del PEER del canale
/// `chan` (ADR-0008). Usato da cardo per iniettare la response ring del client
/// in un driver remoto (vela/console).
#[inline]
pub fn map_in(chan: u64, phys: u64, virt: u64, count: usize) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_MAP_IN, chan, phys, virt, count as u64) };
    if r < 0 {
        Err(Error::Denied)
    } else {
        Ok(())
    }
}

// ── CBS bandwidth reservation (Fase 11.4) ──────────────────────────────

/// Informazioni di un server CBS.
pub struct CbsInfo {
    pub budget: u32,
    pub period: u32,
    pub remaining: i32,
}

/// Crea un server CBS con budget Q e periodo P (in tick, 1 tick = 10 ms).
/// Admission control: ritorna l'id del server o `Err(())` se la bandwidth
/// totale supererebbe il cap (~70%).
#[inline]
pub fn cbs_create(budget_ticks: u32, period_ticks: u32) -> Result<i64, Error> {
    let r = unsafe { syscall4(SYS_CBS_CREATE, budget_ticks as u64, period_ticks as u64, 0, 0) };
    if r < 0 { Err(Error::Busy) } else { Ok(r) }
}

/// Lega il server CBS `server_id` al processo corrente.
#[inline]
pub fn cbs_attach(server_id: i64) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_CBS_ATTACH, server_id as u64, 0, 0, 0) };
    if r < 0 { Err(Error::NotFound) } else { Ok(()) }
}

/// Ritorna le informazioni di un server CBS (budget/period/remaining).
#[inline]
pub fn cbs_get_info(server_id: i64) -> Option<CbsInfo> {
    let (rax, rdi, rsi, _, _) = unsafe { syscall4_out(SYS_CBS_GET_INFO, server_id as u64, 0, 0, 0) };
    if rax < 0 {
        None
    } else {
        Some(CbsInfo {
            budget: rax as u32,
            period: rdi as u32,
            remaining: rsi as i32,
        })
    }
}

/// Fase 32 — `text_stats()`: contatori della shared text (`(hits, misses,
/// live)`). Debug/test: `live` e' la somma dei refcount vivi.
#[inline]
pub fn text_stats() -> (u64, u64, u64) {
    let (rax, rdi, rsi, _, _) = unsafe { syscall4_out(SYS_TEXT_STATS, 0, 0, 0, 0) };
    (rax as u64, rdi, rsi)
}

/// Fase 33 — `cow_count()`: fault COW gestiti finora (stessa syscall di
/// `text_stats`, registro `rdx`). Debug/test: t48 verifica `cow > 0` dopo
/// write su mappatura COW.
#[inline]
pub fn cow_count() -> u64 {
    let (_, _, _, rdx, _) = unsafe { syscall4_out(SYS_TEXT_STATS, 0, 0, 0, 0) };
    rdx
}

/// Fase 52 (P3 durabilita') — `meminfo()`: fermo immagine RAM del PMM come
/// `(free, total, used)` in frame (4 KiB). Mai fallisce (nessun argomento).
/// Sensore per swap futuro (B1) e quota (A3); nessuna decisione qui (il
/// kernel non fa OOM-kill: negativa ADR-0028).
#[inline]
pub fn meminfo() -> (u64, u64, u64) {
    let (rax, rdi, rsi, _, _) = unsafe { syscall4_out(SYS_MEMINFO, 0, 0, 0, 0) };
    (rax as u64, rdi, rsi)
}

/// Fase 2 (root su volume) — `boot_cmdline()`: command line PVH salvata al
/// boot (`-append` QEMU, max `BOOT_CMDLINE_MAX`). Ritorna i byte (vuota =
/// assente); il kernel non tronca mai oltre il bound, niente seconda
/// chiamata. Meccanismo neutro (ADR-0025): byte opachi, il parse
/// `root=UUID=` vive nel chiamante (cardo).
#[inline]
pub fn boot_cmdline() -> alloc::vec::Vec<u8> {
    let mut buf = alloc::vec![0u8; BOOT_CMDLINE_MAX];
    let r = unsafe { syscall4(SYS_BOOT_CMDLINE, buf.as_mut_ptr() as u64, BOOT_CMDLINE_MAX as u64, 0, 0) };
    if r <= 0 {
        alloc::vec::Vec::new()
    } else {
        buf.truncate(r as usize);
        buf
    }
}

/// S-T (ADR-0046) — `thread_create(entry, stack, fs)`: thread 1:1 nel gruppo
/// del chiamante (stesso mm/canali/fd; kernel stack + TSS propri; user stack
/// fornito dal chiamante, tipicamente `mmap`; `fs` = base TLS, 0 = nessuna).
/// Ritorna il tid o `Err(NoMemory)` a pool esaurito / argomenti invalidi.
#[inline]
pub fn thread_create(entry: usize, stack: usize, fs: usize) -> Result<usize, Error> {
    let r = unsafe { syscall4(SYS_THREAD_CREATE, entry as u64, stack as u64, fs as u64, 0) };
    if r < 0 {
        Err(Error::NoMemory)
    } else {
        Ok(r as usize)
    }
}

/// S-T — `thread_exit(code)`: termina SOLO il thread corrente (l'ultimo
/// chiude il gruppo con la via completa di exit).
#[inline]
pub fn thread_exit(code: i64) -> ! {
    unsafe { syscall4(SYS_THREAD_EXIT, code as u64, 0, 0, 0) };
    unreachable!()
}

/// S-T — `thread_set_fs(base)`: aggiorna la base TLS (FS) del thread
/// corrente (0 = via). Programma l'MSR subito + PCB per gli switch futuri.
#[inline]
pub fn thread_set_fs(base: usize) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_THREAD_SET_FS, base as u64, 0, 0, 0) };
    if r < 0 {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}

/// S-T (T3, ADR-0046) — `futex_wait(addr, expected, deadline)`: dorme se
/// `*addr == expected` fino a WAKE o deadline (tick assoluti di `get_ticks`,
/// 0 = mai). Ritorna 0 = svegliato, 1 = non svegliato (mismatch immediato,
/// timeout, spuria), Err = argomenti invalidi o tabella piena. Il loop
/// standard ricontrolla il valore a ogni ritorno.
#[inline]
pub fn futex_wait(addr: usize, expected: u32, deadline: u64) -> Result<bool, Error> {
    let r = unsafe { syscall4(SYS_FUTEX_WAIT, addr as u64, expected as u64, deadline, 0) };
    if r < 0 {
        Err(Error::Invalid)
    } else {
        Ok(r == 0)
    }
}

/// S-T (T3) — `futex_wake(addr, n)`: sveglia fino a `n` waiter (0 = nessuno).
/// Ritorna gli svegliati.
#[inline]
pub fn futex_wake(addr: usize, n: u32) -> usize {
    let r = unsafe { syscall4(SYS_FUTEX_WAKE, addr as u64, n as u64, 0, 0) };
    if r < 0 {
        0
    } else {
        r as usize
    }
}
