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

    // (6b) `ceiling_for` con ops ereditate: `rightshelper` spawnato con
    // `ops = READ` legge (ok) e la scrittura e' negata da cardo.
    if let Some(img) = civis::load_file("/test/rightshelper.bin") {
        let _ = civis::fs::rights::rights_execute_verdict(b"tst", b"test/rightshelper.bin");
        let meta = civis::SpawnMeta::new("rightshelper", 16, &[])
            .map(|m| m.ops(civis::RIGHTS_OPEN | civis::RIGHTS_READ));
        match meta.map(|m| civis::spawn_image(&img, &m)) {
            Some(Ok(chan)) => {
                let mut verdict = None;
                let t0 = civis::get_ticks();
                while civis::get_ticks() - t0 < 500 {
                    match civis::recv_timeout((civis::get_ticks() + 10) as u64) {
                        Ok(m) if m.tag == 0x78 => {
                            let _ = civis::reply(0x78, 0, 0);
                            verdict = Some(m.w0);
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
                match verdict {
                    Some(bits) => {
                        let read = bits & 1 != 0;
                        let write = bits & 2 != 0;
                        if !read || write {
                            println!(
                                "[usertests] t77: ceiling_for read={} write={} (atteso true/false)",
                                read, write
                            );
                            pass = false;
                        }
                    }
                    None => {
                        println!("[usertests] t77: nessun verdetto da rightshelper");
                        pass = false;
                    }
                }
            }
            _ => {
                println!("[usertests] t77: spawn rightshelper (ops READ) fallito");
                pass = false;
            }
        }
    } else {
        println!("[usertests] t77: rightshelper non caricabile");
        pass = false;
    }

    // (6c) fork: il figlio COW eredita floor/ops (`peer_info_rights`).
    if let Some(img) = civis::load_file("/test/demo.bin") {
        let _ = civis::fs::rights::rights_execute_verdict(b"ns", b"test/demo.bin");
        if let Some(meta) = civis::SpawnMeta::new("demo", 16, &[]) {
            if let Ok(chan) = civis::spawn_image(&img, &meta) {
                let pid = civis::peer_pid(chan as u64).ok();
                match civis::peer_info_rights(chan as u64) {
                    Ok((_, _, rights)) => {
                        if rights_floor(rights) != civis::ImageClass::Untrusted
                            || rights_ops(rights) != civis::RIGHTS_ALL
                        {
                            println!("[usertests] t77: fork floor/ops errati");
                            pass = false;
                        }
                    }
                    Err(_) => {
                        println!("[usertests] t77: peer_info_rights (demo) fallita");
                        pass = false;
                    }
                }
                if let Some(p) = pid {
                    let _ = civis::kill(p, 0);
                }
            } else {
                println!("[usertests] t77: spawn demo (fork) fallito");
                pass = false;
            }
        }
    }

    // (6d) `SpawnMeta.rights` con bit riservati -> rifiutato (fail-loud).
    if let Some(img) = civis::load_file("/test/demo.bin") {
        let _ = civis::fs::rights::rights_execute_verdict(b"ns", b"test/demo.bin");
        if let Some(mut meta) = civis::SpawnMeta::new("demo", 16, &[]) {
            meta.rights = 1u64 << 40; // bit riservato
            if civis::spawn_image(&img, &meta).is_ok() {
                println!("[usertests] t77: diritti riservati accettati");
                pass = false;
            }
        }
    }

    // (6e) Gate strutturali da `Untrusted`: trust_image, dma device, mmio.
    let id = civis::image_id(b"x");
    if civis::trust_image(id, civis::ImageClass::System, &[]).is_ok() {
        println!("[usertests] t77: trust_image da Untrusted ammesso");
        pass = false;
    }
    if civis::dma_alloc_slot(1, civis::DMA_SLOT_DEVICE).is_ok() {
        println!("[usertests] t77: dma slot device da Untrusted ammesso");
        pass = false;
    }
    if civis::map_mmio(0, 0, 1).is_ok() {
        println!("[usertests] t77: map_mmio da Untrusted ammesso");
        pass = false;
    }

    // (8) subtree ereditario: il padre restringe il proprio canale a `tmp`,
    // spawna un figlio che **eredita** il prefisso per attestazione: dentro
    // `tmp` legge, fuori no (prima della fase, il figlio aveva root).
    if let Some(img) = civis::load_file("/test/subtreehelper.bin") {
        let _ = civis::fs::rights::rights_execute_verdict(b"tst", b"test/subtreehelper.bin");
        match civis::SpawnMeta::new("subtreehelper", 16, &[])
            .map(|m| civis::spawn_image(&img, &m))
        {
            Some(Ok(chan)) => match helpers::wait_exit(chan as u64) {
                Some((0, _)) => {}
                other => {
                    println!("[usertests] t77: subtree helper {:?} (atteso 0)", other);
                    pass = false;
                }
            },
            _ => {
                println!("[usertests] t77: spawn subtreehelper fallito");
                pass = false;
            }
        }
    } else {
        println!("[usertests] t77: subtreehelper non caricabile");
        pass = false;
    }

    // (7) init (System) continua a spawnare `tst`/`usr`/`sys`: questa suite
    // stessa ne e' la prova (gira e ha eseguito i punti sopra).

    pass
}
