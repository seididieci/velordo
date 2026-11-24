//! t_input — D8d (ADR-0091): i tasti sono della **sessione**.
//!
//! Il test esercita l'input come lo userebbe un programma (e, domani, la tile
//! del DE che ci fa girare dentro):
//!
//! 1. `input::attach` dichiara il processo **lettore** della sessione
//!    attestata (la stessa dello stdout, non un path) e spegne l'eco del
//!    modello: l'editor di linea e' del lettore;
//! 2. `input::read` = 0 sulla coda vuota e `wait_until(deadline)` = `Timeout`
//!    (E1 intatto: nessun risveglio a tick);
//! 3. l'iniezione (come la tastiera) arriva in coda **e sveglia il lettore
//!    dichiarato**: il primo messaggio e' l'hint `SESS_INPUT_EVENT`;
//! 4. eco acceso = il modello mostra i byte (consumatore **senza** editor);
//!    spento = li mostra il lettore, il modello non si tocca;
//! 5. `detach` non chiude la sessione: la coda e la sessione restano, e ci si
//!    riattacca.
//!
//! La coda si drena alla fine: i byte iniettati sono tasti veri e il login
//! che arriva dopo non deve trovare tasti fantasma.

use super::*;

pub fn t_session_input() -> bool {
    match run() {
        Ok(()) => {
            println!("[t75] input: attach/read/wait/iniezione/hint/eco/detach");
            true
        }
        Err(why) => {
            println!("[t75] FAIL: {}", why);
            false
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn run() -> Result<(), &'static str> {
    helpers::drain_stray();

    // 1. Lettore della **mia** sessione (attestazione: i test non hanno un
    //    binding, quindi si cade sullo schermo — la stessa di `sess::out`).
    civis::input::attach().map_err(|_| "input::attach negata")?;
    let sess = civis::sess::current().ok_or("nessuna sessione")?;
    let via = civis::sess::current().ok_or("cache di sessione instabile")?;
    if via.id != sess.id {
        return Err("input e stdout non parlano con la stessa sessione");
    }
    if sess.info().map_err(|_| "info fallita")?.echo {
        return Err("attach non ha spento l'eco del modello");
    }

    // 2. Coda vuota (e primo `read`: si dichiara **lettore** per l'hint).
    let mut buf = [0u8; 32];
    let mut stale = 0usize;
    while civis::input::read(&mut buf) > 0 {
        stale += 1;
        if stale > 64 {
            return Err("la coda di input non si svuota");
        }
    }

    // 3. `wait_until` con scadenza: nessun input → `Timeout` entro il bound e
    //    la coda resta vuota (E1: la coda non viene toccata).
    let deadline = (civis::get_ticks() + 5) as u64;
    loop {
        match civis::input::wait_until(deadline) {
            civis::input::Wait::Timeout => break,
            civis::input::Wait::Dead => return Err("input perso (Dead)"),
            // Un altro messaggio: e' nello stash, si aspetta il resto.
            civis::input::Wait::Event => {}
        }
        if civis::get_ticks() as u64 >= deadline {
            return Err("wait_until non e' scaduto");
        }
    }
    if civis::input::read(&mut buf) != 0 {
        return Err("la coda non era vuota dopo lo scadere del tempo");
    }

    // 4. Iniezione come la tastiera: i byte entrano in coda **e** il lettore
    //    dichiarato viene svegliato (l'hint e' il primo messaggio: FIFO).
    sess.test_input(b"t75\n").map_err(|_| "test_input negata")?;
    let m = civis::recv_timeout((civis::get_ticks() + 20) as u64)
        .map_err(|_| "nessun messaggio dopo l'iniezione")?;
    if m.tag != civis::syscall_numbers::SESS_INPUT_EVENT {
        return Err("il primo messaggio dopo l'iniezione non e' l'hint");
    }
    let n = civis::input::read(&mut buf);
    if &buf[..n] != b"t75\n" {
        return Err("read: byte iniettati diversi da quelli attesi");
    }

    // 5. Eco: acceso = lo mostra il modello (consumatore senza editor);
    //    spento = non lo tocca (lo mostra il lettore, come le shell).
    let mut tex = alloc::vec![0u8; 4096];
    civis::input::echo(true);
    sess.test_input(b"eco-on\n").map_err(|_| "test_input (eco)")?;
    let (n, _) = sess.dump(tex.len(), &mut tex).map_err(|_| "dump (eco)")?;
    if !contains(&tex[..n], b"eco-on") {
        return Err("eco acceso: il testo iniettato non appare nel modello");
    }
    civis::input::echo(false);
    sess.test_input(b"eco-off\n").map_err(|_| "test_input (no eco)")?;
    let (n, _) = sess.dump(tex.len(), &mut tex).map_err(|_| "dump (no eco)")?;
    if contains(&tex[..n], b"eco-off") {
        return Err("eco spento: il testo iniettato e' apparso lo stesso");
    }

    // 6. La coda si drena: i tasti iniettati sono veri e il login che arriva
    //    dopo non deve trovarli.
    let mut drained = 0usize;
    while civis::input::read(&mut buf) > 0 {
        drained += 1;
    }
    if drained == 0 {
        return Err("i byte dell'iniezione non erano in coda");
    }

    // 7. `detach` rinuncia al ruolo di lettore ma **non** chiude la sessione
    //    (nessun `SESS_CLOSE`): ci si riattacca e si ritrova la stessa.
    civis::input::detach();
    civis::input::attach().map_err(|_| "riattach dopo detach negata")?;
    if civis::sess::current().map(|s| s.id) != Some(sess.id) {
        return Err("dopo il detach la sessione e' un'altra");
    }
    if sess.info().is_err() {
        return Err("la sessione non e' sopravvissuta al detach");
    }
    Ok(())
}
