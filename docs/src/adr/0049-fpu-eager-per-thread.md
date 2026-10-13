# ADR-0049: FPU per thread, eager

- Stato: accettata (S2.5b)
- Contesto: ADR-0046 (thread 1:1), ADR-0048 §S2.5 (toolchain nativa)
- Decisione: intero stato FPU (x87/SSE/MXCSR) salvato/ripristinato a ogni
  context switch (`fxsave`/`fxrstor` eager); `OSFXSR`+`OSXMMEXCPT` accesi a
  boot; area 512 B per pid in pool statico; `fninit` pulito a ogni reuse pid
  e a ogni exec; fault handler che stampano PRIMA di prendere lock.

## Contesto

Senza salvataggio FPU, qualunque binario user che esegue SSE si corrompe al
primo preempt (XMM condivisi) — o muore `#UD` se `OSFXSR` e' spento. Fino a
S2.4 niente lo usava (target softfloat: il compilatore non emette SSE), ma
S2.5 ne ha bisogno per due vie indipendenti:

1. i crate dell'ecosistema usano SIMD con dispatch runtime via `cpuid`
   (`blake3` nello span-hashing di rustc — hot path; `memchr` via regex):
   su Velordo la CPU *ha* SSE e `cpuid` lo dice, quindi lo userebbero;
2. `rustc` stesso non puo' girare se `blake2s`-di-span muore o corrompe.

Filtrare `cpuid` e' impossibile (istruzione non privilegiata); patchare ogni
crate e' whack-a-mole senza fine. Il salvataggio nel kernel chiude la classe
in un punto solo (~60 righe + test).

## Decisione

1. **Eager, non lazy**: `fxsave`/`fxrstor` a ogni switch (~150-300 cicli,
   frazione di cr3+ltr+tss+MSR gia' presenti). Lazy (`TS` + handler `#NM`)
   risparmierebbe per i thread che non toccano FPU ma aggiunge un percorso
   fault caldo: no, a 1 VCPU il costo eager e' rumore.
2. **Pool statico per pid** (128 × 512 B = 64 KiB di `.bss`), MAI aree su
   stack o heap: un'area fxsave su stack con allineamento dipendente dal
   layout del chiamante ha prodotto `#GP` + deadlock SCHED nel fault
   handler (muto! vedi lezioni). Il linker garantisce l'allineamento, lo
   stack no. `CpuContext.fpu` tiene l'indirizzo (assert di offset a
   compile-time); `place_process` assegna e inizializza (ogni riuso pid
   riparte pulito); il fork NON eredita (l'exec azzererebbe comunque: i
   thread partono puliti, come i processi dopo `exec` che fa `fninit`).
   Eccezione documentata: `BOOT_CONTEXT` usa lo slot 0 in sequenza (il boot
   termina prima che idle giri).
3. **Preservazione in `fpu_init`**: le creazione PCB avvengono in syscall
   (IF=0) con XMM del chiamante vivi: salva-su-scratch/`fninit`/salva-su-
   target/ripristina-da-scratch. Senza, uno `spawn` cancellerebbe i registri
   del padre (osservato come corruzione fantasma prima della diagnosi).
4. **Fault handler che parlano prima di lockare**: `#GP`/`#PF` stampano
   RIP/codice PRIMA di `process_name` (che locka SCHED). Un fault dentro una
   sezione critica SCHED dead-lockherebbe il lock non-rientrante prima di
   stampare: sistema muto e congelato, diagnosticato solo via monitor QEMU
   (RIP nel lock-spin di `gpf_handler`). Mai fault muti.
5. **Niente SSE nel kernel**: il kernel resta softfloat (il target non
   emette SSE); `fxsave`/`fxrstor`/`fninit` funzionano senza `OSFXSR`.

## Conseguenze

- `threadtest` 11/11 → 12/12 (test XMM: due thread, pattern diversi, switch
  forzati, `movdqa` su statiche allineate — i locali stack hanno tradito una
  volta anche nel test).
- Overhead switch: +~300 cicli. Non misurato in isolamento (rumore su
  tick 10ms); rivalutare se un profilo lo indica.
- `native-full`/SSE per i programmi resta S3 (ora *potrebbe* girare, ma il
  target resta softfloat per S2.5: meno variabili durante il bootstrap).
- Lezione generale: i print di debug con `as`+`%` senza parentesi mentono
  (`&x as *const _ as u64 % 16` ha stampato 0 per un indirizzo 8-mod-16);
  parentizzare sempre nelle diagnostiche.
