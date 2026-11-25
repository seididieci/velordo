//! floorprobe — prova il **floor di classe** su un processo `ThirdParty`
//! (S3.4, ADR-0093).
//!
//! Gira **fuori dal gate**: la shell nativa lo lancia con `floor = ThirdParty`
//! (`usr/bin/floorprobe.bin`), quindi il probe e' `ThirdParty`. Nel gate non
//! puo' stare: `usertests` e' `tst` (`Untrusted`) e non puo' spawnare
//! `ThirdParty` (no elevazione).
//!
//! Prove:
//! 1. un'immagine `Untrusted` (`ns:test/demo.bin`) viene **rifiutata** dal
//!    kernel (floor `ThirdParty`), non da un check del client;
//! 2. un'immagine `ThirdParty` (`usr:bin/runhello.bin`) viene **ammessa**;
//! 3. `exec` dell'immagine `Untrusted` sul probe stesso viene rifiutato.
//!
//! Seedato solo in `usr/bin/floorprobe.bin` (firma `thirdparty`); lo lancia
//! `scripts/run-rights-tests.sh` (login + `floorprobe` dal prompt).

#![no_std]
#![no_main]

use civis;
use civis::println;

civis::entry!(real_main);

fn finish(ok: bool) -> ! {
    if ok {
        println!("[floorprobe] risultato: PASS");
    } else {
        println!("[floorprobe] risultato: FAIL");
    }
    // Lanciato dalla shell (non da init): il `TEST_DONE` e' un hint, non una
    // richiesta sincrona (la shell non risponde -> deadlock).
    let _ = civis::send_async(
        civis::CHANNEL_PARENT,
        civis::TEST_DONE,
        if ok { 1 } else { 0 },
        0,
    );
    civis::exit(if ok { 0 } else { 1 })
}

fn real_main(_sp: u64) -> ! {
    println!("[floorprobe] partenza (floor ThirdParty)");
    let mut ok = true;

    // 1. `Untrusted` rifiutato dal floor: la verifica con cardo registra la
    // classe (senza, il rifiuto sarebbe per "immagine ignota", non per floor).
    let demo = civis::obj_get(b"ns", b"test/demo.bin").unwrap_or_default();
    if demo.is_empty() {
        println!("[floorprobe] demo non caricabile");
        finish(false);
    }
    let _ = civis::fs::rights::rights_execute_verdict(b"ns", b"test/demo.bin");
    let refused = match civis::SpawnMeta::new("demo", 16, &[]) {
        Some(meta) => civis::spawn_image(&demo, &meta).is_err(),
        None => false,
    };
    if !refused {
        println!("[floorprobe] Untrusted ammesso (floor non applicato)");
        ok = false;
    }

    // 2. `ThirdParty` ammesso: la shell nativa (System) ha dichiarato il
    // floor `ThirdParty`, e `runhello` e' firmato third-party.
    let hello = civis::obj_get(b"usr", b"runhello").unwrap_or_default();
    if hello.is_empty() {
        println!("[floorprobe] runhello non caricabile");
        finish(false);
    }
    let _ = civis::fs::rights::rights_execute_verdict(b"usr", b"runhello");
    let allowed = match civis::SpawnMeta::new("runhello", 16, &[]) {
        Some(meta) => match civis::spawn_image(&hello, &meta) {
            Ok(chan) => {
                // Attende la fine del figlio (non lascia un processo appeso).
                let t0 = civis::get_ticks();
                while civis::get_ticks() - t0 < 200 {
                    match civis::recv_timeout((civis::get_ticks() + 10) as u64) {
                        Ok(m) if m.channel as i64 == chan || m.w1 as i64 == civis::peer_pid(chan as u64).unwrap_or(-2) => {
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
                true
            }
            Err(_) => false,
        },
        None => false,
    };
    if !allowed {
        println!("[floorprobe] ThirdParty rifiutato (floor troppo stretto)");
        ok = false;
    }

    // 3. `System` (l'interprete nativo) rifiutato sia in spawn sia in exec:
    // il floor `ThirdParty` e la no-elevazione valgono anche qui.
    let interp = civis::obj_get(b"sys", b"bin/interpres.bin").unwrap_or_default();
    if interp.is_empty() {
        println!("[floorprobe] interpres non caricabile");
        finish(false);
    }
    let _ = civis::fs::rights::rights_execute_verdict(b"sys", b"bin/interpres.bin");
    let sys_spawn_refused = match civis::SpawnMeta::new("interpres", 16, &[]) {
        Some(meta) => civis::spawn_image(&interp, &meta).is_err(),
        None => false,
    };
    if !sys_spawn_refused {
        println!("[floorprobe] spawn System ammesso (no elevazione rotta)");
        ok = false;
    }
    if civis::exec_image(&interp).is_ok() {
        println!("[floorprobe] exec System ammesso (no elevazione rotta)");
        ok = false;
    }

    // 4. `req_floor = Untrusted` (0) significa **eredita**, non abbassa: un
    // figlio `Untrusted` resta rifiutato dal floor `ThirdParty` del padre.
    let floor_inherit = match civis::SpawnMeta::new("demo", 16, &[])
        .map(|m| m.floor(civis::ImageClass::Untrusted))
    {
        Some(meta) => civis::spawn_image(&demo, &meta).is_err(),
        None => false,
    };
    if !floor_inherit {
        println!("[floorprobe] req_floor=Untrusted ha abbassato il floor");
        ok = false;
    }

    // 3. `exec` di Untrusted rifiutato, il probe resta vivo.
    if civis::exec_image(&demo).is_ok() {
        println!("[floorprobe] exec Untrusted ammesso");
        ok = false;
    }

    finish(ok)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[floorprobe] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
