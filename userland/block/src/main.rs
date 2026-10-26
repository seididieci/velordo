//! block — Driver disco ATA in userspace (Fase 16).
//!
//! Possiede le porte ATA del canale primario (via `io_ranges`, TSS
//! per-processo ADR-0006; il secondario e' probato da `detect.rs` ma non
//! concesso: toccarlo e' #GP — su QEMU non esiste), rileva i dischi presenti
//! (`detect.rs`), parsa le partizioni MBR primarie (`part.rs`) ed espone ogni
//! nodo come `/dev/sdX` (`FS_REGISTER` per nodo, solo i presenti) + servizio
//! `Disk` per il data-plane verso cardo.
//!
//! REGOLA ANTI-DEADLOCK (lezione Fase 15 + ciclo cardo↔block osservato in
//! Fase 16.2): block non fa MAI `send` sincrona verso cardo — nemmeno
//! l'handshake `fs_init` di civis (sincrono). E' client FS PURAMENTE async:
//! ring propri allocati raw, `FS_BUF_REG` + `R_REGISTER` via `send_async` con
//! collect per req_id nel loop (state machine come tty). cardo fa solo send
//! sincrone verso block, e block drena sempre (mai bloccato su cardo):
//! nessun ciclo possibile, in nessuna direzione, a boot come a restart.
//!
//! Due protocolli serviti, entrambi con reply implicita (ADR-0008):
//! - `DISK_*` (canale diretto cardo→block, service_lookup(Disk)): HELLO
//!   (fisici nelle reply: w0 = req_phys del DISK_REQ ring, w1 = resp_phys),
//!   OPEN/READ multi-settore (24.2, count≤7 per IPC), CLOSE, RESOLVE
//!   chiave→handle (Fase 16c: block e' l'unico proprietario della mappa;
//!   16d: chiave = nome (`sda`), UUID hex 8 char (seriale volume FAT) o label
//!   (priorità in quest'ordine).
//! - `DEV_*` (relay cardo per gli open raw `/dev/sdX`): OPEN(w0=handle
//!   codificato disco<<16|sub), READ sequenziale con posizione per-fd (solo
//!   multipli di 512), WRITE sempre ERR (read-only), CLOSE, READDIR vuota.
//!
//! Boot: detection (solo HW) → ring FS+DISK → `service_register(Disk)` →
//! SVC_READY al parent SUBITO (block parte PRIMA di cardo: come console,
//! l'ACK non aspetta nulla) → loop (la registrazione FS avanza da sola via SM
//! appena cardo esiste).

#![no_std]
#![no_main]

extern crate alloc;

mod block;
mod cache;
mod detect;
mod dma;
mod part;
mod fs_reg;
mod nodes;
mod rings;
mod server;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use civis::println;

// ── IPC tags (DocsD: single source in `syscall-numbers`, via `civis`) ──
use civis::{DEV_CLOSE, DEV_OPEN, DEV_READ, DEV_READDIR, DEV_WRITE};

/// Handshake data-plane: cardo chiede i fisici dei ring DISK.
/// Reply: w0 = req_phys (anello delle richieste di resolve, Fase 16c),
/// w1 = resp_phys (mappato da cardo per leggere i frame). Niente frame:
/// i fisici stanno nei registri.
use civis::DISK_HELLO;
/// Valida un nodo (w0 = handle codificato). Reply OK/ERR, niente frame.
use civis::DISK_OPEN;
/// Topologia dischi (Fase 51, P2 vocabolario): LIST (count + entry
/// `[sectors:8][flags:8]` per disco) e INFO (w0 = handle → settori/flags +
/// frame `[model_len:8][model][serial_len:8][serial]`).
use civis::{DISK_LIST, DISK_INFO};
/// Barriera write-cache del drive (Fase 52, P3 durabilita'): w0 = handle
/// (vale la parte disco), reply 0/ERR. Usata da `R_SYNC(GROUP)`.
use civis::DISK_FLUSH;
/// Legge UN settore (w0 = handle, w1 = lba nel nodo).
/// Frame: [512:8][0:8][settore]. Fuori range/errore → reply ERR, niente frame.
use civis::DISK_READ;
/// Chiude (stateless: sempre OK, frame vuoto).
use civis::DISK_CLOSE;
/// Risolve un nome nodo ("sda", "sda1") in handle (Fase 16c, single source
/// of truth nel driver). Richiesta: frame `[namelen:8][name]` nel DISK_REQ
/// ring; reply w0 = handle o ERR, niente frame.
use civis::DISK_RESOLVE;
/// Scrive un settore (Fase 20, FAT scrivibile): handle in w0, lba in w1,
/// payload 512 byte nel frame DISK_REQ; reply senza frame.
use civis::DISK_WRITE;

// ── Ring I/O ────────────────────────────────────────────────────────
// Due coppie SEPARATE (lezione CLI_* del fix kbd/tty: mai protocolli diversi
// nello stesso ring):
// - FS_REQ_VA/FS_RESP_VA (propri): traffico FS (FS_BUF_REG + FS_REGISTER).
//   Mai iniettati da nessuno: niente remap, mai sovrascritti. Per i relay DEV
//   in ingresso cardo mappa i ring del client nelle finestre CLI_* dedicate.
// - DISK_REQ_VA/DISK_RESP_VA: data-plane DISK_* con cardo (fisso, noto a
//   cardo via HELLO). VA single source in `syscall-numbers` via `civis`.
//   Libere nella mappa user (CLI fino a +0x23..., heap da +0x400000).

/// Request/response ring FS propri (stesse VA di civis: page table per-processo,
/// nessun conflitto — e block non usa il machinery FS di civis).
const FS_REQ_VA: u64 = 0x0000_4000_0020_0000;
const FS_RESP_VA: u64 = 0x0000_4000_0021_0000;
const CLI_REQ: u64 = civis::CLI_REQ_VA;
const CLI_RESP: u64 = civis::CLI_RESP_VA;
// Geometria ring + errore IPC (A1) + frame helpers (A2): single source in `civis`.
use civis::{ERR, RING_DATA_CAP, RING_HEAD, RING_TAIL};
use civis::{DISK_REQ_VA, DISK_RESP_VA, req_frame_consume, resp_frame_write};

/// Tag IPC FS (DocsB: single source in `syscall-numbers`, via `civis`).
use civis::{FS_BUF_REG, FS_REGISTER};
/// Tag frame nel request ring (single source in `syscall-numbers`, Fase 17).
use civis::R_REGISTER;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[block] panic");
    civis::exit(1)
}
