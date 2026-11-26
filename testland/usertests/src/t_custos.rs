//! t69 — Custos, ruoli e capability per identita' (ADR-0080, fase 5).
//!
//! Verifica il servizio di policy end-to-end:
//!  1. `QUERY` di un'immagine con grant seedato (`netecho` porta 7/8) e di
//!     una porta non concessa -> concesso/negato.
//!  2. Identita' sconosciuta -> negato (fail-closed).
//!  3. Ruoli multipli: `WHOAMI` di root / net-admin / user rispecchia i bit
//!     del dato `sys:identis` (risolti live).
//!  4. Autorita': un `user` non crea utenti e non fa `GRANT`; un `net-admin`
//!     concede rete ma non hardware; root concede tutto.
//!  5. `sys:caps` e' scrivibile solo da Custos/init: `obj_put` da usertests
//!     (che ha `RIGHTS_ALL` da `sys:test-policy`) e' negato.
//!  7. Solo con `net=1` (run di rete, fuori gate): il servizio `net` applica
//!     la stessa decisione — porta nota senza grant negata, effimera libera.
//!
//! Il test parla coi servizi per nome (nessuna syscall nuova) e usa i soli
//! protocolli condivisi (`identis-proto`, `custos-proto`).

use super::*;

use blake2s::blake2s;
use custos_proto as custos;
use identis_proto as identis;

/// Tentativi su `R_BUSY` (transazione gia' in corso).
const RETRIES: u32 = 40;

/// Identita' fittizia per i grant (non corrisponde a nessuna immagine reale).
const FAKE_LO: u64 = 0x1122_3344_5566_7788;
const FAKE_HI: u64 = 0x99AA_BBCC_DDEE_FF00;

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Verifica credenziali: se tornano giuste Identis lega il **pid chiamante**
/// all'utente (S3.3c: nessun token). `true` = ora sono `user`.
fn login(chan: u64, user: &str, pwd: &str) -> bool {
    let hash = blake2s(pwd.as_bytes());
    let payload = identis::build_verify(user, &hash);
    match identis::send_payload_retry(chan, civis::IDENTIS_VERIFY, &payload, RETRIES) {
        Ok(r) => r.ok(),
        Err(_) => false,
    }
}

/// Ruoli correnti dell'identita' del chiamante (`w1 = 0`: il mio pid).
fn whoami(chan: u64) -> Option<u64> {
    match civis::send(chan, civis::IDENTIS_WHOAMI, 0, 0) {
        Ok(r) if r.w0 == identis::R_OK => Some(r.w1),
        _ => None,
    }
}

/// Crea un utente con i ruoli dati; `true` se Identis accetta (l'autorita' e'
/// quella dell'identita' attestata del chiamante).
fn add_user(chan: u64, name: &str, pwd: &str, roles: u64) -> bool {
    let hash = blake2s(pwd.as_bytes());
    let payload = identis::build_add_user(name, &hash, roles);
    match identis::send_payload_retry(chan, civis::IDENTIS_ADD_USER, &payload, RETRIES) {
        Ok(r) => r.ok(),
        Err(_) => false,
    }
}

/// `GRANT`/revoca via Custos (l'identita' e' il mio pid attestato).
fn grant(chan: u64, kind: u64, arg: u64, decision: bool) -> u64 {
    let payload = custos::build_grant(FAKE_LO, FAKE_HI, kind, arg, decision);
    match custos::send_payload_retry(chan, civis::CUSTOS_GRANT, &payload, RETRIES) {
        Ok(r) => r.w0,
        Err(_) => u64::MAX,
    }
}

/// Query di capability per l'identita' fittizia.
fn query_fake(chan: u64, kind: u64, arg: u64) -> bool {
    custos::query(chan, FAKE_LO, FAKE_HI, kind, arg)
}

pub fn t_custos() -> bool {
    helpers::drain_stray();
    let mut pass = true;

    let custos_chan = match civis::service_lookup(civis::Service::Custos) {
        Ok(c) if c > 0 => c as u64,
        _ => {
            println!("[usertests] t69: servizio Custos non trovato");
            return false;
        }
    };
    let identis_chan = match civis::service_lookup(civis::Service::Identis) {
        Ok(c) if c > 0 => c as u64,
        _ => {
            println!("[usertests] t69: servizio Identis non trovato");
            return false;
        }
    };

    // (1) Grant seedati: netecho puo' bindare 7 e 8, non 9.
    let netecho = match civis::load_file("/bin/netecho.bin") {
        Some(b) if !b.is_empty() => civis::image_id(&b),
        _ => {
            println!("[usertests] t69: /bin/netecho.bin non caricabile");
            return false;
        }
    };
    let ok7 = custos::query(custos_chan, netecho.lo, netecho.hi, custos::CAP_NET_BIND, 7);
    let ok8 = custos::query(custos_chan, netecho.lo, netecho.hi, custos::CAP_NET_BIND, 8);
    let no9 = custos::query(custos_chan, netecho.lo, netecho.hi, custos::CAP_NET_BIND, 9);
    if !(ok7 && ok8 && !no9) {
        println!(
            "[usertests] t69: query netecho 7={} 8={} 9={} (attesi true,true,false)",
            ok7, ok8, no9
        );
        pass = false;
    }

    // (2) Identita' sconosciuta: fail-closed.
    if query_fake(custos_chan, custos::CAP_NET_BIND, 7) {
        println!("[usertests] t69: identita' ignota concesso (atteso negato)");
        pass = false;
    }

    // (3) Identita' root + ruoli live.
    if !login(identis_chan, "root", "root") {
        println!("[usertests] t69: login root fallito");
        return false;
    }
    match whoami(identis_chan) {
        Some(m) if m & identis::ROLE_ROOT != 0 => {}
        other => {
            println!("[usertests] t69: WHOAMI root {:?} (atteso bit root)", other);
            pass = false;
        }
    }

    // (4) Ruoli multipli: net-admin + user.
    let admin_roles = identis::ROLE_NET_ADMIN | identis::ROLE_USER;
    if !add_user(identis_chan, "t69admin", "segreto69", admin_roles) {
        println!("[usertests] t69: creazione t69admin fallita");
        pass = false;
    }
    if !login(identis_chan, "t69admin", "segreto69") {
        println!("[usertests] t69: login t69admin fallito");
        return false;
    }
    match whoami(identis_chan) {
        Some(m) if m == admin_roles => {}
        other => {
            println!("[usertests] t69: WHOAMI t69admin {:?} (attesi {:?})", other, admin_roles);
            pass = false;
        }
    }
    // Serve root per creare l'utente semplice (un solo pid, un'identita'):
    // si rientra come root, si crea, poi si entra come t69user.
    if !login(identis_chan, "root", "root") {
        println!("[usertests] t69: re-login root fallito");
        return false;
    }
    if !add_user(identis_chan, "t69user", "segreto69", identis::ROLE_USER) {
        println!("[usertests] t69: creazione t69user fallita");
        pass = false;
    }
    if !login(identis_chan, "t69user", "segreto69") {
        println!("[usertests] t69: login t69user fallito");
        return false;
    }
    match whoami(identis_chan) {
        Some(m) if m == identis::ROLE_USER => {}
        other => {
            println!("[usertests] t69: WHOAMI t69user {:?} (atteso user)", other);
            pass = false;
        }
    }

    // (5) Autorita' di concessione.
    // Un `user` non crea utenti.
    let hash = blake2s(b"x");
    let payload = identis::build_add_user("t69boss", &hash, identis::ROLE_USER);
    match identis::send_payload_retry(identis_chan, civis::IDENTIS_ADD_USER, &payload, RETRIES) {
        Ok(r) if r.w0 == identis::R_UNAUTHORIZED => {}
        other => {
            println!("[usertests] t69: user crea utenti {:?} (atteso UNAUTHORIZED)", other.map(|r| r.w0));
            pass = false;
        }
    }
    // Un `user` non fa GRANT.
    let code = grant(custos_chan, custos::CAP_NET_BIND, 4242, true);
    if code != custos::R_UNAUTHORIZED {
        println!("[usertests] t69: GRANT da user -> {} (atteso UNAUTHORIZED)", code);
        pass = false;
    }
    // Un `net-admin` concede rete ma non hardware.
    if !login(identis_chan, "t69admin", "segreto69") {
        println!("[usertests] t69: re-login t69admin fallito");
        return false;
    }
    let code = grant(custos_chan, custos::CAP_NET_BIND, 4242, true);
    if code != custos::R_OK {
        println!("[usertests] t69: GRANT net-admin rete -> {} (atteso OK)", code);
        pass = false;
    }
    let code = grant(custos_chan, custos::CAP_HW_MMIO, 0, true);
    if code != custos::R_UNAUTHORIZED {
        println!("[usertests] t69: GRANT net-admin hardware -> {} (atteso UNAUTHORIZED)", code);
        pass = false;
    }
    // Il grant di rete e' effettivo.
    if !query_fake(custos_chan, custos::CAP_NET_BIND, 4242) {
        println!("[usertests] t69: grant di rete non applicato");
        pass = false;
    }

    // (6) `sys:caps` e' un dato di Custos: la suite (ALL) non lo scrive.
    match civis::obj_put(b"sys", b"caps", b"tampered\n") {
        Err(civis::Error::Denied) => {}
        other => {
            println!("[usertests] t69: obj_put sys:caps {:?} (atteso Denied)", other);
            pass = false;
        }
    }
    // La riga del grant e' nel dato persistito (persistenza prima della RAM).
    match civis::obj_get(b"sys", b"caps") {
        Ok(data) if contains(&data, b" 4242 1") => {}
        Ok(_) => {
            println!("[usertests] t69: sys:caps senza la riga del grant 4242");
            pass = false;
        }
        Err(e) => {
            println!("[usertests] t69: obj_get sys:caps {:?}", e);
            pass = false;
        }
    }

    // Revoca con root: la decisione torna negata ovunque.
    if !login(identis_chan, "root", "root") {
        println!("[usertests] t69: re-login root per la revoca fallito");
        return false;
    }
    let code = grant(custos_chan, custos::CAP_NET_BIND, 4242, false);
    if code != custos::R_OK {
        println!("[usertests] t69: revoca root -> {} (atteso OK)", code);
        pass = false;
    }
    if query_fake(custos_chan, custos::CAP_NET_BIND, 4242) {
        println!("[usertests] t69: revoca non applicata");
        pass = false;
    }

    // (7) Enforcement nel servizio (solo con `net=1`, fuori dal gate).
    // `usertests` e' `Untrusted` (suite in `tst`): `net` nega ogni bind
    // diretto, anche effimero. Il traffico del codice non firmato passa per
    // netbroker (fase 6b). In un boot senza rete il servizio non c'e' e il
    // ramo si salta.
    if let Ok(c) = civis::service_lookup(civis::Service::Net) {
        use civis::net::{Endpoint, Flow, Proto};
        let net_chan = c as u64;
        // Porta 9: nessun grant, nessun conflitto con un server vivo.
        match Endpoint::open(net_chan, Proto::Udp, Flow::Bind(9)) {
            Err(_) => {}
            Ok(mut ep) => {
                ep.close();
                println!("[usertests] t69: bind porta 9 concesso (atteso negato)");
                pass = false;
            }
        }
        // Effimera: negato a Untrusted (gate di classe, fase 6).
        match Endpoint::open(net_chan, Proto::Udp, Flow::Ephemeral) {
            Err(_) => {}
            Ok(mut ep) => {
                ep.close();
                println!("[usertests] t69: bind effimero concesso a Untrusted (atteso negato)");
                pass = false;
            }
        }
    }

    pass
}
