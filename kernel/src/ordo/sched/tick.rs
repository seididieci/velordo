// Split from ordo/sched.rs (byte-identical move; see facade).
use super::*;
use crate::ordo::process::State;
use core::sync::atomic::Ordering;
use super::ctx::{SCHED, INITIALIZED, switch_to};

pub fn on_tick() {
    crate::pit::tick();

    if !INITIALIZED.load(Ordering::Acquire) {
        return;
    }

    // Diagnostica scheduler (feature `sched_debug`, vedi Cargo.toml): snapshot
    // ogni 100 tick. Tenuta nel tree perche' ha gia' diagnosticato uno stallo
    // apparente (era la coda vuota, non lo scheduler).
    #[cfg(feature = "sched_debug")]
    static TICKDBG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    #[cfg(feature = "sched_debug")]
    let tn = TICKDBG.fetch_add(1, Ordering::Relaxed);

    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");

    // Fase 14: teardown differito dei processi Terminated (in coda di
    // reclaim). Gira qui perche' `current` e' un processo vivo: non si libera
    // mai lo stack di un processo mentre si gira ancora su di esso. Prima del
    // replenish CBS cosi' i frame vengono riusati subito.
    sched.drain_reclaim();

    // S-T (T3): scadenze futex (wake con timeout). A lock tenuto, prima del
    // resto: i waiter scaduti tornano runnable nello stesso tick.
    super::futex::tick_deadlines(sched, crate::pit::ticks());

    // E1 (ADR-0083): scadenze delle `recv` con deadline. Stesso giro, stesso
    // lock: chi scade torna Ready e la syscall ricontrolla la coda prima di
    // rispondere timeout (la coda vince).
    super::ipc::tick_recv_deadlines(sched, crate::pit::ticks());

    // CBS: replenishment — se la deadline di un server e' scaduta, resetta
    // il budget e rimetti il processo in ready queue. Prima del decrement
    // cosi' un processo throttled puo' tornare schedulabile nello stesso tick.
    // Rimetto in ready SOLO processi che sono effettivamente Ready o che
    // erano stati throttle-ati: mai risvegliare un processo Terminated (il suo
    // server CBS deve essere stato rilasciato a exit) ne' uno Blocked (in
    // attesa IPC: lo sblocca la reply, non il CBS).
    let replenished = crate::ordo::aegis::tick_replenish();
    for pid in replenished.iter() {
        if pid < sched.processes.len() {
            let st = sched.processes[pid].state;
            if st == State::Ready || st == State::Blocked {
                if st == State::Ready {
                    sched.set_ready(pid);
                }
            }
        }
    }

    let mut need_switch = match sched.current {
        Some(cur) => {
            sched.ticks_current += 1;
            // Fase 19.1 (colonna TIME di `ps`): contabilizza il tick al
            // processo che lo consuma davvero.
            sched.processes[cur].ticks_used += 1;
            sched.ticks_current >= QUANTUM_TICKS
        }
        None => true,
    };

    // CBS: decrement budget del processo corrente. Se esaurito, toglilo
    // dalla ready queue e forza lo switch.
    if let Some(cur) = sched.current {
        if crate::ordo::aegis::tick_budget(cur) {
            sched.clear_ready(cur);
            need_switch = true;
        }
    }

    // Diagnostica sched_debug (introdotta per il deadlock t24, mantenuta):
    // chi e' Blocked e su cosa, a ogni 100 tick incondizionato (il ramo
    // no-switch campiona male sotto churn).
    #[cfg(feature = "sched_debug")]
    if tn % 100 == 0 {
        for (pid, p) in sched.processes.iter().enumerate() {
            if p.state == State::Blocked {
                crate::serial_println!("[blkdbg] pid={} '{}' ipc={:?} wait={:?} prio={}",
                    pid, p.name_str(), p.ipc_state, p.waiting_pid, p.priority.0);
            }
        }
    }

    if !need_switch {
        #[cfg(feature = "sched_debug")]
        if tn % 100 == 0 {
            crate::serial_println!("[sched] tick={} cur={:?} mask={:#x} l16={:#x} heap_out={} heap_n={}",
                tn, sched.current, sched.ready_prio_mask, sched.ready_by_prio[16],
                crate::arc::heap::outstanding(), crate::arc::heap::allocs_total());
            // Diagnostica sched_debug (vedi sopra): chi e' Blocked e su cosa.
            for (pid, p) in sched.processes.iter().enumerate() {
                if p.state == State::Blocked {
                    crate::serial_println!("[blkdbg] pid={} '{}' ipc={:?} wait={:?}",
                        pid, p.name_str(), p.ipc_state, p.waiting_pid);
                }
            }
        }
        return;
    }

    let prev = sched.current;
    let next = match sched.pick_next() {
        Some(n) if prev != Some(n) => n,
        _ => {
            #[cfg(feature = "sched_debug")]
            if tn % 100 == 0 {
                crate::serial_println!("[sched] tick={} cur={:?} mask={:#x} l16={:#x} heap_out={} heap_n={} (no-switch)",
                    tn, sched.current, sched.ready_prio_mask, sched.ready_by_prio[16],
                    crate::arc::heap::outstanding(), crate::arc::heap::allocs_total());
            }
            return;
        }
    };

    switch_to(prev, next, guard);
}

/// Sveglia un driver su IRQ accodandogli una notify (Fase 15, bridge
/// interrupt→IPC): svegliare e basta non basta — se il processo dorme in
/// `recv()` con coda vuota, tornato Ready al primo giro, non trovando
/// messaggi, si ri-blocca senza mai tornare in userspace (il dato hardware
/// resterebbe unread). Con un messaggio in coda, `recv()` ritorna e il driver
/// drena l'hardware. Fire-and-forget: se la coda e' piena la notify si perde
/// (il drain successivo recupera comunque — il driver drena SEMPRE l'hardware
/// a ogni giro, anche su wake spurio). Sicuro da IRQ (solo lock SCHED).
pub fn notify_irq(id: usize, tag: u64) {
    if !INITIALIZED.load(Ordering::Acquire) {
        return;
    }
    let mut guard = SCHED.lock();
    let sched = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };
    if id < sched.processes.len() {
        let p = &mut sched.processes[id];
        let _ = p.msg_queue.try_push(crate::ordo::process::PendingMsg {
            channel: 0,
            req_id: 0,
            tag,
            w0: 0,
            w1: 0,
        });
        if p.state == State::Blocked {
            p.state = State::Ready;
            sched.set_ready(id);
        } else {
            p.pending_wake = true;
        }
    }
    // 38.2d — wakeup-preemption CENTRALE (kbd+disk): se lo scheduler
    // sceglierebbe proprio `id` adesso, vai subito invece di aspettare il tick
    // (10 ms). Senza, ogni driver event-driven paga ~1 tick per wakeup
    // (misurato A/B KVM: DMA 1.2 ms → 10 ms/op, 140x su fat_small). L'IPC sync
    // fa gia' handoff diretto; l'IRQ era l'unico wakeup differito.
    // Sicuro: gira con IF=0 (gate `x86-interrupt`), il kernel non e' mai
    // interrotto mid-syscall (SFMASK maschera IF all'entry) quindi il lock
    // SCHED non e' mai conteso da qui; EOI fatta dal chiamante PRIMA (il PIC
    // va riarmato prima di cambiare contesto, specie level-triggered).
    // `select_next` pura: `pick_next` avanzerebbe il cursore RR anche quando
    // non si cambia contesto. Nessun cambio se il pick cade altrove (prio piu'
    // alta pendente) o sul corrente: il percorso tick resta invariato.
    if sched.select_next() == Some(id) && sched.current != Some(id) {
        let prev = sched.current;
        switch_to(prev, id, guard);
    }
}
