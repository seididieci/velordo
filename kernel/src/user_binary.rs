//! Binari utente reali (Fase 6.4 + 7 + 8.1; ELF da Fase 31): codice compilato
//! dai crate freestanding di `userland/` (servizi) e `testland/` (test). I
//! binari in `userland/build/*.bin` e `testland/build/*.bin` sono ELF stripped
//! (magic sniffato dal loader), generati da `scripts/build-userland.sh` /
//! `build-tests.sh` e inclusi qui via `include_bytes!`. `spawn_named` crea un
//! processo dal nome (syscall `spawn`). A ogni spawn l'ELF viene caricato in
//! frame privati dal loader per-segmento (`crate::elf`): due istanze della
//! stessa bin non condividono `.bss`/`.data` mutabili.

/// Incapsula un binario ELF embedded e genera la funzione `{elf}` che lo
/// espone come slice `'static` (il loader lo valida/carica per-segmento).
macro_rules! user_binary {
    ($elf:ident, $path:literal) => {
        pub fn $elf() -> &'static [u8] {
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), $path))
        }
    };
}

// Ogni macro viene espansa DENTRO il suo modulo: nessuna collisione tra i
// binari.
//
// Fase 21 (servizi da disco): il kernel embedda SOLO il boot-TCB
// (rector/disk/fs/log, caricati prima che il FS esista — log per primo per
// disegno ADR-0039); tutto il resto vive in `/bin` e `/test` su /fat e parte
// via `spawn_image` (syscall 38).
mod rector_bin { user_binary!(rector_elf, "/../userland/build/rector.bin"); }
mod fs_bin { user_binary!(cardo_elf, "/../userland/build/cardo.bin"); }
mod disk_bin { user_binary!(block_elf, "/../userland/build/block.bin"); }
mod log_bin { user_binary!(vestigia_elf, "/../userland/build/vestigia.bin"); }

use rector_bin::rector_elf;
use fs_bin::cardo_elf;
use disk_bin::block_elf;
use log_bin::vestigia_elf;

/// Spawna un processo user (gira in ring 3) dal binario ELF `elf`. `io_ranges`
/// = porte I/O (inclusive) consentite a ring 3 (TSS per-processo, ADR-0006);
/// vuoto = nessuna porta. Ritorna il pid assegnato, o `None` se l'ELF e'
/// malformato o la creazione fallisce (pool PID/TSS saturo, OOM).
fn spawn_user(
    name: &'static str,
    priority: crate::ordo::sched::Priority,
    elf: &[u8],
    parent: Option<usize>,
    parent_chan: Option<usize>,
    io_ranges: &[(u16, u16)],
) -> Option<usize> {
    let id = unsafe {
        crate::ordo::sched::create_user(
            name,
            priority,
            elf,
            parent,
            parent_chan,
            io_ranges,
            false,
            crate::arc::vmm_user::USER_STACK_FRAMES as u32,
            None,
        )
    }?;
    crate::serial_println!(
        "[user] binary '{}': elf {} byte, entry={:#x}",
        name,
        elf.len(),
        crate::arc::vmm_user::USER_CODE,
    );
    Some(id)
}

/// Spawna un processo user dal binario ELF in memoria del CHIAMANTE (Fase 21,
/// servizi da disco: `spawn_image`). `src/len` e' l'ELF letto da `/fat`; la
/// copia va in frame privati per-segmento (`crate::elf::load`). Corre col CR3
/// del chiamante: la sorgente user e' leggibile direttamente. Il nome display
/// arriva dal chiamante (`owned`, validato): transitorio "image" visibile al
/// massimo per un tick prima di `set_owned_name` (solo display, mai ABI).
/// `detached` (Fase 22): il figlio non partecipa alla cascata di morte del
/// parent (ri-parentato a init); deciso dallo spawner via SpawnMeta.
pub fn spawn_image(
    owned: &[u8],
    priority: crate::ordo::sched::Priority,
    src: *const u8,
    len: usize,
    parent: Option<usize>,
    parent_chan: Option<usize>,
    io_ranges: &[(u16, u16)],
    detached: bool,
    stack_frames: u32,
    args: Option<&crate::arc::vmm_user::ParsedArgs<'_>>,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    // `src` e' user-VA del chiamante (CR3 attivo: lettura diretta), gia'
    // validata come range user da `sys_spawn_image`.
    let elf = unsafe { core::slice::from_raw_parts(src, len) };
    let id = unsafe {
        crate::ordo::sched::create_user(
            "image", priority, elf, parent, parent_chan, io_ranges, detached, stack_frames,
            args,
        )
    }?;
    crate::ordo::sched::set_owned_name(id, owned);
    Some(id)
}

/// Spawna init, il primo processo user (PID 1). Chiamato dal kernel a boot,
/// prima di qualunque altro processo user, cosi' init sia l'antenato dei servizi
/// che poi creera' via `spawn` (Fase 8.1). Parent e canale `None` (kernel).
pub fn spawn_init() -> usize {
    spawn_user("rector", crate::ordo::sched::Priority::Normal, rector_elf(), None, None, &[])
        .expect("spawn di init fallito")
}

/// Descrizione di un binario embedded, per cercarlo per nome (syscall `spawn`).
struct NamedBinary {
    name: &'static str,
    elf: fn() -> &'static [u8],
    /// Porte I/O (inclusive) consentite a ring 3 per questo processo (TSS
    /// per-processo). `&[]` = nessuna porta. Es. `block` → ATA PIO.
    io_ranges: &'static [(u16, u16)],
    /// Priorita' di scheduling del processo.
    priority: crate::ordo::sched::Priority,
}

/// Porte dei controller ATA PIO primario + secondario per il disk driver
/// (Fase 16, `block`: enumerazione master/slave su entrambi i canali).
/// `cardo` non tocca piu' porte (Fase 16.2): qualunque `in/out` li' e' #GP.
/// Fase 38.0d (ATA DMA): + spazio di configurazione PCI (`0xCF8-0xCFF`, per
/// trovare il PIIX3-IDE e programmarne la BAR4 — col boot diretto PVH nessun
/// BIOS lo fa) + finestra Bus-Master `0xC000-0xC00F` scelta da block
/// (`civis::pci::BM_BASE`, QEMU-scoped: sopra il legacy nulla e' programmato).
/// Il conf arriva a `0xCFF` (NON `0xCFC`): la CPU controlla TUTTE le porte
/// della width e un DWORD a `0xCFC` tocca `CFD/CFE/CFF` — con fine `0xCFC`
/// l'`inl` fa #GP (osservato: `out` a `CF8` ok, `in` a `CFC` kill). Stessa
/// regola per la finestra BM: accessi allineati entro i 16 byte.
/// Nota onesta (ADR-0026): il conf PCI permette di riprogrammare qualunque
/// device — contenimento nullo senza IOMMU, dichiarato.
const ATA_PIO_RANGES: &[(u16, u16)] = &[
    (0x1F0, 0x1F7),
    (0x3F6, 0x3F7),
    (0x170, 0x177),
    (0x376, 0x377),
    (0xCF8, 0xCFF),
    (0xC000, 0xC00F),
];

use crate::ordo::sched::Priority;

/// I binari embedded spawabili per nome dalla syscall `spawn`. Fase 21: SOLO
/// lo storage-TCB (init/disk/fs) — Fase 57: +log (boot-TCB: deve partire prima
/// che il FS esista, ADR-0039). Il resto parte da disco via `spawn_image`.
/// I processi di servizio (fs) sono `Normal`.
const NAMED_BINARIES: &[NamedBinary] = &[
    NamedBinary { name: "cardo",       elf: cardo_elf,       io_ranges: &[], priority: Priority::Normal },
    NamedBinary { name: "block",       elf: block_elf,       io_ranges: ATA_PIO_RANGES, priority: Priority::Normal },
    NamedBinary { name: "rector",      elf: rector_elf,      io_ranges: &[], priority: Priority::Normal },
    NamedBinary { name: "vestigia",    elf: vestigia_elf,    io_ranges: &[], priority: Priority::Normal },
];

/// Crea un nuovo processo dal binario embedded chiamato `name`. `parent` e'
/// il pid del creatore, `parent_chan` e' il canale di nascita (ADR-0008) che
/// il figlio usera' come canale 0 verso il parent (creato dallo scheduler).
/// Ritorna il pid, o `None` se il nome non e' noto o la creazione fallisce.
pub fn spawn_named(name: &str, parent: Option<usize>, parent_chan: Option<usize>) -> Option<usize> {
    let bin = NAMED_BINARIES.iter().find(|b| b.name == name)?;
    // Si usa il nome 'static della tabella (non il buffer temporaneo del
    // chiamante) perche' il Process conserva un `&'static str`.
    spawn_user(bin.name, bin.priority, (bin.elf)(), parent, parent_chan, bin.io_ranges)
}
