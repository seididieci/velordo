//! Vela — ciò che permette a Velord di "navigare" l'hardware (R2, ADR-0040).
//!
//! Casa del codice condiviso dei driver userspace: i binari (`kbd`, `gpu`,
//! `vela` hub, `block`) linkano questa lib per scheletri e convenzioni, come
//! i client FS linkano `arcafs` per il wire. Solo `core`+`alloc` qui dentro
//! (stampo `arcafs`/`blake2s`): niente IPC diretta, niente loop server —
//! quelli restano nei binari (il server single-threaded non si eredita).

#![no_std]

/// Hub `/dev` (server `vela`): registry dei prefix driver +
/// pseudo-device (`null`, `zero`). Il mount resta in cardo/cardo.
pub mod hub;

/// Storage a blocchi (server `block`, ex-block): geometria ring DISK_*
/// e convenzioni data-plane condivise coi client futuri.
pub mod block;

/// Input (server `kbd`): scancode e notify verso la terminale.
pub mod input;

/// Terminale video (server `gpu`, ex-console): VGA + tastiera cotta.
/// La line discipline (`porta`, ex-tty) resta servizio autonomo fuori Vela.
pub mod gpu;

/// Rete (ADR-0069): costanti virtio-net condivise tra driver e servizio.
pub mod net;
