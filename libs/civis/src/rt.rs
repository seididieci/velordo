//! `mem*` del runtime userland (N6.2.0, ADR-0071).
//!
//! Con SSE2 abilitato LLVM non espande piu' `memcpy`/`memset` inline: emette
//! chiamate alla libc. I simboli vanno quindi forniti, e la forma conta: sotto
//! QEMU TCG le string op sono tradotte in helper e il costo dipende da quanti
//! byte muove ogni iterazione.
//!
//! Il target **senza** SSE2 (`x86_64-unknown-none`) inlinea `rep movsb` (1 B per
//! iterazione) per le copie e `rep stosl` per i riempimenti: e' la forma di
//! riferimento misurata su `testsarca` append 512 KiB (`dt` ~5971 tick). Qui si
//! replica: `rep movsb` per `memcpy`/`memmove`, `rep stosq` (8 B per iterazione,
//! meglio dello `stosl` che usava LLVM) per `memset`, con testa/coda a byte per
//! l'allineamento. Niente `compiler-builtins-mem`: la sua versione spezza ogni
//! copia in `rep movsb` + `rep movsq` + `rep movsb` e costa +46% sotto TCG.
//!
//! L'ABI e' quella C: questi simboli sono chiamati da LLVM, non dal nostro
//! codice Rust (che usa `core::ptr`).

// I corpi sono interamente unsafe per natura (puntatori grezzi da LLVM).
#![allow(unsafe_op_in_unsafe_fn)]
// La firma e' compatibile con quella di libc (`*mut u8` vs `*mut c_void`):
// stesso layout, e i nostri `mem*` sono l'implementazione di runtime.
#![allow(suspicious_runtime_symbol_definitions)]

use core::arch::asm;

/// Copia `n` byte (regioni non sovrapposte).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    asm!(
        "rep movsb",
        inout("rcx") n => _,
        inout("rdi") dest => _,
        inout("rsi") src => _,
        options(nostack, preserves_flags),
    );
    dest
}

/// Copia `n` byte anche con regioni sovrapposte.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let from = src as usize;
    let to = dest as usize;
    if to <= from || to >= from.wrapping_add(n) {
        return memcpy(dest, src, n);
    }
    // Sovrapposizione con dest > src: indietro, poi `cld` (l'ABI esige DF=0).
    asm!(
        "std",
        "rep movsb",
        "cld",
        inout("rcx") n => _,
        inout("rdi") dest.add(n - 1) => _,
        inout("rsi") src.add(n - 1) => _,
        options(nostack),
    );
    dest
}

/// Riempie `n` byte con `c` (solo gli 8 bit bassi).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, c: i32, n: usize) -> *mut u8 {
    let byte = c as u8;
    let mut d = dest;
    let mut len = n;

    // Testa: porta `d` all'allineamento a 8 per `rep stosq`.
    let head = ((8 - (d as usize & 7)) & 7).min(len);
    if head != 0 {
        asm!(
            "rep stosb",
            inout("rcx") head => _,
            inout("rdi") d => _,
            in("al") byte,
            options(nostack, preserves_flags),
        );
        d = d.add(head);
        len -= head;
    }
    let qwords = len / 8;
    if qwords != 0 {
        let pattern = (byte as u64).wrapping_mul(0x0101_0101_0101_0101);
        asm!(
            "rep stosq",
            inout("rcx") qwords => _,
            inout("rdi") d => _,
            in("rax") pattern,
            options(nostack, preserves_flags),
        );
        d = d.add(qwords * 8);
        len -= qwords * 8;
    }
    if len != 0 {
        asm!(
            "rep stosb",
            inout("rcx") len => _,
            inout("rdi") d => _,
            in("al") byte,
            options(nostack, preserves_flags),
        );
    }
    dest
}

/// Confronto lessicografico: <0, 0, >0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0usize;
    while i + 8 <= n {
        let x = (a.add(i) as *const u64).read_unaligned();
        let y = (b.add(i) as *const u64).read_unaligned();
        if x != y {
            let off = ((x ^ y).trailing_zeros() / 8) as usize;
            let p = i + off;
            return (*a.add(p) as i32) - (*b.add(p) as i32);
        }
        i += 8;
    }
    while i < n {
        let x = *a.add(i);
        let y = *b.add(i);
        if x != y {
            return (x as i32) - (y as i32);
        }
        i += 1;
    }
    0
}

/// Come `memcmp`, ma solo uguaglianza (0 = uguali).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    memcmp(a, b, n)
}
