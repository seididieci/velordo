//! cardo — File system server (Fase 9.1 + 9.2 + 9.3 + 10.2 + 16.2).
//!
//! Riceve IPC dai processi client (open/read/write/close/readdir/mkdir) e
//! gestisce:
//!   - ramfs in memoria sul mount point `/` (scrivibile, Fase 9.1)
//!   - FAT32 dal disco via `block` sul mount point `/fat` (Fase 9.2 su
//!     ATA locale; Fase 16 via IPC `DISK_*`; Fase 16c resolve nome→handle
//!     lato driver; **scrivibile dalla Fase 20**: overwrite/crescita/`OPEN_CREATE`,
//!     niente unlink)
//!   - vela/console remoti via IPC per device `/dev/*` (Fase 9.3)
//!
//! Trasferimento dati (Fase 10.2): ogni client ha DUE pagine ring SPSC
//! (request + response) allocate dalla syscall 26 (`SYS_RING_ALLOC`). Il client
//! scrive un request frame nel request ring, notifica con `FS_NOTIFY`, e cardo
//! legge il frame, processa, e scrive il response frame nel response ring del
//! client. Per i device remoti cardo inietta la response ring del client nel
//! processo driver (`civis::map_in`) cosi' il driver scrive i dati direttamente
//! nella response ring del client — zero copie.

#![no_std]
#![no_main]

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use civis;

mod fat32;
mod ipc_disk;
mod provider;

use fat32::{Fat32, FileInfo};
use ipc_disk::IpcDisk;
use civis::println;

mod arca;
mod btree_drv;
mod ftable;
mod handlers;
mod mount;
mod mount_legacy;
mod pipes;
mod policy;
mod ramfs;
mod rights;
mod rings;
mod server;
mod volume;
// Wall-clock lazy dal servizio Time (Fase 50, P1 orologio): i provider
// producono `Meta.mtime` da qui, mai gli handler.
mod wall;
// Grant single-use per handoff fd al figlio (Fase 40, modello B).
mod dup;

// Geometria ring + errori IPC (A1): single source in `civis`.
use civis::{
    ERR, ERR_NOHANDSHAKE, RING_DATA_CAP, RING_HEAD, RING_TAIL, RING_MAX_PAYLOAD,
    ring_available, ring_positions,
};

// ── Tag delle operazioni (nei frame del ring) ─────────────────────
// Single source in `syscall-numbers` (Fase 17): include R_RIGHTS_DROP/GET;
// Fase 40: R_LSEEK + R_DUP_*.
use civis::{
    R_CLOSE, R_DELETE, R_MKDIR, R_MOUNT, R_OPEN, R_READ, R_READDIR, R_REGISTER, R_UMOUNT,
    R_WRITE, R_RIGHTS_DROP, R_RIGHTS_GET, R_STAT, R_LSEEK, R_DUP_GRANT, R_DUP_CLAIM,
    R_DUP_CANCEL, R_PIPE_CREATE, R_DISK_LIST, R_DISK_INFO, R_SYNC, R_STATVFS, R_GET_HASH,
    R_OBJ_PUT, R_OBJ_GET, R_SNAP_CREATE, R_SNAP_DELETE, R_SNAP_ROLLBACK, R_SNAP_CLONE,
    R_OBJ_GET_ID, R_OBJ_STAT_ID, R_OBJ_DELETE, R_OBJ_STAT, R_ARCA_DEBUG, R_RENAME,
    R_SYMLINK, R_READLINK, R_CHMOD,
};
// Sentinelle di errore FS (Fase 40): i rifiuti tipizzati viaggiano qui invece
// del generico ERR; il client li mappa in `posix::Error`. Fase 42: ERR_EMPTY
// (pipe vuota, riprova) + ERR_CLOSED (estremita' chiusa).
use civis::{ERR_NOTFOUND, ERR_ISDIR, ERR_NOTDIR, ERR_EXISTS, ERR_READONLY, ERR_BUSY, ERR_INVALID, ERR_EMPTY, ERR_CLOSED, ERR_NOSPC};
// Tag DEV_* op + device types (DocsD: single source in `syscall-numbers`).
use civis::{
    DEV_CLOSE, DEV_CONSOLE, DEV_KBD, DEV_KEYBOARD, DEV_NULL, DEV_OPEN, DEV_READ,
    DEV_READDIR, DEV_WRITE, DEV_ZERO,
};
// Tag IPC FS/boot (DocsB): single source in `syscall-numbers`, via `civis`.
use civis::{FS_BUF_REG, FS_NOTIFY, FS_REGISTER};

/// Manifest degli hash dei servizi (Fase 36, identita' misurata, Strato 2 di
/// ADR-0026): generato a build-time da scripts/gen-service-hashes.sh, incluso
/// via `VELORDO_SERVICE_HASHES` (esportata da build-userland.sh — cardo e'
/// compilato DOPO la generazione, vedi ordine di build).
include!(env!("VELORDO_SERVICE_HASHES"));
/// Tetto ops per hash noto (Fase 45, sandbox build): `SERVICE_POLICY`,
/// generato dallo stesso script (referenzia le HASH_*, incluso DOPO).
/// `TEST_POLICY` (hash dei binari testland, generato da
/// scripts/gen-test-policy.sh in build-tests.sh): incluso SOLO qui, mai dai
/// binari test (niente ciclo, vedi policy.rs). Entrambi `env!` = fail loud a
/// variabile mancante (mai policy stale silenziosa).
include!(env!("VELORDO_SERVICE_POLICY"));
include!(env!("VELORDO_TEST_POLICY"));

const MAX_PATH: usize = 256;

// ── IPC tags verso i driver remoti (vela/console/kbd/tty/disk): op DEV_*
// e device types importati sopra da `syscall-numbers` (DocsD) ──────────

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[cardo] panic: {}", info.message());
    civis::exit(1)
}
