//! Utilità esadecimale condivise (S2.5): codifica/decodifica hex senza
//! dipendenze esterne. Le funzioni pure (niente alloc) tranne `encode` che
//! restituisce una `String`.

/// Tabella lookup minuscola per la codifica.
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// Codifica `bytes` in stringa esadecimale minuscola. Usa `alloc`.
pub fn encode(bytes: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_LOWER[(b >> 4) as usize] as char);
        out.push(HEX_LOWER[(b & 0xF) as usize] as char);
    }
    out
}

/// Decode un singolo carattere esadecimale in un nibble (0-15).
/// Supporta maiuscole e minuscole. Ritorna `None` se non è esadecimale.
pub fn decode_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Converte un nibble (0-15) nel carattere esadecimale minuscolo corrispondente.
pub fn encode_nibble(n: u8) -> u8 {
    HEX_LOWER[n as usize]
}

/// Decode una stringa esadecimale in `out` (2 caratteri per byte).
/// `out` deve contenere almeno `s.len() / 2` byte.
/// Ritorna `Err(())` se la lunghezza è dispari, `out` troppo piccolo o
/// ci sono caratteri non esadecimali.
pub fn decode_hex(s: &str, out: &mut [u8]) -> Result<(), ()> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 || b.len() / 2 > out.len() {
        return Err(());
    }
    for i in 0..b.len() / 2 {
        let hi = decode_nibble(b[i * 2]).ok_or(())?;
        let lo = decode_nibble(b[i * 2 + 1]).ok_or(())?;
        out[i] = (hi << 4) | lo;
    }
    Ok(())
}

/// Decode una stringa esadecimale esattamente di `N` byte (2*N caratteri)
/// in un array di output. Ritorna `None` se la lunghezza non combacia o
/// ci sono caratteri non esadecimali.
pub fn decode_hex_fixed<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    decode_hex(s, &mut out).ok()?;
    Some(out)
}

/// Parusa una stringa esadecimale (come slice di byte) in `u64`.
/// Ritorna `None` se vuota, oltre 16 caratteri o contiene caratteri
/// non esadecimali.
pub fn parse_u64(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() || raw.len() > 16 {
        return None;
    }
    let mut v: u64 = 0;
    for &b in raw {
        let d = decode_nibble(b)?;
        v = (v << 4) | d as u64;
    }
    Some(v)
}
