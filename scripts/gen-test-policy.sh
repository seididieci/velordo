#!/usr/bin/env bash
# Tabella policy per i binari test (Fase 45, sandbox build): testland/ + la
# suite della personalita' POSIX in flavours/posix/tests (Fase 58.5) + i
# veicoli test della PAL in pal/build se presenti (S2.4: hello-std e figli
# creano pipe/grant → servono i diritti ALL come ogni binario suite).
#
# Genera `build-meta/test_policy.rs` con `pub const TEST_POLICY: &[(u64,u32)]`
# (hash FNV-1a -> mask ops, TUTTI ALL: la suite esercita ogni op). Incluso
# SOLO da cardo via `VELORDO_TEST_POLICY` (mai dai binari test: niente ciclo
# hash-di-se', stesso motivo dell'esclusione rector/cardo dal manifest
# servizi — vedi gen-service-hashes.sh).
#
# Esclusi:
# - `userforeign.bin` (l'attore "ignoto" di t57: DEVE restare fuori da ogni
#   tabella per provare il default restrittivo fail-closed);
# - `cardo.bin` (artefatto del rebuild di cardo in coda a build-tests.sh:
#   cardo e' fuori da entrambe le tabelle per costruzione, come rector).
#
# Chiamato in coda a build-tests.sh (i .bin test esistono solo allora) e
# seguito dal rebuild di cardo (unico consumatore). Fixpoint in un passaggio:
# cardo e' escluso da entrambe le tabelle, i test non includono questa.
set -euo pipefail
cd "$(dirname "$0")/.."

# Fase 58.5: test meccanismo in testland/build, test personalita' POSIX in
# flavours/posix/tests/build. Entrambe le dir entrano nella tabella.
# S2.4: veicoli test della PAL (hello-std.bin + figli) in pal/build —
# OPZIONALE (il gate default gira senza PAL): entra solo se esiste con .bin.
BUILD_DIRS=(testland/build flavours/posix/tests/build)
if ls pal/build/*.bin >/dev/null 2>&1; then
    BUILD_DIRS+=(pal/build)
    echo "[gen-test-policy] + pal/build (veicoli PAL)"
fi
OUT_DIR="build-meta"
OUT="$OUT_DIR/test_policy.rs"

for d in "${BUILD_DIRS[@]}"; do
    if [ ! -d "$d" ]; then
        echo "[gen-test-policy] ERROR: $d mancante (build-tests.sh prima)" >&2
        exit 1
    fi
done

mkdir -p "$OUT_DIR"
tmp="$OUT.tmp"

python3 - "${BUILD_DIRS[@]}" "$tmp" <<'EOF'
import glob, os, sys

def fnv1a(data: bytes) -> int:
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h

dirs = sys.argv[1:-1]
tmp = sys.argv[-1]
bins = sorted(p for d in dirs for p in glob.glob(os.path.join(d, "*.bin")))
if not bins:
    sys.exit("nessun .bin in %s" % ", ".join(dirs))
# L'attore "ignoto" di t57 resta fuori da OGNI tabella (prova il default);
# cardo.bin (rebuild artefatto, vedi sopra) mai in tabella.
bins = [p for p in bins if os.path.basename(p) not in ("userforeign.bin", "cardo.bin")]

lines = [
    "// Generato da scripts/gen-test-policy.sh — MAI modificare a mano.",
    "// Sandbox build (Fase 45): hash dei binari testland -> mask ALL (la",
    "// suite esercita ogni op; i negativi stanno in t57 sul default ignoto).",
    "// Consumato via `include!(env!(\"VELORDO_TEST_POLICY\"))` SOLO da cardo",
    "// (dopo service_hashes+service_policy). Rigenerato a ogni build test.",
    "pub const TEST_POLICY: &[(u64, u32)] = &[",
]
for p in bins:
    with open(p, "rb") as f:
        data = f.read()
    if not data:
        sys.exit("binario vuoto: %s" % p)
    lines.append("    (0x%016X, 0xFFF), // %s" % (fnv1a(data), os.path.basename(p)))
lines.append("];")

with open(tmp, "w") as f:
    f.write("\n".join(lines) + "\n")
print("[gen-test-policy] %d binari -> %s" % (len(bins), tmp))
EOF

mv "$tmp" "$OUT"
echo "[gen-test-policy] scritto $OUT"
