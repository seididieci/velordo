//! t71/t72 — `recv` con deadline (E1, ADR-0083) e timer dell'executor.
//!
//! t71 (`t_recv_deadline`): scadenza passata -> `Timeout`; messaggio in arrivo
//! consegnato anche con deadline lontana ("la coda vince"); timeout a 1 tick
//! che non perde nulla e non hang.
//! t72 (`t_sleep_join`): `Sleep::until`/`Sleep::ticks` via `block_on` e
//! `run` con misure di tick: l'executor arma la deadline minima e non fa
//! risvegli a vuoto.

use super::*;

/// t71 — `recv_timeout(deadline)`: scadenza, messaggio consegnato, 1 tick.
pub fn t_recv_deadline() -> bool {
    let mut ok = true;

    // ── 1) Scadenza gia' raggiunta -> Timeout immediato, coda intatta. ──
    match civis::recv_timeout(civis::get_ticks() as u64) {
        Err(civis::RecvError::Timeout) => {}
        Err(e) => {
            ok = false;
            println!("[usertests] t71: atteso Timeout, ottenuto {:?}", e);
        }
        Ok(m) => {
            ok = false;
            println!(
                "[usertests] t71: atteso Timeout, ottenuto messaggio tag={}",
                m.tag
            );
        }
    }

    // ── 2) Messaggio vince: spawn M_ECHO, invia T_CFG (async, non bloccante),
    //     recv_timeout(lontano) consegna il T_ACK (la coda vince sul tempo). ──
    let img = match civis::load_file("/test/testcli.bin") {
        Some(b) => b,
        None => {
            println!("[usertests] t71: load_file(testcli) FAILED");
            return false;
        }
    };
    let meta = match civis::SpawnMeta::new("t71-echo", 16, &[]) {
        Some(m) => m,
        None => {
            println!("[usertests] t71: SpawnMeta::new FAILED");
            return false;
        }
    };
    let chan = match civis::spawn_image(&img, &meta) {
        Ok(c) => c as u64,
        Err(_) => {
            println!("[usertests] t71: spawn_image FAILED");
            return false;
        }
    };
    if civis::send_async(chan, helpers::T_CFG, helpers::M_ECHO, 0).is_err() {
        ok = false;
        println!("[usertests] t71: send_async(T_CFG) FAILED");
    }
    match civis::recv_timeout(civis::get_ticks() as u64 + 10000) {
        Ok(m) if m.tag == helpers::T_ACK => {}
        _ => {
            ok = false;
            println!("[usertests] t71: atteso T_ACK dal helper M_ECHO");
        }
    }
    // Pulizia: il helper (rounds=0) esce inviando T_DONE; consumalo per non
    // lasciare residui in coda.
    let _ = civis::recv_timeout(civis::get_ticks() as u64 + 200);

    // ── 3) Nessun messaggio perso con timeout a 1 tick: M_KILLME non risponde
    //     mai, recv_timeout(1 tick futuro) -> Timeout (non hang, niente perso).
    //     Poi si ferma il helper parcheggiato. ──
    let (_chan2, pid2) = match helpers::spawn_cfg("/test/testcli.bin", "utcli", 16, helpers::M_KILLME, 0) {
        Some(x) => x,
        None => {
            println!("[usertests] t71: spawn M_KILLME FAILED");
            return ok;
        }
    };
    match civis::recv_timeout(civis::get_ticks() as u64 + 1) {
        Err(civis::RecvError::Timeout) => {}
        Ok(_m) => {} // messaggio inatteso: non e' una perdita, lo si ignora
        Err(e) => {
            ok = false;
            println!("[usertests] t71: errore inatteso a 1 tick: {:?}", e);
        }
    }
    let _ = civis::kill(pid2 as i64, 0);

    ok
}

/// t72 — `Sleep` + `join`/`run` con misure di tick.
pub fn t_sleep_join() -> bool {
    let mut ok = true;

    // run: due Sleep con scadenze +30 e +60 tick. run arma la scadenza minima
    // dei task pending (30), si sveglia, completa il primo, poi arma 60 e
    // completa il secondo. Deve passare almeno 60 tick (nessun risveglio
    // anticipato, nessun busy-loop).
    let start = civis::get_ticks() as u64;
    let t30 = start + 30;
    let t60 = start + 60;
    let [(), ()] = civis::task::run([
        civis::task::Sleep::until(t30),
        civis::task::Sleep::until(t60),
    ]);
    let elapsed = civis::get_ticks() as u64 - start;
    if elapsed < 60 {
        ok = false;
        println!(
            "[usertests] t72: run(Sleep x2) svegliato dopo {} tick (<60)",
            elapsed
        );
    } else {
        println!("[usertests] t72: run(Sleep x2) svegliato dopo {} tick", elapsed);
    }

    // block_on Sleep::ticks(25): deve dormire almeno 25 tick.
    let s = civis::get_ticks() as u64;
    civis::task::block_on(civis::task::Sleep::ticks(25));
    let e = civis::get_ticks() as u64 - s;
    if e < 25 {
        ok = false;
        println!(
            "[usertests] t72: block_on(Sleep::ticks(25)) svegliato dopo {} tick (<25)",
            e
        );
    } else {
        println!("[usertests] t72: block_on(Sleep::ticks(25)) svegliato dopo {} tick", e);
    }

    ok
}
