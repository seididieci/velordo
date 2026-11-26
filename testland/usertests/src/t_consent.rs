//! t70 — consenso per immagine e broker di rete (ADR-0080, fase 6).
//!
//! Due domande distinte, entrambe necessarie perche' il privilegio esista:
//!
//! 1. **Policy admin** (`sys:caps`, fase 5): l'amministratore concede la
//!    capability all'immagine;
//! 2. **Consenso dell'utente** (`usr:consent`, fase 6): un utente, con una
//!    sessione valida, dichiara di volere che *quell'immagine* usi la rete.
//!    Assente = negato.
//!
//! Casi verificati (tutti senza `net=1` tranne l'ultimo blocco):
//!  1. `AUTHORIZE` di un'immagine con grant ma **senza** consenso → negato.
//!  2. `CONSENT` write con identita' attestata → la riga appare in
//!     `usr:consent` (persistenza) e `AUTHORIZE` diventa vero.
//!     (Il caso negativo "senza identita' → `R_NO_IDENTITY`" vive in
//!     `identistest`, che gira in un processo nuovo senza `VERIFY`: qui il
//!     pid ha gia' l'identita' lasciata da t69.)
//!  3. `CONSENT` revoke (`off`) → `AUTHORIZE` torna falso.
//!  4. Un'immagine **senza** grant admin resta negata anche col consenso:
//!     il consenso non crea privilegio dal nulla.
//!  5. `usr:consent` e' scrivibile solo da Custos/init: la suite (che ha
//!     `RIGHTS_ALL`) non lo scrive.
//!  6. Solo con `net=1` (run di rete): `net` e `siit` negano l'endpoint
//!     diretto a `Untrusted` (gate di classe) e il **broker** lo serve.

use super::*;

use blake2s::blake2s;
use custos_proto as custos;
use identis_proto as identis;

/// Tentativi su `R_BUSY` (transazione gia' in corso).
const RETRIES: u32 = 40;

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Login: se le credenziali tornano, Identis lega il **pid chiamante**
/// all'utente (S3.3c: nessun token). `true` = ora sono `user`.
fn login(chan: u64, user: &str, pwd: &str) -> bool {
    let hash = blake2s(pwd.as_bytes());
    let payload = identis::build_verify(user, &hash);
    match identis::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) => r.ok(),
        Err(_) => false,
    }
}

pub fn t_consent() -> bool {
    helpers::drain_stray();
    let mut pass = true;

    let custos_chan = match civis::service_lookup(civis::Service::Custos) {
        Ok(c) if c > 0 => c as u64,
        _ => {
            println!("[usertests] t70: servizio Custos non trovato");
            return false;
        }
    };
    let identis_chan = match civis::service_lookup(civis::Service::Identis) {
        Ok(c) if c > 0 => c as u64,
        _ => {
            println!("[usertests] t70: servizio Identis non trovato");
            return false;
        }
    };
    if !login(identis_chan, "root", "root") {
        println!("[usertests] t70: login root fallito");
        return false;
    }

    // L'immagine di prova e' `netecho` (seedato in `/bin`, grant 7/8 dal
    // build): cosi' la sola variabile tra i casi e' il consenso.
    let id = match civis::load_file("/bin/netecho.bin") {
        Some(b) if !b.is_empty() => civis::image_id(&b),
        _ => {
            println!("[usertests] t70: /bin/netecho.bin non caricabile");
            return false;
        }
    };

    // (1) Grant admin senza consenso: negato. L'immagine **ha** la
    // capability (`netecho` → porta 7 dal seed di `sys:caps`), ma nessun
    // utente ha ancora acconsentito.
    if custos::authorize(custos_chan, id.lo, id.hi, custos::CAP_NET_BIND, 7) {
        println!("[usertests] t70: autorizzato senza consenso (atteso negato)");
        pass = false;
    }

    // (2) Consenso con identita' attestata: applicato e persistito. La
    // negazione "senza identita'" vive in `identistest` (processo nuovo,
    // nessun VERIFY): qui il pid ha gia' un'identita' da t69.
    if !custos::consent_write(custos_chan, "root", id.lo, id.hi, true) {
        println!("[usertests] t70: consenso con identita' rifiutato (atteso OK)");
        pass = false;
    }
    if !custos::authorize(custos_chan, id.lo, id.hi, custos::CAP_NET_BIND, 7) {
        println!("[usertests] t70: autorizzazione non attiva dopo il consenso");
        pass = false;
    }
    let want_line = alloc::format!("{:016x} {:016x} 1", id.hi, id.lo);
    match civis::obj_get(b"usr", b"consent") {
        Ok(data) if contains(&data, want_line.as_bytes()) => {}
        Ok(_) => {
            println!("[usertests] t70: usr:consent senza la riga del consenso");
            pass = false;
        }
        Err(e) => {
            println!("[usertests] t70: obj_get usr:consent {:?}", e);
            pass = false;
        }
    }
    // Provenienza: la riga per utente (`usr:<nome>:consent`) dice CHI ha
    // acconsentito: senza, la tabella non sarebbe spiegabile ne' revocabile.
    match civis::obj_get(b"usr", b"root:consent") {
        Ok(data) if contains(&data, b" 1") => {}
        Ok(_) => {
            println!("[usertests] t70: usr:root:consent senza la riga di provenienza");
            pass = false;
        }
        Err(e) => {
            println!("[usertests] t70: obj_get usr:root:consent {:?}", e);
            pass = false;
        }
    }

    // (4) Revoca: torna negato.
    if !custos::consent_write(custos_chan, "root", id.lo, id.hi, false) {
        println!("[usertests] t70: revoca rifiutata (atteso OK)");
        pass = false;
    }
    if custos::authorize(custos_chan, id.lo, id.hi, custos::CAP_NET_BIND, 7) {
        println!("[usertests] t70: autorizzazione ancora attiva dopo la revoca");
        pass = false;
    }

    // (5) Il consenso non crea privilegio: un'identita' senza grant admin
    // resta negata anche dopo il consenso.
    let other = civis::ImageId::new(0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210);
    if !custos::consent_write(custos_chan, "root", other.lo, other.hi, true) {
        println!("[usertests] t70: consenso su identita' senza grant rifiutato (atteso OK)");
        pass = false;
    }
    if custos::authorize(custos_chan, other.lo, other.hi, custos::CAP_NET_BIND, 7) {
        println!("[usertests] t70: consenso ha creato privilegio senza grant admin");
        pass = false;
    }
    let _ = custos::consent_write(custos_chan, "root", other.lo, other.hi, false);

    // (6) `usr:consent` e' un dato di Custos: la suite (ALL) non lo scrive.
    match civis::obj_put(b"usr", b"consent", b"tampered\n") {
        Err(civis::Error::Denied) => {}
        other => {
            println!("[usertests] t70: obj_put usr:consent {:?} (atteso Denied)", other);
            pass = false;
        }
    }

    // (7) Gate di classe e broker (solo con `net=1`).
    if let Ok(c) = civis::service_lookup(civis::Service::Net) {
        let net_chan = c as u64;
        // `usertests` e' `Untrusted`: `net` gli nega OGNI endpoint diretto.
        match civis::net::Endpoint::open(net_chan, civis::net::Proto::Udp, civis::net::Flow::Ephemeral) {
            Err(_) => {}
            Ok(mut ep) => {
                ep.close();
                println!("[usertests] t70: net ha concesso un endpoint a Untrusted");
                pass = false;
            }
        }
        // ...e il broker lo serve (allowlist + rate limit, loopback).
        match civis::service_lookup(civis::Service::NetBroker) {
            Ok(b) if b > 0 => {
                use civis::broker::{BrokerEndpoint, Flow, Proto};
                match BrokerEndpoint::open(b as u64, Proto::Udp, Flow::Ephemeral) {
                    Ok(mut ep) => {
                        println!("[usertests] t70: broker ha aperto l'endpoint per Untrusted");
                        ep.close();
                    }
                    Err(e) => {
                        println!("[usertests] t70: broker ha negato l'effimera ({:?})", e);
                        pass = false;
                    }
                }
                // Porta nota non concessa dall'allowlist: negata.
                match BrokerEndpoint::open(b as u64, Proto::Udp, Flow::Bind(9)) {
                    Err(_) => {}
                    Ok(mut ep) => {
                        println!("[usertests] t70: broker ha concesso la porta 9 (atteso negato)");
                        ep.close();
                        pass = false;
                    }
                }
            }
            _ => {
                println!("[usertests] t70: servizio NetBroker non trovato (net=1)");
                pass = false;
            }
        }
    }

    pass
}
