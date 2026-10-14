#!/usr/bin/env bash
# Installa gli rlib PAL nel sysroot del toolchain (S2.5b): cosi' cargo
# risolve `std` per il target custom senza -Zbuild-std (pattern rustup:
# lib/rustlib/<triple>/lib/). Idempotente (rsync). NON tocca altro.
set -euo pipefail
cd "$(dirname "$0")/.."
PAL_SYSROOT="${PAL_WORK:-/tmp/opencode-pal}/sysroot-velordo"
SRC="$PAL_SYSROOT/lib/rustlib/x86_64-unknown-velordo"
if [ ! -d "$SRC" ]; then
    echo "[install-pal-sysroot] ERROR: $SRC assente (scripts/build-pal.sh --force prima)" >&2
    exit 1
fi
DST="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-velordo"
# Pulisci prima: i fingerprint cambiano col target JSON (stale = E0461).
rm -rf "$DST"
mkdir -p "$DST"
cp -r "$SRC"/. "$DST"/
echo "[install-pal-sysroot] $(ls "$DST/lib" | wc -l) file in $DST"
