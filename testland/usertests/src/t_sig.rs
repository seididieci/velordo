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
