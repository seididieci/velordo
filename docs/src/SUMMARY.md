# Summary

[Introduzione](./00-introduzione.md)

---

# Architettura

- [Panoramica Architettura](./01-architettura.md)

# Componenti

- [Boot Process](./02-boot-process.md) — Fasi 1-2
- [Interrupt Handling](./03-interrupts.md) — Fase 3
- [Memory Management](./04-memory.md) — Fase 4
- [Process Scheduler](./05-scheduler.md) — Fase 5
- [System Calls](./06-syscalls.md) — Fase 6
- [IPC](./07-ipc.md) ⭐ — Fase 7
- [User Mode](./08-userspace.md) — Fase 8
- [File System](./09-filesystem.md) — Fase 9
- [RT Scheduler + CBS](./10-scheduler-rt-cbs.md) — Fase 11
- [Test Suite](./11-testing.md) — Fase 9.5
- [Utilities](./12-utilities.md) — Fase 18 (shell + utility utente)
- [Performance](./13-performance.md) — Fasi 23/24/25 (baseline + throughput + cache)
- [Cronologia di sviluppo](./14-cronologia-fasi.md) — log dettagliato per fase

---

# Decisioni Architetturali

- [ADR-0001: Use Rust Nightly](./adr/0001-use-rust-nightly.md)
- [ADR-0002: Use bootloader crate](./adr/0002-bootloader-crate.md)
- [ADR-0003: VGA Text Mode](./adr/0003-vga-text-mode.md)
- [ADR-0004: Boot via PVH con stub custom](./adr/0004-custom-multiboot-boot.md)
- [ADR-0005: Architettura Microkernel](./adr/0005-microkernel-architecture.md)
- [ADR-0006: TSS per-processo con I/O bitmap](./adr/0006-per-process-tss.md)
- [ADR-0007: Scheduler RT + CBS](./adr/0007-rt-scheduler-cbs.md)
- [ADR-0008: IPC per nome — registry + channel](./adr/0008-ipc-by-name-channels.md)
- [ADR-0009: IPC asincrono — request-id interno](./adr/0009-async-ipc.md)
- [ADR-0010: Process lifecycle — cleanup kernel-side](./adr/0010-process-lifecycle-cleanup.md)
- [ADR-0011: Tastiera e terminale in userspace](./adr/0011-userspace-keyboard-terminal.md)
- [ADR-0012: Disk driver ATA in userspace](./adr/0012-userspace-disk-driver.md)
- [ADR-0013: Mount espliciti in userspace](./adr/0013-mount-syscall.md)
- [ADR-0014: Diritti per-canale lato server](./adr/0014-channel-rights-serverside.md)
- [ADR-0015: POSIX come API di libr](./adr/0015-posix-api-libr-protocollo-interno.md)
- [ADR-0016: FAT32 scrivibile](./adr/0016-fat-writable.md)
- [ADR-0017: Servizi caricati da disco](./adr/0017-servizi-da-disco.md)
- [ADR-0018: Cache settoriale write-through in userdisk](./adr/0018-sector-cache-userdisk.md)
- [ADR-0019: async/await in libr (piano, 4 passi)](./adr/0019-async-await-libr.md)
- [ADR-0020: Higher-half kernel + direct map](./adr/0020-higher-half-direct-map.md)
- [ADR-0021: Loader ELF per-segmento (W^X)](./adr/0021-elf-loader.md)
- [ADR-0022: Shared text ELF](./adr/0022-shared-text-elf.md)
- [ADR-0023: COW a livello di frame](./adr/0023-cow-frame-refcount.md)
- [ADR-0024: `fork` — COW dell'address space](./adr/0024-fork.md)
- [ADR-0025: Modello nativo + POSIX come personalità](./adr/0025-native-model-personalities.md)
- [ADR-0026: Threat model + hardening](./adr/0026-threat-model-hardening.md)
- [ADR-0027: Identità misurata (Strato 2)](./adr/0027-measured-identity.md)
- [ADR-0028: `exec` in-place + shell che lancia programmi](./adr/0028-exec-in-place.md)
- [ADR-0029: ATA DMA + IRQ](./adr/0029-ata-dma-irq.md)
- [ADR-0030: Fondamenta posix (Fase 39)](./adr/0030-posix-fondamenta.md)
- [ADR-0031: fd handoff + redirect file (disegno Fase 40)](./adr/0031-fd-handoff-redirect.md)
- [ADR-0032: pipe + heredoc (disegno Fase 42)](./adr/0032-pipe-heredoc.md)
- [ADR-0033: env/PATH/script (disegno Fase 43a)](./adr/0033-env-path-script.md)
- [ADR-0034: history + editing di linea (disegno Fase 43b)](./adr/0034-line-editing.md)
- [ADR-0035: job control (disegno Fase 44a)](./adr/0035-job-control.md)
- [ADR-0036: segnali nativi cooperativi (disegno Fase 44b)](./adr/0036-signals.md)
- [ADR-0037: policy su identita' + sandbox build (Fase 45)](./adr/0037-policy-identity-sandbox.md)
- [ADR-0038: Provider trait per filesystem (Fase 46)](./adr/0038-provider-trait.md)
- [ADR-0039: Logging L1 nativo (Fase 57)](./adr/0039-logging-l1.md)
- [ADR-0040: Pantheon dei nomi + rename a fasi (R0)](./adr/0040-naming-pantheon.md)
- [ADR-0041: Personalità POSIX separata (civis + flavours/posix, Fase 58)](./adr/0041-civis-flavours-posix.md)
- [ADR-0042: Namespace POSIX su ArcaFS — dir emergenti + set RAM (56.3)](./adr/0042-arcaposix-emergent-dirs.md)
- [ADR-0043: Bucket oggetti sys/vela/usr + tree bin, bin/posix, usr/bin (D1)](./adr/0043-object-buckets-sys-drv-usr.md)
- [ADR-0044: Quota e subvolumi ArcaFS — budget per bucket + ERR_NOSPC (A3)](./adr/0044-arca-quota-subvolumi.md)
- [ADR-0045: Self-hosting rustc — survey S0, gap PAL + scala S1…Sn](./adr/0045-selfhosting-s0.md)
- [ADR-0046: Thread kernel 1:1 + TLS + futex (S-T)](./adr/0046-thread-kernel-futex.md)
- [ADR-0047: PAL std Velordo — hello std nativo (S1.3)](./adr/0047-pal-std-velordo.md)
- [ADR-0048: Scope S2 — toolchain Rust nativa](./adr/0048-scope-s2-toolchain-nativa.md)
- [ADR-0049: FPU per thread, eager](./adr/0049-fpu-eager-per-thread.md)