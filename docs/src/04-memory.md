# Memory Management

## Panoramica

Il gestore della memoria ha tre strati fondamentali (Fase 4) + il flip
higher-half (ADR-0020, 27.1/27.2/27.3):

1. **Kernel alto + direct map** — kernel a `-2G+1M`
   (`0xFFFF_FFFF_8010_0000`, LMA 1M), direct map di tutta la RAM a
   `0xFFFF_8880_0000_0000` con pagine 2M (statiche, 64G; fail-loud oltre)
2. **Physical frame allocator** — bitmap dimensionata a runtime dalla memory map, 1 frame = 4 KiB
3. **Kernel heap** — `linked_list_allocator` come `#[global_allocator]`

```
Ordine di inizializzazione:
  guard 27.2           → finestra immagine + pagine 1G (no: 2M, sempre) + CR3
  unmap_low()        → PML4[0] = 0: da qui solo alto + direct map
  boot_info::memmap()→ legge le regioni fisiche da hvm_start_info (via direct)
  max_addr           → tetto RAM = max(addr + size) delle regioni MEM_RAM
  vmm::init(max_addr)→ verifica tetto 64G (fail-loud oltre, niente top-up)
  phys_mem::init(...)→ bitmap allocator dalla memory map (+ riserva tabelle)
  heap::init(bitmap_end) → heap subito dopo kernel + bitmap
```

## Higher-half + direct map (ADR-0020)

Lo stub (`boot.asm`, tutto RIP-relative + alias LMA) carica CR3 con un PML4
dual-map: identity di transizione `[0, 8M)` (PML4[0], rimossa a runtime) +
immagine alta + direct map. Dopo il jump-high lo stack commuta su
`BOOT_HIGH_STACK` (.bss) e `rust_main` fa `unmap_low()` (`PML4[0] = 0` +
flush): da li' un NULL-deref faulta invece di leggere spazzatura. I PML4
user (creati dopo) ereditano il PML4 pulito per copia.

```
PML4[0]     → PDPT low  → identity [0, 8M) di transizione (solo boot)
PML4[511]   → PDPT_K    → PD_K: VMA [KERN-1M, +16M) → phys [0, 16M), pagine 2M
PML4[0x111] → PDPT_DIRECT → 32 PD: phys [0, 64G), pagine 2M
                           └─ PD[0][0] → PT_VGA: primi 2M a 4K, pagina VGA UC
```

Dettagli che hanno morso (ADR-0020 §dettagli): basi PD 2M pari (VMA −2G+1M,
stile Linux — basi dispari = bit riservato → `#PF` RSVD a zero output);
sotto 1M solo `0x90000–0x9FC00` e' RAM (buco PCI/VGA: le PD direct stanno a
LMA fissa 16M, verificata RAM fail-loud); pagine 2M e non 1G (baseline su
ogni x86-64: niente PDPE1GB, niente flag QEMU); tetto statico 64G fail-loud
oltre (config test ≤ 32G); VGA UC via split 4K (PAT di reset).

La pagina scratch dei test (`MAP_TEST_PHYS`, 64M) sta altrove per invariante
compilata: scriverci sopra le PD direct fu il fault ritardato di 27.3.

## Physical Frame Allocator — Bitmap dinamica

Ogni frame fisico di 4 KiB è rappresentato da 1 bit in una bitmap
posizionata **subito dopo `_kernel_end`** e dimensionata a runtime:

```
N frame = max_addr / 4096            (da memory map)
Bitmap  = N frame / 8 byte           (es. 5 GiB → 1.3 MiB di bitmap)
```

Init:
1. Tutti i frame marcati usati (bitmap = 0xFF)
2. Le regioni `MEM_RAM` vengono liberate
3. Kernel (VMA alte, contabilita' in PHYS via `kern_virt_to_phys`), tabelle
   base (0x90000–0x100000), tabelle alte 16M (verificate RAM, fail-loud),
   bitmap stessa, scratch test 64M e VGA buffer (0xB8000) vengono ri-marcati
   usati

API:
- `alloc() -> Option<u64>` — trova e marca un frame libero
- `free(frame)` — libera un frame
- `free_frames()` / `used_frames()` — statistiche
- `bitmap_end()` — fine bitmap (page-aligned), usata per posizionare l'heap

## Kernel Heap

Posizionato subito dopo kernel + bitmap (`bitmap_end()`), 4 MiB iniziali.
Usa `linked_list_allocator::LockedHeap` come `#[global_allocator]`.

Dopo `heap::init()`:
```rust
let v = alloc::vec![1, 2, 3];     // funziona!
let b = alloc::boxed::Box::new(42); // funziona!
```

`#[alloc_error_handler]` gestisce l'out-of-memory con panic.

## Layout fisico

```
0x00000 ─ 0x8FFFF   riservato (BIOS/IVT)
0x90000 ─ 0x96FFF   tabelle base boot (PML4/PDPT/PD/PT low + PDPT_K/PD_K/PDPT_DIRECT)
0x97000 ─ 0x9FBFF   RAM convenzionale libera (sotto il buco PCI/VGA)
0xA0000 ─ 0xFFFFF   buco PCI/VGA (NON RAM: letture 0xFF — mai tabelle qui!)
0x100000 ─ ─ ─ ─    kernel (text/rodata/data/bss, LMA; VMA a -2G+1M)
0x100000+  bitmap frame allocator (dim. variabile)
+          kernel heap (4 MiB)
+          RAM libera (frame allocator)
0x1000000 ─ 0x1021FFF tabelle alte (32 PD direct + PT VGA, LMA fissa 16M)
0x4000000  pagina scratch test MAP_TEST_PHYS (1 frame, oltre tutto)
```

> Nota: gli indirizzi esatti di bitmap/heap dipendono da `_kernel_end`
> e dalla quantità di RAM (bitmap più grande = heap più in alto).

## Errori comuni

| Errore | Causa | Soluzione |
|--------|-------|-----------|
| Page Fault | Accesso a pagina oltre il tetto mappa | Verificare `vmm::mapped_max()` |
| Out of Memory | Heap esaurito | `alloc_error_handler` → panic |
| Copertura insufficiente | RAM > 64 GiB | Alzare le PD statiche (meccanico, vedi ADR-0020) |
| #PF RSVD a zero output | Base PD 2M dispari (VMA/LMA incongrue) | Vedi ADR-0020 §1 + `const assert` in `addr.rs` |
| Tabelle illeggibili | LMA nel buco PCI/VGA (< 1M oltre 0x9FC00) | Solo 0x90000–0x96FFF sotto 1M; resto a 16M |

## Higher-half (fatto, ADR-0020)

Il **kernel higher-half** e' atterrato in 27.1/27.2/27.3 (vedi sopra + ADR-0020):
kernel a `-2G+1M`, direct map 2M, `PML4[0] = 0` a runtime. La protezione U/S
resta (pagine kernel supervisor-only), ma il basso canonico e' ora libero:
NULL-deref faulta, lo spazio user basso e' pulito per futuri mmap/brk.

Nota storica: era rimandato dalla Fase 6 (costo alto, benefici prematuri);
la condizione ("processi user che richiedono spazio basso pulito") e' maturata
con i servizi da disco e gli helper `spawn_image` (Fase 21).

## mmap anonimo nel basso canonico (Fase 28)

Il payoff dell'higher-half: il basso canonico (`[0x10_0000, 0x4000_0000)`,
1M–1G; i primi 64K mai assegnati → NULL faulta) ospita mappe anonime private
con zero-fill lazy (stesso contratto di `sbrk`: VA subito, frame al fault).

- Tabella VMA per-pid (16 record statici, mai heap — anche il fault handler
  fa lookup qui); overlap-check totale (zona/heap/stack/ring/altre VMA).
- Pagine materializzate `OWNED` → il teardown esistente le libera gratis;
  `munmap` solo su VMA intere (two-phase: valida tutto, poi muta).
- `is_user_range` esteso alle VMA vive: ogni syscall con buffer user
  (spawn, write, …) accetta memoria mappata senza cambi puntuali.
- Solo RW in 28 (`prot` diverso = `-1`); niente split, niente file-backed
  (page-in su fault verso cardo e' deadlock-prone: sua fase propria).
- `civis::mmap` / `mmap_fixed` / `munmap`; `sbrk`/heap/scratch invariati.

## Protezioni di memoria (Fase 29)

- Ogni VMA ha un `prot` (`PROT_NONE`/`PROT_READ`/`PROT_READ|PROT_WRITE`);
  `mmap` lo applica alla materializzazione, `mprotect` (syscall 41) lo cambia
  su VMA intere (RO↔RW flippa il bit W; NONE smappa+libera, riuso a zeri).
- EFER.NXE abilitato a boot: heap, stack, `mmap` e pagine iniettate sono
  non-eseguibili. Il binario user e' ancora RWX perche' flat (codice + dati in
  un'unica regione copiata): il W^X richiede i confini `.text`/`.data`
  all'embed-time (29b).
- Il page-fault handler distingue: protection-violation da USER MODE (write su
  RO, exec su NX, accesso a NONE) o fault fuori regione (guard page sotto lo
  stack) → **kill del processo** (`FAULT_EXIT_CODE` 139, mai halt del kernel);
  fault supervisor → bug del kernel, halt. La guard page sta a
  `USER_STACK_GUARD` (pagina sotto lo stack, mai mappata).
- Stessa politica "errore del processo → muore il processo" per l'OOM del
  demand-zero (heap/mmap) e per #GP da user mode (es. `in`/`out` su porta non
  concessa dalla I/O bitmap del TSS).
- Estrazione del phys da una PTE SEMPRE con `PTE_ADDR_MASK` (bit 12..51): mai
  `& !0xFFF`, che con NX lascerebbe il bit 63 e corromperebbe il frame address.

## Memoria condivisa (Fase 30)

- `shm_create(len)` (syscall 42) alloca frame contigui azzerati (max 256 KiB)
  e ritorna un id; `shm_map(id, hint, prot)` (43) li mappa come VMA del
  processo con PTE non-owned **pre-materializzate** (niente demand-zero: le
  pagine esistono da subito e sono le stesse per tutti i mappatori).
- Refcount per regione (`SHM_TABLE` statica, 16 slot): ogni `shm_map` +1,
  ogni `munmap`/teardown -1; a 0 i frame contigui sono liberati. Le VMA
  condivise portano l'id nel record (`shm`), il teardown le rilascia.
- `munmap`/teardown staccano le PTE condivise (non-owned) senza liberarle: il
  free e' solo a refcount. `mprotect` su condivise ammette RO↔RW, non NONE
  (drop di mappatura non supportato, rifiutato senza stato).
- Limite dichiarato: la creazione mai mappata resta finche' il processo non
  mappa (caso d'uso normale = create+map).

## Caricamento ELF (Fase 31, ADR-0021)

- I binari utente sono ELF stripped (non piu' flat `.bin`); il kernel li
  carica **per-segmento** con `kernel/src/elf.rs`: `validate` (nessuna
  allocazione) + `load` (mappa). I segmenti `PT_LOAD` diventano `RX` (codice),
  `RO` (rodata) e `RW` (dati): W^X reale del binario.
- Si carica al `p_vaddr` di link (`USER_CODE`): le `R_X86_64_RELATIVE` sono
  gia' applicate dal linker → nessuna reloc a runtime. `entry` = `e_entry`.
- Validazione stretta (input da disco via `spawn_image`): magic/classe/
  macchina, bound, `p_filesz <= p_memsz`, `p_vaddr` in
  `[USER_CODE, USER_FS_BUFFER)`, **rifiuto W+X**; un ELF malformato non
  alloca nulla. Allocazione contigua ≤ 2 MiB (`MAX_PAGES` 512).
- Il codice e' l'unico mapping eseguibile: scrivere a `USER_CODE` fa
  protection-fault → kill del processo (test `code-write`).

## Shared text (Fase 32, ADR-0022)

- Il loader divide l'immagine a `rw_off` = `align_down(min p_vaddr` scrivibile)`:
  `[base, rw_off)` e' immutabile (codice `RX` + rodata `RO`), `[rw_off, end)`
  e' privato (data/bss + coda immutabile della pagina a cavallo).
- Le pagine immutabili sono **condivise** tra le istanze dello stesso binario
  (`kernel/src/text.rs`, tabella statica, refcount), mappate read-only e
  non-owned; a refcount 0 i frame sono liberati. Identita' = hash FNV-1a
  dell'ELF + **verifica byte-per-byte** (input da disco non fidato).
- Il riferimento sta nel PCB (`text_id`) e si rilascia in `reclaim_one` dopo il
  teardown (il walk libera solo le foglie `owned`). Scope refcount-only:
  condivide tra istanze **concorrenti**; una cache persistente e' un follow-up.
- `text_stats` (syscall 44) espone `hits/misses/live` (debug/test); dalla Fase
  33 `rdx` = fault COW gestiti (`cow_count`).

## Copy-on-write a livello di frame (Fase 33, ADR-0023)

- **Frame refcount** (`phys_mem.rs`): 1 byte/frame allocato a boot subito dopo
  la bitmap (dinamico, stesso schema: niente `.bss` enorme). `alloc` = ref 1;
  `deref` decrementa e libera a 0 (`free` resta per i frame a ref 1). Contatore
  `cow` (fault gestiti) per il test.
- **Bit COW** (`USER_COW`, bit 10 AVL): le PTE `owned|COW` senza W sono pagine
  condivise read-only; al primo write il fault handler tenta **prima**
  `cow_fault` (user e supervisor): alloca un frame, copia 4 KiB, rimappa
  `owned|RW|NX`, `deref` il vecchio, `invlpg`. Senza COW (codice/rodata) o a
  OOM → kill/halt come prima.
- I free delle foglie user (`teardown`, `vma::unmap_user_range`) usano `deref`:
  il frame condiviso sopravvive al primo teardown.
- **Primitiva testabile**: `shm_map` con `MAP_COW` (solo `PROT_READ`) mappa i
  frame della regione `RO`+`COW` con ref++ per mappatura; `munmap`/teardown
  rilasciano via `deref`, `shm_release` a 0 via `deref_contiguous`.
  Salvaguardie: `mprotect` a RW con pagine ancora condivise rifiutato (W
  bypasserebbe il fault); il re-map dell'edge PTE-staccata e' hole-fill (un
  re-map cieco clobbererebbe le copie private); ref saturo (255) mai wrappato.
- Prerequisito di `spawn_copy` (Fase 34); il percorso `exec` resta lo split
  della Fase 32 (il COW sul `.data` dell'immagine duplicherebbe la pagina
  scritta per un guadagno di ~5 KiB: valutato e scartato).

## `spawn_copy` — COW dell'address space (Fase 34, ADR-0024)

- `SYS_SPAWN_COPY = 45` (nessun argomento): walk dell'address space del padre
  (`vmm_user::spawn_copy_share`): foglie owned → condivise `RO`+`COW`
  simmetriche (`ref_inc` + `invlpg` sul padre); non-owned (text/shm/iniettate)
  specchiate (con `text::add_ref`/`shm_ref`); finestre ring saltate;
  large-page rifiutate. OOM → `-1` con unwind (spazio parziale distrutto,
  padre intatto).
- Contesto figlio = fake kernel stack (11 word a offset noti dall'entry, che
  salva anche i callee-saved user) + `spawn_copy_child_exit` (`rax = 0`,
  `sysretq`, mai `PERCPU.ipc_override`). Ritorno multi-registro: padre
  `(pid, chan)`, figlio `(0, chan)`. TSS senza porte,
  VMA/`HEAP_BRK`/`req_next`/priorita' ereditati, IPC/ring/canali no;
  `civis::post_spawn_copy_child` avvelena l'FS.
- Test t49: isolamento bidirezionale su globale COW + report sul canale di
  nascita + exit 0; nessun leak (`heap_out` piatto).

## Grant MMIO device (L0b, ADR-0057)

`SYS_MAP_MMIO = 61` (`map_mmio(phys, virt, count)`) mappa BAR di device nel
VA del chiamante:

- fisico nella finestra **`DEV_MMIO_BASE = 0xFE00_0000`** (32 KiB: buco PCI
  di QEMU, sopra la RAM in ogni configurazione);
- virtuale nella finestra **`USER_MMIO_VA = FS_BLOCK + 0x270_000`** (dopo
  DMA), 32 KiB;
- mapping **UC** (`PCD|PWT`) e **non-owned**: le PTE non hanno
  `USER_OWNED`, quindi `teardown_user_space` non libera frame di device;
- grant ristretto all'**owner di `Service::Gpu`** (il driver video
  registra il servizio prima di mappare).

Perché non `map_physical`: mappa in write-back e accetta solo frame di
sistema (VGA, scratch, ring). Perché una finestra fissa: il kernel non ha
un allocatore di VA device e un solo consumer MMIO non lo giustifica; il
broker arriverà col secondo device (audio). La finestra fisica non si
riserva in `phys_mem` (la bitmap copre solo la RAM; `0xFE00_0000` è fuori).
Fuori QEMU la finestra dovrà arrivare dal firmware.

La finestra DMA (`SYS_DMA_ALLOC`, single-slot, max 4 pagine) resta separata:
la controlq virtio-gpu usa **una** pagina per descrittori + avail/used +
request/response.

## Riferimenti

- [Writing an OS in Rust - Heap Allocation](https://os.phil-opp.com/heap-allocation/)
- [OSDev Wiki - Bitmap Allocator](https://wiki.osdev.org/Bitmap_allocator)
- [OSDev Wiki - Paging](https://wiki.osdev.org/Paging)
- [Intel SDM - Chapter 4: Paging](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html)
