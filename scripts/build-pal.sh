#!/usr/bin/env bash
# Build della PAL std Velordo + hello std nativo (S1.3).
#
# Strategia Xous: si compila SOLO la library (niente bootstrap completo) con
# cargo + RUSTC_BOOTSTRAP, contro una copia di library/ da rust-src + PAL
# applicata da pal/velordo/apply.py. Poi sysroot assemblato + hello linkato
# con rust-lld e il solito linker script USER_CODE.
#
# Uso: scripts/build-pal.sh [--force]
#   --force: ricompila la library anche se gli rlib esistono gia'.
# Output:
#   $PAL_WORK/sysroot-velordo  (sysroot nativo: rlib per il target)
#   $PAL_WORK/hello-std.bin    (ELF stripped, pronto da seedare)
# Env:
#   PAL_WORK  (default /tmp/opencode-pal): tutto il build vive qui, MAI nel
#             repo (sorgenti PAL in pal/velordo, script qui).
#   RUST_SRC  (default dal sysroot del toolchain): library/ di rust-src.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${PAL_WORK:-/tmp/opencode-pal}"
FORCE=0
if [ "${1:-}" = "--force" ]; then FORCE=1; fi

SYSROOT_RUSTC="$(rustc --print sysroot)"
LIBSRC="${RUST_SRC:-$SYSROOT_RUSTC/lib/rustlib/src/rust/library}"
if [ ! -f "$LIBSRC/std/Cargo.toml" ]; then
    echo "[pal] ERROR: library Rust non trovata in $LIBSRC (manca rust-src?)" >&2
    exit 1
fi
RUSTC_VER="$(rustc -vV | rg '^release:' | awk '{print $2}')"
echo "[pal] toolchain $RUSTC_VER, library $LIBSRC"

# 1. Copia fresca dei sorgenti (78M, secondi) + PAL.
rm -rf "$WORK/library"
mkdir -p "$WORK"
cp -r "$LIBSRC" "$WORK/library"
chmod -R u+w "$WORK/library"
python3 "$ROOT/pal/velordo/apply.py" "$WORK/library" "$ROOT/pal/velordo/new"

# 2. Build sysroot crate per il target (solo se manca o --force).
TARGET_JSON="$ROOT/targets/x86_64-unknown-velordo.json"
SYSROOT_OUT="$WORK/sysroot-velordo"
STAMP="$WORK/.rlibs-ok"
if [ "$FORCE" = "1" ] || [ ! -f "$STAMP" ]; then
    rm -f "$STAMP"
    (
        cd "$WORK"
        export RUSTC_BOOTSTRAP=1
        # panic=abort SOLO per il target (gli unit host come proc_macro
        # restano unwind: con RUSTFLAGS globale si avvelenerebbero).
        # -Zforce-unstable-if-unmarked: senza, i mark
        # `rustc_const_stable_indirect` delle dipendenze (hashbrown) non
        # vengono registrati nei metadati e std fallisce con "cannot be
        # (indirectly) exposed to stable" (stesso errore di Xous #133857).
        export CARGO_TARGET_X86_64_UNKNOWN_VELORDO_RUSTFLAGS="-C panic=abort -Zforce-unstable-if-unmarked"
        export CARGO_TARGET_DIR="$WORK/target"
        # Solo std (+deps): il crate `test` richiede `restricted_std` per
        # target senza supporto completo (S1.3: niente harness on-target,
        # hello e' un binario normale). Se un giorno servira', si rivaluta.
        cargo build --release \
            --manifest-path "$WORK/library/Cargo.toml" \
            -p std \
            --target "$TARGET_JSON" \
            --no-default-features \
            --features "compiler-builtins-mem" \
            -Z json-target-spec
    )
    touch "$STAMP"
else
    echo "[pal] rlib esistenti (usa --force per ricompilare)"
fi

# 3. Assembla il sysroot nativo (tutti gli rlib del target, nomi con hash:
# rustc li risolve da solo via --sysroot). Servono ANCHE gli .rmeta: cargo
# compila con `-Z embed-metadata=no`, quindi gli rlib hanno solo uno stub
# di metadati (senza .rmeta: "only metadata stub found").
DEST="$SYSROOT_OUT/lib/rustlib/x86_64-unknown-velordo/lib"
mkdir -p "$DEST"
find "$WORK/target/x86_64-unknown-velordo/release" \( -name 'lib*.rlib' -o -name 'lib*.rmeta' \) -exec cp {} "$DEST"/ \;
echo "[pal] sysroot: $(ls "$DEST"/*.rlib | wc -l) rlib in $DEST"
ls "$DEST" | head -14

# 4. Hello std: println! + Vec + Mutex + HashMap + Instant (niente fs/net).
cat > "$WORK/hello.rs" << 'EOF'
//! S2.1: hello std per Velordo (S1.3 + argv/env + alloc stress).
#![feature(restricted_std)]
#![feature(velordo_std)]
fn main() {
    println!("[stdhello] ciao da std su Velordo");
    let mut v = Vec::new();
    for i in 0..8u32 {
        v.push(i * i);
    }
    println!("[stdhello] vec len={} sum={}", v.len(), v.iter().sum::<u32>());
    let m = std::sync::Mutex::new(0u32);
    *m.lock().unwrap() += 41;
    println!("[stdhello] mutex={}", *m.lock().unwrap());
    let mut map = std::collections::HashMap::new();
    map.insert("chiave", 7u32);
    println!("[stdhello] hashmap[chiave]={}", map["chiave"]);
    let t0 = std::time::Instant::now();
    let mut spin = 0u64;
    while t0.elapsed() < std::time::Duration::from_millis(50) {
        spin += 1;
    }
    println!("[stdhello] instant ok spin={}", spin);
    // S2.0: argv (argv[0] = path digitato), env, cwd.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    println!("[stdhello] argv={}", argv.join(","));
    match std::env::var("VELORDO_S2") {
        Ok(v) => println!("[stdhello] env VELORDO_S2={}", v),
        Err(_) => println!("[stdhello] env VELORDO_S2 missing"),
    }
    println!("[stdhello] env count={}", std::env::vars().count());
    std::env::set_var("STD_SET", "42");
    println!(
        "[stdhello] set_var STD_SET={}",
        std::env::var("STD_SET").unwrap_or_default()
    );
    match std::env::current_dir() {
        Ok(p) => println!("[stdhello] cwd={}", p.display()),
        Err(_) => println!("[stdhello] cwd ERR"),
    }
    // S2.1: churn small + large 2 MiB + over-align + stats.
    let t1 = std::time::Instant::now();
    let mut held = Vec::new();
    for i in 0..2000u32 {
        let mut b = Vec::with_capacity((i % 256 + 1) as usize);
        b.extend((0..b.capacity()).map(|j| (i ^ j as u32) as u8));
        if i % 3 == 0 {
            held.push(b);
        }
    }
    let mut big = Vec::with_capacity(2_000_000);
    big.resize(2_000_000, 0x5Au8);
    assert!(big.iter().all(|&b| b == 0x5A));
    drop(held);
    drop(big);
    println!("[stdhello] alloc churn ok in {}ms", t1.elapsed().as_millis());
    let layout = std::alloc::Layout::from_size_align(100, 64).unwrap();
    let p = unsafe { std::alloc::alloc(layout) };
    println!("[stdhello] overalign ok={}", !p.is_null() && p as usize % 64 == 0);
    unsafe { std::alloc::dealloc(p, layout) };
    let (a, f, live, high) = std::os::velordo::alloc_stats();
    println!("[stdhello] alloc stats a={} f={} live={} high={}", a, f, live, high);
    println!("[stdhello] DONE");
}
EOF
RUSTC_BOOTSTRAP=1 rustc --edition 2021 -O -Z unstable-options \
    --sysroot "$SYSROOT_OUT" \
    --target "$TARGET_JSON" \
    -C relocation-model=pic \
    -C link-arg=-T"$ROOT/pal/velordo/hello.ld" \
    -C link-arg=--apply-dynamic-relocs \
    "$WORK/hello.rs" -o "$WORK/hello-std.elf"
SYSROOT_BIN="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin"
"$SYSROOT_BIN/llvm-objcopy" --strip-all "$WORK/hello-std.elf" "$WORK/hello-std.bin" 2>/dev/null \
    || objcopy --strip-all "$WORK/hello-std.elf" "$WORK/hello-std.bin"
mkdir -p "$ROOT/pal/build"
cp "$WORK/hello-std.bin" "$ROOT/pal/build/hello-std.bin"
echo "[pal] hello: $(stat -c %s "$ROOT/pal/build/hello-std.bin") byte in pal/build/hello-std.bin (pronto da seedare)"
readelf -h "$WORK/hello-std.elf" | rg -i 'type|entry|machine'
