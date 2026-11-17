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

    // (1) Firmato system: prima la verifica (cardo registra), poi lo spawn.
    match verdict(b"tst", b"test/sigtest.bin") {
        Ok((true, c)) if c == civis::ImageClass::System => {}
        other => {
            println!("[usertests] t67: sigtest verdetto {:?} (atteso System)", other);
            pass = false;
        }
    }
    match spawn_and_class("/test/sigtest.bin", "sigtest") {
        Some(civis::ImageClass::System) => {}
        Some(c) => {
            println!("[usertests] t67: sigtest classe {:?} (atteso System)", c);
            pass = false;
        }
        None => {
            println!("[usertests] t67: spawn sigtest fallito");
            pass = false;
        }
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

    // (3) TOCTOU verifica→load: cardo verifica il contenuto su disco (registra
    // `System` per quell'id), poi si spawna una copia con UN BIT diverso in
    // memoria. Il kernel ricalcola l'`ImageId` sui byte caricati: non combacia
    // con la trust entry e la classe cade su `Untrusted`. E' la proprieta'
    // centrale: la classe segue i byte che girano, mai una dichiarazione.
    if let Some(mut img) = civis::load_file("/test/sigtest.bin") {
        // Verifica il contenuto VERO (quello su disco): cardo registra System.
        let _ = verdict(b"tst", b"test/sigtest.bin");
        if img.len() > 400 {
            img[300] ^= 1; // un bit in una sezione dati: ELF ancora valido
        }
        match spawn_bytes_and_class(&img, "toc") {
            Some(civis::ImageClass::Untrusted) => {}
            Some(c) => {
                println!("[usertests] t67: toctou classe {:?} (atteso Untrusted)", c);
                pass = false;
            }
            None => {
                println!("[usertests] t67: spawn toctou fallito");
                pass = false;
            }
        }
    } else {
        println!("[usertests] t67: sigtest non caricabile (TOCTOU)");
        pass = false;
    }

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
