#!/usr/bin/env bash
# Unit test host dei crate condivisi (fuori dal gate, come la suite FAT).
#
# Il gate (`run-tests.sh`) e' QEMU: verifica il sistema in esecuzione, non la
# matematica dei crate. Alcune parti pero' si provano meglio sull'host, in
# millisecondi e senza boot: il rasterizzatore del font (`libs/term`), la
# core dell'allocatore (`libs/valloc`), BLAKE2s (`blake2s`).
#
# Perche' serve uno script: il repo ha `.cargo/config.toml` con
# `build.target = x86_64-unknown-none` + `build-std`, e quella config si
# applica in base alla **directory corrente**. Da qui dentro `cargo test`
# proverebbe a compilare i test per il target freestanding (niente std,
# niente harness, e doppio `alloc`). Quindi i test girano da una directory
# neutra (`mktemp -d`): cargo non vede la config del repo e usa il target
# dell'host.
#
# Uso: ./scripts/run-unit-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

# Target dir dentro il repo (gitignored), separata da quella del kernel/user.
export CARGO_TARGET_DIR="$ROOT/target/unit-tests"

# Crate con `#[cfg(test)] mod tests` (il primo e' quello nato dalla
# regressione della copertura: il rasterizer perdeva l'ultimo pixel di ogni
# span e il testo risultava assottigliato).
CRATES=(libs/term libs/valloc blake2s)

FAIL=0
TOT_PASS=0
TOT_FAIL=0
NEUTRAL="$(mktemp -d)"
trap 'rm -rf "$NEUTRAL"' EXIT
cd "$NEUTRAL"

for c in "${CRATES[@]}"; do
    echo "[unit] $c"
    out="$(cargo test --manifest-path "$ROOT/$c/Cargo.toml" 2>&1)" || FAIL=1
    echo "$out" | grep -E "^test result|^error|panicked" || true
    p=$(echo "$out" | sed -n 's/^test result: ok\. \([0-9]*\) passed.*/\1/p' | head -1)
    f=$(echo "$out" | sed -n 's/.* \([0-9]*\) failed.*/\1/p' | head -1)
    TOT_PASS=$((TOT_PASS + ${p:-0}))
    TOT_FAIL=$((TOT_FAIL + ${f:-0}))
done

echo "[unit] totale: $TOT_PASS passati, $TOT_FAIL falliti"
if [ "$FAIL" != "0" ] || [ "$TOT_FAIL" != "0" ]; then
    echo "[unit] FAIL"
    exit 1
fi
echo "[unit] OK"
