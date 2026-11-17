//! Firma Ed25519 delle immagini eseguibili (ADR-0078, fase 2).
//!
//! Un'immagine si firma sul **contenuto**, non su un path: il sidecar vive in
//! un namespace dedicato indicizzato dal digest (`sig:<blake2s-256 hex>`),
//! quindi una firma copre tutte le copie degli stessi byte (`sys`, `vela`,
//! `usr`, il mirror `ns`). Il formato e' una source unica per guest
//! (`userland/cardo`) e tool host (`tools/sign`).
//!
//! Il messaggio firmato NON sono i byte grezzi: e' `DOMAIN || bytes`, cosi'
//! la stessa chiave non puo' firmare un altro formato (anchor, manifest,
//! config) e una firma su un blob non vale come firma su un'immagine.
//!
//! `no_std` senza `alloc`: la verifica e' incrementale (l'immagine non viene
//! mai copiata) e la firma pure.

#![no_std]

use ed25519_compact::{KeyPair, PublicKey, Seed, Signature, VerifyingState};

/// Prefisso di dominio del messaggio firmato (include il NUL finale).
pub const DOMAIN: &[u8] = b"VELORDO-IMAGE-V1\0";
/// Magic del sidecar.
pub const MAGIC: [u8; 4] = *b"VSIG";
/// Byte di `key_id` nel sidecar (primi 8 byte di BLAKE2s-256 della pubkey).
pub const KEY_ID_LEN: usize = 8;
/// Byte di firma Ed25519.
pub const SIG_LEN: usize = 64;
/// Byte di una chiave pubblica Ed25519.
pub const PUBLIC_KEY_LEN: usize = 32;
/// Byte di un seed (chiave privata di derivazione).
pub const SEED_LEN: usize = 32;
/// Dimensione del sidecar: `[VSIG][key_id:8][sig:64]`.
pub const SIDECAR_LEN: usize = MAGIC.len() + KEY_ID_LEN + SIG_LEN;
/// Byte del digest BLAKE2s-256.
pub const DIGEST_LEN: usize = 32;
/// Cifre esadecimali del digest completo (chiave del namespace `sig`).
pub const DIGEST_HEX_LEN: usize = 2 * DIGEST_LEN;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Digest BLAKE2s-256 del contenuto (chiave del sidecar).
pub fn digest(image: &[u8]) -> [u8; DIGEST_LEN] {
    blake2s::blake2s(image)
}

/// `key_id` di una chiave pubblica: primi 8 byte del suo BLAKE2s-256 (LE).
///
/// Non e' una misura di sicurezza, e' un indice: la pubkey autorevole resta
/// quella dell'anchor, e il `key_id` nel sidecar deve combaciare con lei.
pub fn key_id(pk: &[u8; PUBLIC_KEY_LEN]) -> u64 {
    let d = blake2s::blake2s(pk);
    u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

/// Chiave pubblica derivata da un seed (stessa derivazione della firma).
pub fn public_key(seed: &[u8; SEED_LEN]) -> Option<[u8; PUBLIC_KEY_LEN]> {
    let kp = KeyPair::try_from_seed(Seed::new(*seed)).ok()?;
    let mut pk = [0u8; PUBLIC_KEY_LEN];
    pk.copy_from_slice(kp.pk.as_ref());
    Some(pk)
}

/// Firma `DOMAIN || image` col seed dato (deterministico: nessuna casualita').
pub fn sign(seed: &[u8; SEED_LEN], image: &[u8]) -> Option<[u8; SIG_LEN]> {
    let kp = KeyPair::try_from_seed(Seed::new(*seed)).ok()?;
    // `Noise` a zero = firma deterministica (RFC 8032). La casualita' qui
    // proteggerebbe solo da fault injection, non e' un requisito del formato.
    let mut st = kp.sk.sign_incremental(ed25519_compact::Noise::new([0u8; 16]));
    st.absorb(DOMAIN);
    st.absorb(image);
    let s = st.sign();
    let mut out = [0u8; SIG_LEN];
    out.copy_from_slice(s.as_ref());
    Some(out)
}

/// Verifica incrementale: `DOMAIN` e' assorbito alla creazione.
pub struct Verifier {
    state: VerifyingState,
}

impl Verifier {
    /// Prepara la verifica con `pk` e `sig`. `None` = firma/pubkey malformate
    /// (lunghezza gia' garantita dai tipi, ma il punto puo' non essere valido).
    pub fn new(pk: &[u8; PUBLIC_KEY_LEN], sig: &[u8; SIG_LEN]) -> Option<Self> {
        let pk = PublicKey::from_slice(pk).ok()?;
        let sig = Signature::from_slice(sig).ok()?;
        let mut state = pk.verify_incremental(&sig).ok()?;
        state.absorb(DOMAIN);
        Some(Self { state })
    }

    /// Assorbe un pezzo dell'immagine (l'ordine e' quello dei byte).
    pub fn absorb(&mut self, chunk: &[u8]) {
        self.state.absorb(chunk);
    }

    /// Chiude: `true` se la firma e' valida per il messaggio assorbito.
    pub fn finish(self) -> bool {
        self.state.verify().is_ok()
    }
}

/// Verifica in un colpo (immagine gia' in memoria).
pub fn verify(pk: &[u8; PUBLIC_KEY_LEN], image: &[u8], sig: &[u8; SIG_LEN]) -> bool {
    match Verifier::new(pk, sig) {
        Some(mut v) => {
            v.absorb(image);
            v.finish()
        }
        None => false,
    }
}

/// Sidecar `[VSIG][key_id:8][sig:64]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sidecar {
    /// Chiave che ha firmato (indice nell'anchor).
    pub key_id: u64,
    /// Firma Ed25519 sul messaggio con prefisso di dominio.
    pub sig: [u8; SIG_LEN],
}

impl Sidecar {
    /// Decodifica i 76 byte del sidecar; `None` a magic o lunghezza errati.
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() != SIDECAR_LEN || b[..MAGIC.len()] != MAGIC {
            return None;
        }
        let mut kid = [0u8; KEY_ID_LEN];
        kid.copy_from_slice(&b[MAGIC.len()..MAGIC.len() + KEY_ID_LEN]);
        let mut sig = [0u8; SIG_LEN];
        sig.copy_from_slice(&b[MAGIC.len() + KEY_ID_LEN..]);
        Some(Self { key_id: u64::from_le_bytes(kid), sig })
    }

    /// Encoding canonico (quello scritto dal tool host e seedato nel volume).
    pub fn encode(&self) -> [u8; SIDECAR_LEN] {
        let mut out = [0u8; SIDECAR_LEN];
        out[..MAGIC.len()].copy_from_slice(&MAGIC);
        out[MAGIC.len()..MAGIC.len() + KEY_ID_LEN].copy_from_slice(&self.key_id.to_le_bytes());
        out[MAGIC.len() + KEY_ID_LEN..].copy_from_slice(&self.sig);
        out
    }
}

/// Hex minuscolo di `bytes` in `out` (deve essere lungo `2 * bytes.len()`).
pub fn hex_encode(bytes: &[u8], out: &mut [u8]) {
    for (i, &b) in bytes.iter().enumerate() {
        if i * 2 + 1 < out.len() {
            out[i * 2] = HEX[(b >> 4) as usize];
            out[i * 2 + 1] = HEX[(b & 0x0f) as usize];
        }
    }
}

/// Hex minuscolo del digest completo (chiave del namespace `sig`).
pub fn digest_hex(d: &[u8; DIGEST_LEN]) -> [u8; DIGEST_HEX_LEN] {
    let mut out = [0u8; DIGEST_HEX_LEN];
    hex_encode(d, &mut out);
    out
}

/// Decodifica hex in `out` (deve essere lungo esattamente `s.len() / 2`);
/// `false` a lunghezza dispari, cifra non valida o dimensione sbagliata.
pub fn hex_decode(s: &[u8], out: &mut [u8]) -> bool {
    if s.len() != out.len() * 2 {
        return false;
    }
    let mut i = 0;
    while i < out.len() {
        let (Some(hi), Some(lo)) = (nibble(s[i * 2]), nibble(s[i * 2 + 1])) else {
            return false;
        };
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    true
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8032 §7.1, vettori 1–3: `(seed, public key, messaggio, firma)`.
    /// Verificano la derivazione seed→pubkey e la firma Ed25519 del crate
    /// (i nostri `sign`/`verify` aggiungono il prefisso di dominio, che i
    /// vettori RFC non hanno).
    const RFC8032: [(&str, &str, &[u8], &str); 3] = [
        (
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            b"",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        ),
        (
            "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
            &[0x72],
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        ),
        (
            "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
            "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
            &[0xaf, 0x82],
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
        ),
    ];

    fn hex32(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        assert!(hex_decode(s.as_bytes(), &mut out), "hex32 invalido: {s}");
        out
    }

    fn hex64(s: &str) -> [u8; 64] {
        let mut out = [0u8; 64];
        assert!(hex_decode(s.as_bytes(), &mut out), "hex64 invalido: {s}");
        out
    }

    fn assert_hex(got: &[u8], want: &str) {
        let mut buf = [0u8; 128];
        assert!(got.len() <= 64, "buffer hex troppo corto");
        hex_encode(got, &mut buf);
        assert_eq!(&buf[..got.len() * 2], want.as_bytes());
    }

    #[test]
    fn rfc8032_keygen_and_signature() {
        for (seed_s, pk_s, msg, sig_s) in RFC8032 {
            let seed = hex32(seed_s);
            let pk = public_key(&seed).expect("seed valido");
            assert_hex(&pk, pk_s);

            // Firma "grezza" (senza dominio) per confronto col vettore: la
            // fa il crate, cosi' il test prova la derivazione, non se stesso.
            let kp = KeyPair::try_from_seed(Seed::new(seed)).expect("keypair");
            let s = kp.sk.sign(msg, None);
            assert_hex(s.as_ref(), sig_s);
            let _ = hex64(sig_s); // il vettore deve essere un hex valido
        }
    }

    #[test]
    fn sidecar_round_trip() {
        let sc = Sidecar { key_id: 0x1122_3344_5566_7788, sig: [9u8; SIG_LEN] };
        let enc = sc.encode();
        assert_eq!(enc.len(), SIDECAR_LEN);
        assert_eq!(&enc[..4], b"VSIG");
        let back = Sidecar::parse(&enc).expect("parse");
        assert_eq!(back, sc);
    }

    #[test]
    fn sidecar_rejects_bad_magic_and_length() {
        let sc = Sidecar { key_id: 1, sig: [3u8; SIG_LEN] };
        let mut enc = sc.encode();
        enc[0] = b'X';
        assert!(Sidecar::parse(&enc).is_none(), "magic errato");
        assert!(Sidecar::parse(&sc.encode()[..SIDECAR_LEN - 1]).is_none(), "corto");
        let mut long = [0u8; SIDECAR_LEN + 1];
        long[..SIDECAR_LEN].copy_from_slice(&sc.encode());
        assert!(Sidecar::parse(&long).is_none(), "lungo");
    }

    #[test]
    fn signature_binds_domain_prefix() {
        let seed = [0x42u8; SEED_LEN];
        let pk = public_key(&seed).expect("pk");
        let sig = sign(&seed, b"immagine").expect("firma");

        // Il messaggio col dominio verifica...
        assert!(verify(&pk, b"immagine", &sig));
        // ...e la stessa firma NON verifica sui byte grezzi senza prefisso.
        let raw = ed25519_compact::PublicKey::from_slice(&pk).expect("pk");
        let s = ed25519_compact::Signature::from_slice(&sig).expect("sig");
        assert!(raw.verify(b"immagine", &s).is_err(), "la firma deve coprire il dominio");

        // Un bit flippato nell'immagine rompe la verifica.
        assert!(!verify(&pk, b"immaginE", &sig));
        // Un bit flippato nella firma pure.
        let mut bad = sig;
        bad[0] ^= 1;
        assert!(!verify(&pk, b"immagine", &bad));
    }

    #[test]
    fn verifier_is_incremental() {
        let seed = [3u8; SEED_LEN];
        let pk = public_key(&seed).expect("pk");
        let sig = sign(&seed, b"abcdef").expect("firma");
        let mut v = Verifier::new(&pk, &sig).expect("verifier");
        v.absorb(b"abc");
        v.absorb(b"def");
        assert!(v.finish());
        // Spezzatura diversa, stesso messaggio: valida.
        let mut v = Verifier::new(&pk, &sig).expect("verifier");
        v.absorb(b"ab");
        v.absorb(b"cde");
        v.absorb(b"f");
        assert!(v.finish());
        // Byte in piu' (o in meno): invalida.
        let mut v = Verifier::new(&pk, &sig).expect("verifier");
        v.absorb(b"abcdefg");
        assert!(!v.finish());
    }

    #[test]
    fn key_id_is_digest_prefix_and_distinguishes_keys() {
        let a = public_key(&[1u8; SEED_LEN]).expect("pk a");
        let b = public_key(&[2u8; SEED_LEN]).expect("pk b");
        let d = blake2s::blake2s(&a);
        assert_eq!(key_id(&a), u64::from_le_bytes(d[..8].try_into().unwrap()));
        assert_ne!(key_id(&a), key_id(&b));
    }

    #[test]
    fn hex_helpers_reject_bad_input() {
        let d = [0xabu8; DIGEST_LEN];
        let h = digest_hex(&d);
        assert_eq!(h.len(), DIGEST_HEX_LEN);
        assert!(h.iter().all(|&c| c == b'a' || c == b'b'));
        let mut back = [0u8; DIGEST_LEN];
        assert!(hex_decode(&h, &mut back));
        assert_eq!(back, d);
        assert!(!hex_decode(b"abc", &mut back), "lunghezza dispari");
        assert!(!hex_decode(b"zz", &mut [0u8; 1]), "cifra invalida");
        assert!(!hex_decode(b"abcd", &mut [0u8; 1]), "dimensione errata");
    }
}
