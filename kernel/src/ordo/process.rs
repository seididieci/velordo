//! Process Control Block: descrive un singolo processo utente/kernel.

use x86_64::structures::gdt::SegmentSelector;
use crate::ordo::context::CpuContext;

/// Stack kernel di un processo (inizializzato nel frattempo, fuori dall'heap
/// affinche' non venga mai spostato). Grandezza fissa in frame fisici.
pub const STACK_FRAMES: usize = 4; // 4 × 4 KiB = 16 KiB

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Pronto per essere schedulato.
    Ready,
    /// In attesa di un evento (es. scancode in coda).
    Blocked,
    /// Finito (non piu' schedulabile).
    Terminated,
}

/// Messaggio IPC (ADR-0008): contenuto registro-based trasportato da una
/// `send` su canale a un `recv`. Riposto nel PCB del ricevente (mai in PERCPU
/// perche' e' una zona transitoria single-slot).
#[derive(Clone, Copy, Debug)]
pub struct PendingMsg {
    /// Canale su cui il messaggio e' arrivato (identifica il client quando un
    /// server serve piu' canali). Sostituisce il `sender` per-PID.
    pub channel: usize,
    /// Request-id del messaggio (Fase 13, IPC async). Assegnato dal MITTENTE
    /// (`req_next`): `>= 0` = richiesta; `< 0` = risposta async a `-req_id`.
    pub req_id: i64,
    pub tag: u64,
    pub w0: u64,
    pub w1: u64,
}

/// Risposta IPC in viaggio verso un mittente che sta aspettando la `reply` del
/// suo server. E' il corpo della risposta; il destinatario e' implicito (sta
/// nel `reply_slot` del processo che la riceve).
#[derive(Clone, Copy, Debug)]
pub struct PendingReply {
    pub tag: u64,
    pub w0: u64,
    pub w1: u64,
}

/// Coda circulara a dimensione fissa per i messaggi IPC in entrata.
/// Embeddita nel PCB (nessuna heap allocation), O(1) push/pop.
const MSG_QUEUE_CAP: usize = 8;

pub struct MsgQueue {
    buf: [PendingMsg; MSG_QUEUE_CAP],
    head: usize,
    len: usize,
}

impl MsgQueue {
    pub const fn new() -> Self {
        Self {
            buf: [PendingMsg { channel: 0, req_id: 0, tag: 0, w0: 0, w1: 0 }; MSG_QUEUE_CAP],
            head: 0,
            len: 0,
        }
    }

    pub fn push(&mut self, msg: PendingMsg) {
        if self.len >= MSG_QUEUE_CAP {
            return; // coda piena — non dovrebbe accadere in pratica
        }
        let tail = (self.head + self.len) % MSG_QUEUE_CAP;
        self.buf[tail] = msg;
        self.len += 1;
    }

    /// Come `push`, ma ritorna `false` se la coda e' piena invece di scartare
    /// in silenzio. Usato da `send_async` per dare backpressure al mittente
    /// (Fase 13): un frame non consegnato → errore, niente messaggi persi.
    pub fn try_push(&mut self, msg: PendingMsg) -> bool {
        if self.len >= MSG_QUEUE_CAP {
            return false;
        }
        let tail = (self.head + self.len) % MSG_QUEUE_CAP;
        self.buf[tail] = msg;
        self.len += 1;
        true
    }

    pub fn pop(&mut self) -> Option<PendingMsg> {
        if self.len == 0 {
            return None;
        }
        let msg = self.buf[self.head];
        self.head = (self.head + 1) % MSG_QUEUE_CAP;
        self.len -= 1;
        Some(msg)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Stato IPC di un processo: nessuna attesa, oppure bloccato in `recv` o in
/// attesa di una `reply`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcState {
    None,
    /// Sto aspettando un messaggio (sono in `recv`).
    BlockedOnRecv,
    /// Ho fatto `send` e aspetto la `reply` del destinatario.
    BlockedOnReply,
}

/// Entry di un processo: ciclo infinito, mai ritornare.
pub type ProcessFn = unsafe extern "C" fn() -> !;

/// Massimo numero di peer distinti notificabili alla morte di un processo
/// (Fase 14, notifica unificata): i peer sono PID diversi da se' stesso, con
/// max 32 PID concorrenti → 31 e' un bound provabile (nessuna policy di
/// overflow necessaria). Piu' canali verso lo stesso peer collassano in una
/// sola entry (first-channel-wins): una notifica per peer basta.
pub const MAX_NOTIFY_PEERS: usize = 31;

pub struct Process {
    pub name: &'static str,
    /// Nome owned per i processi da `spawn_image` (Fase 21, servizi da disco:
    /// il nome arriva dal chiamante, non dalla tabella statica). `name_len=0`
    /// = usa `name`; altrimenti i primi `name_len` byte di `name_owned`
    /// (NUL-trimmed, max 16). Lo slot PCB e' riusato ai reclaim: niente leak.
    pub name_owned: [u8; 16],
    pub name_len: u8,
    /// Priorita' BASE del processo (31 = massima, 0 = minima/idle).
    /// Immutabile. `pick_next` sceglie il livello piu' alto via bitmask.
    pub priority: crate::ordo::sched::Priority,
    pub state: State,
    /// Sospeso via `SYS_SUSPEND` (Fase 44a, job control): fuori dalle ready
    /// queue finche' `SYS_RESUME` (meccanismo neutro, semantica POSIX in
    /// shell). Ortogonale a `state`/`ipc_state`: i wake (`set_ready`) lo
    /// saltano e i messaggi restano in coda; al resume si rientra in Ready
    /// (o si resta Blocked se la coda e' ancora vuota). `ps` lo mostra come
    /// Stopped; `terminate` lo ignora (il morto non torna).
    pub suspended: bool,
    /// Processo padre (chi ha creato questo processo via `spawn`). `None` per i
    /// processi creati direttamente dal kernel (es. init, idle).
    pub parent: Option<usize>,
    /// Detached dalla cascata di morte (Fase 22): alla morte del parent NON
    /// termina in cascata ma viene ri-parentato a init. Deciso dallo spawner
    /// via flag `SPAWN_FLAG_DETACH` (il figlio non puo' auto-staccarsi);
    /// irrevocabile. Inerte per i figli di init (init non muore mai).
    pub detached: bool,
    /// Frame dello stack user (`USER_STACK_FRAMES`, o `BIGSTACK_FRAMES` col
    /// flag S2.5d). Ereditato dal fork, preservato dall'exec (che rimappa lo
    /// stack nuovo con la stessa profondita'). Inerte per i thread (stack
    /// proprio, mai rimappato qui).
    pub stack_frames: u32,
    /// Indirizzo base (basso) dello stack kernel, per un futuro rilascio.
    pub stack_base: u64,
    /// CR3 (page table) del processo. Per i processi kernel e' la CR3 di base.
    pub cr3: u64,
    /// Top dello stack kernel usato come RSP0 del TSS quando il processo
    /// gira in user mode (per processi kernel coincide con `stack_top`).
    pub kernel_stack_top: u64,
    /// Contesto CPU salvato (puntato anche dall'assembly, mai spostato).
    pub saved: CpuContext,
    /// Selettore GDT del TSS per-processo (RSP0 + I/O bitmap). Caricato con
    /// `ltr` a ogni context switch (ADR-0006).
    pub tss_sel: SegmentSelector,
    /// Slot del TSS nel pool (1..MAX_TSS_SLOTS-1). Distinto dal `tss_sel`
    /// (indice GDT = base + slot): serve a `free_tss_slot` al reclaim
    /// (Fase 14).
    pub tss_slot: usize,
    /// Stato IPC corrente (Fase 7 / ADR-0008).
    pub ipc_state: IpcState,
    /// Messaggi in coda per questo processo (da `send` su canale in attesa di
    /// `recv`). Ogni messaggio porta il channel sorgente (ADR-0008).
    pub msg_queue: MsgQueue,
    /// Canale di nascita verso il parent (ADR-0008): il figlio lo riceve alla
    /// creazione e lo usa come "canale 0" per parlare col parent. `None` per i
    /// processi kernel (init/idle) che non hanno un parent user.
    pub parent_chan: Option<usize>,
    /// Il canale del messaggio che questo processo sta correntemente
    /// elaborando (impostato da `recv`): la prossima `reply` risponde su quel
    /// canale. Con piu' client concorrenti, il server risponde al messaggio che
    /// ha appena ricevuto (fix 9.2.2, generalizzato a canali).
    pub reply_chan: Option<usize>,
    /// Il `req_id` del messaggio correntemente elaborato (salvato da `recv`
    /// insieme a `reply_chan`, Fase 13). La `reply` del server a un client
    /// async accoda una risposta con `req_id = -reply_req`.
    pub reply_req: i64,
    /// Contatore per il prossimo request-id: ogni `send`/`send_async` di questo
    /// processo assegna `req_id = req_next` poi incrementa (Fase 13).
    pub req_next: u64,
    /// La risposta che sto aspettando (riempita dal server alla `reply`).
    pub reply_slot: Option<PendingReply>,
    /// Evento di risveglio arrivato mentre il processo NON era ancora Bloccato
    /// (race producer/consumer dei wait da IRQ, es. kbd): settato da `wake`,
    /// consumato da `block_current`. Evita che il blocco perda il wake.
    pub pending_wake: bool,
    /// Indice del server CBS associato a questo processo (`None` = nessun
    /// server). Sempre presente: lo scheduler RT unico supporta il CBS.
    pub cbs_server: Option<usize>,
    /// Codice di uscita (Fase 14): significativo quando `state == Terminated`.
    pub exit_code: i64,
    /// Pid sul quale questo processo e' bloccato in attesa di una reply
    /// (`ipc_state == BlockedOnReply`, Fase 14). Usato per svegliare i mittenti
    /// sincroni quando il destinatario muore, evitando il deadlock client-su-
    /// servizio-morto.
    pub waiting_pid: Option<usize>,
    /// Request-id dell'invio su cui siamo bloccati (S-T T5): la reply del
    /// server sveglia il thread GIUSTO del gruppo (stesso canale, req
    /// diverse: senza, la reply andrebbe all'endpoint-leader e il thread
    /// resterebbe appeso). Assegnato dal contatore del leader (unico nel
    /// gruppo), matchato alla reply.
    pub waiting_req: i64,
    /// Coppie `(peer, channel)` da notificare con `EXIT_NOTIFY` al reclaim
    /// (notifica unificata, Fase 14): enumerate in `terminate` prima di
    /// `release_pid`, consumate in `reclaim_one` dopo il teardown. Solo i
    /// primi `die_peer_count` elementi sono validi.
    pub die_peers: [(u32, u32); MAX_NOTIFY_PEERS],
    /// Numero di entry valide in `die_peers`.
    pub die_peer_count: usize,
    /// Tick timer consumati dal processo (Fase 19.1, colonna TIME di `ps`):
    /// incrementato in `on_tick` per il processo corrente.
    pub ticks_used: u64,
    /// Id della text image condivisa usata da questo processo (Fase 32,
    /// `crate::text`): 0 = nessuna (load privato o slot pieni). Rilasciato in
    /// `reclaim_one` dopo il teardown (i frame condivisi non sono owned).
    pub text_id: u32,
    /// Identita' misurata dell'immagine ELF (Fase 36, Strato 2 di ADR-0026):
    /// FNV-1a (`syscall_numbers::image_hash`) sui byte caricati allo spawn.
    /// 0 = nessuna immagine (processi kernel). Ereditato alla copia (stessi
    /// byte). Meccanismo neutro: il kernel misura ed espone (36.2), la policy
    /// vive fuori (init manifest, `FS_REGISTER` in cardo).
    pub image_hash: u64,
    /// Gruppo thread (S-T, ADR-0046): `Some(leader)` se questo PCB e' un
    /// thread 1:1 (condivide cr3/canali/fd/heap/VMA del leader, `None`
    /// per processi e leader. Il teardown mm/canali/notifiche avviene solo
    /// all'uscita dell'ULTIMO del gruppo (conteggio per scansione in
    /// `reclaim_one`: il reclaim e' cold path, niente contatori da
    /// mantenere).
    pub thread_group: Option<usize>,
    /// Base TLS (FS) del thread (S-T): programmata a ogni switch (MSR
    /// FS_BASE). 0 = nessuna (processi storici e thread senza TLS).
    pub fs_base: u64,
    /// Esito dell'ultimo FUTEX_WAIT (S-T T3): `None` = mai dormito/spuria,
    /// `Some(true)` = svegliato da WAKE, `Some(false)` = deadline. Scritto
    /// dallo svegliante (wake/deadline sweep) sotto lock, letto dal waiter
    /// al risveglio. Resettato a ogni WAIT.
    pub futex_woken: Option<bool>,
    /// Directory di lavoro (S1.1): vive sul LEADER del gruppo (i thread la
    /// condividono come POSIX; il campo dei thread resta vuoto e ignorato).
    /// Buffer 256B = MAX_PATH cardo; `cwd_len` byte validi, sempre assoluta
    /// (`/` default). Sopravvive a exec (e' persona, non immagine), ereditata
    /// da spawn_copy/spawn. Il kernel non normalizza mai (`..` testuale).
    pub cwd: [u8; 256],
    pub cwd_len: u16,
}

impl Process {
    /// Nome display: l'owned di `spawn_image` se presente, altrimenti lo
    /// static della tabella embedded. Sempre UTF-8 valido (validato in input).
    pub fn name_str(&self) -> &str {
        if self.name_len > 0 {
            let n = (self.name_len as usize).min(16);
            core::str::from_utf8(&self.name_owned[..n]).unwrap_or("?")
        } else {
            self.name
        }
    }

    /// Imposta il nome owned (Fase 21): copia NUL-trimmed, max 16 B.
    pub fn set_owned_name(&mut self, raw: &[u8]) {
        let mut n = raw.len().min(16);
        while n > 0 && raw[n - 1] == 0 {
            n -= 1;
        }
        self.name_owned[..n].copy_from_slice(&raw[..n]);
        for b in self.name_owned[n..].iter_mut() {
            *b = 0;
        }
        self.name_len = n as u8;
    }

    /// Crea un processo **kernel**. `parent` = pid del creatore (albero
    /// processi, radicato in init), `parent_chan` = canale di nascita verso il
    /// creatore (`None` per init/idle/... creati dal kernel).
    pub fn create(
        name: &'static str,
        priority: crate::ordo::sched::Priority,
        entry: ProcessFn,
        parent: Option<usize>,
        parent_chan: Option<usize>,
        io_ranges: &[(u16, u16)],
    ) -> Option<Process> {
        let stack_base = crate::arc::phys_mem::alloc_contiguous(STACK_FRAMES)?;
        // `stack_base` resta PHYS (free a teardown); `stack_top` e' VIRT
        // (direct map: RSP0 del TSS + scritture del frame iniziale).
        let stack_top = crate::addr::phys_to_virt(stack_base + (STACK_FRAMES as u64 * crate::arc::phys_mem::FRAME_SIZE));

        // Stack kernel in 16 KiB: finestra per il frame CPU fittizio e i
        // frame di interrupt annidati.
        let saved = unsafe { crate::ordo::context::new_context(stack_top, entry as usize as u64) };

        let tss_slot = Self::alloc_tss(stack_top, io_ranges)?;
        let tss_sel = crate::gdt::selectors().tss_selector(tss_slot);

        Some(Process {
            name,
            name_owned: [0u8; 16],
            name_len: 0,
            priority,
            state: State::Ready,
            suspended: false,
            parent,
            detached: false,
            stack_frames: crate::arc::vmm_user::USER_STACK_FRAMES as u32,
            stack_base,
            cr3: crate::arc::vmm_user::kernel_cr3(),
            kernel_stack_top: stack_top,
            saved,
            tss_sel,
            tss_slot,
            ipc_state: IpcState::None,
            msg_queue: MsgQueue::new(),
            parent_chan,
            reply_chan: None,
            reply_req: 0,
            req_next: 1,
            reply_slot: None,
            pending_wake: false,
            cbs_server: None,
            text_id: 0,
            image_hash: 0,
            thread_group: None,
            fs_base: 0,
            futex_woken: None,
            cwd: [0u8; 256],
            cwd_len: 0,
            exit_code: 0,
            waiting_pid: None,
            waiting_req: 0,
            die_peers: [(0, 0); MAX_NOTIFY_PEERS],
            die_peer_count: 0,
            ticks_used: 0,
        })
    }

    /// Crea un processo **user** (gira in ring 3, Fase 6.2).
    ///
    /// Alloca il kernel stack (per `RSP0` e i frame di interrupt), crea un
    /// address space dedicato (`new_address_space`), vi carica l'ELF `elf`
    /// per-segmento (Fase 31, `elf::load`: RX/RO/RW + NX, entry dall'ELF) e lo
    /// stack user, e prepara un frame CPU ring 3 (`new_context_user`).
    ///
    /// # Safety
    /// `parent` e' il processo che richiede la creazione (`None` se dal kernel).
    /// `io_ranges` = porte I/O (inclusive) consentite a ring 3 (TSS ADR-0006).
    pub unsafe fn create_user(
        name: &'static str,
        priority: crate::ordo::sched::Priority,
        elf: &[u8],
        parent: Option<usize>,
        parent_chan: Option<usize>,
        io_ranges: &[(u16, u16)],
        detached: bool,
        stack_frames: u32,
        args: Option<&crate::arc::vmm_user::ParsedArgs<'_>>,
    ) -> Option<Process> {
        // Validazione PRIMA di allocare (ELF malformato = nessun leak).
        let layout = crate::elf::validate(elf)?;

        // Identita' misurata (Fase 36): sui byte validati, gli stessi che il
        // loader mappa qui sotto — misura cio' che gira, mai cio' che il
        // chiamante dichiara (nome owned, SpawnMeta).
        let image_hash = syscall_numbers::image_hash(elf);

        // Kernel stack: RSP0 (per rientrare a ring 0 su interrupt) + frame.
        // Come sopra: base PHYS (teardown), top VIRT (RSP0 + frame iniziale).
        let stack_base = crate::arc::phys_mem::alloc_contiguous(STACK_FRAMES)?;
        let stack_top = crate::addr::phys_to_virt(stack_base + (STACK_FRAMES as u64 * crate::arc::phys_mem::FRAME_SIZE));

        // Address space user dedicato (PML4 proprio, kernel condiviso U=0).
        let cr3 = crate::arc::vmm_user::new_address_space()?;

        // Carica i segmenti ELF + stack user. La pagina FS per-processo viene
        // allocata/mappata lazy al primo uso (syscall 26).
        let text_id = unsafe { crate::elf::load(cr3, elf, &layout) };
        let user_stack_top =
            unsafe { crate::arc::vmm_user::setup_user_stack(cr3, stack_frames as usize, args) };

        // Frame CPU ring 3 sul kernel stack (entry dall'ELF).
        let entry = crate::elf::entry(&layout);
        let saved =
            unsafe { crate::ordo::context::new_context_user(stack_top, entry, user_stack_top) };

        let tss_slot = Self::alloc_tss(stack_top, io_ranges)?;
        let tss_sel = crate::gdt::selectors().tss_selector(tss_slot);

        Some(Process {
            name,
            name_owned: [0u8; 16],
            name_len: 0,
            priority,
            state: State::Ready,
            suspended: false,
            parent,
            detached,
            stack_frames,
            stack_base,
            cr3,
            kernel_stack_top: stack_top,
            saved,
            tss_sel,
            tss_slot,
            ipc_state: IpcState::None,
            msg_queue: MsgQueue::new(),
            parent_chan,
            reply_chan: None,
            reply_req: 0,
            req_next: 1,
            reply_slot: None,
            pending_wake: false,
            cbs_server: None,
            text_id,
            image_hash,
            thread_group: None,
            fs_base: 0,
            futex_woken: None,
            cwd: [0u8; 256],
            cwd_len: 0,
            exit_code: 0,
            waiting_pid: None,
            waiting_req: 0,
            die_peers: [(0, 0); MAX_NOTIFY_PEERS],
            die_peer_count: 0,
            ticks_used: 0,
        })
    }

    /// Crea un processo **figlio-copia** (Fase 34): condivide l'address space del
    /// padre in COW (walk a carico del chiamante su `child_cr3`), riprende come
    /// ritorno dalla syscall con `rax = 0` (`saved` punta al fake stack col
    /// trampoline `spawn_copy_child_exit`). Kernel stack, TSS (senza porte: il
    /// figlio non eredita la bitmap I/O del padre, least privilege) e `text_id`
    /// sono del chiamante; IPC/ring/canali NON si ereditano (solo il canale di
    /// nascita, impostato dopo come in `finish_spawn`). Nome, priorita',
    /// `image_hash` (stessi byte del padre, Fase 36) e `req_next` (i req_id
    /// divergono dopo la copia) copiati dal padre.
    ///
    /// # Safety
    /// `child_cr3`/`stack_base`/`saved` devono essere validi e del figlio.
    pub unsafe fn create_spawn_copy(
        name: &'static str,
        name_owned: [u8; 16],
        name_len: u8,
        priority: crate::ordo::sched::Priority,
        req_next: u64,
        parent_pid: usize,
        child_cr3: u64,
        stack_base: u64,
        kernel_stack_top: u64,
        saved: CpuContext,
        tss_slot: usize,
        tss_sel: SegmentSelector,
        text_id: u32,
        image_hash: u64,
        stack_frames: u32,
    ) -> Process {
        Process {
            name,
            name_owned,
            name_len,
            priority,
            state: State::Ready,
            suspended: false,
            parent: Some(parent_pid),
            detached: false,
            stack_frames,
            stack_base,
            cr3: child_cr3,
            kernel_stack_top,
            saved,
            tss_sel,
            tss_slot,
            ipc_state: IpcState::None,
            msg_queue: MsgQueue::new(),
            parent_chan: None,
            reply_chan: None,
            reply_req: 0,
            req_next,
            reply_slot: None,
            pending_wake: false,
            cbs_server: None,
            text_id,
            image_hash,
            thread_group: None,
            fs_base: 0,
            futex_woken: None,
            cwd: [0u8; 256],
            cwd_len: 0,
            exit_code: 0,
            waiting_pid: None,
            waiting_req: 0,
            die_peers: [(0, 0); MAX_NOTIFY_PEERS],
            die_peer_count: 0,
            ticks_used: 0,
        }
    }

    /// Crea un **thread 1:1** nel gruppo del leader (S-T, ADR-0046): condivide
    /// `cr3` (stesso address space, MAI teardown finche' il gruppo vive),
    /// canali/fd/heap/VMA/cwd del leader (tabelle indicizzate al leader),
    /// CBS/priorita'/nome/identita' del leader. Propri: kernel stack, TSS
    /// (bitmap I/O clonata dal leader: stesso dominio di protezione),
    /// user stack (fornito dal chiamante, tipicamente mmap), FS base (TLS),
    /// coda IPC. `entry` = RIP user iniziale, `user_stack` = RSP iniziale.
    /// Entry/stack nulli = rifiuto loud (mai thread senza dove partire).
    ///
    /// # Safety
    /// `leader_cr3` deve essere il cr3 vivo del leader; lo stack user deve
    /// essere mappato e valido nel gruppo.
    pub unsafe fn create_thread(
        name: &'static str,
        name_owned: [u8; 16],
        name_len: u8,
        priority: crate::ordo::sched::Priority,
        leader: usize,
        leader_cr3: u64,
        leader_tss_slot: usize,
        cbs_server: Option<usize>,
        text_id: u32,
        image_hash: u64,
        entry: u64,
        user_stack: u64,
        fs_base: u64,
    ) -> Option<Process> {
        if entry == 0 || user_stack == 0 {
            return None;
        }
        let stack_base = crate::arc::phys_mem::alloc_contiguous(STACK_FRAMES)?;
        let stack_top = crate::addr::phys_to_virt(stack_base + (STACK_FRAMES as u64 * crate::arc::phys_mem::FRAME_SIZE));
        let saved =
            unsafe { crate::ordo::context::new_context_user(stack_top, entry, user_stack) };
        let tss_slot = Self::alloc_tss(stack_top, &[])?;
        if crate::gdt::clone_tss_iomap(tss_slot, leader_tss_slot).is_none() {
            crate::gdt::free_tss_slot(tss_slot);
            crate::arc::phys_mem::free_contiguous(stack_base, STACK_FRAMES);
            return None;
        }
        let tss_sel = crate::gdt::selectors().tss_selector(tss_slot);
        Some(Process {
            name,
            name_owned,
            name_len,
            priority,
            state: State::Ready,
            suspended: false,
            parent: None,
            detached: false,
            stack_frames: crate::arc::vmm_user::USER_STACK_FRAMES as u32,
            stack_base,
            cr3: leader_cr3,
            kernel_stack_top: stack_top,
            saved,
            tss_sel,
            tss_slot,
            ipc_state: IpcState::None,
            msg_queue: MsgQueue::new(),
            parent_chan: None,
            reply_chan: None,
            reply_req: 0,
            req_next: 1,
            reply_slot: None,
            pending_wake: false,
            cbs_server,
            text_id,
            image_hash,
            thread_group: Some(leader),
            fs_base,
            futex_woken: None,
            cwd: [0u8; 256],
            cwd_len: 0,
            exit_code: 0,
            waiting_pid: None,
            waiting_req: 0,
            die_peers: [(0, 0); MAX_NOTIFY_PEERS],
            die_peer_count: 0,
            ticks_used: 0,
        })
    }

    /// Alloca uno slot TSS dal pool, lo configura (RSP0 + IST + bitmap I/O) e
    /// ritorna lo SLOT del pool (1-based). Il selettore GDT e' derivabile con
    /// `gdt::selectors().tss_selector(slot)`. (Fase 34: `pub(crate)` per spawn_copy.)
    pub(crate) fn alloc_tss(stack_top: u64, io_ranges: &[(u16, u16)]) -> Option<usize> {
        let slot = crate::gdt::alloc_tss_slot()?;
        crate::gdt::configure_tss(slot, x86_64::VirtAddr::new(stack_top), io_ranges);
        Some(slot)
    }
}
