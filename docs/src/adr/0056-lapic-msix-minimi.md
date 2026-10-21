# ADR-0056: LAPIC minimale e MSI-X per virtio-gpu (L0a/L0b)

## Status

Accepted. **L0a implementata** (LAPIC + convivenza PIC); **L0b** (MSI-X nel
trasporto virtio) si implementa con la fase virtio-gpu, che usa questa base.

## Context

Velordo è un microkernel x86_64 (ADR-0005). Il kernel gestisce scheduling,
IPC, memoria e routing degli interrupt; i driver vivono in userspace. Il
driver video attuale (`gpu`) è un servizio nativo che non ha ancora un
framebuffer grafico: la VGA testuale è un residuo e il percorso scelto per il
video è **virtio-gpu**.

virtio-gpu notifica il cambio di risoluzione/scanout con un evento di
configurazione. Il percorso comandi del trasporto virtio è sincrono (used
ring in polling), ma l'evento di config change è asincrono: o lo si polla, o
si usa MSI-X. MSI/MSI-X sono interrupt **locali**: vengono consegnati
all'APIC della CPU, non al PIC 8259. Il kernel era PIC-only (IRQ0/1/14/15 su
0x20-0x2F, PIT come timer). Senza LAPIC non esiste un destinatario per una
MSI-X.

Servono quindi due passi separati:

- **L0a** (questa ADR): accendere il LAPIC e lasciare il PIC come sorgente
  degli IRQ legacy, senza cambiare scheduler, timer o ABI.
- **L0b** (fase virtio-gpu): programmare la tabella MSI-X del device e
  consegnare l'evento al vettore dedicato.

## Decision

### L0a — LAPIC minimale + PIC

- Il LAPIC si abilita da `IA32_APIC_BASE` (MSR `0x1B`), forzando la modalità
  xAPIC MMIO (x2APIC disabilitato) e preservando i bit riservati.
- La pagina 2M della direct map che contiene il LAPIC viene marcata **UC**
  (`PCD|PWT`) prima del primo accesso, con `invlpg`.
- Configurazione:
  - `SVR = 1<<8 | 0xFF` (software-enable + spurious 0xFF);
  - `TPR = 0`;
  - `LINT0 = 0x700` (**ExtINT**): il PIC continua a vettorare IRQ0/1/14/15
    su 0x20-0x2F;
  - `LINT1 = 0x400` (NMI);
  - LVT timer/error mascherati (il PIT resta lo scheduler tick);
  - `ESR` letto e azzerato (write-1-to-clear).
- IDT: vettore `0x50` per il config change di `Gpu`; vettore `0xFF` per lo
  spurious (no EOI). L'handler 0x50 fa `lapic::eoi()` **prima** di
  `notify_irq(Service::Gpu, IRQ_NOTIFY_GPU)`, come IRQ1→kbd e IRQ14/15→block.
- Costanti condivise: `MSI_VECTOR_GPU = 0x50`, `IRQ_NOTIFY_GPU = 0x44`.
  Nessuna nuova syscall, nessuna modifica all'ABI.
- Ordine di boot: `gdt` → `interrupts` (IDT) → `lapic` → `pic` → `pit`.
  Nessun interrupt è abilitato fino a `sti`, quindi abilitare il LAPIC prima
  del PIC non espone a IRQ prematuri.

### L0b — MSI-X nel trasporto virtio (rinviata alla fase video)

- La tabella MSI-X vive nella BAR1 del device (`virtio-gpu-pci` modern-only,
  4 KiB); il driver userspace la mappa e la programma.
- Una sola entry abilitata, per il config change:
  `addr = 0xFEE0_0000` (base LAPIC), `data = 0x50`, `msix_config = 0`;
  le virtqueue restano in **polling sincrono** del used ring
  (`queue_msix_vector = 0xFFFF`, nessuna entry queue).
- Il driver legge `events_read`/ISR e fa `events_clear`; il kernel non tocca
  il device, fa solo routing interrupt→IPC.
- Il config change può anche essere pollato (timeout o thread dedicato) come
  fallback: non è un requisito assoluto per il primo driver, ma MSI-X è il
  percorso event-driven scelto.

### Perché non altre strade

- **Polling puro**: più semplice, ma aggiunge latenza e un thread/timer; non
  usa il meccanismo di notifica nativo del device. Resta il fallback.
- **PCI INTx / PIRQ**: legacy, QEMU-specifico e comunque non abilita MSI-X.
- **IOAPIC + LAPIC timer + SMP**: infrastruttura molto più grande del
  necessario per un singolo device UP; rimandata.
- **Allocatore dinamico di vettori**: con un solo device basta una costante;
  il secondo device MSI-X motiverà una bitmap/allocatore.

## Consequences

- Il PIC resta la sorgente degli IRQ legacy: nessuna modifica a `pic.rs`,
  `pit.rs`, scheduler o handler esistenti.
- L'EOI del PIC resta `pic::end_of_interrupt`; l'EOI LAPIC serve solo ai
  vettori MSI. Un EOI LAPIC su ExtINT sarebbe sbagliato.
- Il primo device MSI-X (virtio-gpu) usa il vettore statico 0x50 e il tag
  0x44; i successivi aggiungeranno costanti o un allocatore.
- Limiti dichiarati: UP, dest APIC 0, base LAPIC letta a runtime ma non
  negoziata con hardware reale, un solo vettore, niente IOMMU. Per hardware
  reale/SMP servirà esporre base/ID (es. `SYS_LAPIC_INFO`) o un allocatore.
- Il driver video resta in userspace: la programmazione MSI-X è un dettaglio
  del trasporto virtio, non del kernel.

## Relazioni

- **ADR-0005** (microkernel): il routing interrupt→IPC resta nel kernel, il
  device in userspace.
- **ADR-0029** (ATA DMA + IRQ): stesso pattern EOI-prima-della-notify e
  bridge interrupt→IPC; l'IRQ14/15 resta sul PIC.
- **ADR-0041** (meccanismo/personalità): L0a è meccanismo neutro, nessun
  POSIX nel kernel.
- **ADR-0048** (niente POSIX dentro): il vettore e il tag sono costanti
  native, non ABI POSIX.

## Implementazione L0a

- `kernel/src/lapic.rs` (nuovo): MSR, UC, SVR/TPR/LINT/LVT, `eoi`, log
  `[lapic] base=... id=... svr=...`.
- `kernel/src/main.rs`: `mod lapic` + `lapic::init()` tra IDT e PIC.
- `kernel/src/interrupts.rs`: entry 0x50/0xFF, `gpu_msi_handler`,
  `spurious_interrupt_handler`.
- `kernel/src/arc/vmm.rs`: `mark_direct_uncached` (PDE 2M UC + `invlpg`).
- `kernel/src/boot_tables.rs`: `PAGE_UC`/`LARGE_PAGE` visibili a `vmm`.
- `syscall-numbers`: `MSI_VECTOR_GPU`, `IRQ_NOTIFY_GPU`; re-export in
  `civis`.

Validazione: gate esistente (PIC/PIT/IRQ1/IRQ14/15) + marker `[lapic]` +
selftest `int 0x50` (solo build `selftest`). Il vettore 0x50 reale arriva
con L0b.

---
*Registro delle modifiche: primo redatto il 21 ottobre 2026 per L0a
(prerequisito virtio-gpu).*
