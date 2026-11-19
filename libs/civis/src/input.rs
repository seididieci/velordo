//! Input della tastiera come **eventi** (E0, ADR-0082).
//!
//! `/dev/input/keyboard` (servito da `porta`) risponde **subito** con i byte
//! gia' decodificati (`0` = vuoto): da solo non dice quando arriva un tasto.
//! L'evento viaggia su IPC — `porta` manda `INPUT_NOTIFY` al sottoscrittore
//! con il focus quando ha decodificato scancode — e il lettore si blocca in
//! `recv()` invece di risvegliarsi ogni tick.
//!
//! Il notify e' un **hint coalescente**, non il dato: la verita' sono i byte
//! letti dal device. Un notify perso (o in piu') non cambia nulla, quindi il
//! burst di tasti non puo' perderne nessuno.
//!
//! Una sola sottoscrizione per processo (il modello IPC e' un `recv` per
//! processo): le chiamate sono globali per address space, non per fd.
//!
//! E1 (ADR-0083): `wait_until(deadline)` e' la stessa attesa con una
//! scadenza — "un tasto **oppure** il tempo" (`recv_timeout`). La usa la
//! shell per tenere vivo l'orologio del prompt mentre dorme: un risveglio al
//! secondo, non un tick.

use crate::{Error, RecvError};

const KEYBOARD_PATH: &str = "/dev/input/keyboard";
/// Tentativi di attach/reattach (uno per tick): copre il riavvio di `porta`
/// supervisionato da init senza busy-wait (ADR-0067). 100 tick = 1 s.
const ATTACH_RETRIES: u64 = 100;

/// Parola futex solo-timer del retry: mai scritta, l'indirizzo e' la chiave
/// del waiter `(mm, addr)` (come gli wait di input degli altri servizi).
static RETRY_WORD: u32 = 0;

struct Sub {
    chan: u64,
    fd: i64,
    /// pid di `porta` al momento dell'attach: riconosce la sua morte
    /// (`EXIT_NOTIFY.w1`) e distingue un riavvio del servizio da un peer
    /// qualsiasi che muore.
    pid: i64,
}

static mut SUB: Option<Sub> = None;

fn sub() -> Option<&'static Sub> {
    unsafe { (*core::ptr::addr_of!(SUB)).as_ref() }
}

fn put_sub(s: Sub) {
    unsafe { core::ptr::write(core::ptr::addr_of_mut!(SUB), Some(s)) }
}

fn take_sub() -> Option<Sub> {
    unsafe { (*core::ptr::addr_of_mut!(SUB)).take() }
}

/// Dorme un tick senza busy-wait (solo durante attach/reattach).
fn tick() {
    let _ = crate::futex_wait(
        core::ptr::addr_of!(RETRY_WORD) as usize,
        0,
        (crate::get_ticks() + 1) as u64,
    );
}

/// Sottoscrive l'input (focus LIFO in `porta`) e apre il device.
///
/// Idempotente: la seconda chiamata e' un no-op. Riprova per un secondo se
/// `porta` non e' ancora raggiungibile (boot/riavvio), poi fallisce.
pub fn attach() -> Result<(), Error> {
    if sub().is_some() {
        return Ok(());
    }
    let mut last = Error::NotReady;
    for _ in 0..ATTACH_RETRIES {
        match try_attach() {
            Ok(s) => {
                put_sub(s);
                return Ok(());
            }
            Err(e) => last = e,
        }
        tick();
    }
    Err(last)
}

fn try_attach() -> Result<Sub, Error> {
    let chan = crate::service_lookup(crate::Service::Porta)? as u64;
    // Sync: la reply arriva quando porta ha registrato il focus, quindi al
    // ritorno nessun `INPUT_NOTIFY` puo' essere gia' in volo prima dell'open.
    crate::send(chan, crate::INPUT_ATTACH, 0, 0)?;
    let fd = crate::open(KEYBOARD_PATH, 0)?;
    let pid = crate::peer_pid(chan).unwrap_or(-1);
    Ok(Sub { chan, fd, pid })
}

/// Byte **gia' disponibili** (un `read_fs`, mai bloccante): `0` = niente,
/// la prossima mossa e' [`wait`]. Dopo ogni [`wait`] il chiamante drena fin
/// che questo non ritorna `0`.
pub fn read(dst: &mut [u8]) -> usize {
    match sub() {
        Some(s) => crate::read_fs(s.fd, dst, dst.len()).unwrap_or(0),
        None => 0,
    }
}

/// Blocca finche' non arriva un messaggio (di norma `INPUT_NOTIFY`), oppure
/// finche' `porta` non muore.
///
/// Ritorna `true` = riprova a drenare (evento o messaggio qualsiasi: il
/// chiamante non deve interpretare il notify). Ritorna `false` = la sorgente
/// e' persa in modo definitivo: il chiamante esce loud (mai un busy-wait,
/// ADR-0067). Su morte di `porta` prova **una** riattaccata (init la
/// risupervisiona), poi rinuncia.
pub fn wait() -> bool {
    matches!(wait_until(0), Wait::Event)
}

/// Esito di [`wait_until`] (E1, ADR-0083).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Un messaggio e' arrivato (di norma `INPUT_NOTIFY`): drena il device.
    Event,
    /// Scadenza raggiunta senza messaggi: la coda non e' stata toccata.
    Timeout,
    /// La sorgente e' persa (riattaccata fallita): uscita loud.
    Dead,
}

/// Come [`wait`], ma si sveglia anche alla `deadline` (tick assoluto,
/// `0` = mai). E' l'attesa **evento o tempo** su un solo `recv_timeout`:
/// il processo resta `Blocked` e nessun risveglio e' sprecato (E1).
///
/// **La coda vince** (kernel): se un tasto e' arrivato mentre il tempo
/// scadeva, si torna `Event` e il drain lo trova — mai un evento perso.
pub fn wait_until(deadline: u64) -> Wait {
    loop {
        let pid = match sub() {
            Some(s) => s.pid,
            None => return Wait::Dead,
        };
        match crate::recv_timeout(deadline) {
            Ok(m) => {
                if m.tag == crate::EXIT_NOTIFY && pid >= 0 && m.w1 as i64 == pid {
                    // porta riavviata: riattacca una volta, poi loud.
                    if let Some(s) = take_sub() {
                        let _ = crate::close(s.fd);
                    }
                    return if attach().is_ok() { Wait::Event } else { Wait::Dead };
                }
                return Wait::Event;
            }
            Err(RecvError::Timeout) => return Wait::Timeout,
            // Wake spurio (peer morto senza EXIT recapitato): se il tempo e'
            // scaduto chiudi l'attesa, altrimenti riprova.
            Err(RecvError::Dead) => {
                if deadline != 0 && crate::get_ticks() as u64 >= deadline {
                    return Wait::Timeout;
                }
            }
        }
    }
}

/// Rinuncia esplicita al focus (il processo puo' riattaccarsi; i byte nel
/// frattempo restano in `porta` e vengono drenati al prossimo attach).
pub fn detach() {
    if let Some(s) = sub() {
        let _ = crate::send(s.chan, crate::INPUT_DETACH, 0, 0);
    }
    if let Some(s) = take_sub() {
        let _ = crate::close(s.fd);
    }
}
