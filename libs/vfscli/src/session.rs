use super::abi::*;
use super::{State, Sys};
use super::error::Error;
use super::ring;
use super::sync;
use core::sync::atomic::Ordering;

/// Hook post-copia lato figlio (Fase 34): avvelena l'FS per questo processo e
/// pulisce il guard 1-in-volo copiato in COW dal padre (un'op in volo del
/// padre non e' raccoglibile dal figlio: i ring e il canale sono del padre).
pub fn post_spawn_copy_child<S: Sys>(st: &'static State<S>) {
    st.child.store(true, Ordering::Relaxed);
    st.pending.store(-1, Ordering::Relaxed);
}

/// Re-inizializza l'FS in un figlio-copia per le pipeline builtin (Fase 42).
/// Il figlio NON eredita canali/ring del padre (aliasing del protocollo
/// SPSC: due processi sulla stessa coppia di ring corrompono teste/code).
/// Azzera le cache COW-copiate (canale, fisici, guard) e rifa da zero:
/// lookup per nome (canale NUOVO) + ring freschi + handshake. Le pagine ring
/// del padre restano intatte (il remap sostituisce solo i mapping COW del
/// figlio). Ritorna false se cardo irraggiungibile: il figlio deve fallire
/// loud (exit 1 con messaggio su seriale), mai usare l'FS a meta'.
/// Limite: il lookup iniziale e' unbounded come `fs_chan` (cardo e'
/// supervisionato e garantito a runtime; una sua morte qui appende il figlio
/// come appenderebbe qualunque client al primo handshake).
pub fn fs_child_reinit<S: Sys>(st: &'static State<S>) -> bool {
    st.child.store(false, Ordering::Relaxed);
    st.chan.store(-1, Ordering::Relaxed);
    st.req.store(0, Ordering::Relaxed);
    st.resp.store(0, Ordering::Relaxed);
    st.pending.store(-1, Ordering::Relaxed);
    st.inited.store(false, Ordering::Relaxed);
    fs_init(st)
}

/// True se questo processo e' un figlio-copia (FS inutilizzabile).
#[inline]
pub fn is_spawn_copy_child<S: Sys>(st: &'static State<S>) -> bool {
    st.child.load(Ordering::Relaxed)
}

/// Risolve (una volta) il canale verso il fs server per nome.
pub fn fs_chan<S: Sys>(st: &'static State<S>) -> i64 {
    let c = st.chan.load(Ordering::Relaxed);
    if c >= 0 {
        return c;
    }
    // Race di boot: cardo potrebbe non essersi ancora registrato come Fs. Con
    // la vecchia send al PID 4 il mittente restava bloccato finche' cardo era
    // pronto; col lookup per nome il servizio potrebbe non esistere ancora.
    // Replica il comportamento bloccante: ritenta finche' Fs non si registra,
    // con lunghi spin puri tra i lookup (IF=1) per non affamare il timer e
    // lasciare a cardo il tempo di partire. A boot cardo e' garantito.
    loop {
        if let Ok(chan) = unsafe { S::cardo_channel() } {
            st.chan.store(chan as i64, Ordering::Relaxed);
            return chan as i64;
        }
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
    }
}

/// C'e' un'operazione FS async in volo (guad 1-in-volo, Fase 13)?
pub fn fs_async_pending<S: Sys>(st: &'static State<S>) -> bool {
    st.pending.load(Ordering::Relaxed) != -1
}

/// Cancello comune dei wrapper FS (Fase 39, fondamenta posix): distingue i tre
/// rifiuti che prima collassavano in un unico -1. Il figlio-copia fallisce qui
/// con `Denied` invece di faultare sul ring assente (kill rumoroso → errore
/// pulito; il figlio che deve fare FS deve prima `exec`-care).
pub fn fs_gate<S: Sys>(st: &'static State<S>) -> Result<(), Error> {
    if is_spawn_copy_child(st) {
        return Err(Error::Denied);
    }
    if !fs_init(st) {
        return Err(Error::NotReady);
    }
    if fs_async_pending(st) {
        return Err(Error::Pending);
    }
    Ok(())
}

/// Converte il result di una reply FS (Fase 39 + 40): il server distingue i
/// rifiuti con le sentinelle `ERR_*` (Fase 40, P1), qui mappate nelle varianti
/// di dominio di `Error` (ADR-0030: i numeri POSIX restano solo in `to_errno`).
/// `ERR` resta `Failed` (rifiuto opaco: driver remoti, mount inattivi, IO).
pub fn fs_reply_check(w0: u64) -> Result<u64, Error> {
    match w0 {
        ring::ERR => Err(Error::Failed),
        ERR_NOTFOUND => Err(Error::NotFound),
        ERR_ISDIR => Err(Error::IsDir),
        ERR_NOTDIR => Err(Error::NotDir),
        ERR_EXISTS => Err(Error::Exists),
        ERR_READONLY => Err(Error::ReadOnly),
        ERR_BUSY => Err(Error::Busy),
        ERR_INVALID => Err(Error::Invalid),
        ERR_EMPTY => Err(Error::Empty),
        ERR_CLOSED => Err(Error::Closed),
        ERR_NOSPC => Err(Error::NoSpace),
        v => Ok(v),
    }
}

/// Bound per il re-lookup runtime (Fase 14, init-restart): ~200 tick di spin
/// totali tra i tentativi. Il boot path (`fs_chan`) resta unbounded (garanzia
/// di boot); a runtime un restart rotto deve dare -1 rumoroso, non hang.
const FS_RELOOKUP_TICKS: i64 = 200;

/// Periodo canonico di polling (Livello 1, buon vicinato): ~20 tick tra i
/// tentativi di operativita'. Ogni tentativo e' un round-trip servito da
/// cardo: martellarlo in busy-loop affama gli altri client (osservato
/// t27/t28: mount di devfs ritardato da 10 s+ a ms col throttling).
pub const POLL_PERIOD_TICKS: i64 = 20;

/// Attesa di operativita' con throttling (Livello 1, buon vicinato): chiama
/// `f()` ogni `period_ticks` finche' ritorna true o scade `bound_ticks`
/// (clock `get_ticks`). Ritorna true se `f()` ha avuto successo.
/// MAI busy-loop su syscall FS/IPC nei chiamanti: usare questa.
pub fn poll_wait<S: Sys>(bound_ticks: i64, period_ticks: i64, mut f: impl FnMut() -> bool) -> bool {
    poll_value::<S, _>(bound_ticks, period_ticks, || if f() { Some(()) } else { None }).is_some()
}

/// Variante di `poll_wait` che ritorna il valore prodotto da `f()`
/// (`None` = "non ancora pronto, riprova"). Utile quando serve il risultato
/// del tentativo riuscito (fd, pid, ...), non solo un booleano.
pub fn poll_value<S: Sys, T>(bound_ticks: i64, period_ticks: i64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let t0 = S::ticks();
    let mut last = t0.wrapping_sub(period_ticks);
    loop {
        let now = S::ticks();
        if now.wrapping_sub(last) >= period_ticks {
            last = now;
            if let Some(v) = f() {
                return Some(v);
            }
        }
        if S::ticks() - t0 > bound_ticks {
            return None;
        }
        for _ in 0..512 {
            core::hint::spin_loop();
        }
    }
}

/// Apre `path` riprovando throttled fino a `bound_ticks` (vedi `poll_wait`).
/// Ritorna l'fd o `Err(NotReady)` a timeout. Sostituisce i busy-loop di open
/// nei test e negli helper: un device non ancora registrato non giustifica
/// mai una tempesta di open verso cardo.
pub fn open_wait<S: Sys>(st: &'static State<S>, path: &str, flags: u32, bound_ticks: i64, period_ticks: i64) -> Result<i64, Error> {
    poll_value::<S, _>(bound_ticks, period_ticks, || sync::open(st, path, flags).ok())
        .ok_or(Error::NotReady)
}

/// Risolve il canale verso il fs server con attesa BOUNDED (init-restart):
/// ritenta finche' il servizio si registra o scade il bound. Aggiorna la
/// cache e ritorna il channel, o -1.
pub fn fs_chan_rt<S: Sys>(st: &'static State<S>) -> i64 {
    let t0 = S::ticks();
    loop {
        if let Ok(chan) = unsafe { S::cardo_channel() } {
            st.chan.store(chan as i64, Ordering::Relaxed);
            return chan as i64;
        }
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
        if S::ticks() - t0 > FS_RELOOKUP_TICKS {
            return -1;
        }
    }
}

/// Invia al fs server sul canale risolto per nome (ADR-0008). Se la send
/// fallisce (server morto, canale invalidato), invalida la cache, ri-risolve
/// (bounded: attende un eventuale restart da init) e ritenta UNA volta sola;
/// poi -1, mai loop infiniti.
/// Caveat write (at-least-once): se il server applica e poi muore prima della
/// reply, il retry duplica. Per ramfs/devfs-console l'effetto e' benigno
/// (overwrite degli stessi byte / device idempotenti); policy fine futura.
pub fn fs_send<S: Sys>(st: &'static State<S>, tag: u64, w0: u64, w1: u64) -> Result<(u64, u64, u64), Error> {
    if is_spawn_copy_child(st) {
        return Err(Error::Denied); // figlio-copia: niente FS (34, mai aliasare i ring)
    }
    let c = fs_chan(st);
    if c >= 0 {
        if let Ok(r) = unsafe { S::send(c as u64, tag, w0, w1) } {
            return Ok(r);
        }
        // Canale morto: invalida e ritenta una volta sola.
        st.chan.store(-1, Ordering::Relaxed);
    }
    let c2 = fs_chan_rt(st);
    if c2 < 0 {
        return Err(Error::NotReady);
    }
    // `ipc::send` fallisce solo a canale morto qui: il server e' irraggiungibile.
    unsafe { S::send(c2 as u64, tag, w0, w1) }.map_err(|_| Error::NotReady)
}

/// Alloca (una sola volta) la coppia di ring del processo SENZA handshake
/// (Fase 57): la syscall mappa gia' le pagine a `REQ/RESP_RING_VA`, i fisici
/// restano in `st.req`/`st.resp`. Serve al client LOG (anelli condivisi
/// col FS in sequenza, mai due coppie: il kernel mappa OGNI coppia sulle
/// STESSE VA e una seconda allocazione rimapperebbe la prima, incrociando i
/// frame — osservato: resync cardo + load falliti a boot parallelo).
/// Ritorna false se la syscall fallisce.
pub fn fs_rings<S: Sys>(st: &'static State<S>) -> bool {
    if st.req.load(Ordering::Relaxed) != 0 {
        return true;
    }
    // syscall SYS_RING_ALLOC via provider: ritorna (req_phys, resp_phys).
    match unsafe { S::ring_alloc() } {
        Some((req, resp)) => {
            st.req.store(req, Ordering::Relaxed);
            st.resp.store(resp, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Cancello leggero per il client LOG (Fase 57): come `fs_gate` ma SENZA
/// handshake FS (il log funziona pre-FS e senza cardo: gli anelli bastano,
/// la registrazione LOG viaggia su `LOG_REG` presso vestigia). Rifiuta sul
/// figlio-copia (aliasing) e su async-FS in volo (un frame LOG interleavato
/// corromperebbe il ring condiviso — il formato non ha lunghezze).
pub fn fs_light_gate<S: Sys>(st: &'static State<S>) -> Result<(), Error> {
    if is_spawn_copy_child(st) {
        return Err(Error::Denied);
    }
    if !fs_rings(st) {
        return Err(Error::NotReady);
    }
    if fs_async_pending(st) {
        return Err(Error::Pending);
    }
    Ok(())
}
pub fn fs_init<S: Sys>(st: &'static State<S>) -> bool {
    if st.inited.load(Ordering::Relaxed) {
        return true;
    }
    if !fs_rings(st) {
        return false;
    }
    let req_phys = st.req.load(Ordering::Relaxed);
    let resp_phys = st.resp.load(Ordering::Relaxed);
    // Registra entrambi gli indirizzi fisici presso cardo
    match fs_send(st, FS_BUF_REG, req_phys, resp_phys) {
        Ok(_) => {
            st.inited.store(true, Ordering::Relaxed);
            true
        }
        Err(_) => false,
    }
}

/// Ripete l'handshake ring dopo un restart di cardo (Fase 14, t28): le pagine
/// persistono nel processo, si re-invia solo la coppia phys salvata. AZZERA
/// anche entrambi i ring (head=tail=0): qualunque contenuto appartiene
/// all'epoca morta (frame scritti ma mai notificati/consumati, risposte
/// orfane di reply perse) e disallineerebbe permanentemente client e server.
/// Il chiamante DEVE riscrivere il frame corrente dopo (vedi `redo` in
/// `fs_notify_result`). Ritorna false se il server non c'e'.
pub fn fs_rehandshake<S: Sys>(st: &'static State<S>) -> bool {
    let req_phys = st.req.load(Ordering::Relaxed);
    let resp_phys = st.resp.load(Ordering::Relaxed);
    if req_phys == 0 {
        return false;
    }
    match fs_send(st, FS_BUF_REG, req_phys, resp_phys) {
        Ok(_) => {
            st.inited.store(true, Ordering::Relaxed);
            ring_reset(ring::REQ_RING_VA);
            ring_reset(ring::RESP_RING_VA);
            true
        }
        Err(_) => false,
    }
}

/// Rimappa i PROPRI ring alle finestre fisse (Fase 14, t28): cardo inietta i
/// ring dei client nei driver via `map_in` sulle STESSE VA condivise,
/// sovrascrivendo il mapping dei ring propri del driver senza ripristinarlo.
/// Prima di usare i propri ring (es. `ensure_mounted`), il driver deve
/// richiamare questa (le pagine persistono, basta rimappare). Ritorna false
/// se i ring non sono mai stati allocati.
pub fn fs_remap_self<S: Sys>(st: &'static State<S>) -> bool {
    let req_phys = st.req.load(Ordering::Relaxed);
    let resp_phys = st.resp.load(Ordering::Relaxed);
    if req_phys == 0 || resp_phys == 0 {
        return false;
    }
    if !unsafe { S::remap_rings(req_phys, st.resp.load(Ordering::Relaxed)) } {
        return false;
    }
    true
}

// ── Boot/handshake helpers lato server (A3) ─────────────────────────
// Prima identici nei server userland (devfs/console/kbd + SVC_READY anche in
// fs/tty/disk): differivano solo nella chiamata di registrazione (A3) o nel
// payload w0 (signal_ready).

/// Attende il servizio Fs via soli lookup (spin puri IF=1, mai `get_ticks`
/// che maschera gli interrupt), poi UN tentativo via `register` (nessun frame
/// scritto finche' cardo non c'e': niente spam nel ring che disallineerebbe
/// gli altri client); se fallisce (race: cardo rimorto nel mentre) ricomincia
/// dal lookup. Unbounded come `fs_chan`: senza Fs il driver e' comunque
/// inutile. Idempotente grazie al replace-on-register in cardo.
pub fn ensure_fs_mount<S: Sys>(st: &'static State<S>, register: fn() -> Result<(), Error>) {
    // Prima i PROPRI ring: le injection map_in di cardo li hanno sovrascritti
    // (stessa VA condivisa, mai ripristinata) — senza remap scriveremmo nelle
    // pagine di un altro client (t28). No-op se mai allocati.
    let _ = fs_remap_self(st);
    loop {
        while unsafe { S::cardo_channel() }.is_err() {
            for _ in 0..1_000_000 {
                core::hint::spin_loop();
            }
        }
        if register().is_ok() {
            return;
        }
    }
}

/// Segnala SVC_READY al parent in fire-and-forget: a boot init potrebbe non
/// essere ancora in recv (una send sync resterebbe bloccata per sempre), su
/// restart nessuno aspetta. Retry bounded con spin puri, mai hang.
/// `w0` = payload prontezza (1 = pronto; cardo passa `reg_ok`).
pub fn signal_ready<S: Sys>(w0: u64) {
    for _ in 0..100 {
        if unsafe { S::try_signal_parent(SVC_READY, w0) } {
            break;
        }
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }
    }
}

/// Alloca una coppia di pagine ring (request, response) SENZA handshake
/// (Fase 16, data-plane `DISK_*` di block): il server riporta i fisici al
/// client nel frame di `DISK_HELLO`, il client li mappa nelle proprie finestre
/// con `map_physical`. Separata dalle pagine FS proprie: niente interleaving
/// di protocolli diversi nello stesso ring (lezione CLI_* del fix kbd/tty).
/// Ritorna `(req_phys, resp_phys)` o `None`.
pub fn ring_alloc_raw<S: Sys>() -> Option<(u64, u64)> {
    unsafe { S::ring_alloc() }
}

/// Azzera un ring SPSC (head=tail=0): tutto il contenuto pendente appartiene
/// a un'epoca morta (server riavviato). Solo per `fs_rehandshake`.
pub fn ring_reset(ring_va: u64) {
    unsafe {
        core::ptr::write_volatile::<u32>(
            core::ptr::with_exposed_provenance_mut((ring_va + ring::RING_HEAD as u64) as usize),
            0,
        );
        core::ptr::write_volatile::<u32>(
            core::ptr::with_exposed_provenance_mut((ring_va + ring::RING_TAIL as u64) as usize),
            0,
        );
    }
}

/// Notifica un'operazione (`tag` con w0=w1=0; il frame e' gia' scritto nel
/// request ring) e raccoglie il result dal response ring. Con UN redo su
/// `ERR_NOHANDSHAKE`: rifa l'handshake (che AZZERA i ring, scartando gli
/// stale dell'epoca morta) e riscrive il frame corrente via `rewrite`
/// (altrimenti la rinotifica leggerebbe spazzatura/vuoto). Poi UNA rinotifica;
/// al secondo NOHANDSHAKE, -1. Mai loop infiniti, mai doppi frame.
pub fn fs_notify_result<S: Sys>(st: &'static State<S>, tag: u64, rewrite: impl Fn() -> bool) -> Option<(u64, u64, usize)> {
    let mut retried = false;
    loop {
        match fs_send(st, tag, 0, 0) {
            Ok((_, w0, _)) => {
                if w0 == ring::ERR_NOHANDSHAKE && !retried && fs_rehandshake(st) && rewrite() {
                    retried = true;
                    continue;
                }
                return ring::resp_ring_read();
            }
            Err(_) => return None,
        }
    }
}

/// Primitivi guard/canale per chi guida il protocollo a mano (es. async in
/// `civis`, che registra/raccoglie fuori dagli op sync): registrano il
/// req_id in volo, lo leggono/azzerano, invalidano/leggono il canale cachato,
/// leggono i fisici dei ring. Stessa semantica dei static diretti di prima.
pub fn guard_take<S: Sys>(st: &'static State<S>, req: i64) {
    st.pending.store(req, Ordering::Relaxed);
}

/// Vedi `guard_take`.
pub fn guard_clear<S: Sys>(st: &'static State<S>) {
    st.pending.store(-1, Ordering::Relaxed);
}

/// Vedi `guard_take`.
pub fn guard_get<S: Sys>(st: &'static State<S>) -> i64 {
    st.pending.load(Ordering::Relaxed)
}

/// Vedi `guard_take`.
pub fn chan_set<S: Sys>(st: &'static State<S>, chan: i64) {
    st.chan.store(chan, Ordering::Relaxed);
}

/// Vedi `guard_take`.
pub fn chan_get<S: Sys>(st: &'static State<S>) -> i64 {
    st.chan.load(Ordering::Relaxed)
}

/// Fisici dei ring (per client che li riusano fuori handshake, es. LOG).
pub fn ring_phys<S: Sys>(st: &'static State<S>) -> (u64, u64) {
    (st.req.load(Ordering::Relaxed), st.resp.load(Ordering::Relaxed))
}
