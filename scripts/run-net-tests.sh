#!/usr/bin/env bash
# Test di rete (ADR-0069): **fuori dal gate** (dipende da `passt`, che e' una
# dipendenza host). Boot con virtio-net + passt (IPv6 reale, unprivileged),
# SLAAC/NDP e ping6 verso il router. Vittoria N1: `[net] echo reply`.
#
# Uso: ./scripts/run-net-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LOG="${NET_LOG:-/tmp/net-tests.log}"
TIMEOUT="${NET_TIMEOUT:-90}"

echo "[net-tests] boot con virtio-net + passt (timeout ${TIMEOUT}s)"
NET=1 RUN_DISPLAY=none timeout "$TIMEOUT" ./run.sh >"$LOG" 2>&1 || true

if rg -q '\[net\] echo reply' "$LOG"; then
    echo "[net-tests] PASS: echo reply"
    rg '\[net\]|\[netdev\]' "$LOG" | tail -20
    exit 0
fi

echo "[net-tests] FAIL: nessun echo reply"
rg '\[net\]|\[netdev\]|\[init\] net' "$LOG" | tail -40 || true
exit 1
