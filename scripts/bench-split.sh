#!/usr/bin/env bash
# Campagna benchmark per-size (full): un boot per gruppo, cosi' un timeout o
# un host lento non fa perdere l'intera matrice. Ogni gruppo viene seedato con
# `/test/bench-group.txt` (vedi arca-tool.sh) e il guest esegue solo le righe
# di quel gruppo; i log restano separati e alla fine vengono uniti da
# bench-compare.py.
#
# Uso:
#   ./scripts/bench-split.sh                  # tutti i gruppi
#   BENCH_GROUPS="1m 4m native" ./scripts/bench-split.sh
#
# Profilo sempre full (1M/4M inclusi); il reduced resta il profilo rapido di
# validazione. I timeout sono per-gruppo e includono build + boot + matrice.
set -euo pipefail
cd "$(dirname "$0")/.."

GROUPS_LIST="${BENCH_GROUPS:-small 4k 16k 64k 256k 1m 4m native}"
LOG_FILES=()

echo "[bench-split] gruppi: $GROUPS_LIST"
for group in $GROUPS_LIST; do
    case "$group" in
        small|4k|16k|64k|256k)
            size=512
            group_timeout=1800
            ;;
        1m)
            size=4096
            group_timeout=2400
            ;;
        4m)
            size=4096
            group_timeout=1800
            ;;
        native)
            size=4096
            group_timeout=1800
            ;;
        *)
            echo "[bench-split] gruppo non valido: $group" >&2
            exit 2
            ;;
    esac

    serial="/tmp/bench-${group}.log"
    out="/tmp/bench-${group}.out"
    echo "[bench-split] gruppo $group: size=${size}MiB timeout=${group_timeout}s (serial $serial)"
    export BENCH_GROUP="$group"

    set +e
    BENCH_PROFILE=full BENCH_GROUP="$group" RUNS=1 \
        ARCA_SIZE_MIB="$size" TIMEOUT_S="$group_timeout" BENCH_NO_COMPARE=1 \
        ./scripts/bench.sh >"$out" 2>&1
    code=$?
    set -e

    # bench.sh scrive sempre l'ultima run in /tmp/bench-run1.log; preservala
    # anche in caso di timeout/errore, per poterla analizzare o riprendere.
    cp /tmp/bench-run1.log "$serial" 2>/dev/null || true
    grep -E '^\[bench\]|^\[run1\]' "$out" || true

    if [ "$code" -ne 0 ] || ! grep -q '\[bench\] DONE ok=1' "$serial" 2>/dev/null; then
        echo "[bench-split] gruppo $group FALLITO (exit $code)" >&2
        tail -40 "$out" >&2 || true
        exit 1
    fi
    LOG_FILES+=("$serial")
done

echo "[bench-split] gruppi completati: ${#LOG_FILES[@]}"
if ! python3 scripts/bench-compare.py "${LOG_FILES[@]}"; then
    echo "[bench-split] comparator: nessuna coppia nei gruppi selezionati (ok per run parziali)" >&2
fi
