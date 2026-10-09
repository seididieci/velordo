//! Harness differenziale per `valloc` (S2.1, solo host).
//!
//! Modello in memoria: ogni alloc scrive un pattern PRNG, ogni free/realloc
//! lo riverifica (niente corruzioni/overlap), a fine run tutto liberato
//! (live==0, slab==0, maps==munmaps: RSS interamente tornata). Seed
//! deterministico da argv (default fisso: run riproducibile).
//!
//! Uso: cargo run -p alloc-fuzz [-- seed iters threads]

use std::alloc::Layout;
use std::collections::HashMap;
use valloc::{Allocator, Provider, State};

struct Libc;
impl Provider for Libc {
    unsafe fn map(len: usize) -> *mut u8 {
        assert!(len % 4096 == 0);
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            std::ptr::null_mut()
        } else {
            p as *mut u8
        }
    }
    unsafe fn unmap(ptr: *mut u8, len: usize) {
        let r = unsafe { libc::munmap(ptr as *mut libc::c_void, len) };
        assert_eq!(r, 0, "munmap fallita su range nostro");
    }
}

static STATE: State = State::new();
static ALLOC: Allocator<Libc> = Allocator::new(&STATE);

/// xorshift64* deterministico (niente dipendenze, seed da argv).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64)) as usize
    }
}

/// Taglia di alloc verosimile (bias small, code large/overalign).
fn pick_size(r: &mut Rng) -> (usize, usize) {
    match r.below(100) {
        0..=59 => (1 + r.below(24), 8),
        60..=79 => (25 + r.below(500), 8),
        80..=89 => (501 + r.below(3600), 16),
        90..=94 => (4097 + r.below(1 << 20), 8),
        _ => {
            let a = [16usize, 32, 64, 128, 512, 4096][r.below(6)];
            (1 + r.below(900), a)
        }
    }
}

/// Scrive il pattern a partire dall'offset `base` (continuazione per le
/// estensioni realloc: il byte a `base+i` vale `seed+base+i`, come `check`).
fn fill(p: *mut u8, len: usize, seed: u8, base: usize) {
    unsafe {
        for i in 0..len {
            *p.add(i) = seed.wrapping_add(((base + i) & 0xFF) as u8);
        }
    }
}

fn check(p: *mut u8, len: usize, seed: u8, what: &str) {
    unsafe {
        for i in 0..len {
            let want = seed.wrapping_add((i & 0xFF) as u8);
            assert_eq!(*p.add(i), want, "corruzione {} off {} ptr={:p} len={}", what, i, p, len);
        }
    }
}

struct Model {
    live: HashMap<usize, (usize, usize, u8)>,
}

impl Model {
    fn overlap(&self, p: usize, len: usize) {
        for (&q, &(qlen, _, _)) in self.live.iter() {
            assert!(
                p + len <= q || q + qlen <= p,
                "overlap: {:x}+{} vs {:x}+{}",
                p,
                len,
                q,
                qlen
            );
        }
    }
}

/// Un round single-thread: alloc/free/realloc casuali contro il modello.
fn round(seed: u64, iters: usize, tag: &str) {
    let log = std::env::var("VALLOC_OPLOG").ok().map(|p| {
        std::io::BufWriter::new(std::fs::File::create(p).unwrap())
    });
    let mut log = log;
    let mut r = Rng(seed);
    let mut m = Model { live: HashMap::new() };
    let mut seedgen: u8 = 1;
    let sweep = std::env::var("VALLOC_SWEEP").is_ok();
    for op in 0..iters {
        if op % 10_000 == 0 {
            eprintln!("[fuzz:{}] op={} live={}", tag, op, m.live.len());
        }
        if sweep {
            for (&k, &(size, _, seed)) in m.live.iter() {
                if size > 0 {
                    check(k as *mut u8, size, seed, "sweep");
                }
            }
        }
        macro_rules! oplog {
            ($($t:tt)*) => {
                if let Some(w) = log.as_mut() {
                    use std::io::Write as _;
                    let _ = writeln!(w, "op={} {}", op, format_args!($($t)*));
                }
            };
        }
        match r.below(100) {
            0..=54 => {
                let (size, align) = pick_size(&mut r);
                let lay = Layout::from_size_align(size, align).unwrap();
                let p = unsafe { ALLOC.alloc(lay) };
                assert!(!p.is_null(), "OOM inattesa in fuzz");
                assert_eq!(p as usize % align, 0, "allineamento");
                m.overlap(p as usize, size.max(1));
                seedgen = seedgen.wrapping_add(1);
                if size > 0 {
                    fill(p, size, seedgen, 0);
                }
                m.live.insert(p as usize, (size, align, seedgen));
                oplog!("alloc {:p} size={} align={} seed={}", p, size, align, seedgen);
            }
            55..=69 if !m.live.is_empty() => {
                let k = *m.live.keys().nth(r.below(m.live.len())).unwrap();
                let (size, align, seed) = m.live.remove(&k).unwrap();
                let p = k as *mut u8;
                if size > 0 {
                    check(p, size, seed, "pre-free");
                }
                unsafe { ALLOC.dealloc(p, Layout::from_size_align(size, align).unwrap()) };
                oplog!("free {:p} size={}", p, size);
            }
            70..=84 if !m.live.is_empty() => {
                // alloc_zeroed: deve tornare zeri.
                let (size, align) = pick_size(&mut r);
                if size == 0 {
                    continue;
                }
                let lay = Layout::from_size_align(size, align).unwrap();
                let p = unsafe { ALLOC.alloc_zeroed(lay) };
                assert!(!p.is_null());
                m.overlap(p as usize, size);
                unsafe {
                    assert!(
                        std::slice::from_raw_parts(p, size).iter().all(|&b| b == 0),
                        "alloc_zeroed non zero"
                    );
                }
                seedgen = seedgen.wrapping_add(1);
                fill(p, size, seedgen, 0);
                m.live.insert(p as usize, (size, align, seedgen));
                oplog!("zeroed {:p} size={} seed={}", p, size, seedgen);
            }
            _ if !m.live.is_empty() => {
                // realloc grow/shrink con preservazione contenuto.
                let k = *m.live.keys().nth(r.below(m.live.len())).unwrap();
                let (size, align, seed) = m.live.remove(&k).unwrap();
                let p = k as *mut u8;
                let ns = 1 + r.below(size.max(1) * 2 + 64);
                let q = unsafe { ALLOC.realloc(p, Layout::from_size_align(size, align).unwrap(), ns) };
                assert!(!q.is_null(), "OOM in realloc fuzz");
                assert_eq!(q as usize % align, 0);
                let keep = size.min(ns);
                if keep > 0 {
                    check(q, keep, seed, "post-realloc");
                }
                m.overlap(q as usize, ns.max(1));
                if ns > size {
                    fill(unsafe { q.add(size) }, ns - size, seed, size);
                }
                m.live.insert(q as usize, (ns, align, seed));
                oplog!("realloc {:p}({}) -> {:p}({}) moved={}", p, size, q, ns, p != q);
            }
            _ => {}
        }
    }
    // Drain: verifica tutto e libera (leak check = live==0 dopo).
    for (&k, &(size, align, seed)) in m.live.iter() {
        let p = k as *mut u8;
        if size > 0 {
            check(p, size, seed, "drain");
        }
        unsafe { ALLOC.dealloc(p, Layout::from_size_align(size, align).unwrap()) };
    }
    println!("[fuzz:{}] iters={} ok", tag, iters);
}

fn edges() {
    // Zero-size, over-align grande, realloc a/da zero, huge (64 MiB transienti).
    let z = Layout::from_size_align(0, 8).unwrap();
    let p = unsafe { ALLOC.alloc(z) };
    assert!(!p.is_null());
    unsafe { ALLOC.dealloc(p, z) };
    let big = Layout::from_size_align(64 << 20, 8).unwrap();
    let q = unsafe { ALLOC.alloc(big) };
    assert!(!q.is_null());
    unsafe {
        *q = 0x11;
        *q.add((64 << 20) - 1) = 0x22;
        assert_eq!(*q, 0x11);
        let r = ALLOC.realloc(q, big, 16);
        assert_eq!(*r, 0x11, "shrink large→small preserva");
        ALLOC.dealloc(r, Layout::from_size_align(16, 8).unwrap());
    }
    let a4k = Layout::from_size_align(100, 4096).unwrap();
    let s = unsafe { ALLOC.alloc(a4k) };
    assert!(!s.is_null() && s as usize % 4096 == 0);
    unsafe { ALLOC.dealloc(s, a4k) };
    println!("[fuzz:edges] ok");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seed: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(0x5EED_2021);
    let iters: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let nthreads: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    println!("[fuzz] seed={} iters={} threads={}", seed, iters, nthreads);

    round(seed, iters, "single");
    edges();

    // Hammer multithread sullo STESSO allocatore (valida il lock prima di S2.3).
    std::thread::scope(|scope| {
        for t in 0..nthreads {
            scope.spawn(move || round(seed + 1 + t as u64, iters / nthreads.max(1), "mt"));
        }
    });

    let st = ALLOC.stats();
    println!(
        "[fuzz] stats: allocs={} frees={} live={} high={} slabs={} maps={} unmaps={} overflow={}",
        st.allocs, st.frees, st.live, st.high, st.slabs, st.maps, st.unmaps, st.slab_overflow
    );
    assert_eq!(st.live, 0, "LEAK: byte vivi a fine run");
    assert_eq!(st.slabs, 0, "LEAK: slab residue");
    assert_eq!(st.maps, st.unmaps, "LEAK: map senza unmap");
    println!("[fuzz] PASS: niente leak, niente corruzioni");
}
