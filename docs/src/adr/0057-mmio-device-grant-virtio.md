# ADR-0057: Grant MMIO device e trasporto virtio-gpu (L0b)

## Status

Accepted. Implementata (L0b, 21 ottobre 2026). Dipende da ADR-0056 (L0a).

## Context

L0a (ADR-0056) ha acceso il LAPIC e installato il vettore MSI `0x50` per
`Gpu`. Il passo successivo è far parlare il driver video userspace con
`virtio-gpu-pci` in modalità **modern**: enumerazione PCI, programmazione
delle BAR, tabella MSI-X, una virtqueue di controllo e un comando reale
(`GET_DISPLAY_INFO`).

Due vincoli del meccanismo esistente non bastano:

- `map_physical` (21) mappa in **write-back** e ha un'allowlist di frame di
  sistema (VGA, scratch, ring): non è utilizzabile per BAR di device, che
  richiedono mapping **UC** e indirizzi arbitrari nella finestra PCI.
- Il driver deve leggere/scrivere lo spazio di configurazione PCI
  (`0xCF8-0xCFF`). Oggi `block` ce l'ha per il PIIX3-IDE; il secondo
  consumer PCI (audio) non esiste ancora, quindi un broker PCI sarebbe
  infrastruttura senza client.

Il kernel deve restare neutro (ADR-0005, ADR-0025): il trasporto virtio è un
dettaglio del driver, il kernel fa solo routing interrupt→IPC.

## Decision

### Grant MMIO: `SYS_MAP_MMIO` (61)

```
map_mmio(phys, virt, count) -> 0 | -1
```

- `phys` allineato a pagina e interamente dentro
  `[DEV_MMIO_BASE, DEV_MMIO_BASE + DEV_MMIO_SIZE)`:
  `DEV_MMIO_BASE = 0xFE00_0000`, `DEV_MMIO_SIZE = 0x8000` (32 KiB).
- `virt` allineato a pagina e dentro
  `[USER_MMIO_VA, USER_MMIO_VA + USER_MMIO_SIZE)`, con
  `USER_MMIO_VA = FS_BLOCK + 0x270_000` (dopo la finestra DMA).
- `count` in `1..=16`.
- Mapping **UC** (`PCD|PWT`) e **non-owned**: le PTE non hanno il bit
  `USER_OWNED`, il teardown non libera frame di device.
- **Solo l'owner di `Service::Gpu`**: il kernel confronta
  `relay::channels::lookup(Service::Gpu)` con il pid corrente. Il grant è
  del driver video, non un'API generale.
- La finestra fisica **non** viene riservata in `phys_mem`: la bitmap copre
  solo la RAM e `0xFE00_0000` è buco PCI. Fuori QEMU la finestra dovrà
  arrivare dal firmware (limite dichiarato).

`gpu` registra `Service::Gpu` **prima** di chiamare `SYS_MAP_MMIO`; la
registrazione è il grant.

### Accesso PCI diretto a `gpu`

`rector` dichiara per `gpu` due range I/O: cursore VGA `0x3D4-0x3D5` e
configurazione PCI `0xCF8-0xCFF` (`GPU_IO_RANGES`). Le primitive generiche
(letture/scritture 8/16/32 bit, `find_device`, walk delle capability,
`bar_addr`) vivono in `civis::pci`, che resta neutro; il trasporto virtio
resta nel binario (`userland/gpu/src/virtio.rs`).

**Broker PCI rinviato**: quando arriverà il secondo consumer PCI (audio)
servirà un servizio che media l'accesso. Nota di hardening (ADR-0026): chi
ha il conf PCI può riprogrammare qualunque device; senza IOMMU il
contenimento è marginale ed è già vero per `block`.

### Trasporto virtio-pci modern

- `virtio-gpu-pci,disable-legacy=on` → `1af4:1050`, BAR1 = 4 KiB
  (**tabella MSI-X**), BAR4 = 16 KiB (**modern**: common @0x0000,
  ISR @0x1000, device @0x2000, notify @0x3000), 64-bit con high dword 0.
- BAR programmate in `DEV_MMIO_BASE + 0x0000` e `+0x4000`; command PCI
  `Memory Space | Bus Master`; readback verificata, altrimenti fallback VGA.
- Feature negoziate: solo `VIRTIO_F_VERSION_1` (feature alta, bit 32).
  Sequenza `RESET → ACK → DRIVER → FEATURES_OK → DRIVER_OK` con rilettura di
  `FEATURES_OK`.
- Controlq (queue 0): split ring su **una pagina DMA** (`SYS_DMA_ALLOC(1)`):
  descrittori @0x0000, avail @0x0400, used @0x0500, request @0x0800,
  response @0x0900. Una richiesta = desc[0] request (`NEXT`) + desc[1]
  response (`WRITE`); attesa del `used.idx` in polling con timeout a tick
  (100 tick = 1 s), poi fallback.
- Nessuna modifica al kernel per le virtqueue: il device non è nel percorso
  dati di sistema.

### MSI-X

- La tabella vive in BAR1 (BIR della capability); `virtio-gpu` espone
  **3 vettori** (config change + 2 queue).
- Tutte le entry sono mascherate; si sblocca **solo la entry 0**:
  `msg_addr = 0xFEE0_0000` (LAPIC, dest 0), `msg_data = 0x50`.
- `common.msix_config = 0` (config change → entry 0);
  `queue_msix_vector = 0xFFFF` (controlq in polling). Le entry queue restano
  mascherate.
- L'handler kernel `0x50` esiste da L0a: EOI LAPIC, poi
  `notify_irq(Service::Gpu, IRQ_NOTIFY_GPU)`.
- Il ramo `IRQ_NOTIFY_GPU` nel loop di `gpu`: legge l'ISR (read-to-clear),
  legge `events_read`, scrive `events_clear`, e su
  `VIRTIO_GPU_EVENT_DISPLAY` rilegge la risoluzione e la logga. È il punto
  in cui D4 aggancerà la sincronizzazione con la finestra host.

### Primo comando e fallback

- `GET_DISPLAY_INFO` (0x0100) → `RESP_OK_DISPLAY_INFO` (0x1101); si logga il
  primo scanout abilitato. Marker: `[gpu] display info: 1280x800 enabled=1`.
- **La VGA resta la console**: virtio-gpu è un canale parallelo. Device
  assente, capability mancante, readback errato, `FEATURES_OK` perso,
  timeout del comando → `[gpu] virtio-gpu ...` + si resta su VGA. Se MSI-X
  era già armata viene disabilitata prima del fallback.
- `run.sh`: `-device virtio-gpu-pci,disable-legacy=on` di default;
  `VIRTIO_GPU=0` per il boot di fallback.

## Consequences

- Una nuova syscall (61) e una nuova finestra VA, ma **nessuna modifica**
  all'ABI esistente, a PIC/PIT, scheduler o console VGA.
- Il grant MMIO è deliberatamente ristretto: un secondo device MMIO
  richiederà o un'estensione della finestra o un broker. Il broker PCI è la
  stessa decisione; entrambi si faranno col secondo consumer PCI.
- `SYS_DMA_ALLOC` resta single-slot e limitata a 4 pagine. Per L0b basta
  una pagina; per il framebuffer di D3/D4 (1280×800×4 ≈ 4 MiB) servono
  multi-slot e un bound maggiore: **precondizione dichiarata di D3**.
- `SYS_LAPIC_INFO` (base LAPIC e APIC ID reali) resta rinviata al lavoro
  IOAPIC: oggi l'indirizzo MSI è la costante QEMU `0xFEE0_0000`.
- Il config change reale è validato **a mano** (`RUN_DISPLAY=gtk` + resize):
  su questo host non ci sono xvfb/xdotool/client VNC e il monitor QEMU non
  ha comandi display. La parte deterministica del gate è il marker di
  `GET_DISPLAY_INFO`.
- Su hardware reale o con più device MSI-X servirà un allocatore di vettori
  (oggi `0x50` è una costante).

## Relazioni

- **ADR-0005** (microkernel): device e trasporto in userspace, kernel solo
  routing.
- **ADR-0006** (TSS per-processo): `gpu` riceve `0xCF8-0xCFF` via I/O
  bitmap; senza range ogni accesso sarebbe `#GP`.
- **ADR-0025/0041** (meccanismo/personalità): `civis::pci` e
  `SYS_MAP_MMIO` sono meccanismo neutro, nessun POSIX.
- **ADR-0026** (hardening): limite dichiarato del conf PCI condiviso, no
  IOMMU.
- **ADR-0056** (LAPIC/MSI-X): L0b usa il vettore `0x50` e il tag
  `IRQ_NOTIFY_GPU` definiti lì.

## Implementazione

- `syscall-numbers`: `SYS_MAP_MMIO = 61`, `DEV_MMIO_BASE/SIZE`,
  `USER_MMIO_VA/SIZE`, `MSI_ADDR_BASE`; re-export in `civis`.
- `kernel/src/syscall/mem.rs`: `sys_map_mmio` (finestra, allineamento,
  owner `Gpu`, mapping UC + flush).
- `kernel/src/arc/vmm_user/paging.rs`: `map_user_region_uc`.
- `kernel/src/syscall/dispatch.rs`: dispatch 61.
- `libs/civis/src/pci.rs`: `config_read8/16`, `config_write8/16`,
  `find_device`, capability walk (`cap_first`/`cap_next`/`find_cap`),
  `bar_addr`.
- `libs/vela/src/gpu.rs`: ID PCI e costanti del protocollo virtio-gpu.
- `userland/gpu/src/virtio.rs`: trasporto modern, MSI-X, controlq, timeout.
- `userland/gpu/src/virtio_gpu.rs`: `GET_DISPLAY_INFO` + parsing difensivo.
- `userland/gpu/src/main.rs`: init dopo la registrazione, ramo
  `IRQ_NOTIFY_GPU`, fallback VGA.
- `userland/rector/src/main.rs`: `GPU_IO_RANGES`.
- `run.sh`: dispositivo virtio-gpu + `VIRTIO_GPU=0`.
- Docs: `03-interrupts.md`, `04-memory.md`, `06-syscalls.md`,
  `11-testing.md`, `14-cronologia-fasi.md`, `SUMMARY.md`, `ROADMAP.md`.

Validazione: gate esistente invariato (testfs 5/5, testfat 7/7,
posixtests 4/4, usertests 58/58, threadtest 12/12; testsarca 61/62 per il
noto flake `append 512KiB`), marker
`[gpu] virtio-gpu 1af4:1050 @00:04.0`,
`[gpu] BAR1=0xfe000000 BAR4=0xfe004000`,
`[gpu] MSI-X: 3 vect, config=0 -> 0x50, queue=0xffff`,
`[gpu] display info: 1280x800 enabled=1`; fallback con `VIRTIO_GPU=0`
(`[gpu] virtio-gpu assente: resto su VGA`); login `root/root` self-test
SUCCESS. Config change GTK: validazione manuale dichiarata.

---
*Registro delle modifiche: primo redatto il 21 ottobre 2026 per L0b
(trasporto virtio-gpu modern + MSI-X + `GET_DISPLAY_INFO`).*
