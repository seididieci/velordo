//! Lato **focus owner** dell'input (D8d, ADR-0091).
//!
//! Chi possiede il focus — la console in D8d, il DE in D8e — si sottoscrive a
//! `porta` con `INPUT_ATTACH` (capability: >= `ThirdParty`), riceve
//! `INPUT_NOTIFY` come **hint coalescente** e **drena** con `INPUT_READ`.
//!
//! Il payload viaggia nei **registri** (8 byte per chiamata): la tastiera non
//! e' un file e non c'e' ne' ring ne' cardo nel percorso — il testo ne e'
//! uscito in D8c2, i tasti non ci rientrano. Un hint perso non perde tasti:
//! la verita' e' la coda di `porta`.

use crate::Error;

/// Byte per `INPUT_READ` (payload in `w0`, little-endian).
const BATCH: usize = 8;

/// Tentativi di attach (uno per tick): copre il boot in cui `porta` parte
/// **dopo** la console. 100 tick = 1 s.
const ATTACH_RETRIES: u64 = 100;

/// Parola futex solo-timer del retry (mai scritta).
static RETRY_WORD: u32 = 0;

struct Kbd {
    chan: u64,
    pid: i64,
}

static mut KBD: Option<Kbd> = None;

fn kbd() -> Option<&'static Kbd> {
    unsafe { (*core::ptr::addr_of!(KBD)).as_ref() }
}

fn put_kbd(k: Option<Kbd>) {
    unsafe { core::ptr::write(core::ptr::addr_of_mut!(KBD), k) }
}

fn tick() {
    let _ = crate::futex_wait(
        core::ptr::addr_of!(RETRY_WORD) as usize,
        0,
        (crate::get_ticks() + 1) as u64,
    );
}

/// Sottoscrive la tastiera (focus LIFO in `porta`) e memorizza il peer.
///
/// Idempotente. Riprova per un secondo se `porta` non e' ancora raggiungibile
/// (la console parte prima): poi fallisce loud, e il chiamante decide.
pub fn attach() -> Result<(), Error> {
    let mut last = Error::NotReady;
    for _ in 0..ATTACH_RETRIES {
        match attach_once() {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
        tick();
    }
    Err(last)
}

/// Un solo tentativo, **mai bloccante**: lo usa un server (la console) nel suo
/// loop per riattaccarsi dopo la morte di `porta` senza fermare i clienti.
pub fn attach_once() -> Result<(), Error> {
    if kbd().is_some() {
        return Ok(());
    }
    let chan = crate::service_lookup(crate::Service::Porta)? as u64;
    // Sync: la reply arriva quando porta ha registrato il focus, quindi al
    // ritorno nessun `INPUT_NOTIFY` puo' essere gia' in volo. La reply va
    // **guardata**: `send` e' `Ok` anche quando il server rifiuta (`w0 = !0`)
    // — la capability `INPUT_ATTACH` si nega ai `Untrusted`, e senza questo
    // controllo il rifiuto passerebbe per un successo (t31 lo ha beccato).
    let r = crate::send(chan, crate::INPUT_ATTACH, 0, 0)?;
    if r.w0 == !0u64 {
        return Err(Error::Denied);
    }
    let pid = crate::peer_pid(chan).unwrap_or(-1);
    put_kbd(Some(Kbd { chan, pid }));
    Ok(())
}

/// `true` se la tastiera e' sottoscritta.
pub fn attached() -> bool {
    kbd().is_some()
}

/// Rinuncia alla sottoscrizione (la toglie anche in `porta`).
pub fn detach() {
    if let Some(k) = kbd() {
        let _ = crate::send(k.chan, crate::INPUT_DETACH, 0, 0);
    }
    put_kbd(None);
}

/// pid di `porta` al momento dell'attach (-1 = sconosciuto, 0 = non attaccati):
/// serve a riconoscerne la morte (`EXIT_NOTIFY`) e riattaccarsi una volta.
pub fn peer_pid() -> i64 {
    kbd().map(|k| k.pid).unwrap_or(0)
}

/// Drena fino a `dst.len()` byte di tasti gia' decodificati (`0` = niente).
///
/// Una `INPUT_READ` ogni [`BATCH`] byte: la coda e' la verita', l'ordine e'
/// quello di arrivo. `0` non distingue "vuoto" da "`porta` morta": chi vuole
/// saperlo guarda `peer_pid`/`EXIT_NOTIFY`.
pub fn read(dst: &mut [u8]) -> usize {
    let Some(k) = kbd() else {
        return 0;
    };
    let mut done = 0usize;
    while done < dst.len() {
        let want = (dst.len() - done).min(BATCH);
        let Ok(r) = crate::send(k.chan, crate::INPUT_READ, 0, want as u64) else {
            return done;
        };
        let n = (r.w1 as usize).min(want);
        for i in 0..n {
            dst[done + i] = ((r.w0 >> (8 * i)) & 0xFF) as u8;
        }
        done += n;
        if n < want {
            break;
        }
    }
    done
}
