#!/usr/bin/env bash
# Applica le patch Velordo ai sorgenti del compilatore (S2.5b): analogo di
# pal/velordo/apply.py per library/std, ma sui sorgenti rustc (git checkout
# pinnato fuori repo). Ogni patch e' un file versionato in tools/patches/
# (nomi 0001-*.patch): a drift lo script fallisce LOUD (mai patch parziali).
# Idempotenza: l'albero deve essere PULITO prima (git status); per
# riapplicare: `git -C "$RUST_SRC" checkout -- compiler/`.
# Uso: scripts/apply-compiler.sh [RUST_SRC] (default ../../rust-src).
set -euo pipefail
cd "$(dirname "$0")/.."
RUST_SRC="${1:-$PWD/../rust-src}"
PDIR="$PWD/tools/patches"
if [ ! -d "$RUST_SRC/compiler" ]; then
    echo "[apply-compiler] ERROR: $RUST_SRC senza compiler/ (clone+checkout prima)" >&2
    exit 1
fi
if [ -n "$(git -C "$RUST_SRC" status --short -- compiler/)" ]; then
    echo "[apply-compiler] ERROR: compiler/ sporco (checkout pulito prima)" >&2
    exit 1
fi
n=0
for p in "$PDIR"/*.patch; do
    [ -e "$p" ] || continue
    git -C "$RUST_SRC" apply --check "$p"
    git -C "$RUST_SRC" apply "$p"
    echo "[apply-compiler] $(basename "$p")"
    n=$((n + 1))
done
echo "[apply-compiler] $n patch su $RUST_SRC"
