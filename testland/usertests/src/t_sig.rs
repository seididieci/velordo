//! t66 — firma delle immagini e classi di privilegio (ADR-0078 fase 2).
//!
//! Il verdetto lo da' cardo (`R_RIGHTS_EXECUTE`: `w0` = permesso, `w1` =
//! `ImageClass`). La firma e' indirizzata dal **contenuto** (`sig:<digest>`),
//! quindi la classe e' una proprieta' dei byte, non del path.
//!
//! Casi (semantica ADR-0078 §fase 2):
//!  1. `vela:bin/gpu.bin`      firmato system      -> eseguibile, classe 2
//!  2. `usr:runhello`          firmato third-party -> eseguibile, classe 1
//!  3. `tst:test/tests.bin`    non firmato (tst)   -> eseguibile, classe 0
//!  4. `vela:bin/unsigned.bin` non firmato (vela)  -> negato
//!  5. `tst:test/sigbad.bin`   sidecar valido di un ALTRO binario -> negato
//!  6. `tst:test/sigunk.bin`   sidecar con chiave ignota          -> negato
//!  7. oggetto inesistente                                        -> NotFound
//!
//! Il bucket decide la classe attesa (mappa in `scripts/sign-bins.sh`):
//! `sys`/`vela` -> chiave `system`, `usr` -> `thirdparty`.
//!
//! Gli artefatti 4–6 sono seedati da `scripts/arca-tool.sh` (host-side): sono
//! l'attacco reale, non una simulazione nel client.
//!
//! t67 (fase 3, ADR-0079) verifica il lato **kernel**: la classe che il PCB
//! riceve allo spawn. Cardo registra l'identita' verificata nella trust table
//! (`SYS_TRUST_IMAGE`); il kernel, al load, ricalcola l'`ImageId` sui byte e fa
//! lookup. `peer_info_class` legge la classe autorevole del peer.

use super::*;

fn verdict(bucket: &[u8], key: &[u8]) -> Result<(bool, civis::ImageClass), civis::Error> {
    civis::fs::rights::rights_execute_verdict(bucket, key)
}

/// Attende `(ok, classe)`; stampa il caso e ritorna l'esito.
fn expect_allowed(
    bucket: &[u8],
    key: &[u8],
    want: civis::ImageClass,
    what: &str,
) -> bool {
    match verdict(bucket, key) {
        Ok((true, got)) if got == want => true,
        Ok((true, got)) => {
            println!(
                "[usertests] t66: {} eseguibile ma classe {:?} != {:?}",
                what, got, want
            );
            false
        }
        Ok((false, _)) => {
            println!("[usertests] t66: {} NEGATO (atteso eseguibile)", what);
            false
        }
        Err(e) => {
            println!("[usertests] t66: {} errore {:?}", what, e);
            false
        }
    }
}

fn expect_denied(bucket: &[u8], key: &[u8], what: &str) -> bool {
    match verdict(bucket, key) {
        Ok((false, _)) => true,
        Ok((true, c)) => {
            println!("[usertests] t66: {} eseguibile (classe {:?}), atteso NEGATO", what, c);
            false
        }
        // La negazione e' un verdetto, non un errore di trasporto: un Err
        // qui significa che il test non ha nemmeno raggiunto il cancello.
        Err(e) => {
            println!("[usertests] t66: {} errore {:?} (atteso NEGATO)", what, e);
            false
        }
    }
}

pub fn t_signature() -> bool {
    helpers::drain_stray();
    let mut pass = true;

    pass &= expect_allowed(b"vela", b"bin/gpu.bin", civis::ImageClass::System, "vela:bin/gpu.bin");
    pass &= expect_allowed(b"usr", b"runhello", civis::ImageClass::ThirdParty, "usr:runhello");
    pass &= expect_allowed(
        b"tst",
        b"test/tests.bin",
        civis::ImageClass::Untrusted,
        "tst:test/tests.bin",
    );

    // Firma obbligatoria nei bucket di software spedito: `vela` non e'
    // enumerato da BIN_LIST (l'artefatto non diventa un comando).
    pass &= expect_denied(b"vela", b"bin/unsigned.bin", "vela:bin/unsigned.bin");

    // Un sidecar presente ma sbagliato non deve MAI degradare a "non
    // firmato" (sarebbe un bypass con un byte di disturbo).
    pass &= expect_denied(b"tst", b"test/sigbad.bin", "tst:test/sigbad.bin");
    pass &= expect_denied(b"tst", b"test/sigunk.bin", "tst:test/sigunk.bin");

    // Oggetto inesistente: `NotFound`, non "non firmato".
    match verdict(b"tst", b"test/nonesiste.bin") {
        Err(civis::Error::NotFound) => {}
        Err(e) => {
            println!("[usertests] t66: chiave assente -> errore {:?} (atteso NotFound)", e);
            pass = false;
        }
        Ok((ok, _)) => {
            println!("[usertests] t66: chiave assente -> esito {} (atteso NotFound)", ok);
            pass = false;
        }
    }

    pass
}

/// Spawna `img` e legge la classe autorevole dal kernel, poi uccide il figlio.
fn spawn_bytes_and_class(img: &[u8], name: &str) -> Option<civis::ImageClass> {
    if img.is_empty() {
        return None;
    }
    let meta = civis::SpawnMeta::new(name, 16, &[])?;
    let chan = civis::spawn_image(img, &meta).ok()? as u64;
    let pid = civis::peer_pid(chan).ok()?;
    let class = civis::peer_info_class(chan).ok().map(|(_, c)| c);
    let _ = civis::kill(pid, 0);
    class
}

/// Spawna `path` (nel namespace) e legge la classe autorevole dal kernel, poi
/// uccide il figlio. `None` = spawn fallito.
fn spawn_and_class(path: &str, name: &str) -> Option<civis::ImageClass> {
    let img = civis::load_file(path)?;
    spawn_bytes_and_class(&img, name)
}

/// t67 — la classe arriva al PCB dal **kernel** (ADR-0079 fase 3).
///
/// La classe non e' una dichiarazione del client: cardo la registra per
/// `ImageId` (contenuto verificato) e il kernel la riapplica al load. Quindi:
///  1. helper firmato `system` spedito in `tst` -> classe `System` (la firma
///     vince sul bucket: e' il contenuto a contare);
///  2. helper non firmato -> `Untrusted`;
///  3. sidecar valido di un ALTRO contenuto (cardo nega) -> anche spawnando
///     direttamente i byte, il kernel non ha una trust entry: `Untrusted`.
///     E' la prova che l'enforcement e' nel kernel, non nel chiamante.
pub fn t_image_class() -> bool {
    helpers::drain_stray();
    let mut pass = true;

    // (1) Classe di un processo **System vivo**: cardo. Il kernel la espone
    // dal PCB (`peer_info_class` -> `rdx`), quindi la classe e' quella
    // misurata sui byte, non una dichiarazione del client.
    match civis::service_lookup(civis::Service::Cardo) {
        Ok(chan) => match civis::peer_info_class(chan as u64) {
            Ok((_, civis::ImageClass::System)) => {}
            other => {
                println!("[usertests] t67: cardo classe {:?} (atteso System)", other);
                pass = false;
            }
        },
        Err(_) => {
            println!("[usertests] t67: cardo non raggiungibile");
            pass = false;
        }
    }
    // S3.4a: da `Untrusted` non si eleva. `sigtest` e' firmato `system`:
    // lo spawn da questa suite deve essere **rifiutato dal kernel**.
    if let Some(img) = civis::load_file("/test/sigtest.bin") {
        let _ = verdict(b"tst", b"test/sigtest.bin"); // cardo registra System
        match civis::SpawnMeta::new("sigtest", 16, &[]) {
            Some(meta) => match civis::spawn_image(&img, &meta) {
                Ok(_) => {
                    println!("[usertests] t67: spawn System da Untrusted ammesso (gate rotto)");
                    pass = false;
                }
                Err(_) => {}
            },
            None => pass = false,
        }
    } else {
        println!("[usertests] t67: sigtest non caricabile");
        pass = false;
    }

    // (2) Non firmato: eseguibile ma `Untrusted`.
    match verdict(b"ns", b"test/demo.bin") {
        Ok((true, c)) if c == civis::ImageClass::Untrusted => {}
        other => {
            println!("[usertests] t67: demo verdetto {:?} (atteso Untrusted)", other);
            pass = false;
        }
    }
    match spawn_and_class("/test/demo.bin", "demo") {
        Some(civis::ImageClass::Untrusted) => {}
        Some(c) => {
            println!("[usertests] t67: demo classe {:?} (atteso Untrusted)", c);
            pass = false;
        }
        None => {
            println!("[usertests] t67: spawn demo fallito");
            pass = false;
        }
    }

    // (3) Il TOCTOU verifica→load (classe System → Untrusted su byte
    // modificati) non e' piu' testabile da qui: S3.4a vieta a un `Untrusted`
    // di spawnare un'immagine `System` (no elevazione), quindi il rifiuto in
    // (1) copre gia' la proprieta' centrale — la classe segue i byte misurati
    // dal kernel, non una dichiarazione.

    // (4) Gate strutturale: usertests e' figlio di init (lineage ok) ma la
    // sua immagine e' `Untrusted` (suite in `tst`). Uno slot di servizio di
    // sistema non si registra: e' il gate su classe, isolato dal lineage.
    match civis::service_register(civis::Service::Net) {
        Err(_) => {}
        Ok(()) => {
            println!("[usertests] t67: registrato Service::Net da Untrusted (gate rotto)");
            pass = false;
        }
    }

    pass
}

/// t68 — porte I/O dal manifest (ADR-0079 fase 4).
///
/// La sonda `ioprobe` e' firmata `system` e la sua riga in `sys:manifest`
/// concede `80-80`. Dopo la verifica (cardo registra classe **e** porte nella
/// trust table) il kernel scrive il range nel TSS del processo: la lettura di
/// `0x80` riesce senza che il chiamante dichiari nulla.
///
/// Poi la stessa sonda, spawnata con `SpawnMeta.io_ranges` che dichiara
/// `0x2F8`, legge quella porta: il kernel **ignora** la dichiarazione, il TSS
/// non la consente → `#GP` → il processo muore con `FAULT_EXIT_CODE`.
pub fn t_io_ranges() -> bool {
    helpers::drain_stray();
    let mut pass = true;

    // S3.4a — `ioprobe` e' firmato `system`; questa suite gira come `tst`
    // (`Untrusted`), quindi il kernel **rifiuta** lo spawn (no elevazione).
    // Le porte del manifest restano testate dal probe `usr` fuori dal gate
    // (floor `ThirdParty`, immagine `ThirdParty` con porte): qui si prova il
    // muro dei privilegi, non un check facoltativo del client.
    if let Some(img) = civis::load_file("/test/ioprobe.bin") {
        let _ = verdict(b"tst", b"test/ioprobe.bin"); // cardo registra System
        match civis::SpawnMeta::new("ioprobe", 16, &[]) {
            Some(meta) => match civis::spawn_image(&img, &meta) {
                Ok(_) => {
                    println!("[usertests] t68: spawn System da Untrusted ammesso (gate rotto)");
                    pass = false;
                }
                Err(_) => {}
            },
            None => pass = false,
        }
    } else {
        println!("[usertests] t68: ioprobe non caricabile");
        pass = false;
    }

    pass
}
