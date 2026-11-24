//! Input della **sessione** (D8d, ADR-0091): i tasti non sono un file.
//!
//! Il lettore legge i tasti dalla **propria** sessione — la stessa attestazione
//! che gli da' lo stdout (`civis::sess::current`), non un device aperto per
//! nome: nessun programma puo' leggere i tasti di un'altra tile.
//!
//! - [`read`] = i byte gia' disponibili (`SESS_READ`, mai bloccante);
//! - [`wait`]/[`wait_until`] = "un tasto **oppure** il tempo" sull'hint
//!   `SESS_INPUT_EVENT` che il server manda al **lettore dichiarato** (E0/E2:
//!   hint coalescente, la coda e' la verita' — un hint perso non perde tasti).
//!
//! [`attach`] dichiara il processo come lettore e **spegne l'eco del modello**:
//! chi legge ha il suo editor di linea (cursore, history, continuazioni ESC) e
//! scrive lui i caratteri sulla sessione ([`crate::sess::out_screen`]). Un
//! consumatore *senza* editor (D8e: prompt di chrome) riaccende l'eco con
//! [`echo`].
//!
//! Una sola sottoscrizione per processo (il modello IPC e' un `recv` per
//! processo): le funzioni sono globali per address space, non per fd.

use crate::{Error, RecvError};

/// Tentativi di attach (uno per tick): copre il boot e il riavvio della
/// console senza busy-wait (ADR-0067). 100 tick = 1 s.
const ATTACH_RETRIES: u64 = 100;

/// Parola futex solo-timer del retry: mai scritta, l'indirizzo e' la chiave
/// del waiter `(mm, addr)`.
static RETRY_WORD: u32 = 0;

/// Dorme un tick senza busy-wait (solo durante attach/reattach).
fn tick() {
    let _ = crate::futex_wait(
        core::ptr::addr_of!(RETRY_WORD) as usize,
        0,
        (crate::get_ticks() + 1) as u64,
    );
}

/// Dichiara il processo **lettore** della sua sessione.
///
/// Idempotente. La sessione si risolve per attestazione ([`crate::sess::current`]):
/// prima la sessione legata al pid, poi lo schermo. Riprova per un secondo se
/// il servizio non risponde ancora (boot/riavvio), poi fallisce loud.
pub fn attach() -> Result<(), Error> {
    let mut last = Error::NotReady;
    for _ in 0..ATTACH_RETRIES {
        if let Some(s) = crate::sess::current() {
            // Chi legge e' il lettore: l'eco la fa lui. Se il flag resta
            // acceso il carattere comparirebbe due volte (l'editor + il
            // modello) e il backspace del modello e' solo "cursore a
            // sinistra", non cancellazione.
            match s.echo(false) {
                Ok(()) => return Ok(()),
                // La sessione in cache non e' piu' nostra (chiusa dal
                // proprietario, o console ripartita): si risolve di nuovo.
                Err(_) => {
                    crate::sess::forget_current();
                    last = Error::Denied;
                }
            }
        }
        tick();
    }
    Err(last)
}

/// Riaccende/spegne l'**eco del modello**: `true` = la sessione mostra i byte
/// che riceve (consumatore senza editor), `false` = li mostra il lettore.
pub fn echo(on: bool) {
    if let Some(s) = crate::sess::current() {
        let _ = s.echo(on);
    }
}

/// Byte **gia' disponibili** (`0` = niente, la prossima mossa e' [`wait`]).
/// Dopo ogni [`wait`] il chiamante drena fin che questo non ritorna `0`.
pub fn read(dst: &mut [u8]) -> usize {
    let Some(s) = crate::sess::current() else {
        return 0;
    };
    match s.read(dst) {
        Ok(n) => n,
        Err(_) => {
            // Cache stantia (console ripartita): si risolve al prossimo giro.
            crate::sess::forget_current();
            0
        }
    }
}

/// Blocca finche' non arriva qualcosa (di norma l'hint `SESS_INPUT_EVENT`),
/// oppure finche' il servizio non muore.
///
/// Ritorna `true` = riprova a drenare (evento o messaggio qualsiasi: il
/// chiamante non deve interpretare il notify). Ritorna `false` = la sorgente
/// e' persa in modo definitivo: il chiamante esce loud (mai un busy-wait,
/// ADR-0067). Sulla morte della console prova **una** riattaccata, poi
/// rinuncia.
pub fn wait() -> bool {
    matches!(wait_until(0), Wait::Event)
}

/// Esito di [`wait_until`] (E1, ADR-0083).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Un messaggio e' arrivato (di norma l'hint): drena la sessione.
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
///
/// I messaggi che **non** sono il nostro hint finiscono nello stash di
/// [`crate::ipc`] e `recv`/`recv_poll` li restituiscono: mai persi (E2). Un
/// `EXIT_NOTIFY` sul canale della console e' `Dead`/riattaccata: senza, un
/// lettore dormirebbe per sempre su una console morta.
pub fn wait_until(deadline: u64) -> Wait {
    loop {
        let chan = crate::sess::current_chan();
        match crate::recv_timeout(deadline) {
            Ok(m) => {
                if m.tag == crate::SESS_INPUT_EVENT {
                    return Wait::Event;
                }
                if chan != 0 && crate::is_exit_notify(&m) && m.channel == chan {
                    crate::sess::forget_current();
                    return if attach().is_ok() { Wait::Event } else { Wait::Dead };
                }
                // Non e' roba dell'input: non si butta.
                crate::ipc::stash_push(m);
                return Wait::Event;
            }
            Err(RecvError::Timeout) => return Wait::Timeout,
            // Wake spurio (peer morto senza EXIT recapitato): se il tempo e'
            // scaduto chiudi l'attesa, altrimenti riprova.
            Err(RecvError::Dead) => {
                if deadline != 0 && (crate::get_ticks() as u64) >= deadline {
                    return Wait::Timeout;
                }
            }
        }
    }
}

/// Rinuncia esplicita al ruolo di lettore: la sessione **non** si chiude
/// (l'attachment e' del processo, non del lettore), si smette solo di usare la
/// cache — la prossima [`attach`] risolve di nuovo.
pub fn detach() {
    crate::sess::forget_current();
}

pub mod device;
