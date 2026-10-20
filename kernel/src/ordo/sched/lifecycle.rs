// Split from ordo/sched.rs (byte-identical move; see facade).
use super::*;
use crate::ordo::process::State;
use core::sync::atomic::Ordering;
use x86_64::instructions::hlt;
use super::ctx::{SCHED, INITIALIZED, switch_to};
use super::ps::{name_buf, name_str_of};
use super::queue::Scheduler;

/// `SYS_EXIT`: termina il PROCESSO, da qualunque thread (semantica di
/// `exit()`: il gruppo muore intero). Un exit da un thread non-leader non
/// deve lasciare il leader bloccato in join — caso reale: `abort()` del
/// driver rustc nativo eseguito su un thread secondario (S2.5f/F2).
pub fn exit_process(code: i64) -> ! {
    exit_target(code, true)
}

/// `SYS_THREAD_EXIT`: termina SOLO il thread corrente (trampoline di
/// `thread::spawn` dopo il wake del join). Se chiamata dal leader la via
/// resta quella completa (morte del gruppo), come prima di S2.5f/F2.
pub fn exit_current(code: i64) -> ! {
    exit_target(code, false)
}

fn exit_target(code: i64, whole_process: bool) -> ! {
    if !INITIALIZED.load(Ordering::Acquire) {
        loop {
            hlt();
        }
    }
    let mut guard = SCHED.lock();

    let action: Option<(usize, usize)> = {
        let sched = guard.as_mut().expect("scheduler non inizializzato");
        match sched.current {
            Some(prev) => {
                // Fase 14: morte logica (terminate) + switch via. Il teardown
                // fisico e' differito (`drain_reclaim` in `on_tick`).
                let target = if whole_process { group_of(sched, prev) } else { prev };
                sched.terminate(target, code);
                match sched.pick_next() {
                    Some(n) if n != prev => Some((prev, n)),
                    _ => None,
                }
            }
            None => None,
        }
    };

    match action {
        Some((prev, next)) => {
            switch_to(Some(prev), next, guard);
            loop {
                hlt();
            }
        }
        None => {
            drop(guard);
            loop {
                hlt();
            }
        }
    }
}

/// Leader del gruppo di `pid` (S-T): se' stesso per processi e leader.
/// Chiamare a lock tenuto.
fn group_of(sched: &Scheduler, pid: usize) -> usize {
    sched.processes[pid].thread_group.unwrap_or(pid)
}

/// `kill(pid, code)`: termina un processo user per la stessa via di `exit`
/// (cleanup differito + cascata sulla discendenza + notifica al parent).
/// S-T: il target e' sempre il GRUPPO (un tid risolve al leader: POSIX kill
/// non conosce i thread, `terminate(leader)` uccide anche i thread).
/// Killabile: qualunque processo user tranne init (pid 1), i processi kernel
/// (cr3 = kernel_cr3) e se stesso (per se' usare `exit`). Ritorna `true` se
/// il processo e' stato terminato.
pub fn kill(pid: usize, code: i64) -> bool {
    if !INITIALIZED.load(Ordering::Acquire) {
        return false;
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");

    if pid >= sched.processes.len() || pid == 1 || sched.current == Some(pid) {
        return false;
    }
    // S-T: kill di un tid = kill del gruppo (il gate parent/init a monte
    // vede solo il leader: i thread hanno parent None, mai killabili
    // direttamente se non da init).
    let pid = group_of(sched, pid);
    if sched.current == Some(pid) {
        return false; // il gruppo del chiamante: per se' usare `exit`
    }
    let p = &sched.processes[pid];
    if p.state == State::Terminated {
        return false;
    }
    if p.cr3 == crate::arc::vmm_user::kernel_cr3() {
        return false; // processo kernel (solo idle oltre init)
    }
    let name = name_buf(p);
    sched.terminate(pid, code);
    crate::serial_println!("[kill ] pid {} '{}' ucciso (code {})", pid, name_str_of(&name), code);
    true
}

/// `suspend(pid)` (Fase 44a, job control): congela un processo user (non piu'
/// schedulato) finche' `resume`. S-T: congela TUTTO il gruppo (leader +
/// thread: un thread che gira mentre il leader e' stoppato violerebbe lo
/// stop di job control). Meccanismo neutro (ADR-0025): niente segnali
/// numerati, solo fuori/dentro le ready queue. Gate come `kill`: qualunque
/// processo user tranne init (pid 1), i processi kernel e se stesso.
/// Idempotente (doppio suspend = ok). Ritorna `true` se il processo e'
/// sospeso (o gia' sospeso).
pub fn suspend(pid: usize) -> bool {
    if !INITIALIZED.load(Ordering::Acquire) {
        return false;
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");

    if pid >= sched.processes.len() || pid == 1 || sched.current == Some(pid) {
        return false;
    }
    // S-T: il target e' il gruppo (come kill).
    let leader = group_of(sched, pid);
    if sched.current == Some(leader)
        || sched.processes[leader].state == State::Terminated
        || sched.processes[leader].cr3 == crate::arc::vmm_user::kernel_cr3()
    {
        return false;
    }
    let name = name_buf(&sched.processes[leader]);
    for i in 0..sched.processes.len() {
        let member = i == leader || sched.processes[i].thread_group == Some(leader);
        if !member || sched.processes[i].state == State::Terminated {
            continue;
        }
        sched.processes[i].suspended = true;
        if sched.processes[i].state == State::Ready {
            // Fuori dalle ready queue; se Blocked non c'e' da togliere nulla
            // (i wake via `set_ready` lo saltano, i messaggi restano in coda).
            sched.clear_ready(i);
        }
    }
    crate::serial_println!("[job  ] pid {} '{}' sospeso (gruppo)", leader, name_str_of(&name));
    true
}

/// `resume(pid)` (Fase 44a, job control): rimette in schedulazione un processo
/// sospeso. S-T: tutto il gruppo (speculare a `suspend`). Stessi gate di
/// `suspend` (parent/init a monte, qui i controlli di esistenza/vitalita').
/// Idempotente (resume di un running = ok, no-op). Un bloccato in `recv`
/// con messaggi in coda si sveglia subito; un bloccato con coda vuota (o in
/// attesa di reply/futex) resta bloccato e i waker futuri lo riaggiungono
/// (ora `set_ready` funziona di nuovo).
pub fn resume(pid: usize) -> bool {
    if !INITIALIZED.load(Ordering::Acquire) {
        return false;
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");

    if pid >= sched.processes.len() || pid == 1 || sched.current == Some(pid) {
        return false;
    }
    // S-T: il target e' il gruppo (come kill/suspend).
    let leader = group_of(sched, pid);
    if sched.current == Some(leader)
        || sched.processes[leader].state == State::Terminated
        || sched.processes[leader].cr3 == crate::arc::vmm_user::kernel_cr3()
    {
        return false;
    }
    let name = name_buf(&sched.processes[leader]);
    // Resume di gruppo: ogni membro torna alla sua disciplina (Ready in
    // coda, Blocked resta ai waker — stessa logica del singolo, per membro).
    let mut any = false;
    for i in 0..sched.processes.len() {
        let member = i == leader || sched.processes[i].thread_group == Some(leader);
        if !member
            || sched.processes[i].state == State::Terminated
            || !sched.processes[i].suspended
        {
            continue;
        }
        any = true;
        sched.processes[i].suspended = false;
        if sched.processes[i].ipc_state == crate::ordo::process::IpcState::BlockedOnRecv
            && !sched.processes[i].msg_queue.is_empty()
        {
            sched.processes[i].ipc_state = crate::ordo::process::IpcState::None;
            sched.processes[i].state = State::Ready;
            sched.set_ready(i);
        } else if sched.processes[i].state == State::Ready {
            sched.set_ready(i);
        }
        // Blocked (coda vuota o attesa reply/futex): resta; i waker futuri
        // lo riprendono (set_ready funziona di nuovo).
    }
    crate::serial_println!("[job  ] pid {} '{}' ripreso (gruppo)", leader, name_str_of(&name));
    // Idempotente: gruppo gia' running = ok (come il singolo).
    let _ = any;
    true
}

impl Scheduler {
    /// Morte logica del processo `pid` (Fase 14, ADR-0010): marca
    /// `Terminated`, sblocca i mittenti sincroni che attendevano una reply da
    /// `pid`, enumera i peer da notificare (coppie peer/channel salvate nel
    /// PCB per il reclaim), termina la discendenza NON-detached in cascata e
    /// ri-parenta a init i figli detached (Fase 22), libera canali/servizi/CBS
    /// e accoda il processo al reclaim. Il teardown fisico (stack/TSS/address
    /// space) e la notifica EXIT ai peer sono differiti a `drain_reclaim`. Se `pid` e' il processo corrente, il chiamante deve
    /// poi fare lo switch (vedi `exit_current`).
    /// Il gruppo ha thread vivi oltre `pid`? (S-T: scansione O(n) sul cold
    /// path del reclaim; niente contatori da mantenere coerenti.)
    fn group_has_live(&self, leader: usize, except: usize) -> bool {
        for (i, p) in self.processes.iter().enumerate() {
            if i != except
                && p.state != State::Terminated
                && (i == leader || p.thread_group == Some(leader))
            {
                return true;
            }
        }
        false
    }

    pub(super) fn terminate(&mut self, pid: usize, code: i64) {
        if pid >= self.processes.len() || self.processes[pid].state == State::Terminated {
            return;
        }
        // init e' la radice della process tree: non deve mai morire.
        if pid == 1 {
            panic!("init terminato (pid 1)");
        }

        // S-T: morte di UN thread (non l'ultimo per costruzione: la morte
        // del leader uccide prima tutti i thread, vedi sotto). Via breve:
        // niente cascata (parent None), niente canali/CBS/peer del leader
        // (restano al gruppo finche' vive), solo wake dei mittenti su `pid`.
        if let Some(leader) = self.processes[pid].thread_group {
            let name = name_buf(&self.processes[leader]);
            {
                let p = &mut self.processes[pid];
                p.state = State::Terminated;
                p.suspended = false;
                p.exit_code = code;
                p.ipc_state = crate::ordo::process::IpcState::None;
                p.reply_chan = None;
                p.reply_req = 0;
                p.reply_slot = None;
                p.waiting_pid = None;
                p.waiting_req = 0;
                p.pending_wake = false;
            }
            self.clear_ready(pid);
            self.wake_senders(pid);
            super::futex::purge_tid(self, pid);
            self.push_reclaim(pid);
            crate::serial_println!(
                "[proc ] thread pid {} del gruppo '{}' terminato (code {})",
                pid, name_str_of(&name), code
            );
            return;
        }
        // S-T: morte del leader = morte del gruppo: prima tutti i thread
        // vivi (via breve sopra, niente ricorsione oltre un livello), poi
        // la via completa (canali/CBS/peer/cascata del processo).
        for tid in 0..self.processes.len() {
            let t = self.processes[tid].thread_group == Some(pid)
                && self.processes[tid].state != State::Terminated;
            if t {
                self.terminate(tid, code);
            }
        }

        let name = name_buf(&self.processes[pid]);

        {
            let p = &mut self.processes[pid];
            p.state = State::Terminated;
            // Il morto non torna (44a): azzera il flag cosi' nessun percorso
            // (wake/reuse) lo vede mai insieme a Terminated.
            p.suspended = false;
            p.exit_code = code;
            p.ipc_state = crate::ordo::process::IpcState::None;
            p.reply_chan = None;
            p.reply_req = 0;
            p.reply_slot = None;
            p.waiting_pid = None;
                p.waiting_req = 0;
            p.pending_wake = false;
        }
        self.clear_ready(pid);
        crate::ordo::aegis::release_pid(pid);

        // Niente notifica QUI: sblocca solo i mittenti bloccati su `pid` e
        // accoda il reclaim. Le notifiche EXIT ai peer avvengono in
        // `reclaim_one`, DOPO il teardown: cosi' quando un peer si sveglia
        // (es. il parent che spawa di nuovo nel churn) le risorse
        // (PID/TSS/frame) sono gia' libere e il pool non si esaurisce.
        self.wake_senders(pid);

        // Cascata: tutta la discendenza NON-detached muore con il capostipite.
        // I figli detached (Fase 22, flag dello spawner) sopravvivono e
        // vengono ri-parentati a init (pid 1, che non muore mai): niente
        // orfani con parent morto, niente cascata oltre il flag.
        for child in 0..self.processes.len() {
            if child == pid {
                continue;
            }
            let is_child = self.processes[child].state != State::Terminated
                && self.processes[child].parent == Some(pid);
            if !is_child {
                continue;
            }
            if self.processes[child].detached {
                self.processes[child].parent = Some(1);
                let name = name_buf(&self.processes[child]);
                crate::serial_println!(
                    "[proc ] '{}' pid {} detached: ri-parentato a init (morto parent {})",
                    name_str_of(&name), child, pid
                );
            } else {
                self.terminate(child, code);
            }
        }

        // Notifica unificata: enumera le coppie (peer, channel) PRIMA di
        // `release_pid` (che rimuove i canali) e salvale nel PCB del morente:
        // `reclaim_one` le consuma DOPO il teardown. Il parent e' uno dei peer
        // (la sua coppia porta il birth channel): nessun caso speciale.
        let (peers, npeer) = crate::relay::channels::enumerate_peers(pid);
        {
            let p = &mut self.processes[pid];
            p.die_peers = peers;
            p.die_peer_count = npeer;
        }

        // Canali e slot servizi del morto (prima che il pid torni nel free-set
        // a reclaim).
        crate::relay::channels::release_pid(pid);

        self.push_reclaim(pid);
        crate::serial_println!(
            "[proc ] '{}' pid {} terminato (code {}), reclaim accodato",
            name_str_of(&name), pid, code
        );
    }

    /// Sblocca i processi bloccati in `send` sincrono in attesa di una reply
    /// dal morente `pid`: tornano da `ipc_send` con errore e possono rifare
    /// un `service_lookup` (niente deadlock client-su-servizio-morto).
    pub(super) fn wake_senders(&mut self, pid: usize) {
        for i in 0..self.processes.len() {
            if i == pid {
                continue;
            }
            let blocked_on_dead = self.processes[i].state == State::Blocked
                && self.processes[i].ipc_state == crate::ordo::process::IpcState::BlockedOnReply
                && self.processes[i].waiting_pid == Some(pid);
            if blocked_on_dead {
                let p = &mut self.processes[i];
                p.ipc_state = crate::ordo::process::IpcState::None;
                p.waiting_pid = None;
                p.waiting_req = 0;
                p.reply_slot = None;
                p.state = State::Ready;
                self.set_ready(i);
            }
        }
    }

    /// Teardown differito dei processi in coda di reclaim: libera lo stack
    /// kernel, lo slot TSS e (per i processi user) l'address space (foglie
    /// `owned` + page table), poi rimette il PID nel free-set per il riuso.
    /// Gira da `on_tick`: `current` e' sempre un processo vivo, quindi non si
    /// libera mai lo stack del processo su cui si sta eseguendo.
    pub(super) fn drain_reclaim(&mut self) {
        // Passata limitata (S-T): ogni item al piu' una volta per drain. Il
        // leader con thread vivi ruota in coda (riprova al prossimo tick):
        // mai spin sul posto, reclaim eventuale alla morte dei thread (un
        // gruppo con thread vivi E' vivo per definizione).
        let mut n = self.reclaim_len;
        while n > 0 {
            n -= 1;
            let pid = self.reclaim_q[self.reclaim_head];
            let leader_wait = pid < self.processes.len()
                && self.processes[pid].thread_group.is_none()
                && self.processes[pid].state == State::Terminated
                && self.group_has_live(pid, pid);
            if leader_wait {
                self.reclaim_head = (self.reclaim_head + 1) % MAX_PIDS;
                self.reclaim_q[(self.reclaim_head + self.reclaim_len - 1) % MAX_PIDS] = pid;
                continue;
            }
            self.reclaim_head = (self.reclaim_head + 1) % MAX_PIDS;
            self.reclaim_len -= 1;
            self.reclaim_one(pid);
        }
    }

    /// Teardown di un singolo PID Terminated (skip se non reclamabile).
    pub(super) fn reclaim_one(&mut self, pid: usize) {
        if pid >= self.processes.len() {
            return;
        }
        if self.current == Some(pid) {
            return; // mai liberare il processo in esecuzione
        }
        if self.processes[pid].state != State::Terminated {
            return;
        }

        // S-T: teardown di UN thread — solo stack+TSS+PID. Niente mm (cr3
        // condiviso: lo chiude il gruppo), niente text (condivisa), niente
        // notify (solo morte di gruppo), niente canali/CBS (del leader).
        if self.processes[pid].thread_group.is_some() {
            let (stack_base, tss_slot, exit_code) = {
                let p = &self.processes[pid];
                (p.stack_base, p.tss_slot, p.exit_code)
            };
            crate::arc::phys_mem::free_contiguous(stack_base, crate::ordo::process::STACK_FRAMES);
            crate::gdt::free_tss_slot(tss_slot);
            self.free_pids |= 1u128 << pid;
            crate::serial_println!(
                "[reap ] thread pid {} reclamato (code {})",
                pid, exit_code
            );
            return;
        }

        let (die_peers, npeer, name, stack_base, cr3, exit_code, tss_slot, text_id) = {
            let p = &self.processes[pid];
            (p.die_peers, p.die_peer_count, name_buf(p), p.stack_base, p.cr3, p.exit_code, p.tss_slot, p.text_id)
        };

        crate::arc::phys_mem::free_contiguous(stack_base, crate::ordo::process::STACK_FRAMES);
        crate::gdt::free_tss_slot(tss_slot);

        let is_user = cr3 != crate::arc::vmm_user::kernel_cr3();
        if is_user {
            unsafe { crate::arc::vmm_user::teardown_user_space(cr3, pid) };
        }
        // Fase 32: rilascia la text image condivisa DOPO il teardown (il walk
        // non libera le foglie non-owned; a refcount 0 i frame sono liberati).
        if text_id != 0 {
            crate::text::release(text_id);
        }

        // Notifica EXIT UNIFICATA a tutti i peer DOPO il teardown: parent,
        // client e server ricevono tutti lo stesso messaggio sul canale che
        // li collegava al morente (single path). Quando un peer si sveglia le
        // risorse del morto sono gia' liberate (pool non esauribile nei loop
        // spawn/exit). Peer Terminated (cascata) skippati.
        for i in 0..npeer {
            let (peer_u32, chan_u32) = die_peers[i];
            let peer = peer_u32 as usize;
            if peer >= self.processes.len()
                || self.processes[peer].state == State::Terminated
            {
                continue;
            }
            let msg = crate::ordo::process::PendingMsg {
                channel: chan_u32 as usize,
                req_id: 0,
                tag: syscall_numbers::EXIT_NOTIFY,
                w0: exit_code as u64,
                w1: pid as u64,
            };
            if self.processes[peer].msg_queue.try_push(msg) {
                if self.processes[peer].ipc_state
                    == crate::ordo::process::IpcState::BlockedOnRecv
                {
                    self.processes[peer].ipc_state = crate::ordo::process::IpcState::None;
                    self.processes[peer].state = State::Ready;
                    self.set_ready(peer);
                }
            } else {
                crate::serial_println!(
                    "[reap ] exit-notify per peer {} persa (coda piena), morto {}",
                    peer, pid
                );
            }
        }

        // Il PID torna disponibile SOLO ora: canali/servizi/CBS del morto sono
        // gia' stati rilasciati da `terminate`.
        self.free_pids |= 1u128 << pid;

        crate::serial_println!(
            "[reap ] '{}' pid {} reclamato{}: frame liberi = {}",
            name_str_of(&name),
            pid,
            if is_user { " (addr space)" } else { "" },
            crate::arc::phys_mem::free_frames()
        );
    }
}
