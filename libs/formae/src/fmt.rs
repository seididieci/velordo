//! `fmt` — numeri e tempi in testo, **senza allocare**.
//!
//! Scrivono in un buffer del chiamante e ritornano i byte usati (`0` = non ci
//! stanno). Servono al monitor (e a ogni dashboard) per non avere ognuno la
//! propria copia di "quanti MiB sono 800000 frame".

/// Scrive `v` in decimale.
pub fn uint(v: u64, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    let mut v = v;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if n > out.len() {
        return 0;
    }
    for i in 0..n {
        out[i] = tmp[n - 1 - i];
    }
    n
}

/// Scrive `v` su **due cifre** (troncando l'eccesso di ordine superiore).
fn pad2(v: u64, out: &mut [u8]) -> usize {
    if out.len() < 2 {
        return 0;
    }
    out[0] = b'0' + ((v / 10) % 10) as u8;
    out[1] = b'0' + (v % 10) as u8;
    2
}

/// `hh:mm:ss` da un numero di tick a `hz` Hz. Le ore non hanno tetto.
pub fn clock(ticks: u64, hz: u64, out: &mut [u8]) -> usize {
    let secs = ticks / hz.max(1);
    let h = secs / 3600;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    let mut tmp = [0u8; 24];
    let mut n = uint(h, &mut tmp);
    if n == 0 || n >= 20 {
        return 0;
    }
    tmp[n] = b':';
    n += 1;
    n += pad2(m, &mut tmp[n..]);
    tmp[n] = b':';
    n += 1;
    n += pad2(s, &mut tmp[n..]);
    if n > out.len() {
        return 0;
    }
    out[..n].copy_from_slice(&tmp[..n]);
    n
}

/// Percentuale da permille: `decimals = 0` → `12%`, `1` → `1.4%` (troncata,
/// mai arrotondata per eccesso: un 99,6% non deve leggersi 100%).
pub fn percent(permille: u32, decimals: u8, out: &mut [u8]) -> usize {
    let mut n = uint((permille / 10) as u64, out);
    if n == 0 {
        return 0;
    }
    if decimals > 0 {
        if n + 2 > out.len() {
            return 0;
        }
        out[n] = b'.';
        out[n + 1] = b'0' + (permille % 10) as u8;
        n += 2;
    }
    if n + 1 > out.len() {
        return 0;
    }
    out[n] = b'%';
    n + 1
}

/// Capacita' in unita' **binarie** da frame di 4 KiB: `(intero, decimo,
/// unita')`, con MiB sotto il GiB.
pub fn mem(frames: u64) -> (u64, u64, &'static str) {
    let mib = frames / 256;
    if mib >= 1024 {
        let tenths = mib.saturating_mul(10) / 1024;
        (tenths / 10, tenths % 10, "GiB")
    } else {
        (mib, 0, "MiB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: usize, b: &[u8]) -> &str {
        core::str::from_utf8(&b[..n]).unwrap()
    }

    #[test]
    fn interi_e_percentuali() {
        let mut b = [0u8; 16];
        let n = uint(0, &mut b);
        assert_eq!(s(n, &b), "0");
        let n = uint(12345, &mut b);
        assert_eq!(s(n, &b), "12345");
        let n = percent(0, 0, &mut b);
        assert_eq!(s(n, &b), "0%");
        let n = percent(996, 0, &mut b);
        assert_eq!(s(n, &b), "99%", "tronca, non arrotonda");
        let n = percent(14, 1, &mut b);
        assert_eq!(s(n, &b), "1.4%");
        let n = percent(1000, 1, &mut b);
        assert_eq!(s(n, &b), "100.0%");
        // Buffer troppo piccolo: zero byte, nessuno sbordo.
        let mut tiny = [0u8; 2];
        assert_eq!(uint(1234, &mut tiny), 0);
    }

    #[test]
    fn orologio() {
        let mut b = [0u8; 16];
        let n = clock(0, 100, &mut b);
        assert_eq!(s(n, &b), "0:00:00");
        let n = clock(100 * (12 * 3600 + 34 * 60 + 56), 100, &mut b);
        assert_eq!(s(n, &b), "12:34:56");
        // 100 Hz: un'ora sono 360000 tick.
        let n = clock(360_000, 100, &mut b);
        assert_eq!(s(n, &b), "1:00:00");
    }

    #[test]
    fn memoria_binaria() {
        // 4 KiB per frame: 256 frame = 1 MiB.
        assert_eq!(mem(256 * 800), (800, 0, "MiB"));
        assert_eq!(mem(256 * 1024), (1, 0, "GiB"));
        // 1.4 GiB circa: 1433 MiB = 14330 decimi / 1024 = 13 decimi.
        let (i, d, u) = mem(256 * 1433);
        assert_eq!((i, d, u), (1, 3, "GiB"));
    }
}
