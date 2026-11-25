//! t77 — diritti ereditari all'avvio (S3.4a, ADR-0093).
//!
//! La suite gira come `tst` (`Untrusted`, classe 0): prova la **no-elevazione**
//! e l'**ereditarieta'** su ogni via di creazione raggiungibile da user.
//! Il caso positivo `ThirdParty` (floor `ThirdParty`, immagine `ThirdParty`)
//! vive nel probe `usr/bin/floorprobe`, fuori dal gate.

use core::sync::atomic::{AtomicI32, Ordering};

use civis;
use civis::syscall_numbers::{rights_floor, rights_ops};

use super::*;
use crate::helpers;

/// `sigtest` e' firmato `system`: da `tst` (`Untrusted`) non si puo' spawnare
/// ne' eseguire. Ritorna `true` se il kernel **rifiuta** (atteso).
fn spawn_system_refused() -> bool {
    let Some(img) = civis::load_file("/test/sigtest.bin") else {
        return false;
    };
    // La verifica registra la trust entry (classe System): senza, il rifiuto
    // sarebbe per "immagine ignota" (Untrusted), non per no-elevazione.
    let _ = civis::fs::rights::rights_execute_verdict(b"tst", b"test/sigtest.bin");
    match civis::SpawnMeta::new("sigtest", 16, &[]) {
        Some(meta) => civis::spawn_image(&img, &meta).is_err(),
        None => false,
    }
}

/// `exec` di un'immagine System: il kernel rifiuta e il processo resta intatto.
fn exec_system_refused() -> bool {
    let Some(img) = civis::load_file("/test/sigtest.bin") else {
        return false;
    };
    let _ = civis::fs::rights::rights_execute_verdict(b"tst", b"test/sigtest.bin");
    civis::exec_image(&img).is_err()
}

static THREAD_REFUSED: AtomicI32 = AtomicI32::new(-1);

extern "C" fn thread_spawn_sigtest() {
    // Il thread condivide l'address space del gruppo: scrive l'esito qui.
    THREAD_REFUSED.store(if spawn_system_refused() { 1 } else { 0 }, Ordering::SeqCst);
    civis::thread_exit(0);
}

/// Un thread del gruppo eredita floor/ops dal leader: `spawn_image` di un
/// System resta rifiutato.
fn thread_system_refused() -> bool {
    let Some(stack) = civis::mmap(0, 16 * 1024).ok() else {
        return false;
    };
    let top = stack + 16 * 1024;
    if civis::thread_create(thread_spawn_sigtest as *const () as usize, top, 0).is_err() {
        return false;
    }
    let t0 = civis::get_ticks();
    while THREAD_REFUSED.load(Ordering::SeqCst) < 0 {
        if civis::get_ticks() - t0 > 500 {
            return false;
        }
        civis::spin_ticks(1);
    }
    THREAD_REFUSED.load(Ordering::SeqCst) == 1
}

/// `t77` — nessuna via di creazione eleva un `Untrusted`.
pub fn t_spawn_rights() -> bool {
    helpers::drain_stray();
    let mut pass = true;

    // (1) `spawn_image` nudo di un'immagine System -> rifiutato.
    if !spawn_system_refused() {
        println!("[usertests] t77: spawn_image System da Untrusted ammesso");
        pass = false;
    }

    // (2) `exec_image` dello stesso target -> rifiutato (e il processo resta).
    if !exec_system_refused() {
        println!("[usertests] t77: exec System da Untrusted ammesso");
        pass = false;
    }
    if civis::service_lookup(civis::Service::Cardo).is_err() {
        println!("[usertests] t77: processo perso dopo exec rifiutato");
        pass = false;
    }

    // (3) fork + exec: il figlio (COW) prova a exec-are System. Il padre
    // carica e verifica PRIMA del fork (il figlio non eredita i canali FS);
    // il figlio fa solo `exec` (syscall pura) e riporta sul canale di nascita.
    if let Some(img) = civis::load_file("/test/sigtest.bin") {
        let _ = civis::fs::rights::rights_execute_verdict(b"tst", b"test/sigtest.bin");
        let (rax, _, _, _, _) = unsafe {
            civis::syscall4_out(civis::syscall_numbers::SYS_SPAWN_COPY, 0, 0, 0, 0)
        };
        if rax < 0 {
            println!("[usertests] t77: spawn_copy fallita");
            pass = false;
        } else if rax == 0 {
            // Figlio: exec rifiutato -> segnala al padre ed esce.
            let refused = civis::exec_image(&img).is_err();
            let _ = civis::send(0, 0x77, refused as u64, 0);
            civis::exit(0);
        } else {
            // Padre: attende il verdetto (ignorando messaggi estranei) e
            // sblocca il figlio con la reply.
            let mut got = false;
            let t0 = civis::get_ticks();
            while civis::get_ticks() - t0 < 500 {
                let deadline = (civis::get_ticks() + 10) as u64;
                match civis::recv_timeout(deadline) {
                    Ok(m) if m.tag == 0x77 => {
                        let _ = civis::reply(0x77, 0, 0);
                        got = true;
                        if m.w0 != 1 {
                            println!("[usertests] t77: fork+exec System ammesso");
                            pass = false;
                        }
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
            if !got {
                println!("[usertests] t77: nessun verdetto dal figlio (fork+exec)");
                pass = false;
            }
        }
    } else {
        println!("[usertests] t77: sigtest non caricabile (fork+exec)");
        pass = false;
    }

    // (4) thread del gruppo: eredita il floor del leader (spawn System negato).
    if !thread_system_refused() {
        println!("[usertests] t77: thread con floor riallargato");
        pass = false;
    }

    // (5) `SYS_SPAWN` per nome da un non-init -> rifiutato (regressione).
    if civis::spawn(b"vela").is_ok() {
        println!("[usertests] t77: spawn per nome da Untrusted ammesso");
        pass = false;
    }

    // (6) attenuazione annidata: `demo` (Untrusted) con ops READ. Il figlio
    // eredita la maschera; `peer_info` la riporta (il padre non ha ancora
    // ceduto la CPU, quindi il figlio e' vivo).
    if let Some(img) = civis::load_file("/test/demo.bin") {
        let _ = civis::fs::rights::rights_execute_verdict(b"ns", b"test/demo.bin");
        let meta = civis::SpawnMeta::new("demo", 16, &[]).map(|m| m.ops(civis::RIGHTS_READ));
        match meta.map(|m| civis::spawn_image(&img, &m)) {
            Some(Ok(chan)) => {
                match civis::peer_info_rights(chan as u64) {
                    Ok((_, _, rights)) => {
                        let ok = rights_floor(rights) == civis::ImageClass::Untrusted
                            && rights_ops(rights) == civis::RIGHTS_READ;
                        if !ok {
                            println!(
                                "[usertests] t77: ops ereditate {} (atteso {})",
                                rights_ops(rights),
                                civis::RIGHTS_READ
                            );
                            pass = false;
                        }
                    }
                    Err(_) => {
                        println!("[usertests] t77: peer_info_rights fallita");
                        pass = false;
                    }
                }
                if let Some(pid) = civis::peer_pid(chan as u64).ok() {
                    let _ = civis::kill(pid, 0);
                }
            }
            _ => {
                println!("[usertests] t77: spawn demo attenuato fallito");
                pass = false;
            }
        }
    } else {
        println!("[usertests] t77: demo non caricabile");
        pass = false;
    }

    // (7) init (System) continua a spawnare `tst`/`usr`/`sys`: questa suite
    // stessa ne e' la prova (gira e ha eseguito i punti sopra).

    pass
}
