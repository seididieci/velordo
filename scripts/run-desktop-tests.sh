#!/usr/bin/env bash
# Test delle **tile** e del DE (D8e, ADR-0092): fuori dal gate.
#
# Perche' fuori dal gate: il layout lo dispone solo un'immagine >= ThirdParty
# (il DE), e la suite del gate gira come `tst` (`Untrusted`) — stessa scelta
# del net gate (`posixsock` e' `usr`). Dentro il gate restano le prove che si
# possono fare da `Untrusted` (t73/t74/t75) e il boot a **una** tile: le 33
# check della shell sono la prova che una tile disegna il terminale giusto e
# che i tasti arrivano (il DE e' il focus owner).
#
# Due parti:
#  1. `tileprobe` (`usr/bin`, ThirdParty): due tile affiancate, il CRC della
#     prima **non** cambia mentre la seconda scrive, zoom/unzoom, focus,
#     `TILE_DROP` (prova il **server**: clip strutturale, un rect per tile);
#  2. i **keybinding del DE**, digitati dal monitor HMP (`sendkey`): Super+%
#     (split), Super+z (zoom), Super+h (focus), Super+Shift+q (chiudi). Il DE
#     stampa una riga per ogni comando: la prova che la tastiera e' sua.
#
# Uso: ./scripts/run-desktop-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LOG="${DESKTOP_LOG:-/tmp/desktop-tests.log}"
MON="${DESKTOP_MON:-/tmp/velordo-desktop-mon.sock}"
TIMEOUT="${DESKTOP_TIMEOUT:-300}"

# Un comando HMP sul monitor di QEMU (stesso schema di scripts/shell_harness.py).
send_mon() {
    python3 - "$MON" "$1" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.sendall((sys.argv[2] + "\n").encode())
time.sleep(0.05)
try:
    s.recv(4096)
except Exception:
    pass
s.close()
PY
    sleep 1
}

rm -f "$LOG" "$MON"
echo "[desktop] boot con desktop=1 (log $LOG)"
DESKTOP=1 RUN_TESTS=0 SERIAL="file:$LOG" MONITOR="unix:$MON,server=on,wait=off" \
    DISPLAY=none ./run.sh >/dev/null 2>&1 &
QEMU_PID=$!

# Il probe stampa il verdetto e il boot resta al prompt del login (dove il DE
# ha gia' la tastiera): si aspetta quello, non il timeout.
START=$SECONDS
while kill -0 "$QEMU_PID" 2>/dev/null; do
    if grep -aqF '[tileprobe] risultato:' "$LOG" 2>/dev/null; then
        sleep 1
        break
    fi
    if [ $((SECONDS - START)) -ge "$TIMEOUT" ]; then
        echo "[desktop] timeout ${TIMEOUT}s: probe mai concluso" >&2
        break
    fi
    sleep 1
done

# ── I keybinding del DE ─────────────────────────────────────────────
# `Super+%` = split verticale, `z` = zoom (on/off), `h` = focus a sinistra,
# `Super+Shift+q` = chiudi. Con una tile di partenza: split -> 2 tile, il focus
# va sulla nuova (1); `h` torna sulla 0; `Shift+q` chiude la 0 e resta 1.
send_mon "sendkey meta_l-shift-5"
send_mon "sendkey meta_l-z"
send_mon "sendkey meta_l-z"
send_mon "sendkey meta_l-h"
send_mon "sendkey meta_l-shift-q"
sleep 1

kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true

fail=0
for want in \
    '[tileprobe] tile di boot: PASS' \
    '[tileprobe] due tile affiancate: PASS' \
    '[tileprobe] la prima tile non cambia: PASS' \
    '[tileprobe] la seconda tile cambia: PASS' \
    '[tileprobe] zoom: una tile visibile: PASS' \
    '[tileprobe] unzoom: di nuovo due tile: PASS' \
    '[tileprobe] focus sulla seconda tile: PASS' \
    '[tileprobe] TILE_DROP torna a una tile: PASS' \
    '[tileprobe] risultato: PASS' \
    '[impluvium] tastiera sottoscritta' \
    '[impluvium] split Col -> tile=2 focus=1' \
    '[impluvium] zoom on (tile=2)' \
    '[impluvium] zoom off (tile=2)' \
    '[impluvium] focus 0 (h)'
do
    if ! grep -aqF -- "$want" "$LOG"; then
        echo "[desktop] manca: $want" >&2
        fail=1
    fi
done
# Con RUN_TESTS=0 non gira la suite: un FAIL o un PANIC sono veri.
if grep -aqE 'FAIL|PANIC' "$LOG"; then
    echo "[desktop] FAIL/PANIC nel log:" >&2
    grep -anE 'FAIL|PANIC' "$LOG" | head -10 || true
    fail=1
fi
if [ "$fail" -eq 0 ]; then
    echo "[desktop] PASS (tile: layout, clip, zoom, focus, CRC per tile; DE: Super+hjkl/%/z)"
else
    echo "[desktop] FAIL (log $LOG)" >&2
fi
exit "$fail"
