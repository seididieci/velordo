//! t57 — policy su identita' + diritti GRANT/PIPE (Fase 45).
//!
//! Due attori:
//! 1. helper testcli M_GRANTDENY (riga test-policy ALL: il suo GET default
//!    prova la riga applicata): drop GRANT → grant negato, drop PIPE →
//!    pipe_create negata, read valida dopo (detail bitmask 15).
//! 2. `foreign.bin` (FUORI da ogni tabella: hash ignoto): mount, grant e
//!    pipe_create negati dal default fail-closed; open+read lecite ok;
//!    read valida dopo i rifiuti (esiti bitmask 31).
//!
//! Il canale di usertests NON viene droppato qui (t34 gira dopo e pretende
//! il GET default ALL). Ordine in main.rs: dopo t56, prima di t34.

use super::*;

/// Attende T_DONE dal canale `chan` e ne ritorna w1 (esiti), scartando con
/// reply gli estranei (come `recv_expect`, ma serve il detail).
fn recv_done_w1(chan: u64) -> Option<u64> {
    loop {
        match civis::recv() {
            Ok(m) => {
                let _ = civis::reply(helpers::T_ACK, 0, 0);
                if m.tag == helpers::T_DONE && m.channel == chan {
                    if m.w0 != 1 {
                        return None;
                    }
                    return Some(m.w1);
                }
            }
            Err(_) => return None,
        }
    }
}

pub fn t_policy() -> bool {
    helpers::drain_stray();
    // 1. Helper noto: riga test-policy ALL + dinieghi post-drop.
    let (g_chan, g_pid) = match helpers::spawn_cfg(
        "/test/testcli.bin", "utcli", 16, helpers::M_GRANTDENY, 0,
    ) {
        Some(x) => x,
        None => {
            println!("[usertests] t57: spawn grantdeny FAILED");
            return false;
        }
    };
    let _ = g_pid;
    match recv_done_w1(g_chan) {
        Some(15) => {}
        Some(d) => {
            println!("[usertests] t57: grantdeny detail {:#b} (atteso 0b1111)", d);
            let _ = helpers::wait_exit(g_chan);
            return false;
        }
        None => {
            println!("[usertests] t57: grantdeny T_DONE mancante");
            let _ = helpers::wait_exit(g_chan);
            return false;
        }
    }
    let _ = helpers::wait_exit(g_chan);
    // 2. Attore ignoto: default restrittivo (esiti attesi = tutti i bit).
    let img = match civis::load_file("/test/foreign.bin") {
        Some(i) => i,
        None => {
            println!("[usertests] t57: load foreign.bin FAILED");
            return false;
        }
    };
    let meta = match civis::SpawnMeta::new("foreign", 16, &[]) {
        Some(m) => m,
        None => {
            println!("[usertests] t57: SpawnMeta FAILED");
            return false;
        }
    };
    let f_chan = match civis::spawn_image(&img, &meta) {
        Ok(c) => c as u64,
        Err(_) => {
            println!("[usertests] t57: spawn foreign FAILED");
            return false;
        }
    };
    // 511 = bit0..bit8: i 5 della policy + i 3 dei bucket protetti
    // (ADR-0077: sys, bin/ con TRUNC, credenziali) + la lettura riservata di
    // `sys:identis` (ADR-0096, bit8).
    match recv_done_w1(f_chan) {
        Some(511) => {}
        Some(e) => {
            println!("[usertests] t57: foreign esiti {:#b} (attesi 0b111111111)", e);
            let _ = helpers::wait_exit(f_chan);
            return false;
        }
        None => {
            println!("[usertests] t57: foreign T_DONE mancante");
            let _ = helpers::wait_exit(f_chan);
            return false;
        }
    }
    let _ = helpers::wait_exit(f_chan);
    true
}
