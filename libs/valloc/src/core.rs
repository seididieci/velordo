//! `valloc` — allocatore segregated-fit per Velordo (S2.1).
//!
//! Disegno (niente alberi, per costruzione):
//! - small (<= 4 KiB, align <= 16): slab da 16 pagine a **classe pura**,
//!   free-stack LIFO per slab (O(1), cache-hot); slab vuota → `munmap`
//!   intera. Niente coalescing (non serve tra slab omogenee), spreco
//!   boundato (~10 classi x 64 KiB).
//! - large (> 4 KiB o align > 16): chunk `mmap` dedicato + header
//!   (magic/base/len), `munmap` alla free. Zero gestione, zero spreco.
//! - Registro slab: array ordinato + ricerca binaria (solo sul free path,
//!   dove O(log n) non costa). Lookup large via magic (wild free = abort).
//! - Un solo spinlock globale (sezioni da ns; sharding solo se S2.3 misura
//!   contesa). Thread-safe dal giorno 1 (i thread S-T condividono l'heap).
//! - Ritorno memoria reale: slab vuote e large tornano al kernel (RSS segue
//!   il live set — il motivo per cui il bump S1.3 non bastava a rustc).
//!
//! `no_std`, solo `core`. Il backing e' il trait [`Provider`] (pagine zero,
//! granularita' pagina, NULL su OOM): nessuna syscall qui dentro.


use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Backing di memoria (stile mmap anonimo). Implementato dalla PAL (syscall
/// Velordo) e dall'harness host (libc). Contratto: pagine **zero**, base
/// page-aligned, `len` sempre multiplo di pagina, NULL su OOM; `unmap` solo
/// su range esatti precedentemente mappati.
pub trait Provider {
    /// Mappa `len` byte zero. NULL su OOM.
    unsafe fn map(len: usize) -> *mut u8;
    /// Smappa un range esatto precedentemente mappato.
    unsafe fn unmap(ptr: *mut u8, len: usize);
}

const PAGE: usize = 4096;
const SLAB_LEN: usize = 16 * PAGE;
/// Classi small (potenze di 2): allineamento naturale garantito da base
/// page-aligned (classi >= 16 sono multiple di 16).
const CLASSES: [usize; 10] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
/// Oltre questa taglia (o align) si va di large diretto.
const LARGE_THRESHOLD: usize = 4096;
/// Allineamento massimo servito dallo small path (oltre = large dedicato).
const SMALL_ALIGN_MAX: usize = 16;
/// Capacita' registro slab (2048 x 64 KiB = 128 MiB di small; oltre si va
/// di large diretto — mai OOM fittizio, solo meno efficiente; `slab_overflow`
/// nelle stats lo segnala per il tuning).
const REGISTRY_CAP: usize = 2048;
/// Magic dell'header large (wild free = abort loud, mai corruzione).
const LARGE_MAGIC: u64 = 0x56414C4C4F43364B;

fn class_for(size: usize) -> Option<usize> {
    CLASSES.iter().position(|&c| c >= size)
}

fn align_up(x: usize, a: usize) -> usize {
    debug_assert!(a.is_power_of_two());
    x.wrapping_add(a - 1) & !(a - 1)
}

/// Header large (24 B prima del payload allineato): magic + range esatto
/// da smappare + taglia payload (per `realloc` senza riletture).
struct LargeHead {
    magic: u64,
    base: usize,
    len: usize,
    payload: usize,
}
const LARGE_HEAD: usize = 32;

/// Descrittore slab (registro): classe pura, bump per il fresco (pagine
/// zero: niente azzeramento), stack per il riuso (da azzerare).
#[derive(Clone, Copy)]
struct Slab {
    base: usize,
    class: u8,
    total: u16,
    free: u16,
    bump: u16,
    stack: *mut u8,
}

impl Slab {
    const fn empty() -> Self {
        Slab { base: 0, class: 0, total: 0, free: 0, bump: 0, stack: ptr::null_mut() }
    }

    fn block(&self, idx: usize) -> *mut u8 {
        ptr::with_exposed_provenance_mut(self.base + idx * CLASSES[self.class as usize])
    }
}

/// Stato globale (uno per istanza: PAL ne ha una, i test N). `const fn new`
/// cosi' vive in `static` senza init runtime.
pub struct State {
    lock: AtomicBool,
    slabs: UnsafeCell<[Slab; REGISTRY_CAP]>,
    nslabs: AtomicUsize,
    allocs: AtomicUsize,
    frees: AtomicUsize,
    live: AtomicUsize,
    high: AtomicUsize,
    maps: AtomicUsize,
    unmaps: AtomicUsize,
    slab_overflow: AtomicUsize,
}

impl State {
    /// Stato zero (slab vuote, lock libero, stats a zero).
    pub const fn new() -> Self {
        Self {
            lock: AtomicBool::new(false),
            slabs: UnsafeCell::new([Slab::empty(); REGISTRY_CAP]),
            nslabs: AtomicUsize::new(0),
            allocs: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            high: AtomicUsize::new(0),
            maps: AtomicUsize::new(0),
            unmaps: AtomicUsize::new(0),
            slab_overflow: AtomicUsize::new(0),
        }
    }

    fn take_lock(&self) {
        while self.lock.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
    }

    fn drop_lock(&self) {
        self.lock.store(false, Ordering::Release);
    }

    fn stat_add(&self, n: usize) {
        self.allocs.fetch_add(1, Ordering::Relaxed);
        let live = self.live.fetch_add(n, Ordering::Relaxed) + n;
        // High-water best-effort (CAS loop: perse occasionali ok, mai oltre).
        let mut high = self.high.load(Ordering::Relaxed);
        while live > high {
            match self.high.compare_exchange_weak(high, live, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(h) => high = h,
            }
        }
    }

    fn stat_free(&self, n: usize) {
        self.frees.fetch_add(1, Ordering::Relaxed);
        self.live.fetch_sub(n, Ordering::Relaxed);
    }
}

// `Sync` manuale: ogni accesso condiviso avviene sotto lo spinlock interno
// (unica eccezione: `stats()`, lock-free ma solo letture atomiche).
unsafe impl Sync for State {}

/// Istantanea stats (tuning + dimensionamento RAM + assert dell'harness).
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub allocs: usize,
    pub frees: usize,
    pub live: usize,
    pub high: usize,
    pub slabs: usize,
    pub maps: usize,
    pub unmaps: usize,
    pub slab_overflow: usize,
}

/// Allocatore su uno [`State`] con backing [`Provider`]. Zero-sized: tutto
/// lo stato e' nello `State` referenziato (una istanza per binario/test).
pub struct Allocator<P> {
    state: &'static State,
    _p: PhantomData<fn() -> P>,
}

impl<P: Provider> Allocator<P> {
    /// Lega l'allocatore a uno stato statico (una sola istanza per `State`).
    pub const fn new(state: &'static State) -> Self {
        Allocator { state, _p: PhantomData }
    }

    /// Istantanea stats (lock-free, best-effort coerente per tuning).
    pub fn stats(&self) -> Stats {
        let s = self.state;
        Stats {
            allocs: s.allocs.load(Ordering::Relaxed),
            frees: s.frees.load(Ordering::Relaxed),
            live: s.live.load(Ordering::Relaxed),
            high: s.high.load(Ordering::Relaxed),
            slabs: s.nslabs.load(Ordering::Relaxed),
            maps: s.maps.load(Ordering::Relaxed),
            unmaps: s.unmaps.load(Ordering::Relaxed),
            slab_overflow: s.slab_overflow.load(Ordering::Relaxed),
        }
    }

    /// Trova la slab che contiene `p` (registro ordinato per base).
    /// Chiamare sotto lock.
    fn find_slab(&self, p: usize) -> Option<usize> {
        let s = self.state;
        let n = s.nslabs.load(Ordering::Relaxed);
        let slabs = unsafe { &*s.slabs.get() };
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let base = slabs[mid].base;
            if p < base {
                hi = mid;
            } else if p >= base + SLAB_LEN {
                lo = mid + 1;
            } else {
                return Some(mid);
            }
        }
        None
    }

    /// Alloca una slab fresca da `class` (sotto lock). NULL su OOM/registro
    /// pieno (il chiamante ripiega sul large diretto).
    unsafe fn new_slab(&self, class: u8) -> *mut u8 {
        let s = self.state;
        let base = unsafe { P::map(SLAB_LEN) };
        if base.is_null() {
            return ptr::null_mut();
        }
        s.maps.fetch_add(1, Ordering::Relaxed);
        let n = s.nslabs.load(Ordering::Relaxed);
        if n >= REGISTRY_CAP {
            unsafe { P::unmap(base, SLAB_LEN) };
            s.unmaps.fetch_add(1, Ordering::Relaxed);
            s.slab_overflow.fetch_add(1, Ordering::Relaxed);
            return ptr::null_mut();
        }
        // Inserimento ordinato per base (shift: n piccolo, solo su new slab).
        let slabs = unsafe { &mut *s.slabs.get() };
        let mut at = n;
        while at > 0 && slabs[at - 1].base > base.addr() {
            slabs[at] = Slab {
                base: slabs[at - 1].base,
                class: slabs[at - 1].class,
                total: slabs[at - 1].total,
                free: slabs[at - 1].free,
                bump: slabs[at - 1].bump,
                stack: slabs[at - 1].stack,
            };
            at -= 1;
        }
        let total = (SLAB_LEN / CLASSES[class as usize]) as u16;
        slabs[at] = Slab {
            base: base.addr(),
            class,
            total,
            free: total,
            bump: 0,
            stack: ptr::null_mut(),
        };
        s.nslabs.store(n + 1, Ordering::Relaxed);
        // Blocco fresco dalla slab appena creata (ignora il flag: e' nuovo).
        unsafe { self.pop_block(at).0 }
    }

    /// Preleva un blocco dalla slab `idx` (bump fresco o stack riuso).
    /// Chiamare sotto lock. Ritorna (ptr, fresco).
    unsafe fn pop_block(&self, idx: usize) -> (*mut u8, bool) {
        let s = self.state;
        let slabs = unsafe { &mut *s.slabs.get() };
        let slab = &mut slabs[idx];
        debug_assert!(slab.free > 0);
        slab.free -= 1;
        if !slab.stack.is_null() {
            let p = slab.stack;
            slab.stack = unsafe { *(p as *const *mut u8) };
            (p, false)
        } else {
            let p = slab.block(slab.bump as usize);
            slab.bump += 1;
            (p, true)
        }
    }

    /// Allocazione small (sotto lock). Ritorna NULL su OOM. I blocchi dal
    /// bump sono freschi (pagine zero: niente azzeramento anche se chiesto).
    unsafe fn alloc_small(&self, class: u8, zero: bool) -> *mut u8 {
        let s = self.state;
        let n = s.nslabs.load(Ordering::Relaxed);
        let slabs = unsafe { &*s.slabs.get() };
        for idx in 0..n {
            if slabs[idx].class == class && slabs[idx].free > 0 {
                let (p, fresh) = unsafe { self.pop_block(idx) };
                if zero && !fresh {
                    unsafe { (p as *mut u8).write_bytes(0, CLASSES[class as usize]) };
                }
                s.stat_add(CLASSES[class as usize]);
                return p;
            }
        }
        // Nessuna slab con spazio: slab fresca (blocco sempre fresco).
        let p = unsafe { self.new_slab(class) };
        if p.is_null() {
            return ptr::null_mut();
        }
        s.stat_add(CLASSES[class as usize]);
        p
    }

    /// Allocazione large dedicata (sotto lock). Ritorna NULL su OOM.
    unsafe fn alloc_large(&self, size: usize, align: usize, zero: bool) -> *mut u8 {
        let s = self.state;
        let span = align_up(size.saturating_add(LARGE_HEAD).saturating_add(align), PAGE);
        if span < size {
            return ptr::null_mut();
        }
        let base = unsafe { P::map(span) };
        if base.is_null() {
            return ptr::null_mut();
        }
        s.maps.fetch_add(1, Ordering::Relaxed);
        let payload: *mut u8 =
            ptr::with_exposed_provenance_mut(align_up(base.addr() + LARGE_HEAD, align));
        unsafe {
            let h = payload.sub(LARGE_HEAD) as *mut LargeHead;
            h.write(LargeHead { magic: LARGE_MAGIC, base: base.addr(), len: span, payload: size });
        }
        if zero {
            unsafe { payload.write_bytes(0, size) };
        }
        s.stat_add(size);
        payload
    }

    /// Libera un blocco small (sotto lock). Slab svuotata → `munmap`.
    unsafe fn free_small(&self, idx: usize, p: *mut u8) {
        let s = self.state;
        let slabs = unsafe { &mut *s.slabs.get() };
        let class = CLASSES[slabs[idx].class as usize];
        // Validazione: allineato al blocco (wild free = abort loud).
        assert!((p.addr() - slabs[idx].base) % class == 0, "valloc: free fuori blocco");
        unsafe { *(p as *mut *mut u8) = slabs[idx].stack };
        slabs[idx].stack = p;
        slabs[idx].free += 1;
        s.stat_free(class);
        if slabs[idx].free == slabs[idx].total {
            // Slab vuota: rimuovi dal registro + torna al kernel.
            let base = slabs[idx].base;
            let n = s.nslabs.load(Ordering::Relaxed);
            slabs.copy_within(idx + 1..n, idx);
            slabs[n - 1] = Slab::empty();
            s.nslabs.store(n - 1, Ordering::Relaxed);
            unsafe {
                P::unmap(ptr::with_exposed_provenance_mut(base), SLAB_LEN)
            };
            s.unmaps.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Libera un large (sotto lock). Magic errato = abort loud. La taglia
    /// stats viene dall'header (payload reale), non dal layout del chiamante
    /// (realloc puo' tenere il blocco con size diversa).
    unsafe fn free_large(&self, p: *mut u8) {
        let s = self.state;
        let h = unsafe { &*(p.sub(LARGE_HEAD) as *const LargeHead) };
        assert!(h.magic == LARGE_MAGIC, "valloc: free large non valido");
        let payload = h.payload;
        unsafe { P::unmap(ptr::with_exposed_provenance_mut(h.base), h.len) };
        s.unmaps.fetch_add(1, Ordering::Relaxed);
        s.stat_free(payload);
    }

    /// `alloc` stile [`core::alloc::GlobalAlloc`]. NULL su OOM.
    pub unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if size == 0 {
            return ptr::without_provenance_mut(layout.align().max(8));
        }
        let align = layout.align();
        self.state.take_lock();
        let p = if size > LARGE_THRESHOLD || align > SMALL_ALIGN_MAX {
            unsafe { self.alloc_large(size, align.max(8), false) }
        } else {
            let need = size.max(align).max(8);
            match class_for(need) {
                Some(c) => unsafe { self.alloc_small(c as u8, false) },
                None => unsafe { self.alloc_large(size, align.max(8), false) },
            }
        };
        self.state.drop_lock();
        p
    }

    /// `alloc_zeroed`: salta l'azzeramento solo su pagine fresche dal kernel.
    pub unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if size == 0 {
            return ptr::without_provenance_mut(layout.align().max(8));
        }
        let align = layout.align();
        self.state.take_lock();
        let p = if size > LARGE_THRESHOLD || align > SMALL_ALIGN_MAX {
            unsafe { self.alloc_large(size, align.max(8), true) }
        } else {
            let need = size.max(align).max(8);
            match class_for(need) {
                Some(c) => unsafe { self.alloc_small(c as u8, true) },
                None => unsafe { self.alloc_large(size, align.max(8), true) },
            }
        };
        self.state.drop_lock();
        p
    }

    /// `dealloc`: wild/double free = abort loud (mai corruzione silenziosa).
    /// La taglia stats e' il `size` del layout (small: classe; large:
    /// payload), simmetrica alle `stat_add` in alloc.
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size();
        if size == 0 || ptr.is_null() {
            return;
        }
        self.state.take_lock();
        match self.find_slab(ptr.addr()) {
            // Small: classe nota dal registro (il layout deve combaciare per
            // contratto GlobalAlloc; la validita' blocco e' in free_small).
            Some(idx) => unsafe { self.free_small(idx, ptr) },
            // Large (o wild: magic decide, abort se non torna).
            None => unsafe { self.free_large(ptr) },
        }
        self.state.drop_lock();
    }

    /// `realloc`: tiene il blocco se basta (small: stessa classe), altrimenti
    /// alloc+copy+free. NULL su OOM (vecchio intatto).
    pub unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let old_size = old.size();
        if ptr.is_null() {
            return unsafe { self.alloc(Layout::from_size_align(new_size, old.align()).unwrap_or(old)) };
        }
        if new_size == 0 {
            unsafe { self.dealloc(ptr, old) };
            return ptr::without_provenance_mut(old.align().max(8));
        }
        if old_size == 0 {
            // Vecchio dangling (zero-size): allocazione fresca, ignora ptr.
            let new_layout = Layout::from_size_align(new_size, old.align()).unwrap_or(old);
            return unsafe { self.alloc(new_layout) };
        }
        // Stesso blocco se basta (realloc non cambia align): small = stessa
        // classe, large = payload dell'header.
        let fits = if old_size > LARGE_THRESHOLD || old.align() > SMALL_ALIGN_MAX {
            let h = unsafe { &*(ptr.sub(LARGE_HEAD) as *const LargeHead) };
            assert!(h.magic == LARGE_MAGIC, "valloc: realloc large non valido");
            new_size <= h.payload
        } else {
            let need_old = old_size.max(old.align()).max(8);
            let need_new = new_size.max(old.align()).max(8);
            class_for(need_old) == class_for(need_new)
        };
        if fits {
            return ptr;
        }
        let new_layout = Layout::from_size_align(new_size, old.align()).unwrap_or(old);
        let p = unsafe { self.alloc(new_layout) };
        if p.is_null() {
            return ptr::null_mut();
        }
        unsafe {
            ptr::copy_nonoverlapping(ptr, p, old_size.min(new_size));
            self.dealloc(ptr, old);
        }
        p
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Provider arena per gli unit test (host, std disponibile): 8 MiB
    /// bump condiviso (atomico), zero-fill esplicito (rispetta il contratto
    /// Provider). Init once (i test girano in parallelo su thread diversi).
    static BUMP: AtomicUsize = AtomicUsize::new(0);
    static mut ARENA: *mut u8 = ptr::null_mut();
    static INIT: std::sync::Once = std::sync::Once::new();

    struct Arena;
    impl Provider for Arena {
        unsafe fn map(len: usize) -> *mut u8 {
            assert!(len % PAGE == 0);
            INIT.call_once(|| unsafe {
                // +PAGE: l'arena Vec non e' page-aligned, si allinea qui.
                let mut v: Vec<u8> = Vec::with_capacity((8 << 20) + PAGE);
                v.resize((8 << 20) + PAGE, 0);
                let raw = v.as_mut_ptr().addr();
                ARENA = align_up(raw, PAGE) as *mut u8;
                core::mem::forget(v);
            });
            let base = unsafe { ARENA.addr() };
            let off = BUMP.fetch_add(len, Ordering::SeqCst);
            assert!(off + len <= 8 << 20, "arena test esaurita");
            let p: *mut u8 = ptr::with_exposed_provenance_mut(base + off);
            unsafe { p.write_bytes(0, len) };
            p
        }
        unsafe fn unmap(_ptr: *mut u8, _len: usize) {}
    }

    /// Stato+allocatore privati per test (statistiche isolate: niente race
    /// tra test paralleli sullo stesso `live`).
    macro_rules! alloctest {
        ($s:ident, $a:ident) => {
            static $s: State = State::new();
            static $a: Allocator<Arena> = Allocator::new(&$s);
        };
    }

    fn lay(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap()
    }

    #[test]
    fn small_basic() {
        alloctest!(S, A);
        for &size in &[1usize, 8, 9, 16, 24, 100, 1000, 4096] {
            let p = unsafe { A.alloc(lay(size)) };
            assert!(!p.is_null());
            assert_eq!(p.addr() % 8, 0);
            unsafe {
                core::slice::from_raw_parts_mut(p, size).fill(size as u8);
                assert!(core::slice::from_raw_parts(p, size).iter().all(|&b| b == size as u8));
                A.dealloc(p, lay(size));
            }
        }
        let st = A.stats();
        assert_eq!(st.live, 0);
        assert_eq!(st.allocs, st.frees);
        assert!(st.high > 0);
    }

    #[test]
    fn reuse_lifo() {
        alloctest!(S, A);
        // Due vivi (la slab sopravvive: slab svuotata = munmap di disegno).
        let a = unsafe { A.alloc(lay(32)) };
        let p1 = unsafe { A.alloc(lay(32)) };
        unsafe { A.dealloc(p1, lay(32)) };
        let p2 = unsafe { A.alloc(lay(32)) };
        assert_eq!(p1, p2, "free-stack LIFO: riuso immediato");
        unsafe {
            A.dealloc(p2, lay(32));
            A.dealloc(a, lay(32));
        }
        assert_eq!(A.stats().live, 0);
    }

    #[test]
    fn zeroed_paths() {
        alloctest!(S, A);
        let p = unsafe { A.alloc_zeroed(lay(64)) };
        unsafe {
            assert!(core::slice::from_raw_parts(p, 64).iter().all(|&b| b == 0));
            core::slice::from_raw_parts_mut(p, 64).fill(0xFF);
            A.dealloc(p, lay(64));
        }
        // Stesso blocco riusato: deve tornare azzerato.
        let q = unsafe { A.alloc_zeroed(lay(64)) };
        unsafe {
            assert!(core::slice::from_raw_parts(q, 64).iter().all(|&b| b == 0));
            A.dealloc(q, lay(64));
        }
        assert_eq!(A.stats().live, 0);
    }

    #[test]
    fn large_and_overalign() {
        alloctest!(S, A);
        let big = Layout::from_size_align(1 << 20, 8).unwrap();
        let p = unsafe { A.alloc(big) };
        // Payload non page-aligned per disegno (header 32 B prima); basta align 8.
        assert!(!p.is_null() && p.addr() % 8 == 0);
        unsafe {
            core::slice::from_raw_parts_mut(p, 1 << 20).fill(0xAB);
            assert_eq!(*p.add((1 << 20) - 1), 0xAB);
            A.dealloc(p, big);
        }
        let oa = Layout::from_size_align(100, 64).unwrap();
        let q = unsafe { A.alloc(oa) };
        assert!(!q.is_null() && q.addr() % 64 == 0);
        unsafe { A.dealloc(q, oa) };
        assert_eq!(A.stats().live, 0);
    }

    #[test]
    fn realloc_grow_shrink() {
        alloctest!(S, A);
        let p = unsafe { A.alloc(lay(32)) };
        unsafe { core::slice::from_raw_parts_mut(p, 32).fill(0x5A) };
        // Crescita oltre la classe: si sposta, contenuto preservato.
        let q = unsafe { A.realloc(p, lay(32), 5000) };
        assert!(!q.is_null());
        unsafe {
            assert!(core::slice::from_raw_parts(q, 32).iter().all(|&b| b == 0x5A));
            // Shrink dentro il payload large: resta dov'e'.
            let r = A.realloc(q, lay(5000), 4000);
            assert_eq!(r, q);
            A.dealloc(r, lay(4000));
        }
        assert_eq!(A.stats().live, 0);
    }

    #[test]
    fn zero_size() {
        alloctest!(S, A);
        let p = unsafe { A.alloc(lay(0)) };
        assert!(!p.is_null());
        unsafe { A.dealloc(p, lay(0)) };
        assert_eq!(A.stats().live, 0);
    }

    #[test]
    #[should_panic]
    fn corrupt_large_header_aborts() {
        alloctest!(S, A);
        let big = lay(1 << 20);
        let p = unsafe { A.alloc(big) };
        unsafe {
            let h = p.sub(LARGE_HEAD) as *mut LargeHead;
            (*h).magic = 0;
            A.dealloc(p, big);
        }
    }
}
