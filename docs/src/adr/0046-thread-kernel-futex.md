# ADR-0046: Thread kernel 1:1 + TLS + futex (S-T)

## Status

Accepted

## Context

S0 (ADR-0045) ha deciso con dati che rustc vuole thread OS (32 clone anche
per hello-world): senza thread non gira, e la PAL single-thread stile uefi
non basta. Servono thread veri prima di qualunque esecuzione di toolchain.

## Decision

Thread = PCB completo con `thread_group = leader`: condivide cr3 (mai
teardown finche' il gruppo vive — l'ultimo chiude), canali/fd/heap/VMA/cwd
(tabelle indicizzate al leader via `current_mm` scritto allo switch,
lock-free), CBS/priorita'/nome/identita' del leader. Propri: kernel stack +
TSS (bitmap I/O clonata: stesso dominio di protezione, a differenza del
fork che parte senza porte), user stack (dal chiamante via mmap), FS base,
coda IPC. Pool entity 32→128 (bitmask `u128`; tabelle mm gia' a 128, TSS
pool cresciuto, VMA 16→64: 32 stack ne vogliono 32).

TLS: FS base per thread programmata a ogni switch (MSR) + FSGSBASE abilitato
da CPUID per rdfsbase/wrfsbase da ring 3 (qemu64 richiede `+fsgsbase` in
run.sh; senza, MSR-only e il test lo direbbe loud).

Futex WAIT (addr, expected, deadline a tick assoluti) / WAKE (n): chiave
(mm, addr), tabella 64 entry sotto il lock scheduler (check-then-block
atomici come `ipc_recv`), validazione via walk di presenza con bit U (le
`entry_at` mascherano i flag: walk raw dedicato), deadline sweep in
`on_tick`. Ritorni: 0 svegliato / 1 no (mismatch, timeout, spuria da
solo-runnable) / -1 errore. Join/mutex restano userspace (PAL/libc).

Lifecycle di gruppo: `exit` (SYS_EXIT) da QUALUNQUE thread = morte di
tutto il gruppo (semantica di `exit()`; S2.5f/F2: `abort()` del driver
rustc su un thread secondario lasciava il leader appeso nel join — il
kernel chiude il gruppo, non il solo chiamante), e cosi' i fault user
(#PF/#GP → `exit_process`, mai un leader zombie); kill = morte di tutto il
gruppo; thread_exit = solo il thread (reclaim: stack+TSS+PID, mai
mm/text/notify); kill/suspend/resume su tid risolvono al leader
(suspend/resume a tutto il gruppo); fork da thread = figlio single del
gruppo (parent = leader, VMA/brk del gruppo); exec solo dal leader (da
thread = -1 loud: il morph di leadership non ha semantica sicura senza
trasferimento canali); churn con rotazione in coda per il leader con thread
vivi (mai spin sul posto).

IPC di gruppo (T5): i thread inviano come leader (`sender_of`: stessa
sessione FS/fd, req_id dal contatore unico) ma dormono da thread; la reply
sveglia il membro con `(waiting_pid, waiting_req)` matchati (senza, la
reply andrebbe all'endpoint e il thread resterebbe appeso — osservato).
Async resta per-coda-thread (stesso-thread recv). `service_lookup` dai
thread riusa il canale del leader (`find`); spawn da thread ha parent =
leader (mai orfani con tid riusabile).

## Consequences

- Nuove syscall 54–58 + `Error` invariato (futex usa `Invalid`/`Ok(bool)`).
- Suite `threadtest` 11/11 (create/stack/TLS×2/futex×3/IPC/fd/stress-32).
- Sblocca S1-PAL-thread e S2 (rustc nativo); A8 (rete) resta fuori scala.
- S2.5f/F2: `SYS_EXIT` e i fault user chiudono il gruppo da qualunque
  thread (`abort()` su un thread secondario non lascia piu' il leader in
  join); `SYS_THREAD_EXIT` resta l'unica via thread-only (trampolino).
- Limiti dichiarati: niente thread-server (reply a thread-server non
  instradata), niente exec da non-leader, niente join nel kernel, VMA 64 e
  waiter 64 come bound fail-loud.
