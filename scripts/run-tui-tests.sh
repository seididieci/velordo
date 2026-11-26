#!/usr/bin/env bash
# Test dell'applicazione a **schermo intero** (ADR-0098): fuori dal gate.
#
# Perche' fuori dal gate: `fullscreen` prende la tela della sessione (alt screen,
# cursore nascosto, cancellazioni): dentro il gate le 33 check della shell
# digiterebbero su un terminale occupato. La **base** (parser CSI, alt screen,
# resize) e' invece coperta dagli unit test host di `littera`, che girano in
# millisecondi e senza boot.
#
# Cosa prova, sul sistema vero:
#  1. l'app parte, prende la tela e riporta la misura (`[fullscreen] start WxH`);
#  2. i tasti arrivano (la stella si muove: la posizione finisce nell'header della
#     sessione, che il mirror seriale riporta);
#  3. `q` esce: la tela alternativa lascia il posto alla principale e la shell
#     **ridisegna il suo prompt** (prova che il testo di prima e' tornato);
#  4. il cursore torna visibile (`ESC[?25h`) e il mirror non contiene piu' il
#     frame dell'app.
#
# Uso: ./scripts/run-tui-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LOG="${TUI_LOG:-/tmp/tui-tests.log}"
MON="${TUI_MON:-/tmp/velordo-tui-mon.sock}"
TIMEOUT="${TUI_TIMEOUT:-240}"

# Un comando HMP sul monitor di QEMU (stesso schema di run-desktop-tests.sh).
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

wait_for() {
    local what="$1" limit="$2" start=$SECONDS
    while ! grep -aqF "$what" "$LOG" 2>/dev/null; do
        if [ $((SECONDS - start)) -ge "$limit" ]; then
            echo "[tui] timeout ${limit}s: manca '$what'" >&2
            return 1
        fi
        sleep 1
    done
}

rm -f "$LOG" "$MON"
echo "[tui] boot (log $LOG)"
RUN_TESTS=0 SERIAL="file:$LOG" MONITOR="unix:$MON,server=on,wait=off" \
    DISPLAY=none ./run.sh >/dev/null 2>&1 &
QEMU_PID=$!
trap 'kill "$QEMU_PID" 2>/dev/null || true' EXIT

# Al prompt del login (D8f: la shell parte solo dopo l'autenticazione).
wait_for '[gate] boot: login-ready' "$TIMEOUT"
send_text 'root'
send_text $'\n'
send_text 'root'
send_text $'\n'
wait_for '[shell] Velordo shell nativa' "$TIMEOUT"
sleep 1

# 1. L'app prende la tela.
send_text $'fullscreen\n'
if ! wait_for '[fullscreen] start' 60; then
    echo "[tui] FAIL: l'app non e' partita" >&2
    exit 1
fi
SIZE="$(grep -aF '[fullscreen] start' "$LOG" | tail -1 | sed 's/.*start //' || true)"
echo "[tui] tela: $SIZE"
sleep 1

# 2. I tasti arrivano (la stella si muove: lo dicono i marker sul seriale).
send_mon 'sendkey l'
send_mon 'sendkey l'
send_mon 'sendkey j'
sleep 1

# 2b. Toppa di pixel (ADR-0099): barra+linea disegnate **sotto** le celle.
send_mon 'sendkey p'
if ! wait_for '[fullscreen] patch bytes=' 20; then
    echo "[tui] FAIL: toppa non disegnata" >&2
    exit 1
fi
PATCH_LINE="$(grep -aF '[fullscreen] patch bytes=' "$LOG" | tail -1 || true)"
PATCH_BYTES="$(printf '%s' "$PATCH_LINE" | sed 's/.*bytes=//; s/ at=.*//')"
PATCH_CRC="$(printf '%s' "$PATCH_LINE" | sed 's/.*crc=//')"
echo "[tui] toppa: ${PATCH_BYTES:-0} byte, crc ${PATCH_CRC:-?}"

# 3. Resize: l'app chiede un modeset e deve **riadattarsi** (la misura non la
#    sceglie lei: arriva dal display, via DE, via hint).
BEFORE_RESIZE="$(grep -aF '[fullscreen] start' "$LOG" | tail -1 | grep -ao '[0-9]\+x[0-9]\+' || true)"
send_mon 'sendkey m'
if ! wait_for '[fullscreen] resized' 40; then
    echo "[tui] FAIL: nessun riadattamento dopo il modeset" >&2
    exit 1
fi
RESIZED="$(grep -aF '[fullscreen] resized' "$LOG" | tail -1 | sed 's/.*resized //')"
echo "[tui] tela dopo modeset: $BEFORE_RESIZE -> $RESIZED"

# 4. `q` esce: la tela torna alla principale (la shell ridisegna il prompt).
send_mon 'sendkey q'
if ! wait_for '[fullscreen] exit' 20; then
    echo "[tui] FAIL: uscita non segnalata" >&2
    exit 1
fi
sleep 2

START_POS="$(grep -aF '[fullscreen] start' "$LOG" | tail -1 | grep -ao 'star=([0-9]*,[0-9]*)' || true)"
END_POS="$(grep -aF '[fullscreen] exit' "$LOG" | tail -1 | grep -ao 'star=([0-9]*,[0-9]*)' || true)"
echo "[tui] stella: $START_POS -> $END_POS"

kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true
trap - EXIT

fail=0
check() {
    local what="$1" ok="$2"
    if [ "$ok" = "1" ]; then
        echo "[tui] PASS: $what"
    else
        echo "[tui] FAIL: $what" >&2
        fail=1
    fi
}
# `if`, non `cmd; check $?`: con `set -e` un grep che non trova abortisce.
if [ -n "$END_POS" ] && [ "$START_POS" != "$END_POS" ]; then ok=1; else ok=0; fi
check "i tasti muovono la stella ($START_POS -> $END_POS)" "$ok"
if [ -n "$PATCH_BYTES" ] && [ "$PATCH_BYTES" -gt 0 ] 2>/dev/null; then ok=1; else ok=0; fi
check "toppa di pixel disegnata ($PATCH_BYTES byte)" "$ok"
# Il CRC della riga prima/dopo: i pixel sono arrivati al framebuffer e sono
# **diversi** (una toppa che non disegna sarebbe indistinguibile).
A="${PATCH_CRC%%/*}"; B="${PATCH_CRC##*/}"
if [ -n "$A" ] && [ -n "$B" ] && [ "$A" != "$B" ]; then ok=1; else ok=0; fi
check "la toppa cambia i pixel della riga (crc $PATCH_CRC)" "$ok"
if [ -n "$RESIZED" ] && [ "$RESIZED" != "$BEFORE_RESIZE" ]; then ok=1; else ok=0; fi
check "riadattamento dopo il modeset ($BEFORE_RESIZE -> $RESIZED)" "$ok"
if grep -aqF '[fullscreen] exit' "$LOG"; then ok=1; else ok=0; fi
check "uscita dalla tela" "$ok"
# Dopo l'uscita la shell torna a scrivere: la principale era messa da parte,
# non distrutta.
if tail -c 200000 "$LOG" | grep -aqF 'fullscreen: uscita'; then ok=1; else ok=0; fi
check "la tela principale torna (uscita stampata)" "$ok"
if tail -c 200000 "$LOG" | grep -aqF ' > '; then ok=1; else ok=0; fi
check "il prompt della shell e' tornato" "$ok"

if grep -aqE 'FAIL|PANIC|#.* FAULT' "$LOG"; then
    echo "[tui] FAIL/PANIC nel log:" >&2
    grep -anE 'FAIL|PANIC|#.* FAULT' "$LOG" | head -5 || true
    fail=1
fi

if [ "$fail" -eq 0 ]; then
    echo "[tui] PASS (schermo intero: alt screen, posizionamento, tasti, ritorno)"
else
    echo "[tui] FAIL (log $LOG)" >&2
fi
exit "$fail"
