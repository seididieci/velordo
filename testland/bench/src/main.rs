//! userbench — micro-benchmark throughput client→FS (Fase 23; A/B ArcaFS/FAT).
//!
//! Misura il percorso dati END-TO-END con il TSC (calibrato sul PIT).
//! Piattaforma di riferimento: KVM (`scripts/bench.sh`, N run con media: i
//! tempi TCG non sono reali).
//!
//! Op storiche (anchor Fase 23/53):
//!   b1 zero_1B .............. read 1 B da /dev/zero (solo IPC, niente disco)
//!   b2 sda_512B_seq ......... 200 read sequenziali da /dev/sda (IPC + PIO)
//!   b3 fat_small_orc ........ open+read+close di /fat/HELLO.TXT
//!   b4 ramfs_4K_write/read .. write/read 4 KiB su /tmp (ramfs vera)
//!   b5 fat_4K_oow ........... open+overwrite+close 4 KiB su /fat
//!   b6 bulk_ramfs_16K/64K ... ramfs R/W (hot=cold in RAM)
//!
//! A/B ArcaFS vs FAT (stesso client POSIX/VFS):
//!   small_orc / small_ow / stat / readdir
//!   bulk_{arca,fat}_<size>_{write,read}_{hot,cold} 4K/16K/64K
//!   bulk_*_256K_{write,read}_hot, 1M_{write,read}_{hot,cold},
//!   4M_read_{hot,cold} (scaling)
//!
//! Nativo ArcaFS (R_OBJ_*) vs POSIX sullo stesso volume: righe `*_native`
//! (get/put/stat; il PUT nativo committa per chiamata — semantica diversa
//! dal fd-write a epoca, documentata in 13-performance.md).
//!
//! Freddo deterministico: file/oggetti DISTINTI + `spoil_cache()` (300
//! settori: eviction delle 256 entry di block) + `arca_cache_flush()` per
//! ArcaFS (cache nodi in cardo). Un solo flush prima della misura, come per
//! i cold FAT storici: i nodi indice tornano caldi dopo la prima iterazione
//! (caveat dichiarato).
//!
//! Output egrepabile: righe `[bench] <nome> iters=<n> warm=<w> cyc_op=<c>
//! max_cyc=<m> kb_s=<k>`. Fine: TEST_DONE a init + exit.
//!
//! Modalita' per-size (`scripts/bench-split.sh`): il marker
//! `/test/bench-group.txt` limita l'esecuzione a un gruppo
//! (`small`/`4k`/`16k`/`64k`/`256k`/`1m`/`4m`/`native`); senza marker (o con
//! `all`) il benchmark esegue la matrice monolitica.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use civis::{OPEN_CREATE, OPEN_TRUNC, println};
use core::sync::atomic::{AtomicU8, Ordering};

// ── Percorsi fixture ────────────────────────────────────────────────
// Arca: radice del volume root (nessuna dir da creare: il mount catch-all
// risolve i path; nessun cleanup, il volume e' rigenerato a ogni run.sh).
// FAT: radice, nomi 8.3 (mkdir/remove fuori scope); stessa rigenerazione.
const ARCA_ROOT: &str = "";
const ARCA_SMALL: &str = "/AB_SMALL.TXT";
const ARCA_4K: &str = "/AB_4K.BIN";
const ARCA_16K: &str = "/AB_16K.BIN";
const ARCA_64K: &str = "/AB_64K.BIN";
const ARCA_256K: &str = "/AB_256K.BIN";
const ARCA_1M: &str = "/AB_1M.BIN";
const ARCA_4M: &str = "/AB_4M.BIN";

const FAT_SMALL: &str = "/fat/AB_SML.TXT";
const FAT_4K: &str = "/fat/AB_4K.BIN";
const FAT_16K: &str = "/fat/AB_16K.BIN";
const FAT_64K: &str = "/fat/AB_64K.BIN";
const FAT_256K: &str = "/fat/AB_256K.BIN";
const FAT_1M: &str = "/fat/AB_1M.BIN";
const FAT_4M: &str = "/fat/AB_4M.BIN";

const RAMFS_4K: &str = "/tmp/BENCH.TMP";
const RAMFS_16K: &str = "/tmp/BULK16.TMP";
const RAMFS_64K: &str = "/tmp/BULK64.TMP";

const BUCKET: &[u8] = b"bch";

/// Piccolo overwrite Arca: 200 misurate (non 1000 come FAT). Ogni close di
/// fd-write committa un'epoca e la orphan-GC di ArcaFS gira solo al bind:
/// 1000 commit con l'immagine di default da 128 MiB esaurivano i blocchi
/// (verificato: ERR a meta' campagna), e comunque il costo/op e' dominato
/// dal commit, non dalla dimensione del campione. Il rapporto con FAT resta
/// su cyc/op.
const ARCA_SMALL_OW_ITERS: u64 = 200;

/// Profilo `reduced` (feature cargo, `BENCH_REDUCED=1`): stessa harness ma
/// senza 1M/4M e native 1M, con conteggi bassi. Serve alla validazione su
/// host lenti, non alla campagna di riferimento.
const REDUCED: bool = cfg!(feature = "reduced");

/// Conteggio full/reduced in un punto solo.
const fn pick(full: u64, reduced: u64) -> u64 {
    if REDUCED { reduced } else { full }
}

// ── Selezione per-size (`BENCH_PROFILE=split`) ───────────────────────
// Il profilo split lancia un boot per gruppo (evita di perdere l'intera
// campagna su un timeout) e seeda `/test/bench-group.txt`; senza marker il
// comportamento resta quello monolitico (`all`).
const G_ALL: u8 = 0;
const G_SMALL: u8 = 1;
const G_4K: u8 = 2;
const G_16K: u8 = 3;
const G_64K: u8 = 4;
const G_256K: u8 = 5;
const G_1M: u8 = 6;
const G_4M: u8 = 7;
const G_NATIVE: u8 = 8;

static GROUP: AtomicU8 = AtomicU8::new(G_ALL);

fn group_name(g: u8) -> &'static str {
    match g {
        G_SMALL => "small",
        G_4K => "4k",
        G_16K => "16k",
        G_64K => "64k",
        G_256K => "256k",
        G_1M => "1m",
        G_4M => "4m",
        G_NATIVE => "native",
        _ => "all",
    }
}

fn group_from_marker() -> u8 {
    let Ok(fd) = civis::open("/test/bench-group.txt", 0) else {
        return G_ALL;
    };
    let mut buf = [0u8; 16];
    let cap = buf.len();
    let n = civis::read_fs(fd, &mut buf, cap).unwrap_or(0);
    let _ = civis::close(fd);
    let s = core::str::from_utf8(&buf[..n]).unwrap_or("").trim();
    match s {
        "small" => G_SMALL,
        "4k" => G_4K,
        "16k" => G_16K,
        "64k" => G_64K,
        "256k" => G_256K,
        "1m" => G_1M,
        "4m" => G_4M,
        "native" => G_NATIVE,
        _ => G_ALL,
    }
}

fn want(g: u8) -> bool {
    let active = GROUP.load(Ordering::Relaxed);
    active == G_ALL || active == g
}

fn group_for_size(size: usize) -> u8 {
    match size {
        4096 => G_4K,
        16384 => G_16K,
        65536 => G_64K,
        262144 => G_256K,
        1048576 => G_1M,
        4194304 => G_4M,
        _ => G_ALL,
    }
}

fn group_for_bulk(label: &str, size: usize) -> u8 {
    if label.starts_with("ramfs") {
        G_SMALL
    } else {
        group_for_size(size)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Fs {
    Arca,
    Fat,
}

/// Spoiler cache (Fase 53): 300 settori sequenziali da /dev/sda letti e
/// scartati — oltre le 256 entry CLOCK di block. Svuota la cache settoriale
/// (dati + metadati) di OGNI disco; per ArcaFS resta la cache nodi in cardo
/// (`arca_cache_flush`). ~0.4 s a chiamata (KVM).
fn spoil_cache() -> bool {
    let Ok(fd) = civis::open("/dev/sda", 0) else {
        return false;
    };
    let mut sec = [0u8; 512];
    let mut ok = true;
    for _ in 0..300 {
        if civis::read_fs(fd, &mut sec, 512) != Ok(512) {
            ok = false;
            break;
        }
    }
    let _ = civis::close(fd);
    ok
}

/// Freddo per `fs`: block sempre; cardo solo per ArcaFS.
fn flush_for(fs: Fs) -> bool {
    if !spoil_cache() {
        return false;
    }
    if fs == Fs::Arca {
        return civis::arca_cache_flush().is_ok();
    }
    true
}

/// Crea `path` con `size` byte di pattern (una write, chunking civis).
/// `OPEN_CREATE|OPEN_TRUNC`: idempotente tra righe che riusano lo stesso path
/// con size diverse (su FAT il remove non esiste: i fixture restano,
/// l'immagine e' rigenerata a ogni run.sh).
fn create_file(path: &str, size: usize, seed: u8) -> bool {
    let Ok(fd) = civis::open(path, OPEN_CREATE | OPEN_TRUNC) else {
        return false;
    };
    let buf: Vec<u8> = (0..size)
        .map(|i| ((seed as usize + i) % 251) as u8)
        .collect();
    let n = civis::write_fs(fd, &buf, size);
    let _ = civis::close(fd);
    n == Ok(size)
}

/// Fixture gia' pronto (seed host-side, `RUN_BENCH=1`, vedi arca-tool.sh) o
/// creato qui. Evita di ricreare 1M/4M via fd: su ArcaFS la creazione costa
/// 262/1024 chunk e lascia orfani COW (spazio e tempo inutili prima della
/// misura). La size e' il criterio: il contenuto non entra nelle metriche.
fn ensure_fixture(path: &str, size: usize, seed: u8) -> bool {
    let mut st = civis::Stat {
        size: 0,
        kind: 0,
        readonly: false,
        mtime: 0,
    };
    if civis::stat(path, &mut st).is_ok() && st.size == size as u64 {
        return true;
    }
    create_file(path, size, seed)
}

/// Esegue `warm` iterazioni di riscaldamento poi `iters` misurate, stampa la
/// riga `[bench]`. `bytes` = byte utili per iter (per i KiB/s).
fn run(
    name: &str,
    iters: u64,
    warm: u64,
    bytes: u64,
    hz: u64,
    mut f: impl FnMut() -> bool,
) -> bool {
    for _ in 0..warm {
        if !f() {
            println!("[bench] {}: FAIL (warmup)", name);
            return false;
        }
    }
    // Fase 0: snapshot write-path dopo il warmup (esclude il setup) e
    // delta sulle sole iterazioni misurate. Una IPC di sola lettura: non
    // tocca nodi, cache o disco (righe FAT/ramfs restano a delta zero).
    let io_before = civis::arca_io_stats().ok();
    let t0 = civis::rdtsc();
    let mut max_cyc = 0u64;
    for _ in 0..iters {
        let s = civis::rdtsc();
        if !f() {
            println!("[bench] {}: FAIL (iter)", name);
            return false;
        }
        let dt = civis::rdtsc().wrapping_sub(s);
        if dt > max_cyc {
            max_cyc = dt;
        }
    }
    let total = civis::rdtsc().wrapping_sub(t0);
    if total == 0 {
        println!("[bench] {}: FAIL (tsc fermo)", name);
        return false;
    }
    let cyc_op = total / iters;
    let kb_s = bytes * iters * hz / total / 1024;
    println!(
        "[bench] {} iters={} warm={} cyc_op={} max_cyc={} kb_s={}",
        name, iters, warm, cyc_op, max_cyc, kb_s
    );
    if let (Some(before), Ok(after)) = (io_before, civis::arca_io_stats()) {
        let d = after.delta(before);
        if !d.is_zero() {
            println!(
                "[bench-io] {} put={}/{}B ext={}/{}B nodeW={} (data={} packed={} leaf={} int={} map={}) nodeR={} freeW={} freeB={} xh={}/{}B diskWr={}/{}s diskRd={}/{}s alloc={} trim={} freed={} commits={}",
                name,
                d.put_chunks, d.put_bytes, d.ext_spans, d.ext_bytes,
                d.node_writes, d.node_data, d.node_packed, d.node_leaf,
                d.node_internal, d.node_map, d.node_reads,
                d.free_block_writes, d.free_block_bytes,
                d.xh_writes, d.xh_bytes,
                d.disk_write_reqs, d.disk_write_sectors,
                d.disk_read_reqs, d.disk_read_sectors,
                d.allocs, d.trim_drops, d.freed_versions, d.commits
            );
        }
    }
    true
}

/// Path distinti per il cold: `<root>/<tag><NN>.BIN` (tag ≤ 3 char per stare
/// negli 8.3 su FAT; il chiamante usa tag diversi per write/read).
fn cold_paths(root: &str, tag: &str, total: u64) -> Vec<String> {
    (0..total)
        .map(|i| format!("{}/{}{:02}.BIN", root, tag, i))
        .collect()
}

/// Hot bulk: fixture pre-cresciuto, open/op/close a offset 0 per iter.
/// `suffix` = `_hot` per le righe A/B, vuoto per gli anchor ramfs storici.
fn bulk_hot(
    label: &str,
    path: &str,
    size: usize,
    iters: u64,
    warm: u64,
    hz: u64,
    suffix: &str,
) -> bool {
    if !want(group_for_bulk(label, size)) {
        return true;
    }
    if !ensure_fixture(path, size, 0xA5) {
        println!(
            "[bench] bulk_{}_{}{}: FAIL (create)",
            label, "write", suffix
        );
        return false;
    }
    let wbuf: Vec<u8> = (0..size).map(|i| ((0xA5usize + i) % 251) as u8).collect();
    let wname = format!("bulk_{}_write{}", label, suffix);
    let mut ok = run(&wname, iters, warm, size as u64, hz, || {
        let Ok(fd) = civis::open(path, 0) else {
            println!("[bench] {}: FAIL (open)", wname);
            return false;
        };
        let n = civis::write_fs(fd, &wbuf, size);
        let c = civis::close(fd);
        if n != Ok(size) || c.is_err() {
            println!("[bench] {}: FAIL (write={:?} close={:?})", wname, n, c);
        }
        n == Ok(size)
    });
    if ok {
        let mut rbuf: Vec<u8> = alloc::vec![0u8; size];
        let rname = format!("bulk_{}_read{}", label, suffix);
        ok = run(&rname, iters, warm, size as u64, hz, || {
            let Ok(fd) = civis::open(path, 0) else {
                println!("[bench] {}: FAIL (open)", rname);
                return false;
            };
            let n = civis::read_fs(fd, &mut rbuf, size);
            let c = civis::close(fd);
            if n != Ok(size) || c.is_err() {
                println!("[bench] {}: FAIL (read={:?} close={:?})", rname, n, c);
            }
            n == Ok(size)
        });
    }
    ok
}

/// Hot read-only (4M scaling): fixture pre-cresciuto, open+read+close.
fn bulk_read_hot(label: &str, path: &str, size: usize, iters: u64, warm: u64, hz: u64) -> bool {
    if !want(group_for_bulk(label, size)) {
        return true;
    }
    if !ensure_fixture(path, size, 0xA5) {
        println!("[bench] bulk_{}_read_hot: FAIL (create)", label);
        return false;
    }
    let mut rbuf: Vec<u8> = alloc::vec![0u8; size];
    let name = format!("bulk_{}_read_hot", label);
    let ok = run(&name, iters, warm, size as u64, hz, || {
        let Ok(fd) = civis::open(path, 0) else {
            println!("[bench] {}: FAIL (open)", name);
            return false;
        };
        let n = civis::read_fs(fd, &mut rbuf, size);
        let c = civis::close(fd);
        if n != Ok(size) || c.is_err() {
            println!("[bench] {}: FAIL (read={:?} close={:?})", name, n, c);
        }
        n == Ok(size)
    });
    ok
}

/// Cold bulk: `iters + warm` file distinti pre-creati + UN flush prima della
/// misura (spoiler block + cache cardo). Scrittura = overwrite di ogni file;
/// lettura = file con dati mai letti in questa misura.
fn bulk_cold(
    fs: Fs,
    label: &str,
    root: &str,
    tag: &str,
    size: usize,
    iters: u64,
    warm: u64,
    hz: u64,
    write: bool,
) -> bool {
    if !want(group_for_size(size)) {
        return true;
    }
    let total = iters + warm;
    let paths = cold_paths(root, tag, total);
    for p in &paths {
        if !ensure_fixture(p.as_str(), size, if write { 0x5A } else { 0xA5 }) {
            println!(
                "[bench] bulk_{}_{}_cold: FAIL (create)",
                label,
                if write { "write" } else { "read" }
            );
            return false;
        }
    }
    if !flush_for(fs) {
        println!(
            "[bench] bulk_{}_{}_cold: FAIL (flush)",
            label,
            if write { "write" } else { "read" }
        );
        return false;
    }
    let wbuf: Vec<u8> = (0..size).map(|i| ((0x5Ausize + i) % 251) as u8).collect();
    let mut rbuf: Vec<u8> = alloc::vec![0u8; size];
    let name = format!(
        "bulk_{}_{}_cold",
        label,
        if write { "write" } else { "read" }
    );
    let mut idx = 0usize;
    run(&name, iters, warm, size as u64, hz, || {
        let path = paths[idx].as_str();
        idx += 1;
        let Ok(fd) = civis::open(path, 0) else {
            return false;
        };
        let n = if write {
            civis::write_fs(fd, &wbuf, size)
        } else {
            civis::read_fs(fd, &mut rbuf, size)
        };
        let _ = civis::close(fd);
        n == Ok(size)
    })
}

/// open+read+close di un file piccolo.
fn small_orc(label: &str, path: &str, size: usize, iters: u64, hz: u64) -> bool {
    if !want(G_SMALL) {
        return true;
    }
    let mut buf = alloc::vec![0u8; size];
    let name = format!("{}_small_orc", label);
    run(&name, iters, 5, size as u64, hz, || {
        let Ok(fd) = civis::open(path, 0) else {
            return false;
        };
        let n = civis::read_fs(fd, &mut buf, size);
        let _ = civis::close(fd);
        n == Ok(size)
    })
}

/// open+write+close di un file piccolo (steady state, nessun truncate).
fn small_ow(label: &str, path: &str, size: usize, iters: u64, hz: u64) -> bool {
    if !want(G_SMALL) {
        return true;
    }
    let buf = alloc::vec![0x5Au8; size];
    let name = format!("{}_small_ow", label);
    let mut iter = 0u64;
    run(&name, iters, 5, size as u64, hz, || {
        let this = iter;
        iter += 1;
        let Ok(fd) = civis::open(path, 0) else {
            println!("[bench] {} open FAIL (iter {})", label, this);
            return false;
        };
        let n = civis::write_fs(fd, &buf, size);
        let c = civis::close(fd);
        if n != Ok(size) || c.is_err() {
            println!(
                "[bench] {} op FAIL (iter {}) write={:?} close={:?}",
                label, this, n, c
            );
            return false;
        }
        true
    })
}

/// `stat` puro (metadati senza fd).
fn stat_row(label: &str, path: &str, size: usize, iters: u64, hz: u64) -> bool {
    if !want(G_SMALL) {
        return true;
    }
    let name = format!("{}_stat", label);
    run(&name, iters, 5, size as u64, hz, || {
        let mut st = civis::Stat {
            size: 0,
            kind: 0,
            readonly: false,
            mtime: 0,
        };
        civis::stat(path, &mut st).is_ok() && st.size == size as u64
    })
}

/// `readdir` della radice del mount (contenuti diversi tra i FS: caveat in
/// 13-performance.md; misura il costo del listing, non l'uguaglianza).
fn readdir_row(label: &str, path: &str, iters: u64, hz: u64) -> bool {
    if !want(G_SMALL) {
        return true;
    }
    let mut buf = [0u8; 4096];
    let cap = buf.len();
    let name = format!("{}_readdir", label);
    run(&name, iters, 5, cap as u64, hz, || {
        civis::readdir(path, &mut buf, cap)
            .map(|n| n > 0)
            .unwrap_or(false)
    })
}

// ── ArcaFS nativo (R_OBJ_*): confronto con lo stesso volume via POSIX ──
fn native_get(label: &str, key: &[u8], size: usize, iters: u64, warm: u64, hz: u64) -> bool {
    if !want(G_NATIVE) {
        return true;
    }
    let name = format!("{}_native", label);
    run(&name, iters, warm, size as u64, hz, || {
        civis::obj_get(BUCKET, key)
            .map(|v| v.len() == size)
            .unwrap_or(false)
    })
}

fn native_put(label: &str, key: &[u8], size: usize, iters: u64, warm: u64, hz: u64) -> bool {
    if !want(G_NATIVE) {
        return true;
    }
    let buf: Vec<u8> = (0..size).map(|i| ((0x3Cusize + i) % 251) as u8).collect();
    let name = format!("{}_native", label);
    run(&name, iters, warm, size as u64, hz, || {
        civis::obj_put(BUCKET, key, &buf) == Ok(size as u64)
    })
}

fn native_stat(label: &str, key: &[u8], size: usize, iters: u64, hz: u64) -> bool {
    if !want(G_NATIVE) {
        return true;
    }
    let name = format!("{}_native", label);
    run(&name, iters, 5, size as u64, hz, || {
        civis::obj_stat(BUCKET, key)
            .map(|(_, s, _, _)| s == size as u64)
            .unwrap_or(false)
    })
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    println!("[bench] starting, pid={}", civis::getpid());
    println!(
        "[bench] profilo: {}",
        if REDUCED { "reduced" } else { "full" }
    );
    let group = group_from_marker();
    GROUP.store(group, Ordering::Relaxed);
    println!("[bench] gruppo: {}", group_name(group));
    let hz = civis::tsc_calibrate(20);
    println!("[bench] tsc_hz={}", hz);
    let mut ok = hz != 0;

    // Setup fixture piccole (radice Arca e radice FAT).
    if ok && want(G_SMALL) {
        ok = create_file(ARCA_SMALL, 25, 0x11) && create_file(FAT_SMALL, 25, 0x11);
        if !ok {
            println!("[bench] setup: FAIL (fixture 25B)");
        }
    }

    // b1: solo IPC — 1 B da /dev/zero.
    if ok && want(G_SMALL) {
        match civis::open("/dev/zero", 0) {
            Err(e) => {
                println!("[bench] zero_1B: FAIL (open {:?})", e);
                ok = false;
            }
            Ok(fd) => {
                let mut one = [0u8; 1];
                ok &= run("zero_1B", 2000, 5, 1, hz, || {
                    civis::read_fs(fd, &mut one, 1) == Ok(1)
                });
                let _ = civis::close(fd);
            }
        }
    }

    // b2: catena completa — 200 settori raw da /dev/sda.
    if ok && want(G_SMALL) {
        match civis::open("/dev/sda", 0) {
            Err(e) => {
                println!("[bench] sda_512B_seq: FAIL (open {:?})", e);
                ok = false;
            }
            Ok(fd) => {
                let mut sec = [0u8; 512];
                ok &= run("sda_512B_seq", 200, 5, 512, hz, || {
                    civis::read_fs(fd, &mut sec, 512) == Ok(512)
                });
                let _ = civis::close(fd);
            }
        }
    }

    // b3: file piccolo su FAT (anchor storico: HELLO.TXT) + gemello Arca.
    if ok {
        ok &= small_orc("fat", "/fat/HELLO.TXT", 25, 500, hz);
    }
    if ok {
        ok &= small_orc("arca", ARCA_SMALL, 25, 500, hz);
    }

    // Metadati A/B.
    if ok {
        ok &= small_ow("fat", FAT_SMALL, 25, pick(1000, 200), hz);
    }
    if ok {
        ok &= small_ow("arca", ARCA_SMALL, 25, pick(ARCA_SMALL_OW_ITERS, 50), hz);
    }
    if ok {
        // HELLO.TXT e' 26 B ("Hello from Velordo FAT32!\n"): stat puro.
        ok &= stat_row("fat", "/fat/HELLO.TXT", 26, pick(1000, 200), hz);
    }
    if ok {
        ok &= stat_row("arca", ARCA_SMALL, 25, pick(1000, 200), hz);
    }
    if ok {
        ok &= readdir_row("fat", "/fat", pick(200, 100), hz);
    }
    if ok {
        ok &= readdir_row("arca", "/", pick(200, 100), hz);
    }

    // b4: ramfs 4 KiB (vera ramfs: /tmp) — write poi read.
    if ok && want(G_SMALL) {
        match civis::open(RAMFS_4K, OPEN_CREATE | OPEN_TRUNC) {
            Err(e) => {
                println!("[bench] ramfs_4K: FAIL (create {:?})", e);
                ok = false;
            }
            Ok(fd) => {
                let wbuf = [0xA5u8; 4096];
                ok &= run("ramfs_4K_write", 100, 5, 4096, hz, || {
                    civis::write_fs(fd, &wbuf, 4096) == Ok(4096)
                });
                let _ = civis::close(fd);
            }
        }
    }
    if ok && want(G_SMALL) {
        match civis::open(RAMFS_4K, 0) {
            Err(e) => {
                println!("[bench] ramfs_4K_read: FAIL (open {:?})", e);
                ok = false;
            }
            Ok(fd) => {
                let mut rbuf = [0u8; 4096];
                ok &= run("ramfs_4K_read", 100, 5, 4096, hz, || {
                    civis::read_fs(fd, &mut rbuf, 4096) == Ok(4096)
                });
                let _ = civis::close(fd);
                if civis::remove(RAMFS_4K).is_err() {
                    println!("[bench] ramfs cleanup: FAIL (remove)");
                    ok = false;
                }
            }
        }
    }

    // b5: overwrite 4 KiB su FAT (anchor storico).
    if ok && want(G_SMALL) {
        match civis::open("/fat/BENCH.TMP", OPEN_CREATE) {
            Err(e) => {
                println!("[bench] fat_4K_oow: FAIL (create {:?})", e);
                ok = false;
            }
            Ok(fd) => {
                let _ = civis::close(fd);
                let wbuf = [0x5Au8; 4096];
                ok &= run("fat_4K_oow", 50, 5, 4096, hz, || {
                    let Ok(fd) = civis::open("/fat/BENCH.TMP", 0) else {
                        return false;
                    };
                    let n = civis::write_fs(fd, &wbuf, 4096);
                    let _ = civis::close(fd);
                    n == Ok(4096)
                });
            }
        }
    }

    // b6: ramfs 16K/64K (hot=cold in RAM).
    if ok {
        ok &= bulk_hot("ramfs_16K", RAMFS_16K, 16384, 25, 5, hz, "");
    }
    if ok {
        ok &= bulk_hot("ramfs_64K", RAMFS_64K, 65536, 10, 5, hz, "");
    }

    // A/B hot 4K/16K/64K.
    if ok {
        ok &= bulk_hot("fat_4K", FAT_4K, 4096, pick(25, 10), pick(5, 2), hz, "_hot");
    }
    if ok {
        ok &= bulk_hot(
            "arca_4K",
            ARCA_4K,
            4096,
            pick(25, 10),
            pick(5, 2),
            hz,
            "_hot",
        );
    }
    if ok {
        ok &= bulk_hot(
            "fat_16K",
            FAT_16K,
            16384,
            pick(12, 6),
            pick(5, 2),
            hz,
            "_hot",
        );
    }
    if ok {
        ok &= bulk_hot(
            "arca_16K",
            ARCA_16K,
            16384,
            pick(12, 6),
            pick(5, 2),
            hz,
            "_hot",
        );
    }
    if ok {
        ok &= bulk_hot(
            "fat_64K",
            FAT_64K,
            65536,
            pick(8, 4),
            pick(5, 2),
            hz,
            "_hot",
        );
    }
    if ok {
        ok &= bulk_hot(
            "arca_64K",
            ARCA_64K,
            65536,
            pick(8, 4),
            pick(5, 2),
            hz,
            "_hot",
        );
    }

    // A/B cold 4K/16K/64K (file distinti + un flush).
    if ok {
        ok &= bulk_cold(
            Fs::Fat,
            "fat_4K",
            "/fat",
            "W4K",
            4096,
            pick(15, 5),
            pick(5, 2),
            hz,
            true,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_4K",
            ARCA_ROOT,
            "W4K",
            4096,
            pick(15, 5),
            pick(5, 2),
            hz,
            true,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Fat,
            "fat_4K",
            "/fat",
            "R4K",
            4096,
            pick(15, 5),
            pick(5, 2),
            hz,
            false,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_4K",
            ARCA_ROOT,
            "R4K",
            4096,
            pick(15, 5),
            pick(5, 2),
            hz,
            false,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Fat,
            "fat_16K",
            "/fat",
            "W16",
            16384,
            pick(8, 3),
            pick(5, 2),
            hz,
            true,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_16K",
            ARCA_ROOT,
            "W16",
            16384,
            pick(8, 3),
            pick(5, 2),
            hz,
            true,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Fat,
            "fat_16K",
            "/fat",
            "R16",
            16384,
            pick(8, 3),
            pick(5, 2),
            hz,
            false,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_16K",
            ARCA_ROOT,
            "R16",
            16384,
            pick(8, 3),
            pick(5, 2),
            hz,
            false,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Fat,
            "fat_64K",
            "/fat",
            "W64",
            65536,
            pick(5, 2),
            pick(5, 2),
            hz,
            true,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_64K",
            ARCA_ROOT,
            "W64",
            65536,
            pick(5, 2),
            pick(5, 2),
            hz,
            true,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Fat,
            "fat_64K",
            "/fat",
            "R64",
            65536,
            pick(5, 2),
            pick(5, 2),
            hz,
            false,
        );
    }
    if ok {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_64K",
            ARCA_ROOT,
            "R64",
            65536,
            pick(5, 2),
            pick(5, 2),
            hz,
            false,
        );
    }

    // Grandi: 256K hot, 1M hot+cold, 4M read hot+cold (scaling).
    if ok {
        ok &= bulk_hot(
            "fat_256K",
            FAT_256K,
            262144,
            pick(5, 1),
            pick(1, 0),
            hz,
            "_hot",
        );
    }
    if ok {
        ok &= bulk_hot(
            "arca_256K",
            ARCA_256K,
            262144,
            pick(5, 1),
            pick(1, 0),
            hz,
            "_hot",
        );
    }
    // Profilo reduced: 1M/4M restano fuori (dominati dal costo per chunk su
    // host lenti); il profilo full li copre con fixture pre-seedati (C).
    if ok && !REDUCED {
        ok &= bulk_hot("fat_1M", FAT_1M, 1048576, 3, 1, hz, "_hot");
    }
    if ok && !REDUCED {
        ok &= bulk_hot("arca_1M", ARCA_1M, 1048576, 3, 1, hz, "_hot");
    }
    if ok && !REDUCED {
        ok &= bulk_cold(Fs::Fat, "fat_1M", "/fat", "W1M", 1048576, 2, 0, hz, true);
    }
    if ok && !REDUCED {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_1M",
            ARCA_ROOT,
            "W1M",
            1048576,
            2,
            0,
            hz,
            true,
        );
    }
    if ok && !REDUCED {
        ok &= bulk_cold(Fs::Fat, "fat_1M", "/fat", "R1M", 1048576, 2, 0, hz, false);
    }
    if ok && !REDUCED {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_1M",
            ARCA_ROOT,
            "R1M",
            1048576,
            2,
            0,
            hz,
            false,
        );
    }
    if ok && !REDUCED {
        ok &= bulk_read_hot("fat_4M", FAT_4M, 4194304, 2, 1, hz);
    }
    if ok && !REDUCED {
        ok &= bulk_read_hot("arca_4M", ARCA_4M, 4194304, 2, 1, hz);
    }
    if ok && !REDUCED {
        ok &= bulk_cold(Fs::Fat, "fat_4M", "/fat", "R4M", 4194304, 1, 0, hz, false);
    }
    if ok && !REDUCED {
        ok &= bulk_cold(
            Fs::Arca,
            "arca_4M",
            ARCA_ROOT,
            "R4M",
            4194304,
            1,
            0,
            hz,
            false,
        );
    }

    // Spoiler reference (metodologia).
    if ok && want(G_SMALL) {
        ok &= run(
            "bulk_spoil_300sec",
            pick(15, 3),
            0,
            300 * 512,
            hz,
            spoil_cache,
        );
    }

    // ArcaFS nativo vs POSIX: fixture oggetti nel bucket `bch`.
    if ok && want(G_NATIVE) {
        ok &= civis::obj_put(BUCKET, b"small", &[0x11u8; 25]) == Ok(25)
            && civis::obj_put(BUCKET, b"k4", &[0x22u8; 4096]) == Ok(4096);
        // `k1m` e' il fixture pesante: solo full. Se `arca-tool.sh` l'ha
        // pre-seedato (`RUN_BENCH=1`) evitiamo 262 commit di setup; il PUT
        // nativo misurato piu' avanti lo sovrascrive comunque.
        if ok && !REDUCED {
            let preseeded = civis::obj_stat(BUCKET, b"k1m")
                .map(|(_, s, _, _)| s == 1048576)
                .unwrap_or(false);
            if !preseeded {
                ok &= civis::obj_put(BUCKET, b"k1m", &alloc::vec![0x33u8; 1048576]) == Ok(1048576);
            }
        }
        if !ok {
            println!("[bench] native setup: FAIL (obj_put)");
        }
    }
    if ok {
        ok &= native_get(
            "arca_small_orc",
            b"small",
            25,
            pick(500, 100),
            pick(5, 2),
            hz,
        );
    }
    if ok {
        ok &= native_stat("arca_stat", b"k4", 4096, pick(1000, 200), hz);
    }
    if ok {
        ok &= native_put(
            "bulk_arca_4K_write_hot",
            b"k4",
            4096,
            pick(25, 10),
            pick(5, 2),
            hz,
        );
    }
    if ok {
        ok &= native_get(
            "bulk_arca_4K_read_hot",
            b"k4",
            4096,
            pick(50, 20),
            pick(5, 2),
            hz,
        );
    }
    // PUT nativo 1M = 262 commit (uno per chunk, API senza fd): 1 op misurata
    // (finding sulla durevolezza per chiamata, non throughput generale).
    if ok && !REDUCED {
        ok &= native_put("bulk_arca_1M_write_hot", b"k1m", 1048576, 1, 0, hz);
    }
    if ok && !REDUCED {
        ok &= native_get("bulk_arca_1M_read_hot", b"k1m", 1048576, 3, 1, hz);
    }
    if ok && !REDUCED && want(G_NATIVE) {
        if !flush_for(Fs::Arca) {
            println!("[bench] bulk_arca_1M_read_cold_native: FAIL (flush)");
            ok = false;
        } else {
            ok &= native_get("bulk_arca_1M_read_cold", b"k1m", 1048576, 1, 0, hz);
        }
    }

    // Niente cleanup: il volume e' rigenerato a ogni run. Su ArcaFS ogni
    // remove e' un'altra mutazione B+tree (commit + orfani) e falserebbe la
    // campagna; su FAT remove e' fuori scope.

    if ok {
        println!("[bench] DONE ok=1");
    } else {
        println!("[bench] DONE ok=0");
    }
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if let Some(loc) = info.location() {
        println!("[bench] panic @ {}:{}", loc.file(), loc.line());
    } else {
        println!("[bench] panic");
    }
    civis::exit(1)
}
