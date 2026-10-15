//! casualita' Velordo (S1.3): RDRAND hardware quando c'e', fallback TSC.
//!
//! S2.5d: su qemu64+TCG la CPUID espone RDRAND ma l'istruzione fallisce
//! sempre (10/10 retry → prima abortiva qui, kill silenzioso 134 dello
//! staticone al primo RandomState/thread-RNG). Ora: RDRAND se riesce,
//! altrimenti fallback xorshift64* seminato da (rdtsc, pid, stack, ticks)
//! con UN log loud alla prima attivazione. Onesta' dichiarata: il fallback
//! NON e' crypto-grade (hash DoS-sensitive degradati) — ma mente mai in
//! silenzio; su HW con RDRAND vero il path resta quello.

use core::arch::x86_64::{__cpuid, _rdrand64_step};
use core::sync::atomic::{AtomicBool, Ordering};

/// CPUID.01H:ECX.RDRAND [bit 30] (S2.5d: il bit puo' esserci senza che
/// l'istruzione funzioni — vedi TCG sopra: il retry decide comunque).
fn rdrand_supported() -> bool {
    unsafe { (__cpuid(1).ecx & (1 << 30)) != 0 }
}

fn rdtsc_now() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}

static FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

/// Riempie `out` con il fallback (xorshift64*): `state` entra ed esce
/// aggiornato (niente stato globale oltre il log once).
fn fallback_fill(out: &mut [u8], mut state: u64) -> u64 {
    if !FALLBACK_LOGGED.swap(true, Ordering::Relaxed) {
        crate::println!("[pal] RDRAND assente/fallito: RNG debole (rdtsc), non crypto-grade");
    }
    let mut chunks = out.chunks_exact_mut(8);
    for c in &mut chunks {
        // xorshift64* (Marsaglia): periodo pieno, basta per seed non-crypto.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        c.copy_from_slice(&state.wrapping_mul(0x2545F4914F6CDD1D).to_ne_bytes());
    }
    let rem = chunks.into_remainder();
    if !rem.is_empty() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        rem.copy_from_slice(&state.wrapping_mul(0x2545F4914F6CDD1D).to_ne_bytes()[..rem.len()]);
    }
    state
}

/// Prova un u64 da RDRAND (10 retry Intel): `Some` se riesce.
fn rdrand_u64() -> Option<u64> {
    for _ in 0..10 {
        unsafe {
            let mut tmp = 0u64;
            if _rdrand64_step(&mut tmp) == 1 {
                return Some(tmp);
            }
        }
    }
    None
}

pub fn fill_bytes(buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    // Path veloce: RDRAND se la CPU lo dichiara E riesce davvero.
    if rdrand_supported() {
        let mut chunks = buf.chunks_exact_mut(8);
        let mut all_ok = true;
        for c in &mut chunks {
            match rdrand_u64() {
                Some(v) => c.copy_from_slice(&v.to_ne_bytes()),
                None => {
                    all_ok = false;
                    break;
                }
            }
        }
        let rem = chunks.into_remainder();
        if all_ok && !rem.is_empty() {
            match rdrand_u64() {
                Some(v) => rem.copy_from_slice(&v.to_ne_bytes()[..rem.len()]),
                None => all_ok = false,
            }
        }
        if all_ok {
            return;
        }
        // Caduto a meta': ricomincia intero col fallback (mai mezzo RDRAND
        // e mezzo PRNG nello stesso buffer: boundary onesto).
    }
    // Fallback: seed da entropia disponibile (rdtsc + stack + ticks; niente
    // pid: il kernel non espone il proprio e la PAL lo nega con panic).
    let sp: usize;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    let mut seed = rdtsc_now()
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(sp as u64)
        .wrapping_add(crate::sys::pal::ticks());
    if seed == 0 {
        seed = 0x243F6A8885A308D3;
    }
    fallback_fill(buf, seed);
}
