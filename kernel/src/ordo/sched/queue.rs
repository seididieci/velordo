// Split from ordo/sched.rs (byte-identical move; see facade).
use super::*;
use crate::*;
use alloc::vec::Vec;
use crate::ordo::process::Process;

pub(super) struct Scheduler {
    pub(super) processes: Vec<Process>,
    pub(super) current: Option<usize>,
    pub(super) ticks_current: u64,
    /// Cursore round-robin PER LIVELLO: ultimo PID scelto al livello p. La
    /// rotazione riparte dal bit successivo all'ultimo scelto A QUESTO
    /// LIVELLO (vero round-robin sul sottoinsieme presente). Un contatore
    /// globale condiviso tra sottoinsiemi diversi NON e' equo: con cicli IPC
    /// deterministici il cursore si aggancia in fase e un membro muore di fame
    /// per sempre (osservato sotto KVM: pid 7 mai scelto tra {4,7}/{7,8}/{7,9}
    /// per parita' bloccata — 968+484+484 pick senza mai 7 — mentre TCG, piu'
    /// lento, rompeva la fase con i pick dei quanti e lo mascherava).
    pub(super) rr_cursor: [u32; 32],
    pub(super) next_id: usize,
    /// free_pids: bit i = PID i libero (processo reclamato, riusabile).
    /// `u128`: il pool e' MAX_PIDS=128 (S-T), i bit oltre restano a zero.
    pub(super) free_pids: u128,
    /// Coda di reclaim (Fase 14): PID Terminated in attesa di teardown
    /// differito. Fixed-size (max `MAX_PIDS`): nessuna allocazione nel
    /// percorso di exit/kill.
    pub(super) reclaim_q: [usize; MAX_PIDS],
    pub(super) reclaim_head: usize,
    pub(super) reclaim_len: usize,
    /// ready_by_prio[p] : bit i = entita' i pronta al livello p (S-T:
    /// processi + thread). I 32 livelli di priorita' restano (`u32` mask).
    pub(super) ready_by_prio: [u128; 32],
    /// ready_prio_mask : bit p = almeno un processo pronto al livello p.
    pub(super) ready_prio_mask: u32,
    /// Waiter futex (S-T T3): tabella fissa sotto questo lock (niente
    /// ordinamenti tra lock, mai lost-wakeup: check-then-block atomici).
    pub(super) futex_waiters: [super::futex::FutexWaiter; super::futex::FUTEX_MAX_WAITERS],
}

impl Scheduler {
    pub(super) fn new() -> Self {
        Self {
            // Pre-alloca la capacita' massima a init (unica alloc del Vec,
            // mai piu' realloc a runtime): `place_process` sovrascrive a PID
            // riusato e fa `push` solo in crescita, i PID sono ≤ MAX_PIDS-1
            // riusati dal free-set (Fase 14) e mai rimossi → capacita'
            // monotona, path spawn senza alloc dopo il warmup. Se MAX_PIDS
            // cresce (growth path oltre 32, vedi AGENTS) la capacita' segue
            // la costante da sola.
            processes: Vec::with_capacity(MAX_PIDS),
            current: None,
            ticks_current: 0,
            rr_cursor: [0u32; 32],
            next_id: 0,
            free_pids: 0,
            reclaim_q: [0; MAX_PIDS],
            reclaim_head: 0,
            reclaim_len: 0,
            ready_by_prio: [0u128; 32],
            ready_prio_mask: 0,
            futex_waiters: [super::futex::FutexWaiter::empty(); super::futex::FUTEX_MAX_WAITERS],
        }
    }

    /// Alloca un PID: riusa prima i PID liberati dal reclaim, poi cresce
    /// fino a `MAX_PIDS`. `None` se tutti i 128 sono occupati.
    pub(super) fn alloc_pid(&mut self) -> Option<usize> {
        if self.free_pids != 0 {
            let pid = self.free_pids.trailing_zeros() as usize;
            self.free_pids &= !(1u128 << pid);
            return Some(pid);
        }
        if self.next_id < MAX_PIDS {
            let pid = self.next_id;
            self.next_id += 1;
            return Some(pid);
        }
        None
    }

    /// Inserisce (o sovrascrive, se il PID era riusato) il processo.
    /// S2.5b (FPU eager): assegna qui l'area fxsave del pool e la
    /// inizializza a `fninit` pulito — UNICO punto di assegnazione (ogni
    /// riuso pid riparte pulito; mai indirizzi stack-dipendenti).
    pub(super) fn place_process(&mut self, id: usize, mut process: Process) {
        process.saved.fpu = crate::ordo::context::fpu_slot(id);
        unsafe { crate::ordo::context::fpu_init(process.saved.fpu as *mut [u128; 32]) };
        if id < self.processes.len() {
            self.processes[id] = process;
        } else {
            self.processes.push(process);
        }
    }

    /// Rimette a disposizione un PID allocato ma non utilizzato (spawn
    /// fallito dopo `alloc_pid`).
    pub(super) fn release_pid(&mut self, pid: usize) {
        self.free_pids |= 1u128 << pid;
    }

    pub(super) fn push_reclaim(&mut self, pid: usize) {
        if self.reclaim_len >= MAX_PIDS {
            // Coda satura (cascata massiccia): libera subito i gia' pronti.
            self.drain_reclaim();
        }
        if self.reclaim_len >= MAX_PIDS {
            crate::serial_println!("[sched] reclaim queue piena (pid {})", pid);
            return;
        }
        let tail = (self.reclaim_head + self.reclaim_len) % MAX_PIDS;
        self.reclaim_q[tail] = pid;
        self.reclaim_len += 1;
    }

    /// Marca pronto (Fase 44a: salta i sospesi — i wake lasciano i messaggi
    /// in coda senza risvegliare; il resume rientra esplicitamente). Unico
    /// choke point di tutti i risvegli (IPC, IRQ, tick, resume).
    pub(super) fn set_ready(&mut self, pid: usize) {
        if pid < MAX_PIDS && pid < self.processes.len() {
            if self.processes[pid].suspended {
                return;
            }
            let p = self.processes[pid].priority.0 as usize;
            self.ready_by_prio[p] |= 1u128 << pid;
            self.ready_prio_mask |= 1u32 << p;
        }
    }

    pub(super) fn clear_ready(&mut self, pid: usize) {
        if pid < MAX_PIDS {
            if let Some(proc) = self.processes.get(pid) {
                let p = proc.priority.0 as usize;
                self.ready_by_prio[p] &= !(1u128 << pid);
                if self.ready_by_prio[p] == 0 {
                    self.ready_prio_mask &= !(1u32 << p);
                }
            }
        }
    }

    /// Selezione pura (38.2d): chi `pick_next` sceglierebbe ADESSO, senza
    /// avanzare il cursore RR. Serve a `notify_irq` per la wakeup-preemption:
    /// chiamare `pick_next` a vuoto perturberebbe la rotazione anche quando
    /// non si cambia contesto. Stesso algoritmo di `pick_next`, zero effetti.
    pub(super) fn select_next(&self) -> Option<usize> {
        if self.ready_prio_mask == 0 {
            return None;
        }
        let p = 31 - self.ready_prio_mask.leading_zeros() as usize;
        let mask = self.ready_by_prio[p];
        let k = (self.rr_cursor[p] + 1) % MAX_PIDS as u32;
        let rot = mask.rotate_right(k);
        let j = rot.trailing_zeros() as usize;
        Some((j + k as usize) % MAX_PIDS)
    }

    pub(super) fn pick_next(&mut self) -> Option<usize> {
        let bit = self.select_next()?;
        // (mask != 0 per invariante: il bit p di ready_prio_mask e' alto solo
        // se la word del livello non e' vuota — mantenuto sotto lock.)
        let p = 31 - self.ready_prio_mask.leading_zeros() as usize;
        self.rr_cursor[p] = bit as u32;
        Some(bit)
    }
}
