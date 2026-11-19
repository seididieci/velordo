// Split from ordo/sched.rs (byte-identical move; see facade).
use crate::ordo::process::State;
use core::sync::atomic::Ordering;
use super::ctx::{SCHED, INITIALIZED, switch_to};
use super::queue::Scheduler;

#[derive(Clone, Copy, Debug)]
pub struct IpcResult {
    pub rax: i64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub r10: u64,
}

fn ok_result() -> IpcResult {
    IpcResult { rax: 0, rdi: 0, rsi: 0, rdx: 0, r10: 0 }
}

fn err_result() -> IpcResult {
    IpcResult { rax: -1, rdi: 0, rsi: 0, rdx: 0, r10: 0 }
}

/// E1 (ADR-0083): scadenza della `recv` (nessun messaggio consumato).
fn timeout_result() -> IpcResult {
    IpcResult { rax: -2, rdi: 0, rsi: 0, rdx: 0, r10: 0 }
}

/// Canale 0 = canale di nascita verso il parent (ADR-0008).
fn resolve_chan(cur: usize, chan: usize, sched: &Scheduler) -> Option<usize> {
    if chan == syscall_numbers::CHANNEL_PARENT as usize {
        sched.processes.get(cur)?.parent_chan
    } else {
        Some(chan)
    }
}

/// Identita' mittente sul wire (S-T T5): i thread operano sui canali del
/// leader (stessa sessione FS/fd per tutto il gruppo). Il blocco/sveglia
/// resta sul `cur` reale (il thread dorme, non il leader).
fn sender_of(sched: &Scheduler, cur: usize) -> usize {
    sched.processes[cur].thread_group.unwrap_or(cur)
}

/// Assegna il prossimo request-id del processo `pid` (Fase 13).
fn next_req_id(sched: &mut Scheduler, pid: usize) -> i64 {
    let p = &mut sched.processes[pid];
    let id = p.req_next as i64;
    p.req_next += 1;
    id
}

pub fn ipc_send(channel: usize, tag: u64, w0: u64, w1: u64) -> IpcResult {
    loop {
        if !INITIALIZED.load(Ordering::Acquire) {
            return err_result();
        }
        let mut guard = SCHED.lock();
        let sched = guard.as_mut().expect("scheduler non inizializzato");

        let cur = match sched.current {
            Some(c) => c,
            None => return err_result(),
        };
        let chan = match resolve_chan(cur, channel, sched) {
            Some(c) => c,
            None => return err_result(),
        };
        let snd = sender_of(sched, cur);
        let dest = match crate::relay::channels::peer(chan, snd) {
            Some(p) => p,
            None => return err_result(),
        };

        let req_id = next_req_id(sched, snd);

        {
            let d = &mut sched.processes[dest];
            d.msg_queue.push(crate::ordo::process::PendingMsg { channel: chan, req_id, tag, w0, w1 });
            if d.ipc_state == crate::ordo::process::IpcState::BlockedOnRecv {
                d.ipc_state = crate::ordo::process::IpcState::None;
                // E1: il messaggio chiude l'attesa con deadline (la coda
                // vince: nessuna scadenza residua da far scattare).
                d.recv_deadline = 0;
                d.state = State::Ready;
                sched.set_ready(dest);
            }
        }

        {
            let c = &mut sched.processes[cur];
            c.ipc_state = crate::ordo::process::IpcState::BlockedOnReply;
            // Fase 14: ricorda su chi siamo bloccati, cosi' la morte del
            // destinatario ci sblocca con un errore (niente deadlock).
            c.waiting_pid = Some(dest);
            // S-T (T5): ricorda anche il req_id: la reply sveglia il thread
            // giusto del gruppo (stesso canale, req diverse).
            c.waiting_req = req_id;
            c.state = State::Blocked;
            sched.clear_ready(cur);
        }

        let next = match sched.pick_next() {
            Some(n) if n != cur => n,
            _ => {
                sched.processes[cur].state = State::Ready;
                sched.set_ready(cur);
                sched.processes[cur].ipc_state = crate::ordo::process::IpcState::None;
                sched.processes[cur].waiting_pid = None;
                sched.processes[cur].waiting_req = 0;
                return err_result();
            }
        };
        switch_to(Some(cur), next, guard);

        let reply = {
            let sched = SCHED.lock();
            let s = sched.as_ref().expect("scheduler non inizializzato");
            s.processes[cur].reply_slot
        };
        {
            // Risvegliati (reply arrivata o mittente morto): non aspettiamo
            // piu' nessuno.
            let mut sched = SCHED.lock();
            let s = sched.as_mut().expect("scheduler non inizializzato");
            s.processes[cur].waiting_pid = None;
            s.processes[cur].waiting_req = 0;
        }
        return match reply {
            Some(r) => IpcResult { rax: 0, rdi: 0, rsi: r.tag, rdx: r.w0, r10: r.w1 },
            None => err_result(),
        };
    }
}

pub fn ipc_send_async(channel: usize, tag: u64, w0: u64, w1: u64) -> IpcResult {
    if !INITIALIZED.load(Ordering::Acquire) {
        return err_result();
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");

    let cur = match sched.current {
        Some(c) => c,
        None => return err_result(),
    };
    let chan = match resolve_chan(cur, channel, sched) {
        Some(c) => c,
        None => return err_result(),
    };
    let snd = sender_of(sched, cur);
    let dest = match crate::relay::channels::peer(chan, snd) {
        Some(p) => p,
        None => return err_result(),
    };

    let req_id = next_req_id(sched, snd);

    let ok = {
        let d = &mut sched.processes[dest];
        let pushed = d.msg_queue.try_push(crate::ordo::process::PendingMsg { channel: chan, req_id, tag, w0, w1 });
        if pushed && d.ipc_state == crate::ordo::process::IpcState::BlockedOnRecv {
            d.ipc_state = crate::ordo::process::IpcState::None;
            // E1: la coda vince sulla deadline (vedi `ipc_send`).
            d.recv_deadline = 0;
            d.state = State::Ready;
            sched.set_ready(dest);
        }
        pushed
    };

    if ok {
        IpcResult { rax: req_id, rdi: 0, rsi: 0, rdx: 0, r10: 0 }
    } else {
        err_result()
    }
}

/// Estrae il prossimo messaggio dalla coda del processo `cur` e prepara il
/// risultato IPC, oppure ritorna `None` se la coda e' vuota (Fase 13).
fn pop_msg(sched: &mut Scheduler, cur: usize) -> Option<IpcResult> {
    if sched.processes[cur].msg_queue.is_empty() {
        return None;
    }
    let m = sched.processes[cur].msg_queue.pop().expect("coda non vuota");
    if m.req_id >= 0 {
        // 38.2a/38.2e — i messaggi che non attendono mai reply NON toccano la
        // reply implicita: notify kernel senza peer (canale 0: solo
        // `notify_irq`; mai un canale reale) e EXIT_NOTIFY (peer morto:
        // rispondere e' impossibile per disegno — tutti i server fanno
        // `continue` senza reply, verificato). Senza, un server in `recv` con
        // reply in sospeso (block in `wait_dma`) perde la reply al primo
        // EXIT altrui: osservato (morte usertests a fine suite durante una DMA
        // di shell-load → reply persa → wedge cardo↔block permanente,
        // tutto il Normal bloccato). `rdi` (canale) e tag restano visibili:
        // gli EXIT si riconoscono dal tag come prima.
        if m.channel != 0 && m.tag != syscall_numbers::EXIT_NOTIFY {
            sched.processes[cur].reply_chan = Some(m.channel);
            sched.processes[cur].reply_req = m.req_id;
        }
        Some(IpcResult { rax: 0, rdi: m.channel as u64, rsi: m.tag, rdx: m.w0, r10: m.w1 })
    } else {
        Some(IpcResult { rax: 0, rdi: m.req_id as u64, rsi: m.tag, rdx: m.w0, r10: m.w1 })
    }
}

/// E1 (ADR-0083) — `recv` con deadline: `deadline` = tick assoluto, 0 = mai.
///
/// Invarianti:
/// - **la coda vince sul timeout**: un messaggio gia' in coda si consegna
///   anche se nel frattempo il tick e' passato (mai perso perche' il timer
///   e' scattato mentre lo schedulavamo);
/// - timeout -> `rax = -2` e nessun messaggio consumato; il chiamante puo'
///   riprovare senza ambiguita' (la coda non e' stata toccata);
/// - la scadenza vive nel PCB (`recv_deadline`), non in una tabella: muore
///   col processo, nessuno slot orfano da ripulire.
pub fn ipc_recv_until(deadline: u64) -> IpcResult {
    loop {
        if !INITIALIZED.load(Ordering::Acquire) {
            return err_result();
        }
        let mut guard = SCHED.lock();
        let action: Option<(Option<usize>, usize)> = {
            let sched = guard.as_mut().expect("scheduler non inizializzato");
            let cur = match sched.current {
                Some(c) => c,
                None => return err_result(),
            };
            if let Some(res) = pop_msg(sched, cur) {
                // Attesa finita con un messaggio: la deadline e' consumata.
                sched.processes[cur].recv_deadline = 0;
                sched.processes[cur].ipc_state = crate::ordo::process::IpcState::None;
                return res;
            }
            if deadline != 0 && crate::pit::ticks() >= deadline {
                sched.processes[cur].recv_deadline = 0;
                return timeout_result();
            }
            sched.processes[cur].recv_deadline = deadline;
            sched.processes[cur].ipc_state = crate::ordo::process::IpcState::BlockedOnRecv;
            sched.processes[cur].state = State::Blocked;
            sched.clear_ready(cur);
            match sched.pick_next() {
                Some(n) if n != cur => Some((Some(cur), n)),
                _ => {
                    sched.processes[cur].state = State::Ready;
                    sched.set_ready(cur);
                    sched.processes[cur].ipc_state = crate::ordo::process::IpcState::None;
                    sched.processes[cur].recv_deadline = 0;
                    None
                }
            }
        };
        match action {
            Some((prev, next)) => switch_to(prev, next, guard),
            None => return err_result(),
        }
    }
}

/// Sweep delle scadenze `recv` (E1, ADR-0083): chiamato da `on_tick` a lock
/// tenuto, dopo i futex. Solo chi e' **ancora** bloccato in `recv` con una
/// deadline: un processo gia' svegliato da un messaggio e' `Ready` (la coda
/// vince) e non viene toccato qui.
pub(super) fn tick_recv_deadlines(sched: &mut Scheduler, now: u64) {
    for pid in 0..sched.processes.len() {
        let hit = {
            let p = &sched.processes[pid];
            p.state == State::Blocked
                && p.ipc_state == crate::ordo::process::IpcState::BlockedOnRecv
                && p.recv_deadline != 0
                && now >= p.recv_deadline
        };
        if !hit {
            continue;
        }
        sched.processes[pid].recv_deadline = 0;
        sched.processes[pid].ipc_state = crate::ordo::process::IpcState::None;
        sched.processes[pid].state = State::Ready;
        sched.set_ready(pid);
    }
}

pub fn ipc_recv_nonblock() -> IpcResult {
    if !INITIALIZED.load(Ordering::Acquire) {
        return err_result();
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");
    let cur = match sched.current {
        Some(c) => c,
        None => return err_result(),
    };
    match pop_msg(sched, cur) {
        Some(res) => res,
        None => err_result(),
    }
}

pub fn ipc_reply(tag: u64, w0: u64, w1: u64) -> IpcResult {
    if !INITIALIZED.load(Ordering::Acquire) {
        return err_result();
    }
    let mut guard = SCHED.lock();
    let sched = guard.as_mut().expect("scheduler non inizializzato");
    let cur = match sched.current {
        Some(c) => c,
        None => return err_result(),
    };

    let target_chan = match sched.processes[cur].reply_chan {
        Some(c) => c,
        None => return err_result(),
    };
    let reply_req = sched.processes[cur].reply_req;
    let target = match crate::relay::channels::peer(target_chan, cur) {
        Some(t) => t,
        None => return err_result(),
    };

    sched.processes[cur].reply_chan = None;
    sched.processes[cur].reply_req = 0;

    // S-T (T5): il destinatario sync puo' essere un THREAD del gruppo
    // dell'endpoint (stesso canale, req_id del leader unica nel gruppo):
    // cerca il membro bloccato su (server, req) e sveglia LUI, non il leader.
    // Senza match: via esistente (async all'endpoint).
    let mut target = target;
    if sched.processes[target].ipc_state != crate::ordo::process::IpcState::BlockedOnReply {
        for i in 0..sched.processes.len() {
            let p = &sched.processes[i];
            let hit = p.state == State::Blocked
                && p.ipc_state == crate::ordo::process::IpcState::BlockedOnReply
                && p.waiting_pid == Some(cur)
                && p.waiting_req == reply_req
                && (i == target || p.thread_group == Some(target));
            if hit {
                target = i;
                break;
            }
        }
    }

    let sync = sched.processes[target].ipc_state == crate::ordo::process::IpcState::BlockedOnReply;

    if sync {
        {
            let t = &mut sched.processes[target];
            t.reply_slot = Some(crate::ordo::process::PendingReply { tag, w0, w1 });
            t.ipc_state = crate::ordo::process::IpcState::None;
            t.state = State::Ready;
        }
        sched.set_ready(target);
    } else {
        let delivered = {
            let t = &mut sched.processes[target];
            let pushed = t.msg_queue.try_push(crate::ordo::process::PendingMsg {
                channel: target_chan,
                req_id: -reply_req,
                tag,
                w0,
                w1,
            });
            if pushed && t.ipc_state == crate::ordo::process::IpcState::BlockedOnRecv {
                t.ipc_state = crate::ordo::process::IpcState::None;
                t.state = State::Ready;
                sched.set_ready(target);
            }
            pushed
        };
        if !delivered {
            crate::serial_println!(
                "[ipc] reply async a pid {} persa (msg_queue piena), req_id={}",
                target, reply_req
            );
        }
    }

    ok_result()
}
