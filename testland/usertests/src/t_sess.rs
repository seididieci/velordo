//! t_sess — D8c1 (ADR-0089): sessioni di terminale del DE.
//!
//! Il test esercita l'endpoint di sessione come lo userebbe un programma
//! (e, domani, una tile del DE):
//!
//! 1. `SESS_STATS` conta la sessione 0 e la indica in focus;
//! 2. `whoami` **prima** di crearne una fallisce (attestazione: nessun
//!    handle, nessun ambiente);
//! 3. `WIN_OPEN` con `w0 = 0` ("la tile della mia sessione") e' negato a chi
//!    non ha una sessione attaccata — nessun numero magico;
//! 4. `create` restituisce sessione e mailbox; `whoami` ritrova la **stessa**
//!    sessione **per pid**, senza argomenti;
//! 5. la geometria e' quella del display, l'eco e' accesa, il focus resta 0;
//! 6. il feed scrive nel modello (`state`/`dump`), il resize fa reflow senza
//!    perdere il testo;
//! 7. eco spento: l'input iniettato si legge ma **non** appare sul modello;
//!    eco acceso: appare;
//! 8. `bind` e' negato a un'immagine `Untrusted` (il DE positivo e' D8e), il
//!    focus su una sessione diversa dalla 0 e' negato (D8c1 disegna una sola
//!    sessione) mentre `focus(0)` riesce senza cambiare un pixel;
//! 9. `close` libera la sessione (torna a 1, `whoami` torna a fallire); la
//!    sessione 0 e una inesistente non si chiudono.
//!
//! La sessione **0 e' lo schermo**: il test non ci scrive mai, quindi il gate
//! (login e shell) non viene toccato. Uscita a punto unico: qualunque
//! fallimento lascia il focus a 0.

use super::*;
use crate::t_display::console_chan;

/// Corpo del test: `Err(motivo)` = FAIL (l'uscita ripristina il focus).
fn run(console: u64) -> Result<(), &'static str> {
    // 1. La sessione 0 esiste ed e' quella disegnata.
    let (count, focus) = civis::sess::stats().map_err(|_| "stats fallite")?;
    if count < 1 || focus != 0 {
        return Err("stats iniziali: attese >=1 sessioni, focus 0");
    }

    // 2. Prima di creare una sessione, whoami non la trova (attestazione).
    if !matches!(civis::sess::whoami(), Err(civis::sess::SessError::NotAttached)) {
        return Err("whoami senza sessione non e' NotAttached");
    }

    // 3. `WIN_OPEN.w0 = 0` senza sessione e' negato.
    if civis::win::open(0, (16 << 16) | 16).is_ok() {
        return Err("WIN_OPEN(0) accettato senza sessione");
    }

    // 4. Crea una sessione legata a me e ritrovatala per pid.
    let sess = civis::sess::create().map_err(|_| "create negata")?;
    if sess.id < 1 || sess.shm == 0 {
        return Err("create: id/shm vuoti");
    }
    let me = civis::sess::whoami().map_err(|_| "whoami non trova la sessione creata")?;
    if me.id != sess.id {
        return Err("whoami: id diverso da quello creato");
    }
    drop(me);

    // 5. Geometria del display, eco accesa, non in focus.
    let q = civis::send(console, civis::syscall_numbers::CONSOLE_QUERY, 0, 0)
        .map_err(|_| "CONSOLE_QUERY fallita")?;
    let info = sess.info().map_err(|_| "info fallita")?;
    if (info.cols as u64, info.rows as u64) != (q.w0, q.w1) {
        return Err("info: geometria diversa dal display");
    }
    if !info.echo || info.focus {
        return Err("info: attese eco accesa e focus spento");
    }

    // 6. Feed: il testo arriva nel modello.
    sess.write(b"littera\n").map_err(|_| "write negata")?;
    let (non_empty, _crc) = sess.state(0).map_err(|_| "state fallita")?;
    if non_empty < 7 {
        return Err("state: celle non vuote insufficienti dopo il feed");
    }
    let mut tex = [0u8; 2048];
    let (n, _total) = sess.dump(tex.len(), &mut tex).map_err(|_| "dump fallito")?;
    if !contains(&tex[..n], b"littera") {
        return Err("dump: il testo scritto non c'e'");
    }

    // 7. Resize: il reflow conserva il testo.
    sess.resize(20, 5).map_err(|_| "resize negata")?;
    let info = sess.info().map_err(|_| "info dopo resize fallita")?;
    if (info.cols, info.rows) != (20, 5) {
        return Err("resize: geometria non aggiornata");
    }
    let (n, _) = sess.dump(tex.len(), &mut tex).map_err(|_| "dump dopo resize")?;
    if !contains(&tex[..n], b"littera") {
        return Err("resize: il testo e' andato perso");
    }

    // 8. Eco spento: l'input si legge, ma non entra nel modello.
    sess.echo(false).map_err(|_| "echo(false) negata")?;
    sess.test_input(b"pw").map_err(|_| "test_input(pw) negata")?;
    let mut inbuf = [0u8; 16];
    let rn = sess.read(&mut inbuf).map_err(|_| "read negata")?;
    if &inbuf[..rn] != b"pw" {
        return Err("read: input diverso da pw");
    }
    let (n, _) = sess.dump(tex.len(), &mut tex).map_err(|_| "dump dopo input muto")?;
    if contains(&tex[..n], b"pw") {
        return Err("eco spento: l'input e' finito sul modello");
    }

    // 9. Eco acceso: l'input appare sul modello.
    sess.echo(true).map_err(|_| "echo(true) negata")?;
    sess.test_input(b"ok").map_err(|_| "test_input(ok) negata")?;
    let rn = sess.read(&mut inbuf).map_err(|_| "read (eco) negata")?;
    if &inbuf[..rn] != b"ok" {
        return Err("read: input diverso da ok");
    }
    let (n, _) = sess.dump(tex.len(), &mut tex).map_err(|_| "dump dopo eco")?;
    if !contains(&tex[..n], b"ok") {
        return Err("eco acceso: l'input non appare sul modello");
    }

    // 10. bind e' del DE (>= ThirdParty): a noi e' negato.
    let pid = civis::getpid();
    if civis::sess::bind(sess.id, pid).is_ok() {
        return Err("bind accettato da un'immagine Untrusted");
    }

    // 11. Focus su una sessione diversa dalla 0: negato. `focus(0)` riesce e
    //     non cambia un pixel (la 0 e' gia' quella disegnata).
    if civis::sess::focus(sess.id).is_ok() {
        return Err("focus su una sessione non in focus accettato in D8c1");
    }
    let crc_before = frame_crc(console);
    let default = civis::sess::attach_default().map_err(|_| "attach_default negata")?;
    civis::sess::focus(0).map_err(|_| "focus(0) negata")?;
    let crc_after = frame_crc(console);
    drop(default);
    if let (Some(a), Some(b)) = (crc_before, crc_after) {
        if a != b {
            return Err("il giro di focus ha cambiato il frame");
        }
    }

    // 12. Chiudi la sessione (con l'attachment riportato su di lei da
    //     whoami) e verifica il ritorno allo stato iniziale.
    let again = civis::sess::whoami().map_err(|_| "whoami prima di close")?;
    if again.id != sess.id {
        return Err("whoami: id cambiato prima di close");
    }
    drop(again);
    sess.close().map_err(|_| "close negata")?;
    let (count, focus) = civis::sess::stats().map_err(|_| "stats dopo close")?;
    if count != 1 || focus != 0 {
        return Err("close: stats non tornate a 1/0");
    }
    if !matches!(civis::sess::whoami(), Err(civis::sess::SessError::NotAttached)) {
        return Err("whoami dopo close non e' NotAttached");
    }

    // 13. La sessione 0 e una inesistente non si chiudono.
    let d = civis::sess::attach_default().map_err(|_| "attach_default (close 0)")?;
    if d.close().is_ok() {
        return Err("close(0) accettata");
    }
    let d = civis::sess::attach_default().map_err(|_| "attach_default (close 999)")?;
    if close_unknown(&d).is_ok() {
        return Err("close di una sessione inesistente accettata");
    }
    drop(d);
    Ok(())
}

/// `SESS_CLOSE` su un id arbitrario (il client non ha una `Session` per un id
/// che non esiste: si manda il messaggio a mano).
fn close_unknown(_d: &civis::sess::Session) -> Result<(), ()> {
    let chan = civis::service_lookup(civis::Service::Console).map_err(|_| ())? as u64;
    let r = civis::send(chan, civis::SESS_CLOSE, 999, 0).map_err(|_| ())?;
    if r.w0 == !0u64 { Err(()) } else { Ok(()) }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn frame_crc(chan: u64) -> Option<u64> {
    let r = civis::send(chan, civis::syscall_numbers::CONSOLE_FRAME_CRC, 0, 0).ok()?;
    if r.w1 == 0 { None } else { Some(r.w0) }
}

pub fn t_session_endpoint() -> bool {
    let Some(console) = console_chan() else {
        println!("[t74] console non registrato: SKIP");
        return true;
    };
    match run(console) {
        Ok(()) => {
            println!("[t74] sessioni: create/whoami/feed/resize/eco/focus/close");
            true
        }
        Err(why) => {
            // Uscita a punto unico: il focus torna alla sessione 0 (lo
            // schermo), altrimenti il login resterebbe su una tile vuota.
            let _ = civis::sess::attach_default().map(|d| {
                let _ = civis::sess::focus(0);
                drop(d);
            });
            println!("[t74] FAIL: {}", why);
            false
        }
    }
}
