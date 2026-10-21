# Interrupt Handling

## Panoramica

Gli interrupt sono il meccanismo con cui la CPU risponde a eventi hardware
o software. Velordo gestisce tre categorie:

| Tipo | Range | Esempi |
|------|-------|--------|
| CPU Exceptions | 0-31 | page fault, breakpoint, double fault |
| Hardware IRQ | 32-47 | PIT timer (IRQ 0), tastiera PS/2 (IRQ 1) |
| MSI/MSI-X (L0a) | 0x50, 0xFF | config change virtio-gpu, spurious LAPIC |
| Software | futuri | syscall (SYSCALL instruction) |

## IDT (Interrupt Descriptor Table)

L'IDT è una tabella di 256 entry. Il kernel la costruisce con il crate
`x86_64` e la carica con `lidt`:

```rust
static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.double_fault.set_handler_fn(double_fault_handler);
    idt[0x20].set_handler_fn(timer_handler);
    idt[0x21].set_handler_fn(keyboard_handler);
    // ...
    idt
});
```

Le eccezioni CPU usano la firma `extern "x86-interrupt" fn(ISF)`.
Gli interrupt hardware usano la stessa firma: `extern "x86-interrupt" fn(ISF)`.

## PIC 8259 — Remapping

Il PIC seriale va rimappato dopo il boot perché i suoi IRQ 0-7
si sovrappongono alle eccezioni CPU 0-7:

```
PRIMA:  IRQ 0-7  → INT 0-7   (conflitto con eccezioni CPU!)
DOPO:   IRQ 0-7  → INT 32-39  (master, offset 0x20)
        IRQ 8-15 → INT 40-47  (slave,  offset 0x28)
```

```rust
use pic8259::ChainedPics;
static PICS: Mutex<ChainedPics> = /* privato a pic.rs */
    Mutex::new(unsafe { ChainedPics::new(0x20, 0x28) });

// Init: PICS.lock().initialize();
// Maschera: solo IRQ 0 (timer) e IRQ 1 (keyboard):
//   PICS.lock().write_masks(0b1111_1100, 0b1111_1111);
// EOI via wrapper (numero INT, non IRQ):
//   unsafe { crate::pic::end_of_interrupt(0x20) }; // IRQ0 → INT 0x20
```

**EOI (End of Interrupt)**: dopo ogni interrupt handler, il kernel deve
mandare EOI al PIC. Senza, il PIC non genera altri interrupt e il
sistema si blocca silenziosamente.

## PIT — Programmable Interval Timer

Il PIT genera IRQ 0 a frequenza costante. È il cuore dello scheduler:

```
Frequenza target: 100 Hz (ogni ~10 ms)
Divisore = 1_193_182 / 100 = 11_931
Porta 0x43: comando 0x36 (channel 0, lobyte/hibyte, rate generator)
Porta 0x40: low byte + high byte del divisore
```

```rust
extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    // EOI PRIMA dello scheduling: se on_tick fa switch, il PIC non deve
    // restare in attesa con i timer successivi bloccati.
    unsafe { crate::pic::end_of_interrupt(0x20) };
    crate::ordo::sched::on_tick(); // tick PIT + scheduler preemptive (Fase 5/11)
}
```

Il contatore `TICKS: AtomicU64` (`pit.rs`) avanza dentro il tick; lo
scheduler lo legge per quantum/CBS e `get_ticks` (22) lo espone in userspace.

## Tastiera PS/2

La tastiera invia uno scancode su IRQ 1 ogni volta che un tasto è
premuto o rilasciato:

```
Porta 0x60: scancode (byte)
IRQ 1 → INT 0x21 dopo remapping
```

Il crate `pc-keyboard` gestisce la decodifica:
scancode set 1 → evento tasto → decodifica layout → carattere Unicode.

```rust
// Fase 15: niente piu' decodifica nel kernel e niente accessi alle porte.
// L'handler risolve l'owner per nome (restart-safe), gli accoda una notify
// (bridge interrupt→IPC: un wake senza messaggio non farebbe mai ritorno
// da `recv()`) e manda EOI in ogni caso.
extern "x86-interrupt" fn keyboard_handler(_stack_frame: InterruptStackFrame) {
    if let Some(owner) = crate::relay::channels::lookup(syscall_numbers::Service::Kbd) {
        crate::ordo::sched::notify_irq(owner, syscall_numbers::IRQ_NOTIFY_KBD);
    }
    unsafe { crate::pic::end_of_interrupt(0x21) };
}
```

**Fase 15 completata**: tastiera interamente in userspace. Il driver PS/2
`kbd` (ring 3, porte 0x60-0x64) drena l'i8042, pubblica gli scancode su
`/dev/kbd` e `porta` li decodifica (layout US) su
`/dev/input/keyboard` (v. ADR-0011). Se i servizi non sono pronti, lo scancode
va perso (fire-and-forget a coda piena: il drain successivo recupera).

## LAPIC e MSI-X (L0a/L0b)

Il PIC resta la sorgente degli IRQ legacy, ma dal L0a esiste anche il Local
APIC. `LINT0` è configurato in delivery mode **ExtINT** (`0x700`): il segnale
`INTR` del PIC viene inoltrato alla CPU e i vettori restano `0x20-0x2F`.
L'EOI del PIC è invariato; non si manda EOI LAPIC per ExtINT.

Il LAPIC serve alle **MSI/MSI-X**, che sono interrupt locali e non passano dal
PIC. L0a lo abilita da `IA32_APIC_BASE` (MSR `0x1B`), forza xAPIC MMIO,
marca UC la pagina 2M della direct map e configura `SVR` (enable + spurious
`0xFF`), `TPR=0`, LINT0 ExtINT, LINT1 NMI, LVT timer/error mascherati. Il PIT
resta il timer: niente LAPIC timer, niente IOAPIC, niente SMP.

```
PIC (IRQ0/1/14/15) ── INTR ──► LAPIC LINT0 (ExtINT) ──► CPU (0x20-0x2F)
virtio-gpu MSI-X ── addr=0xFEE00000, data=0x50 ──────► CPU (0x50)
LAPIC spurious ─────────────────────────────────────► CPU (0xFF)
```

Vettori dedicati:

| Vettore | Uso | EOI |
|---------|-----|-----|
| `0x50` | config change MSI-X di `Gpu` (L0b) | LAPIC EOI, poi `notify_irq` |
| `0xFF` | spurious LAPIC | nessuna (non c'è vettore in servizio) |

L'handler 0x50 è routing puro: `lapic::eoi()` **prima** di
`notify_irq(Service::Gpu, IRQ_NOTIFY_GPU)`, perché la notify può cambiare
contesto (wakeup-preemption). Il driver `gpu` legge l'ISR virtio e fa
`events_clear`; il kernel non tocca il device.

L0b (fase virtio-gpu) programma la tabella MSI-X nella BAR1: una entry per
il config change (`addr=0xFEE0_0000`, `data=0x50`, `msix_config=0`), virtqueue
in polling sincrono (`queue_msix_vector=0xFFFF`). Il config change può anche
essere pollato come fallback.

## Ordine di inizializzazione

```
GDT → IDT → LAPIC (enable + LINT0 ExtINT) → PIC (remap + maschera)
    → PIT (~100 Hz) → syscall → ... → STI
```

`lapic::init()` gira dopo `interrupts::init()` (entry 0x50/0xFF già in IDT)
e prima di `pic::init()`. Nessun interrupt è abilitato fino a `sti`, quindi
l'ordine non espone a IRQ prematuri.

`sti` (enable interrupts) viene molto dopo aver configurato tutto (dopo
`sched::init`, spawn di idle/init, `BOOT_OK`): se lo chiamassimo prima, un
IRQ non gestito causerebbe un triple fault. (Niente step tastiera: il driver
PS/2 vive in userspace dalla Fase 15, il kernel fa solo routing+EOI.)

## Interrupt Stack Frame

Quando un interrupt si verifica, la CPU pusha automaticamente:

```
┌─────────────────┐
│ SS (User Stack)  │  Solo per Ring 3 → Ring 0
│ RSP             │
│ RFLAGS          │
│ CS (Code Seg)   │
│ RIP (Return)    │
│ Error Code      │  Solo per alcune eccezioni
└─────────────────┘
```

## Riferimenti

- [Writing an OS in Rust - Interrupts](https://os.phil-opp.com/diving-in/)
- [OSDev Wiki - PIC](https://wiki.osdev.org/PIC)
- [OSDev Wiki - PIT](https://wiki.osdev.org/Programmable_Interval_Timer)
- [OSDev Wiki - PS/2 Keyboard](https://wiki.osdev.org/PS/2_Keyboard)
- [Intel SDM - Chapter 6: Interrupts](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html)
