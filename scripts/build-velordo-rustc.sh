#!/usr/bin/env bash
# Build dello staticone velordo-rustc (S2.5b): rustc_driver_impl + backend
# Cranelift statico. Uso: scripts/build-velordo-rustc.sh [host|velordo]
#   host    = sanity su Linux (default se omesso? no: esplicito).
#   velordo = cross per il target custom (PAL sysroot installato).
# Requisiti: RUSTC_BOOTSTRAP (rustc_private), sorgenti pinnati in
# ../../rust-src + backend vendor, cargo con rete (registry).
# Eseguire con CWD fuori dal repo (config discovery: .cargo/config.toml del
# repo forza -Zbuild-std che duplica i lang item — vedi S2.5b).
set -euo pipefail
MODE="${1:?uso: build-velordo-rustc.sh [host|velordo]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$ROOT/tools/velordo-rustc/Cargo.toml"

export RUSTC_BOOTSTRAP=1
export CFG_RELEASE="1.100.0-nightly"
export CFG_RELEASE_CHANNEL="nightly"
export CFG_VERSION="1.100.0-nightly (67eda617e 2026-09-10)"
export CFG_VER_HASH="67eda617e"
export CFG_VER_DATE="2026-09-10"
export RUSTC_INSTALL_BINDIR="bin"

# Lockfile: tools/velordo-rustc/Cargo.lock (copiato da rust-src al setup:
# niente skew unicode-ident/properties; committed come ogni bin crate).
# Patch compiler (dll_path & co.): applicate se l'albero e' pulito,
# altrimenti si assume gia' applicato (fail-loud di apply-compiler.sh).
if [ -z "$(git -C "$ROOT/../rust-src" status --short -- compiler/)" ]; then
    "$ROOT/scripts/apply-compiler.sh" "$ROOT/../rust-src"
fi
# Anti-skew sysroot (S2.5d): cargo non vede il sysroot installato nel
# fingerprint — dopo un reinstall con PAL diversa il target va pulito,
# altrimenti E0460 (rmeta contro std nuova) o, peggio, link silenzioso
# contro std vecchia. Lo stamp confronta gli rlib installati.
# CWD fuori dal repo (vedi sopra): cargo non deve vedere .cargo/config.toml.
cd /tmp
if [ "$MODE" = "velordo" ]; then
    SYSROOT_VE="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-velordo/lib"
    STAMP="/tmp/vrustc-velordo/sysroot-stamp.txt"
    CUR="$( (stat -c '%Y %s' "$SYSROOT_VE"/libstd.rlib 2>/dev/null || echo MISSING) | sha256sum | cut -d' ' -f1)"
    if [ ! -f "$STAMP" ] || [ "$(cat "$STAMP")" != "$CUR" ]; then
        echo "[build-velordo-rustc] sysroot cambiato: pulizia target (anti E0460/skew)"
        rm -rf /tmp/vrustc-velordo
        mkdir -p /tmp/vrustc-velordo
        echo "$CUR" > "$STAMP"
    fi
fi

if [ "$MODE" = "host" ]; then
    export CFG_COMPILER_HOST_TRIPLE="x86_64-unknown-linux-gnu"
    cargo build --release --manifest-path "$MANIFEST" \
        --target x86_64-unknown-linux-gnu --target-dir /tmp/vrustc-host
elif [ "$MODE" = "velordo" ]; then
    export CFG_COMPILER_HOST_TRIPLE="x86_64-unknown-velordo"
    # getrandom su RDRAND (niente backend OS: S2.5b).
    # cap-lints allow come x.py (il target softfloat nega future-incompat
    # su `target_feature(sse2)` usato con dispatch runtime: sicuro con FPU).
    # S2.5d: image-base a USER_CODE + apply-dynamic-relocs come i binari
    # userland (il loader mappa vaddr finali in finestra, niente slide:
    # un PIE a base zero verrebbe rifiutato in validazione).
    export RUSTFLAGS='--cap-lints=allow --cfg getrandom_backend="rdrand" -C link-arg=--image-base=0x400000000000 -C link-arg=--apply-dynamic-relocs'
    cargo build -Zjson-target-spec --release --manifest-path "$MANIFEST" \
        --target "$ROOT/targets/x86_64-unknown-velordo.json" --target-dir /tmp/vrustc-velordo
else
    echo "modo ignoto: $MODE" >&2
    exit 1
fi
echo "[build-velordo-rustc] $MODE ok"
