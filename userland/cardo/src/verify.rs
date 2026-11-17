//! Verifica della firma di un'immagine (ADR-0078, fase 2).
//!
//! Il sidecar e' indicizzato dal **contenuto**: `sig:<blake2s-256 hex>` →
//! `[VSIG][key_id:8][sig:64]`. Quindi la classe e' una proprieta' dei byte, non
//! del path: la copia in `sys` e quella in `vela` (stessi byte) hanno la stessa
//! firma, e una copia manomessa non eredita la firma dell'originale (il digest
//! cambia, il sidecar non c'e' piu').
//!
//! Regola (ADR-0078/0079): firma **obbligatoria** nei bucket di software
//! spedito (`sys`, `vela`, `usr`); altrove l'assenza di firma e' ammessa e
//! l'immagine e' `Untrusted`. Un sidecar presente ma non valido (malformato,
//! chiave ignota, byte diversi) nega **sempre**: mai un downgrade silenzioso a
//! "non firmato".
//!
//! Costo: due letture dell'oggetto (digest, poi verifica) e una Ed25519. La
//! cache evita la parte crittografica quando il sidecar e' lo stesso; la
//! chiave della cache include `key_id` e firma, quindi **sostituire** un
//! sidecar (revoca) invalida di fatto la entry senza hook di purge.

use super::*;

/// Bucket in cui un'immagine senza firma **non** e' eseguibile.
const MANDATORY_SIGNED: [&[u8]; 3] = [b"sys", b"vela", b"usr"];
/// Prefissi di path (nel namespace `ns`) che corrispondono a software
/// spedito: la firma e' obbligatoria anche quando l'oggetto e' indirizzato
/// per path POSIX (`/usr/bin/foo` → `ns:usr/bin/foo`). Il VFS detiene la
/// verita' sui path: cardo decide qui, non il client.
const MANDATORY_PATH_PREFIXES: [&[u8]; 2] = [b"bin/", b"usr/bin/"];

/// Esito della verifica (la classe e' valida solo per `Signed`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Nessun sidecar per questo contenuto.
    Unsigned,
    /// Firma valida: classe trasmessa dalla chiave.
    Signed(civis::ImageClass),
    /// Sidecar presente ma inaccettabile (malformato, chiave ignota, firma
    /// non valida). Negato ovunque.
    Bad,
}

/// Il bucket (o il path nel namespace `ns`) pretende una firma valida?
///
/// Due forme, stessa regola: il software spedito e' firmato, che lo si
/// indirizzi per bucket (`sys`/`vela`/`usr`) o per path POSIX (`bin/`,
/// `usr/bin/` nel namespace). Il path e' la forma che usa `libr::exec`.
pub fn mandatory(bucket: &[u8], key: &[u8]) -> bool {
    if MANDATORY_SIGNED.iter().any(|b| *b == bucket) {
        return true;
    }
    bucket == b"ns" && MANDATORY_PATH_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Voce di cache: il verdetto dipende dal contenuto (ImageId), dalla chiave
/// dichiarata e dalla firma; includerli rende la cache sicura anche se il
/// sidecar viene sostituito a runtime (solo init puo' farlo).
#[derive(Clone, Copy)]
struct Entry {
    used: bool,
    id: civis::ImageId,
    key_id: u64,
    sig: [u8; sig::SIG_LEN],
    class: u8,
}

const CACHE_MAX: usize = 32;
/// Codifiche interne (mai esposte): `Bad` e `Unsigned` occupano valori fuori
/// dallo spazio di `ImageClass`.
const CODE_BAD: u8 = 0xFE;
const CODE_UNSIGNED: u8 = 0xFF;

static mut CACHE: [Entry; CACHE_MAX] = [Entry {
    used: false,
    id: civis::ImageId { lo: 0, hi: 0 },
    key_id: 0,
    sig: [0u8; sig::SIG_LEN],
    class: 0,
}; CACHE_MAX];

fn code_of(v: Verdict) -> u8 {
    match v {
        Verdict::Unsigned => CODE_UNSIGNED,
        Verdict::Signed(c) => c.as_u64() as u8,
        Verdict::Bad => CODE_BAD,
    }
}

fn verdict_of(code: u8) -> Verdict {
    match code {
        CODE_UNSIGNED => Verdict::Unsigned,
        CODE_BAD => Verdict::Bad,
        c => Verdict::Signed(civis::ImageClass::from_u64(c as u64)),
    }
}

/// Cerca una entry; `None` = miss.
fn cache_get(id: civis::ImageId, key_id: u64, image_sig: &[u8; sig::SIG_LEN]) -> Option<Verdict> {
    unsafe {
        let cache = core::ptr::addr_of!(CACHE).cast::<Entry>();
        for i in 0..CACHE_MAX {
            let e = &*cache.add(i);
            if e.used && e.id == id && e.key_id == key_id && &e.sig == image_sig {
                return Some(verdict_of(e.class));
            }
        }
    }
    None
}

/// Inserisce (rimpiazza la prima entry libera, altrimenti la 0: politiche
/// LRU inutili qui, l'insieme e' piccolo e le entry costano 128 B).
fn cache_put(id: civis::ImageId, key_id: u64, image_sig: &[u8; sig::SIG_LEN], v: Verdict) {
    unsafe {
        let cache = core::ptr::addr_of_mut!(CACHE).cast::<Entry>();
        let mut slot = 0usize;
        for i in 0..CACHE_MAX {
            let e = &*cache.add(i);
            if !e.used {
                slot = i;
                break;
            }
        }
        core::ptr::write(
            cache.add(slot),
            Entry { used: true, id, key_id, sig: *image_sig, class: code_of(v) },
        );
    }
}

/// Verifica l'oggetto `bucket/key` e ne ritorna il verdetto **e l'`ImageId`**
/// dei byte verificati (ADR-0079 fase 3: cardo registra l'id nella trust table
/// del kernel, che lo riusa allo spawn/exec per assegnare la classe).
///
/// `#[inline(never)]`: firewall LTO. Cardo ha 16 KiB di stack e questo e' il
/// punto in cui entrano LTO + catena del motore; i byte dell'immagine vivono
/// in un `Vec` (heap), mai su stack.
///
/// Errori: `ERR_NOTFOUND` se l'oggetto non esiste, `ERR` se il volume non e'
/// leggibile. Un'immagine mancante **non e'** "non firmata": e' un errore.
#[inline(never)]
pub fn verify_object(
    disk: &btree_drv::DiskEngine,
    bucket: &[u8],
    key: &[u8],
) -> Result<(Verdict, civis::ImageId), u64> {
    let data = disk.get(bucket, key).ok_or(civis::ERR_NOTFOUND)?;
    let digest = sig::digest(&data);
    let id = civis::ImageId::from_digest(&digest);
    let hex = sig::digest_hex(&digest);

    // Il sidecar si cerca SOLO per digest: nessuna mappa path→firma.
    let sidecar = disk.get(b"sig", &hex).and_then(|b| sig::Sidecar::parse(&b));
    let (key_id, image_sig) = match sidecar {
        Some(s) => (s.key_id, s.sig),
        None => (0u64, [0u8; sig::SIG_LEN]),
    };

    if let Some(v) = cache_get(id, key_id, &image_sig) {
        return Ok((v, id));
    }

    let verdict = match sidecar {
        None => Verdict::Unsigned,
        Some(s) => match policy::anchor_key(s.key_id) {
            Some(k) if sig::verify(&k.pk, &data, &s.sig) => Verdict::Signed(k.class),
            // Chiave ignota o firma non valida: sempre rifiutato.
            _ => Verdict::Bad,
        },
    };
    cache_put(id, key_id, &image_sig, verdict);
    Ok((verdict, id))
}

/// Verdetto applicato alla policy dei bucket: `Ok(0)` eseguibile + classe,
/// `Err(civis::ERR_DENIED)` negato.
///
/// Tabella (ADR-0078 §fase 2):
/// - `Bad` → negato ovunque;
/// - `Unsigned` → negato nei bucket obbligatori (e nei path di sistema),
///   altrimenti `Untrusted`;
/// - `Signed(c)` → eseguibile con classe `c`.
pub fn verdict_for(
    bucket: &[u8],
    key: &[u8],
    v: Verdict,
) -> Result<(u64, civis::ImageClass), u64> {
    match v {
        Verdict::Bad => Err(civis::ERR_DENIED),
        Verdict::Unsigned if mandatory(bucket, key) => Err(civis::ERR_DENIED),
        Verdict::Unsigned => Ok((0, civis::ImageClass::Untrusted)),
        Verdict::Signed(c) => Ok((0, c)),
    }
}
