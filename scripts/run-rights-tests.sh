#!/usr/bin/env bash
# Test dei **diritti ereditari** (S3.4, ADR-0093): fuori dal gate.
#
# Perche' fuori dal gate: il caso positivo richiede un processo `ThirdParty`
# (la shell nativa lancia `floorprobe` con floor `ThirdParty`), mentre la suite
# del gate gira come `tst` (`Untrusted`) e non puo' spawnare `ThirdParty`
# (no elevazione: e' proprio la regola che questa fase introduce).
#
# Il probe (`usr/bin/floorprobe.bin`, firmato third-party) prova:
#  1. un'immagine `Untrusted` viene rifiutata dal kernel (floor);
#  2. un'immagine `ThirdParty` viene ammessa;
#  3. `exec` di un `Untrusted` e' rifiutato.
#
# Uso: ./scripts/run-rights-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LOG="${RIGHTS_LOG:-/tmp/rights-tests.log}"
MON="${RIGHTS_MON:-/tmp/velordo-rights-mon.sock}"
TIMEOUT="${RIGHTS_TIMEOUT:-300}"

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

send_text() {
    python3 - "$MON" "$1" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
for ch in sys.argv[2]:
    key = "ret" if ch == "\n" else ch
    s.sendall(("sendkey %s\n" % key).encode())
    time.sleep(0.05)
try:
    s.recv(4096)
except Exception:
    pass
s.close()
PY
    sleep 0.3
}

rm -f "$LOG" "$MON"
echo "[rights] boot (log $LOG)"
RUN_TESTS=0 SERIAL="file:$LOG" MONITOR="unix:$MON,server=on,wait=off" \
    DISPLAY=none ./run.sh >/dev/null 2>&1 &
QEMU_PID=$!

# Login (D8f: il DE e la shell partono dopo l'autenticazione).
START=$SECONDS
while ! grep -aqF 'Nome utente:' "$LOG" 2>/dev/null; do
    if [ $((SECONDS - START)) -ge "$TIMEOUT" ]; then
        echo "[rights] timeout ${TIMEOUT}s: prompt del login mai comparso" >&2
        break
    fi
    sleep 1
done
send_text "root"
send_text $'\n'
send_text "root"
send_text $'\n'

# Shell pronta (il DE ha preso la tastiera): digita il probe.
START=$SECONDS
while ! grep -aqF '[shell] Velordo shell nativa' "$LOG" 2>/dev/null; do
    if [ $((SECONDS - START)) -ge "$TIMEOUT" ]; then
        echo "[rights] timeout ${TIMEOUT}s: shell mai partita" >&2
        break
    fi
    sleep 1
done
sleep 1
send_text "floorprobe"
send_text $'\n'

START=$SECONDS
while kill -0 "$QEMU_PID" 2>/dev/null; do
    if grep -aqF '[floorprobe] risultato:' "$LOG" 2>/dev/null; then
        sleep 1
        break
    fi
    if [ $((SECONDS - START)) -ge "$TIMEOUT" ]; then
        echo "[rights] timeout ${TIMEOUT}s: probe mai concluso" >&2
        break
    fi
    sleep 1
done

kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true

fail=0
for want in \
    '[floorprobe] risultato: PASS' \
    '[floorprobe] partenza (floor ThirdParty)'
do
    if ! grep -aqF -- "$want" "$LOG"; then
        echo "[rights] manca: $want" >&2
        fail=1
    fi
done
if grep -aqE 'FAIL|PANIC' "$LOG"; then
    echo "[rights] FAIL/PANIC nel log:" >&2
    grep -anE 'FAIL|PANIC' "$LOG" | head -10 || true
    fail=1
fi
if [ "$fail" -eq 0 ]; then
    echo "[rights] PASS (floor ThirdParty: Untrusted rifiutato, ThirdParty ammesso, exec negato)"
else
    echo "[rights] FAIL (log $LOG)" >&2
fi
exit "$fail"
