// Split from syscall.rs (byte-identical move; see facade).
use core::ptr::addr_of_mut;
use super::entry::{current_id, PERCPU};

/// ADR-0008 — `service_register(service)`: il chiamante occupa lo slot del
/// servizio `service`. -1 se gia' occupato da un processo vivo.
/// Fase 35 (hardening): i servizi di sistema si registrano solo da figli di
/// init (tutti i driver veri lo sono): impedisce lo squat a slot libero dopo
/// un kill. `Test` resta aperto (slot sacrificale della suite); `Init` e'
/// registrabile solo da un figlio di init (nessun processo reale lo registra,
/// quindi non cambia nulla — la regola e' uniforme).
pub(super) fn sys_service_register(service_disc: u64) -> i64 {
    let service = match service_from_disc(service_disc) {
        Some(s) => s,
        None => return -1,
    };
    let me = current_id() as usize;
    if service != syscall_numbers::Service::Test {
        let is_init_child = matches!(crate::ordo::sched::process_ps(me), Some(s) if s.parent == Some(1));
        if !is_init_child {
            crate::serial_println!(
                "[svc] register '{}' da pid={} rifiutato (non figlio di init)",
                service_name(service), me
            );
            return -1;
        }
    }
    match crate::relay::channels::register(service, me) {
        Ok(()) => {
            crate::serial_println!(
                "[svc] '{}' registrato da pid={}",
                service_name(service), me
            );
            0
        }
        Err(()) => -1,
    }
}

/// ADR-0008 — `service_lookup(service)`: risolve il servizio in un channel
/// verso l'attuale owner. Ritorna il channel id, o -1 se non registrato.
pub(super) fn sys_service_lookup(service_disc: u64) -> i64 {
    let service = match service_from_disc(service_disc) {
        Some(s) => s,
        None => return -1,
    };
    // S-T (T5): i thread riusano il canale del leader (stessa sessione
    // FS/fd per il gruppo: `alloc` trova l'esistente via `find`, mai duplicati).
    let me = crate::ordo::sched::group_leader(current_id() as usize);
    match crate::relay::channels::lookup(service) {
        Some(owner) => match crate::relay::channels::alloc(me, owner) {
            Some(chan) => chan as i64,
            None => -1,
        },
        None => -1,
    }
}

/// Fase 14 (init-restart) — `service_pid(service)`: ritorna il pid
/// dell'attuale owner del servizio, o -1 se non registrato. Nota: come
/// `lookup`, si fida dello slot owner (azzerato da `release_pid` alla morte;
/// riuso PID da parte di terzi nel mentre = futura generazione, vedi ADR-0010).
pub(super) fn sys_service_pid(service_disc: u64) -> i64 {
    let service = match service_from_disc(service_disc) {
        Some(s) => s,
        None => return -1,
    };
    match crate::relay::channels::lookup(service) {
        Some(owner) => owner as i64,
        None => -1,
    }
}

/// Fase 35 (hardening) — `peer_pid(chan)`: pid del peer del canale `chan`
/// (0 = canale di nascita, come `send`/`recv`), o -1. I server lo usano per
/// attribuire una richiesta a un processo (es. la policy `FS_REGISTER` di
/// cardo: replace di un prefix solo da figli di init). Non rivela nulla in
/// piu' di `ps_info` (gia' pubblico).
pub(super) fn sys_peer_pid(chan: usize) -> i64 {
    let me = current_id() as usize;
    let real = if chan == syscall_numbers::CHANNEL_PARENT as usize {
        match crate::ordo::sched::parent_channel(me) {
            Some(c) => c,
            None => return -1,
        }
    } else {
        chan
    };
    match crate::relay::channels::peer(real, me) {
        Some(p) => p as i64,
        None => -1,
    }
}

/// Fase 36 (identita' misurata, Strato 2 di ADR-0026) — `peer_info(chan)`:
/// hash dell'immagine del peer del canale `chan` (0 = canale di nascita, come
/// `peer_pid`), o -1 se il canale non esiste/il peer e' morto. Multi-registro
/// (pattern `ps_info`): rax = 0 + rdi = hash. I server lo usano per la policy
/// su identita' (manifest init, `FS_REGISTER` in cardo); non rivela nulla
/// oltre l'identita' del binario (nomi/pid gia' pubblici via `ps`).
pub(super) fn sys_peer_info(chan: usize) -> i64 {
    let me = current_id() as usize;
    let real = if chan == syscall_numbers::CHANNEL_PARENT as usize {
        match crate::ordo::sched::parent_channel(me) {
            Some(c) => c,
            None => return -1,
        }
    } else {
        chan
    };
    let peer = match crate::relay::channels::peer(real, me) {
        Some(p) => p,
        None => return -1,
    };
    match crate::ordo::sched::process_image_hash(peer) {
        Some(h) => {
            unsafe {
                let p = addr_of_mut!(PERCPU);
                (*p).ipc_override = 1;
                (*p).ret_rdi = h;
            }
            0
        }
        None => -1,
    }
}

/// Converti un discriminant in un `Service` valido.
/// Match esplicito (mai transmute): con SERVICE_COUNT > numero di varianti
/// (slot 11-15 liberi; 9 assegnato a Time in Fase 50, 10 a Log in Fase 57)
/// il transmute di un discriminant libero sarebbe UB (valore senza variante).
/// Solo gli slot assegnati risolvono.
fn service_from_disc(disc: u64) -> Option<syscall_numbers::Service> {
    use syscall_numbers::Service::*;
    match disc {
        0 => Some(Gpu),
        1 => Some(Cardo),
        2 => Some(Vela),
        3 => Some(Init),
        4 => Some(Test),
        5 => Some(Kbd),
        6 => Some(Porta),
        7 => Some(Block),
        8 => Some(Posix),
        9 => Some(Time),
        10 => Some(Vestigia),
        11 => Some(Identis),
        12 => Some(Console),
        13 => Some(Login),
        _ => None,
    }
}

/// Nome leggibile di un servizio (per log di debug).
fn service_name(s: syscall_numbers::Service) -> &'static str {
    match s {
        syscall_numbers::Service::Gpu => "gpu",
        syscall_numbers::Service::Cardo => "cardo",
        syscall_numbers::Service::Vela => "vela",
        syscall_numbers::Service::Init => "init",
        syscall_numbers::Service::Test => "test",
        syscall_numbers::Service::Kbd => "kbd",
        syscall_numbers::Service::Porta => "porta",
        syscall_numbers::Service::Block => "block",
        syscall_numbers::Service::Posix => "posix",
        syscall_numbers::Service::Time => "time",
        syscall_numbers::Service::Vestigia => "vestigia",
        syscall_numbers::Service::Identis => "identis",
        syscall_numbers::Service::Console => "console",
        syscall_numbers::Service::Login => "login",
    }
}

/// Coda comune di spawn (ADR-0008): canale di nascita tra parent e figlio.
/// Ritorna il channel id o -1 a pool esaurito (mai panic a boot).
pub(super) fn finish_spawn(parent: usize, pid: usize, log_name: &str) -> i64 {
    match crate::relay::channels::alloc(parent, pid) {
        Some(chan) => {
            crate::ordo::sched::set_parent_chan(pid, Some(chan));
            crate::serial_println!("[spawn] '{}' → pid={} canale={}", log_name, pid, chan);
            chan as i64
        }
        None => {
            crate::serial_println!("[syscall] spawn: pool canali esaurito");
            -1
        }
    }
}
