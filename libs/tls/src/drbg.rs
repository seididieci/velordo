//! DRBG per TLS: ChaCha20 seminato dall'entropia HW.
//!
//! `embedded-tls` chiede un `CryptoRngCore` per i nonce e le chiavi di
//! handshake. Qui c'e' un DRBG ChaCha20 (`rand_chacha`) seminato da
//! **RDRAND** (CPUID-gated: senza supporto l'istruzione farebbe `#UD`, non
//! basta il flag CF) e, in mancanza, da `rdtsc` + un valore di mix fornito
//! dal chiamante.
//!
//! Il fallback e' **dichiarato debole**: sotto QEMU TCG senza RDRAND
//! l'entropia e' solo temporale. E' accettabile per i test e per un sistema
//! senza TPM/HSM; in produzione il seed va preso dall'HW (RDRAND c'e' su
//! qualunque x86_64 recente e `run.sh` passa `+rdrand,+rdseed`).
//!
//! Reseed: ogni MiB generato il DRBG si risemina (solo per le istanze
//! `from_hw`: quelle `from_seed` restano deterministiche per i test).

use rand_chacha::ChaCha20Rng;
use rand_core::{CryptoRng, RngCore, SeedableRng};

/// Byte generati prima di una risemina (solo istanze `from_hw`).
const RESEED_BYTES: u64 = 1 << 20;

/// DRBG ChaCha20 (implementa `rand_core::RngCore + CryptoRng`).
///
/// `ChaCha20Rng` di `rand_chacha` ha align 16 (per le word SIMD di
/// `ppv-lite86`), ma l'heap di civis supporta solo align <= 8. Per poter
/// allocare `Drbg` (e quindi `Provider`, che lo contiene) sull'heap del
/// processo user, lo avvolgiamo in una struct a align 8: `ChaCha20Rng`
/// lavora su array di `u32` e non richiede align 16 per correttezza, solo
/// per performance, quindi forzare align 8 e' sicuro.
#[repr(align(8))]
struct ChaCha8(ChaCha20Rng);

pub struct Drbg {
    rng: ChaCha8,
    produced: u64,
    reseed: bool,
    mix: u64,
}

impl Drbg {
    /// DRBG deterministico da un seed esplicito (test, riproducibilita').
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            rng: ChaCha8(ChaCha20Rng::from_seed(seed)),
            produced: 0,
            reseed: false,
            mix: 0,
        }
    }

    /// DRBG seminato dall'entropia HW. `mix` e' un valore del chiamante
    /// (es. `pid ^ ticks`) per diversificare due istanze nello stesso tick.
    pub fn from_hw(mix: u64) -> Self {
        Self {
            rng: ChaCha8(ChaCha20Rng::from_seed(seed_material(mix))),
            produced: 0,
            reseed: true,
            mix,
        }
    }

    fn account(&mut self, n: u64) {
        self.produced = self.produced.saturating_add(n);
        if self.reseed && self.produced >= RESEED_BYTES {
            self.rng = ChaCha8(ChaCha20Rng::from_seed(seed_material(self.mix)));
            self.produced = 0;
        }
    }
}

impl RngCore for Drbg {
    fn next_u32(&mut self) -> u32 {
        self.account(4);
        self.rng.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.account(8);
        self.rng.0.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.account(dest.len() as u64);
        self.rng.0.fill_bytes(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.account(dest.len() as u64);
        self.rng.0.try_fill_bytes(dest)
    }
}

impl CryptoRng for Drbg {}

/// Materiale di seed: 4 word da RDRAND, altrimenti `rdtsc` mescolato.
fn seed_material(mix: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let w = rdrand64()
            .unwrap_or_else(|| tsc())
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ mix.rotate_left((i as u32) * 13)
            ^ (i as u64);
        chunk.copy_from_slice(&w.to_le_bytes());
    }
    out
}

#[cfg(target_arch = "x86_64")]
fn has_rdrand() -> bool {
    // CPUID leaf 1, ECX bit 30 = RDRAND. Necessario: su CPU che non lo
    // espongono l'istruzione va in #UD (il flag CF non c'e' proprio).
    let ecx = core::arch::x86_64::__cpuid(1).ecx;
    ecx & (1 << 30) != 0
}

#[cfg(target_arch = "x86_64")]
fn rdrand64() -> Option<u64> {
    if !has_rdrand() {
        return None;
    }
    let mut v = 0u64;
    // RDRAND puo' fallire transitoriamente (CF=0): pochi tentativi, poi
    // fallback. Mai un loop infinito.
    for _ in 0..10 {
        if unsafe { core::arch::x86_64::_rdrand64_step(&mut v) } == 1 {
            return Some(v);
        }
    }
    None
}

#[cfg(target_arch = "x86_64")]
fn tsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(not(target_arch = "x86_64"))]
fn rdrand64() -> Option<u64> {
    None
}

#[cfg(not(target_arch = "x86_64"))]
fn tsc() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Drbg::from_seed([7u8; 32]);
        let mut b = Drbg::from_seed([7u8; 32]);
        let mut x = [0u8; 64];
        let mut y = [0u8; 64];
        a.fill_bytes(&mut x);
        b.fill_bytes(&mut y);
        assert_eq!(x, y);
    }

    #[test]
    fn different_seed_different_stream() {
        let mut a = Drbg::from_seed([1u8; 32]);
        let mut b = Drbg::from_seed([2u8; 32]);
        let mut x = [0u8; 64];
        let mut y = [0u8; 64];
        a.fill_bytes(&mut x);
        b.fill_bytes(&mut y);
        assert_ne!(x, y);
    }

    #[test]
    fn stream_does_not_repeat() {
        let mut a = Drbg::from_seed([3u8; 32]);
        let mut x = [0u8; 64];
        let mut y = [0u8; 64];
        a.fill_bytes(&mut x);
        a.fill_bytes(&mut y);
        assert_ne!(x, y);
    }

    #[test]
    fn hw_seed_is_usable() {
        // Non verifichiamo la qualita' (dipende dall'host): solo che il
        // generatore produca byte e non vada in panic.
        let mut d = Drbg::from_hw(0xdead_beef);
        let mut buf = [0u8; 48];
        d.fill_bytes(&mut buf);
        assert!(buf.iter().any(|b| *b != 0));
    }

    #[test]
    fn mix_changes_seed() {
        let mut a = Drbg::from_hw(1);
        let mut b = Drbg::from_hw(2);
        let mut x = [0u8; 32];
        let mut y = [0u8; 32];
        a.fill_bytes(&mut x);
        b.fill_bytes(&mut y);
        // Su host con RDRAND il seed e' comunque diverso per costruzione.
        assert_ne!(x, y);
    }
}
