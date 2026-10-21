//! civis — libreria di sistema per processi user (equivalente minimale di una
//! libc per Velordo). Fornisce i wrapper alle syscall del kernel.
//!
//! ABI syscall (vedi docs/src/06-syscalls.md — numerazione arbitraria del
//! progetto, NON standard):
//!   - `rax` : numero di syscall
//!   - `rdi` : arg1
//!   - `rsi` : arg2
//!   - `rdx` : arg3
//!   - `r10` : arg4
//!   - ritorno in `rax`
//!
//! Syscall implementate dal kernel (Fase 6):
//!   - 0 = exit(code)
//!   - 2 = write(fd, buf, count)
//!   - 8 = getpid()
//!
//! Nota: `syscall`/`sysret` salvano `RCX` (RIP) e `R11` (RFLAGS) senza toccarli,
//! quindi lo shim li marca come `lateout` per non affermare di conservarli.

#![no_std]

extern crate alloc;

pub extern crate syscall_numbers;
use syscall_numbers::*;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

/// Pagina fisica scratch riservata dal kernel per i test `map_physical`.
pub use syscall_numbers::MAP_TEST_PHYS;
/// Finestra staging DMA (Fase 38.1): `dma_alloc` mappa qui i frame contigui.
pub use syscall_numbers::USER_DMA_VA;
/// Finestre ring/relay (request/response/proprie/relay): single source in
/// `syscall-numbers` (S2.2).
pub use syscall_numbers::{USER_FS_BUFFER, USER_RESP_RING, CLI_REQ_VA, CLI_RESP_VA};
/// Servizi di sistema raggiungibili per nome (ADR-0008).
pub use syscall_numbers::Service;
/// Tag della notifica kernel→parent della morte di un figlio (Fase 14).
pub use syscall_numbers::EXIT_NOTIFY;
/// Tag IPC del servizio Identis (ADR-0055).
pub use syscall_numbers::{IDENTIS_ADD_USER, IDENTIS_LIST, IDENTIS_VERIFY};
/// Tag del cancel cooperativo parent→figlio (Fase 44b, job control: Ctrl-C).
pub use syscall_numbers::JOB_CANCEL;
/// Tag della notify kernel→kbd su IRQ1 (Fase 15, bridge interrupt→IPC).
pub use syscall_numbers::IRQ_NOTIFY_KBD;
/// Tag della notify kernel→block su IRQ14/15 (Fase 38, ATA DMA: stesso
/// bridge interrupt→IPC — block drena lo status Bus-Master ad ogni giro).
pub use syscall_numbers::IRQ_NOTIFY_DISK;
/// Tag della notify kernel→gpu sul config change virtio-gpu (L0a/L0b).
pub use syscall_numbers::IRQ_NOTIFY_GPU;
/// Vettore MSI-X dedicato a gpu (0x50): il driver lo scrive in `data` (L0b).
pub use syscall_numbers::MSI_VECTOR_GPU;
/// Bound di scansione PID per `ps` (Fase 19.1, = MAX_PIDS del kernel).
pub use syscall_numbers::PS_SCAN_MAX;
/// Identita' misurata di un'immagine ELF (Fase 36, Strato 2 di ADR-0026):
/// FNV-1a sui byte dell'ELF — stesso valore che il kernel misura allo spawn.
/// init/cardo la ricalcolano sui byte caricati (manifest, policy FS_REGISTER).
pub use syscall_numbers::image_hash;
/// Protocollo DISK_* cardo→block (Fase 16, single source in
/// `syscall-numbers`, Fase 16c): handshake/open/read/close + resolve
/// nome→handle di proprieta' del driver.
pub use syscall_numbers::{DISK_CLOSE, DISK_HELLO, DISK_OPEN, DISK_READ, DISK_RESOLVE, DISK_WRITE, DISK_FLUSH};
/// Topologia disco (Fase 51, P2): LIST/INFO cardo→block + relay R_*
/// verso i client (riusato da `arca list` in P5).
pub use syscall_numbers::{DISK_LIST, DISK_INFO, R_DISK_LIST, R_DISK_INFO};

// ── Tag delle operazioni (nel frame del ring, non nell'IPC) ────────
// Single source in `syscall-numbers` (Fase 17): prima duplicati qui, in
// cardo e (R_REGISTER) block.
pub use syscall_numbers::{
    R_CLOSE, R_DELETE, R_MKDIR, R_MOUNT, R_OPEN, R_READ, R_READDIR, R_REGISTER, R_UMOUNT,
    R_WRITE, R_RIGHTS_DROP, R_RIGHTS_GET, R_STAT, R_LSEEK, R_DUP_GRANT, R_DUP_CLAIM,
    R_DUP_CANCEL, R_PIPE_CREATE, R_SYNC, R_STATVFS, R_GET_HASH, R_RENAME,
    R_SYMLINK, R_READLINK, R_CHMOD,
};
/// Protocollo ArcaFS: casa `arcafs` (tag, bound, wire, formato); `civis` li
/// riesporta cosi' i client esistenti (`civis::R_OBJ_*`, ...) non cambiano.
pub use arcafs::proto::{
    R_OBJ_PUT, R_OBJ_GET, OBJ_BUCKET_MAX, OBJ_KEY_MAX, R_SNAP_CREATE, R_SNAP_DELETE,
    R_SNAP_ROLLBACK, R_SNAP_CLONE, R_OBJ_GET_ID, R_OBJ_STAT_ID, R_OBJ_DELETE, R_OBJ_STAT,
    R_ARCA_DEBUG, ARCA_SUB_OPEN, ARCA_SUB_ALLOC, ARCA_SUB_FREE, ARCA_SUB_READ,
    ARCA_SUB_WRITE, ARCA_SUB_STAT, ARCA_SUB_USEDISK, ARCA_SUB_QUOTA_SET,
    ARCA_SUB_QUOTA_GET, ARCA_SUB_XSTATS, ARCA_SUB_CACHE_FLUSH,
    ARCA_SUB_IO_STATS, ARCA_IO_STATS_WORDS,
};
/// Blocchi on-disk ArcaFS (Fase 56.2a): casa `arcafs`, riesportati qui.
pub use arcafs::format::{
    ARCA_BLOCK_SECTORS, ARCA_XHDROFF, ARCA_XMAGIC, ARCA_XVER, ARCA_XHDRLEN,
    ARCA_XHOFF_FREE, ARCA_XHOFF_HIGH, ARCA_XHOFF_NEXTID, ARCA_XHOFF_NEXTSNAP,
    ARCA_XHOFF_FLAGS, ARCA_XHOFF_CHECK, ARCA_NMAGIC, ARCA_NODE_TYPE_RAW,
    ARCA_NODE_TYPE_LEAF, ARCA_NODE_TYPE_INTERNAL, ARCA_NODE_PAYLOAD,
    ARCA_NODE_PAYLOAD_LEN, ARCA_NODE_CHECK,
};
/// Bit dei diritti per-canale (Fase 17, self-restriction; DELETE in 18.2;
/// SEEK in 40; GRANT/PIPE in 45; SYNC in 52): mask per `rights_drop`, valore
/// di ritorno di `rights_get`.
pub use syscall_numbers::{
    RIGHTS_ALL, RIGHTS_DELETE, RIGHTS_GRANT, RIGHTS_MKDIR, RIGHTS_MOUNT, RIGHTS_OPEN,
    RIGHTS_PIPE, RIGHTS_READ, RIGHTS_READDIR, RIGHTS_SEEK, RIGHTS_SYNC, RIGHTS_UMOUNT,
    RIGHTS_WRITE,
};
/// `kind` per R_STAT (Fase 19.2): bit 0-1 tipo + bit 7 readonly.
pub use syscall_numbers::{STAT_DEVICE, STAT_DIR, STAT_FILE, STAT_READONLY};

/// Flag `open` (Fase 18.2: OPEN_CREATE; Fase 40: OPEN_TRUNC/OPEN_APPEND) +
/// origini `SEEK_*` per R_LSEEK + sentinelle di errore FS (Fase 40: il server
/// distingue i rifiuti, la personalita' li traduce in errno al bordo).
pub use syscall_numbers::{
    OPEN_APPEND, OPEN_CREATE, OPEN_TRUNC, SEEK_CURRENT, SEEK_END, SEEK_START, ERR_BUSY,
    ERR_EXISTS, ERR_INVALID, ERR_ISDIR, ERR_NOTDIR, ERR_NOTFOUND, ERR_READONLY, ERR_EMPTY,
    ERR_CLOSED, ERR_NOSPC,
};
/// Modi `R_SYNC` (Fase 52, P3 durabilita'): nessuna garanzia / barriera con
/// flush / ogni write stabile (single source in `syscall-numbers`).
pub use syscall_numbers::{SYNC_NONE, SYNC_GROUP, SYNC_PERWRITE};

/// Fase 29 (mmap/mprotect): protezioni + codice di uscita per fault di
/// memoria, e layout stack condiviso (guard page) per i test.
pub use syscall_numbers::{
    FAULT_EXIT_CODE, MAP_COW, MMAP_FIXED, PROT_NONE, PROT_READ, PROT_WRITE, USER_CODE,
    USER_STACK_FRAMES, USER_STACK_GUARD, USER_STACK_TOP,
};
/// Fase 44b (job control): causa di morte per Ctrl-C su job non cooperante
/// (128 + SIGINT, stessa convenzione di `FAULT_EXIT_CODE`).
pub use syscall_numbers::EXIT_SIGINT;
/// Maniglie console per SYS_WRITE (bonifica POSIX: non fd, nessun fd-table;
/// i valori 1/2 sono coincidenza storica coi numeri Unix).
pub use syscall_numbers::{CONSOLE_ERR, CONSOLE_OUT};

/// Tag IPC FS/boot/kbd (DocsB): single source in `syscall-numbers` (prima
/// duplicati qui, in cardo/block/init/tty/kbd e come letterali nei test).
/// `civis` li riesporta: i server/test usano i path `civis::`, mai i valori.
pub use syscall_numbers::{
    FS_BUF_REG, FS_NOTIFY, FS_REGISTER, INIT_BOUNCE, KBD_NOTIFY, SVC_READY, TEST_DONE,
};
/// Tag DEV_* op + device type (DocsD): stesso pattern, prima duplicati in
/// cardo/block/vela/gpu/kbd/tty.
pub use syscall_numbers::{
    DEV_CLOSE, DEV_KBD, DEV_KEYBOARD, DEV_CONSOLE, DEV_NULL, DEV_OPEN, DEV_READ,
    DEV_READDIR, DEV_WRITE, DEV_ZERO,
};
/// Tag TIME_* (Fase 50, P1 orologio): client→usertime, data/ora di sistema.
/// Reply `TIME_NOW`: `w0` = secondi epoch (UTC), `w1` = centesimi di secondo.
pub use syscall_numbers::TIME_NOW;
/// Tag LOG_* (Fase 57, L1 logging, ADR-0039): client→vestigia, gateway centrale
/// (bucket `log` nativo + coda RAM senza volume). Reply `APPEND`: `w0` = seq,
/// `w1` = durable; `READ`: `w0` = len + frame, `w1` = seq; `SEAL`: `w0` =
/// snap_id; `STATS`: registri + frame `[durable:8][last_seal:8]`.
pub use syscall_numbers::{
    LOG_APPEND, LOG_FLUSH, LOG_MSG_MAX, LOG_RAM_TAIL, LOG_READ, LOG_REG, LOG_SEAL, LOG_SRC_MAX,
    LOG_STATS,
};
/// Formato superblock ArcaFS: casa `arcafs` (condiviso guest/host); `civis`
/// li riesporta cosi' i client esistenti non cambiano import.
pub use arcafs::format::{
    ARCA_BLOCK_SIZE, ARCA_LABEL_MAX, ARCA_MAGIC, ARCA_MOUNT_MAX, ARCA_OFF_ALLOC,
    ARCA_OFF_AUTO, ARCA_OFF_BLOCK_SIZE, ARCA_OFF_CHECK, ARCA_OFF_FLAGS, ARCA_OFF_GEN,
    ARCA_OFF_LABEL, ARCA_OFF_MAGIC, ARCA_OFF_META, ARCA_OFF_MOUNT, ARCA_OFF_REFCOUNT,
    ARCA_OFF_ROOT, ARCA_OFF_UUID, ARCA_OFF_VERSION, ARCA_SUPER_LEN, ARCA_VERSION, Superblock,
    superblock_identity,
};

// ── Meccanismo neutro (ADR-0025, ADR-0041) ───────────────────────────
// `civis` e' il MECCANISMO (neutro, usabile da chiunque): sys, ipc,
// heap/scratch, task (async-first nativo), pio/pci, tsc, test, error
// (vocabolario condiviso), plus i wrapper POSIX-named ma meccanismo (fs,
// spawn/exec_image, print, args: li usano anche i server nativi).
// La PERSONALITA' POSIX vive fuori, in `flavours/posix/libr` (`posix`:
// errno; `stdio`: vfd+redirect; `fork`/`exec`/`serialize_argv*`). Il bordo e'
// una dipendenza Cargo: `civis` non riferisce mai `posix` (verifica con
// (verifica: zero token POSIX nei sorgenti di `libs/civis`). `persona` e' l'unico aggancio
// consentito meccanismo→personalita' (hook di routing, installato al bordo).
// Una seconda personalita' riusa `civis` invariato.

/// Allocatore globale on-demand (free-list + `sbrk`): unico per tutto il
/// userland. Vive qui cosi' ogni binario che linka `civis` lo usa senza
/// duplicare codice.
pub mod heap;

/// Scratch arena per-op (bump + `reset()`, backing `sbrk` dedicato fuori
/// free-list): per i temporanei con lifetime = una richiesta. Mai heap
/// globale nei percorsi per-op (regola 24.2 aggiornata).
pub mod scratch;

/// Executor async minimale sopra l'IPC asincrona (ADR-0019): `Future`
/// (`WaitReply`, `RecvMsg`), tratto `Receivable` per l'instradamento,
/// `block_on` single-task e `run` multi-task a router centrale.
/// Kernel invariato; vincoli Fase 13 invariati (vedi modulo).
pub mod task;

/// Port I/O x86 in ring 3 (A4: prima duplicato in block/kbd).
pub mod pio;

/// Spazio di configurazione PCI in ring 3 (Fase 38.0d, ATA DMA): modulo
/// condiviso e traslocabile (servizio `userland/pci` solo al secondo consumer).
pub mod pci;

/// Harness condiviso per la test suite (A4: traversal readdir).
pub mod test;

/// Vocabolario condiviso di tutti i wrapper (Fase 39, ADR-0030): vive in un
/// modulo neutro perche' meccanismo e personalita' lo usano senza distinzione.
/// La traduzione in errno sta SOLO nella personalita' POSIX (bordo).
pub mod error;

pub use error::Error;

/// Unico aggancio meccanismo→personalita' (ADR-0041): hook di routing stdout
/// installato dal bordo (es. redirect POSIX). Vedi modulo.
pub mod persona;

pub mod fs;
pub mod ipc;
pub mod print;
pub mod spawn;
pub mod sys;
/// Data/ora di sistema (Fase 50, P1 orologio): client sincrono del servizio
/// `Time` (`TIME_NOW` → secondi epoch UTC + centesimi). Meccanismo neutro
/// (mai POSIX): serve il FS (mtime), i log e qualunque servizio.
pub mod time;
/// Logging L1 nativo (Fase 57, ADR-0039; servizio `Vestigia`, R8): client
/// sincrono del gateway (append + lettura + seal + contatori) + formato
/// record condiviso col server. Meccanismo neutro (mai POSIX).
pub mod vestigia;
pub mod tsc;

/// Convenzione argv sullo stack iniziale (Fase 37.1): macro `entry!` (CRT
/// minimale) + parser `args_from_stack`. Layout stile Linux come convenzione
/// di dati neutra (ADR-0025 §Neutral).
pub mod args;

/// Esegue una syscall a 4 argomenti e ne restituisce il risultato in `rax`.
///
/// # Safety
/// Il numero e gli argomenti devono essere validi per il kernel del target.
#[inline]
pub unsafe fn syscall4(
    number: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Come `syscall4`, ma cattura anche i registri di ritorno `rdi/rsi/rdx/r10`:
/// usato dalle syscall IPC (ADR-0008) che restituiscono piu' parole (channel,
/// tag, w0, w1) oltre allo stato in `rax`.
///
/// # Safety
/// Il numero e gli argomenti devono essere validi per il kernel del target.
#[inline(always)]
pub unsafe fn syscall4_out(
    number: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> (i64, u64, u64, u64, u64) {
    let rax: i64;
    let rdi: u64;
    let rsi: u64;
    let rdx: u64;
    let r10: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => rax,
            inlateout("rdi") arg1 => rdi,
            inlateout("rsi") arg2 => rsi,
            inlateout("rdx") arg3 => rdx,
            inlateout("r10") arg4 => r10,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    (rax, rdi, rsi, rdx, r10)
}

pub use fs::{ops_async::*, ring::*, session::*, sync::*, obj::*, cwd::*};
pub use ipc::*;
pub use print::*;
pub use spawn::*;
pub use sys::*;
pub use tsc::*;
pub use args::{Args, Env, args_from_stack, env_from_stack, ARGS_MAX};
// `CHANNEL_PARENT` e' anche in `syscall-numbers` (glob privato sopra):
// il single-item esplicito vince sui glob e preserva `civis::CHANNEL_PARENT`.
pub use ipc::CHANNEL_PARENT;
