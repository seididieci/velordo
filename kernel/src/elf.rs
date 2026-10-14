//! Loader ELF64 (Fase 31) + split condiviso/privato (Fase 32): carica i
//! segmenti `PT_LOAD` di un ELF x86_64 nello spazio user `cr3` con i flag
//! dell'ELF (W^X: `R E`→RX, `R`→RO, `RW`→RW, NX su tutto tranne il codice),
//! all'indirizzo di link (`p_vaddr`). Carichiamo sempre al vaddr di link,
//! quindi le `R_X86_64_RELATIVE` (gia' applicate dal linker con
//! `--apply-dynamic-relocs`) restano valide: **nessuna reloc a runtime**.
//!
//! Fase 32: i segmenti immutabili (`RX`/`RO`) sotto `rw_off` (la prima pagina
//! scrivibile) sono **condivisi** tra le istanze dello stesso binario
//! (`crate::text`); `[rw_off, end)` resta privato (data/bss + coda immutabile
//! della pagina a cavallo). Due fasi separate per non leakare frame su un ELF
//! malformato: `validate` (nessuna allocazione) e `load` (mappa; OOM = panic
//! come il resto dello spawn). Usato sia dai binari embedded sia da
//! `spawn_image` (ELF letto da disco: input non fidato → validazione stretta).

use crate::arc::vmm_user::{USER_CODE, USER_FS_BUFFER};

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_X86_64: u16 = 62;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
pub(crate) const PF_X: u32 = 1;
pub(crate) const PF_W: u32 = 2;
const PT_LOAD: u32 = 1;
pub(crate) const PAGE: u64 = 0x1000;
/// Program header massimi accettati (i nostri binari ne hanno 4-7).
const MAX_PHNUM: usize = 16;
/// Pagine massime dell'immagine (256 MiB, S2.5c: era 2 MiB): la finestra
/// e' [USER_CODE, USER_CODE+256M) — well dentro [USER_CODE, USER_FS_BUFFER)
/// (+1T). Oltre = rifiuto in validazione (mai OOM a load).
const MAX_PAGES: usize = 65536;

/// Un segmento `PT_LOAD` validato.
#[derive(Clone, Copy)]
pub(crate) struct Segment {
    pub(crate) offset: usize,
    pub(crate) vaddr: u64,
    pub(crate) filesz: usize,
    pub(crate) memsz: u64,
    pub(crate) flags: u32,
}

/// Immagine ELF validata (nessuna allocazione fatta).
pub struct Layout {
    pub(crate) entry: u64,
    /// Base page-aligned dell'immagine.
    pub(crate) base: u64,
    /// Fine page-aligned dell'immagine.
    pub(crate) end: u64,
    /// Prima pagina scrivibile: `[base, rw_off)` e' immutabile (condivisibile),
    /// `[rw_off, end)` e' privato. `rw_off == end` se non c'e' alcun segmento W.
    pub(crate) rw_off: u64,
    pub(crate) segments: [Segment; MAX_PHNUM],
    pub(crate) nseg: usize,
}

fn rd16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn rd64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

/// Valida l'ELF e ritorna il layout (nessuna allocazione): `None` se
/// malformato/insicuro (magic, classe, macchina, bound, W+X, entry).
pub fn validate(bytes: &[u8]) -> Option<Layout> {
    if bytes.len() < 64 {
        return None;
    }
    if &bytes[0..4] != b"\x7fELF" {
        return None;
    }
    if bytes[4] != ELFCLASS64 || bytes[5] != ELFDATA2LSB || bytes[6] != 1 {
        return None;
    }
    let e_type = rd16(bytes, 16);
    if e_type != ET_EXEC && e_type != ET_DYN {
        return None;
    }
    if rd16(bytes, 18) != EM_X86_64 {
        return None;
    }
    let e_entry = rd64(bytes, 24);
    let e_phoff = rd64(bytes, 32) as usize;
    let e_ehsize = rd16(bytes, 52);
    let e_phentsize = rd16(bytes, 54) as usize;
    let e_phnum = rd16(bytes, 56) as usize;
    if e_ehsize != 64 || e_phentsize != 56 || e_phnum == 0 || e_phnum > MAX_PHNUM {
        return None;
    }
    if e_phoff.checked_add(e_phnum * 56)? > bytes.len() {
        return None;
    }

    let mut segments = [Segment { offset: 0, vaddr: 0, filesz: 0, memsz: 0, flags: 0 }; MAX_PHNUM];
    let mut nseg = 0usize;
    let mut min_va = u64::MAX;
    let mut max_va = 0u64;
    let mut entry_ok = false;
    for i in 0..e_phnum {
        let ph = e_phoff + i * 56;
        if rd32(bytes, ph) != PT_LOAD {
            continue;
        }
        let p_flags = rd32(bytes, ph + 4);
        let p_offset = rd64(bytes, ph + 8) as usize;
        let p_vaddr = rd64(bytes, ph + 16);
        let p_filesz = rd64(bytes, ph + 32) as usize;
        let p_memsz = rd64(bytes, ph + 40);
        let p_align = rd64(bytes, ph + 48);
        if p_filesz as u64 > p_memsz || p_memsz == 0 {
            return None;
        }
        if p_offset.checked_add(p_filesz)? > bytes.len() {
            return None;
        }
        if p_align != 0 && (p_align & (p_align - 1)) != 0 {
            return None;
        }
        let vend = p_vaddr.checked_add(p_memsz)?;
        if p_vaddr < USER_CODE || vend > USER_FS_BUFFER {
            return None;
        }
        if p_vaddr < min_va {
            min_va = p_vaddr;
        }
        if vend > max_va {
            max_va = vend;
        }
        if p_flags & PF_X != 0 && e_entry >= p_vaddr && e_entry < vend {
            entry_ok = true;
        }
        segments[nseg] = Segment { offset: p_offset, vaddr: p_vaddr, filesz: p_filesz, memsz: p_memsz, flags: p_flags };
        nseg += 1;
    }
    if nseg == 0 || !entry_ok {
        return None;
    }

    let base = min_va & !(PAGE - 1);
    let end = max_va.checked_add(PAGE - 1)? & !(PAGE - 1);
    let npages = (end.checked_sub(base)? / PAGE) as usize;
    if npages == 0 || npages > MAX_PAGES {
        return None;
    }

    // Rifiuto W+X: nessuna pagina puo' essere scrivibile ed eseguibile insieme
    // (invariante di sicurezza; un ELF che lo chiede e' rifiutato). Check a
    // coppie di segmenti (nseg <= 16, niente array per-pagina: con MAX_PAGES
    // a 64K l'array sfonderebbe lo stack kernel).
    for (i, a) in segments[..nseg].iter().enumerate() {
        for b in &segments[..nseg][..i] {
            let aw = a.flags & PF_W != 0;
            let ax = a.flags & PF_X != 0;
            let bw = b.flags & PF_W != 0;
            let bx = b.flags & PF_X != 0;
            if (aw && bx) || (ax && bw) {
                let (lo, hi) = (a.vaddr.max(b.vaddr), (a.vaddr + a.memsz).min(b.vaddr + b.memsz));
                if lo < hi {
                    return None; // overlap W+X
                }
            }
        }
    }

    // Confine condiviso/privato: la prima pagina scrivibile (o `end` se nessun
    // segmento W). Le pagine sotto sono immutabili per costruzione.
    let mut rw = end;
    for s in &segments[..nseg] {
        if s.flags & PF_W != 0 && s.vaddr < rw {
            rw = s.vaddr;
        }
    }
    let rw_off = rw & !(PAGE - 1);

    Some(Layout { entry: e_entry, base, end, rw_off, segments, nseg })
}

/// `(writable, executable)` per la pagina che contiene `va`, unione dei
/// segmenti che la coprono (mai entrambi per il check W+X di `validate`).
pub(crate) fn page_flags(l: &Layout, va: u64) -> (bool, bool) {
    let mut w = false;
    let mut x = false;
    for s in &l.segments[..l.nseg] {
        if va + PAGE > s.vaddr && va < s.vaddr + s.memsz {
            if s.flags & PF_W != 0 {
                w = true;
            }
            if s.flags & PF_X != 0 {
                x = true;
            }
        }
    }
    (w, x)
}

/// Copia i file bytes dei segmenti dentro `[from, to)` in un blocco contiguo
/// privato (owned) e lo mappa. Buchi e bss restano zero.
unsafe fn map_private(cr3: u64, bytes: &[u8], l: &Layout, from: u64, to: u64) {
    if to <= from {
        return;
    }
    let pages = ((to - from) / PAGE) as usize;
    let phys = crate::arc::phys_mem::alloc_contiguous(pages).expect("oom per l'ELF");
    let base_ptr = crate::addr::phys_to_virt(phys) as *mut u8;
    unsafe { core::ptr::write_bytes(base_ptr, 0, pages * PAGE as usize); }
    for s in &l.segments[..l.nseg] {
        let s_end = s.vaddr + s.filesz as u64;
        let a = s.vaddr.max(from);
        let b = s_end.min(to);
        if a >= b {
            continue;
        }
        let dst = unsafe { base_ptr.add((a - from) as usize) };
        let src_off = s.offset + (a - s.vaddr) as usize;
        let n = (b - a) as usize;
        unsafe { core::ptr::copy_nonoverlapping(bytes[src_off..src_off + n].as_ptr(), dst, n); }
    }
    for p in 0..pages {
        let va = from + (p as u64) * PAGE;
        let (w, x) = page_flags(l, va);
        unsafe {
            crate::arc::vmm_user::map_user_leaf(cr3, va, phys + (p as u64) * PAGE, w, x);
        }
    }
}

/// Carica l'immagine validata in `cr3`. I segmenti immutabili sono condivisi
/// (`crate::text`, se c'e' spazio in tabella) o copiati privatamente come
/// fallback; `[rw_off, end)` e' sempre privato. Ritorna l'`id` del text image
/// condiviso (0 = nessuno), da rilasciare al teardown del processo.
///
/// # Safety
/// `cr3` deve essere un address space VUOTO appena creato da
/// `new_address_space` (spawn ed exec, che poi passano a quello nuovo);
/// `layout` deve venire da `validate` sullo stesso `bytes`.
pub unsafe fn load(cr3: u64, bytes: &[u8], layout: &Layout) -> u32 {
    let mut text_id = 0u32;
    if layout.rw_off > layout.base {
        match unsafe { crate::text::acquire(bytes, layout) } {
            Some(id) => {
                unsafe { crate::text::map_shared(cr3, layout, id) };
                text_id = id;
            }
            None => unsafe { map_private(cr3, bytes, layout, layout.base, layout.rw_off) },
        }
    }
    unsafe { map_private(cr3, bytes, layout, layout.rw_off, layout.end) };
    text_id
}

/// Entry point dell'immagine validata.
pub fn entry(layout: &Layout) -> u64 {
    layout.entry
}
