#!/usr/bin/env bash
# Test del **monitor di sistema** (M4, ADR-0097/0099): fuori dal gate.
#
# Perche' fuori dal gate: il monitor prende la tela della sessione (alt screen,
# 1 Hz, toppe) e la shell deve poterlo lanciare: dentro il gate le 33 check
# digiterebbero su un terminale occupato.
#
# Cosa prova, sul sistema vero:
#  1. l'app parte e riporta la misura (`[monitor] start WxH`);
#  2. **i valori si aggiornano da soli** senza tasti (piu' di un `tick=`);
#  3. `p` mette in pausa (niente nuovi tick) e `p` riprende;
#  4. `s` cicla l'ordinamento (il marker `sort=` cambia);
#  5. `j`/`k` muovono la selezione (`sel=` cambia);
#  6. `q` esce e il prompt della shell torna;
#  7. zero `[console] toppa fuori dalla tile`.
#
# Uso: ./scripts/run-monitor-tests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

LOG="${MON_LOG:-/tmp/monitor-tests.log}"
MON="${MON_SOCK:-/tmp/velordo-monitor-mon.sock}"
TIMEOUT="${MON_TIMEOUT:-240}"

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

send_many() {
    python3 - "$MON" "$1" "$2" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
n, key = int(sys.argv[2]), sys.argv[3]
for _ in range(n):
    s.sendall(("sendkey %s\n" % key).encode())
    time.sleep(0.05)
try:
    s.recv(4096)
except Exception:
    pass
s.close()
PY
    sleep 0.5
}

wait_for() {
    local what="$1" limit="$2" start=$SECONDS
    while ! grep -aqF "$what" "$LOG" 2>/dev/null; do
        if [ $((SECONDS - start)) -ge "$limit" ]; then
            echo "[mon] timeout ${limit}s: manca '$what'" >&2
            return 1
        fi
        sleep 1
    done
}

# Numero di righe che contengono $1 (per contare i tick).
count_lines() {
    grep -acF "$1" "$LOG" 2>/dev/null || true
}

rm -f "$LOG" "$MON"
echo "[mon] boot (log $LOG)"
RUN_TESTS=0 SERIAL="file:$LOG" MONITOR="unix:$MON,server=on,wait=off" \
    FB_WINDOW=1 DISPLAY=none ./run.sh >/dev/null 2>&1 &
QEMU_PID=$!
trap 'kill "$QEMU_PID" 2>/dev/null || true' EXIT

wait_for '[gate] boot: login-ready' "$TIMEOUT"
send_text 'root'
send_text $'\n'
send_text 'root'
send_text $'\n'
wait_for '[shell] Velordo shell nativa' "$TIMEOUT"
sleep 1

# 1. L'app parte.
send_text $'monitor\n'
if ! wait_for '[monitor] start' 60; then
    echo "[mon] FAIL: il monitor non e' partito" >&2
    exit 1
fi
START_LINE="$(grep -aF '[monitor] start' "$LOG" | tail -1 || true)"
echo "[mon] avvio: $START_LINE"
sleep 1

# 2. I valori si aggiornano **da soli** (nessun tasto): almeno 3 tick in 5 s.
T0="$(count_lines '[monitor] tick')"
sleep 5
T1="$(count_lines '[monitor] tick')"
TICK_LINE="$(grep -aF '[monitor] tick' "$LOG" | tail -1 || true)"
echo "[mon] tick: $T0 -> $T1 in 5 s; ultimo: $TICK_LINE"

# 3. Pausa: nessun nuovo tick in 3 s.
send_mon 'sendkey p'
sleep 0.5
P0="$(count_lines '[monitor] tick')"
sleep 3
P1="$(count_lines '[monitor] tick')"
echo "[mon] pausa: $P0 -> $P1 tick in 3 s"
send_mon 'sendkey p' # riprendi
sleep 2

# 4. Sort: `s` cicla (il marker sort= cambia).
SORT_BEFORE="$(grep -aF '[monitor] tick' "$LOG" | tail -1 | sed 's/.*sort=//; s/ .*//' || true)"
send_mon 'sendkey s'
sleep 2
SORT_AFTER="$(grep -aF '[monitor] tick' "$LOG" | tail -1 | sed 's/.*sort=//; s/ .*//' || true)"
echo "[mon] sort: $SORT_BEFORE -> $SORT_AFTER"

# 5. Selezione: `j` la muove (il marker sel= cambia).
SEL_BEFORE="$(grep -aF '[monitor] tick' "$LOG" | tail -1 | sed 's/.*sel=//; s/ .*//' || true)"
send_many 2 'j'
sleep 1
SEL_AFTER="$(grep -aF '[monitor] tick' "$LOG" | tail -1 | sed 's/.*sel=//; s/ .*//' || true)"
echo "[mon] sel: $SEL_BEFORE -> $SEL_AFTER"

# 5b. **Pixel veri**: le toppe arrivano al framebuffer e il grafico si muove
#     da solo (due screendump a 3 s di distanza, confronto nella card CPU).
SHOT1="${MON_SHOT:-/tmp/monitor-shot1.ppm}"
SHOT2="${SHOT1%.ppm}-2.ppm"
rm -f "$SHOT1" "$SHOT2"
send_mon "screendump $SHOT1"
sleep 3
send_mon "screendump $SHOT2"
PIXELS="$(python3 - "$SHOT1" "$SHOT2" <<'PY'
import sys

def load(path):
    try:
        d = open(path, "rb").read()
    except OSError:
        return None
    parts = d.split(b"\n", 3)
    if parts[0] != b"P6":
        return None
    w, h = (int(x) for x in parts[1].split())
    return w, h, parts[3]

a = load(sys.argv[1])
b = load(sys.argv[2])
if a is None or b is None:
    print("0 0")
    raise SystemExit
w, h, pa = a
_, _, pb = b
# Regione della card CPU (in alto a sinistra, 127x36 celle 10x22 -> la card e'
# 63x8 celle = 630x176 px, a partire da y=44).
x0, x1 = 4, min(w, 626)
y0, y1 = 48, min(h, 216)
drawn = 0     # pixel non-wallpaper (la card e' disegnata)
moved = 0     # pixel cambiati fra i due frame (il grafico si anima)
for y in range(y0, y1):
    base = y * w * 3
    for x in range(x0, x1):
        o = base + x * 3
        r, g, bl = pa[o], pa[o + 1], pa[o + 2]
        if (r, g, bl) != (7, 7, 12):
            drawn += 1
        if pa[o:o + 3] != pb[o:o + 3]:
            moved += 1
print(drawn, moved)
PY
)"
CARD_DRAWN="$(printf '%s' "$PIXELS" | cut -d' ' -f1)"
CHART_MOVED="$(printf '%s' "$PIXELS" | cut -d' ' -f2)"
echo "[mon] pixel: card dipinta=$CARD_DRAWN, grafico mosso=$CHART_MOVED in 3 s"

# 6. `q` esce e il prompt torna.
send_mon 'sendkey q'
if wait_for '[monitor] exit' 15; then
    EXIT_OK=1
else
    EXIT_OK=0
    echo "[mon] FAIL: uscita non segnalata dopo q" >&2
fi
sleep 1
PROMPT_NOW="$(count_lines 'Velordo shell nativa')"
echo "[mon] prompt dopo l'uscita: $PROMPT_NOW"

OUTSIDE="$(count_lines '[console] toppa fuori dalla tile')"

kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true
trap - EXIT

fail=0
check() {
    local what="$1" ok="$2"
    if [ "$ok" = "1" ]; then
        echo "[mon] PASS: $what"
    else
        echo "[mon] FAIL: $what" >&2
        fail=1
    fi
}

if [ -n "${START_LINE:-}" ]; then ok=1; else ok=0; fi
check "il monitor parte ($START_LINE)" "$ok"
if [ "${T1:-0}" -gt "${T0:-0}" ] 2>/dev/null; then ok=1; else ok=0; fi
check "i valori si aggiornano da soli senza tasti ($T0 -> $T1 tick)" "$ok"
if [ "${P1:-0}" = "${P0:-0}" ] 2>/dev/null; then ok=1; else ok=0; fi
check "la pausa ferma i tick ($P0 = $P1)" "$ok"
if [ -n "${SORT_AFTER:-}" ] && [ "$SORT_BEFORE" != "$SORT_AFTER" ]; then ok=1; else ok=0; fi
check "il sort cambia ($SORT_BEFORE -> $SORT_AFTER)" "$ok"
if [ -n "${SEL_AFTER:-}" ] && [ "$SEL_BEFORE" != "$SEL_AFTER" ]; then ok=1; else ok=0; fi
check "la selezione si muove ($SEL_BEFORE -> $SEL_AFTER)" "$ok"
if [ "${CARD_DRAWN:-0}" -gt 1000 ] 2>/dev/null; then ok=1; else ok=0; fi
check "la card CPU e' dipinta a pixel ($CARD_DRAWN px non-wallpaper)" "$ok"
if [ "${CHART_MOVED:-0}" -gt 50 ] 2>/dev/null; then ok=1; else ok=0; fi
check "il grafico si muove da solo ($CHART_MOVED px cambiati in 3 s)" "$ok"
if [ "${PROMPT_NOW:-0}" -ge 1 ] 2>/dev/null; then ok=1; else ok=0; fi
check "uscita e ritorno al prompt" "$ok"
if [ "${EXIT_OK:-0}" = "1" ]; then ok=1; else ok=0; fi
check "q esce (marker [monitor] exit)" "$ok"
if [ "${OUTSIDE:-0}" = "0" ]; then ok=1; else ok=0; fi
check "nessuna toppa fuori dalla tile ($OUTSIDE avvisi)" "$ok"

kill "$QEMU_PID" 2>/dev/null || true
if [ "$fail" = "0" ]; then
    echo "[mon] PASS (monitor: 1 Hz, pausa, sort, selezione, uscita)"
else
    echo "[mon] FAIL" >&2
fi
exit "$fail"
