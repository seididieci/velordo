#!/usr/bin/env bash
# Test di rete (ADR-0069): **fuori dal gate** (dipende da `passt`, che e' una
# dipendenza host). Boot con virtio-net + passt (IPv6 reale, unprivileged),
# SLAAC/NDP e ping6 verso il router. Vittoria N1: `[net] echo reply`.
#
# Uso: ./scripts/run-net-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LOG="${NET_LOG:-/tmp/net-tests.log}"
# N5: la sequenza netudp→net3→net4→dns arriva oltre i 90 s su host lenti
# (il probe DNS parte dopo ~98 s); il default copre l'intera suite.
TIMEOUT="${NET_TIMEOUT:-180}"

echo "[net-tests] boot con virtio-net + passt (timeout ${TIMEOUT}s)"
NET=1 RUN_TESTS=1 RUN_DISPLAY=none timeout "$TIMEOUT" ./run.sh >"$LOG" 2>&1 || true

if rg -q '\[net\] echo reply' "$LOG" && rg -q '\[netudp\] risultato: PASS' "$LOG" && rg -q '\[net3\] risultato: PASS' "$LOG" && rg -q '\[net4\] risultato: PASS' "$LOG" && rg -q '\[dns\] risultato: PASS' "$LOG"; then
    echo "[net-tests] PASS: echo reply + UDP echo + TCP SYN + IPv4 SIIT + DNS probe"
    rg '\[net\]|\[netdev\]|\[netecho\]|\[netudp\]|\[net3\]|\[net4\]|\[siit\]|\[dns\]' "$LOG" | tail -30
    exit 0
fi

echo "[net-tests] FAIL: mancante echo reply, UDP echo, TCP SYN, IPv4 SIIT o DNS probe"
rg '\[net\]|\[netdev\]|\[netecho\]|\[netudp\]|\[net3\]|\[net4\]|\[siit\]|\[dns\]|\[init\] net' "$LOG" | tail -40 || true
exit 1
