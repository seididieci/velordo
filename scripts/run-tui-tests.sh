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
# `FB_WINDOW=1`: senza la VGA il terminale e' la superficie virtio-gpu, e lo
# **screendump** del monitor prende quella. Con la VGA presente il dump prende
# la VGA (l'altra console, "morta" nel path pixel) e i check sui pixel di questa
# suite guarderebbero lo schermo sbagliato.
RUN_TESTS=0 SERIAL="file:$LOG" MONITOR="unix:$MON,server=on,wait=off" \
    FB_WINDOW=1 DISPLAY=none ./run.sh >/dev/null 2>&1 &
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

# 1b. Il primo frame deve essere **a video senza premere tasti**: il feed di
#     una tela (`SESS_WRITE.w1 = 1`) non e' "bulk" (output di un comando), e il
#     server non deve differirne il render. Differito, lo schermo resta vuoto
#     fino alla write successiva: misurato con `fullscreen` (bug pagato).
SHOT="${TUI_SHOT:-/tmp/tui-first-frame.ppm}"
rm -f "$SHOT"
send_mon "screendump $SHOT"
sleep 2
# Il frame disegnato lascia pixel chiari nella fascia in alto a sinistra
# (bordo + titolo); senza frame quella fascia e' wallpaper scuro (misurato: 0).
# Cercare un colore esatto non funziona: i glifi sono antialiasati sul fondo.
TITLE_PX="$(python3 - "$SHOT" <<'PY'
import sys

try:
    d = open(sys.argv[1], "rb").read()
except OSError:
    print(0)
    raise SystemExit
parts = d.split(b"\n", 3)
if parts[0] != b"P6":
    print(0)
    raise SystemExit
w, h = (int(x) for x in parts[1].split())
px = parts[3]
# Luminanza (Rec.601) dei pixel nella fascia; soglia bassa: il wallpaper in
# alto a sinistra e' quasi nero (11,13,20).
n = 0
for y in range(5, min(h, 70)):
    for x in range(0, min(w, 640)):
        o = (y * w + x) * 3
        r, g, b = px[o], px[o + 1], px[o + 2]
        if (r * 299 + g * 587 + b * 114) // 1000 >= 120:
            n += 1
print(n)
PY
)"
echo "[tui] primo frame a video: ${TITLE_PX:-0} pixel chiari in alto a sinistra (senza tasti)"

# 2. I tasti arrivano (la stella si muove: lo dicono i marker sul seriale).
send_mon 'sendkey l'
send_mon 'sendkey l'
send_mon 'sendkey j'
sleep 1
# 2a. Il **costo di un tasto** (ADR-0098, addendum "il frame e' l'unita' di
#     disegno"): un frame piccolo in **una** feed. Prima l'app faceva un IPC per
#     cella, cioe' un render+flush per cella (~660 per un tasto): il frame si
#     vedeva arrivare a pezzi, partendo dallo schermo vuoto. Era il lampeggio.
FRAMES="$(grep -aF '[fullscreen] frame bytes=' "$LOG" || true)"
FIRST_FRAME="$(printf '%s\n' "$FRAMES" | head -1)"
LAST_FRAME="$(printf '%s\n' "$FRAMES" | tail -1)"
FIRST_BYTES="$(printf '%s' "$FIRST_FRAME" | sed 's/.*bytes=//; s/ .*//')"
FRAME_BYTES="$(printf '%s' "$LAST_FRAME" | sed 's/.*bytes=//; s/ .*//')"
FRAME_CHUNKS="$(printf '%s' "$LAST_FRAME" | sed 's/.*chunks=//')"
echo "[tui] frame: intero=${FIRST_BYTES:-?} byte, dopo i tasti=${FRAME_BYTES:-?} byte in ${FRAME_CHUNKS:-?} chunk"

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

# 5. La **Graphic TUI** (ADR-0099): testo + toppe di pixel, e le toppe vivono
#    quanto il loro testo. L'app verifica da sola e scrive i marker sul seriale.
sleep 1
send_text $'gtui\n'
if ! wait_for '[gtui] log' 60; then
    echo "[tui] FAIL: gtui non e' partito" >&2
    exit 1
fi
GTUI_LOG="$(grep -aF '[gtui] log' "$LOG" | tail -1 || true)"
# Scroll dentro la cronologia: le toppe restano.
if ! wait_for '[gtui] dopo 40 righe' 60; then
    echo "[tui] FAIL: gtui non ha finito lo scroll breve" >&2
    exit 1
fi
GTUI_KEEP="$(grep -aF '[gtui] dopo 40 righe' "$LOG" | tail -1 | sed 's/.*patches=//' || true)"
# Scroll oltre la cronologia (1100 righe su 1000): le righe escono, i pixel con loro.
if ! wait_for '[gtui] oltre la cronologia' 120; then
    echo "[tui] FAIL: gtui non ha finito lo scroll lungo" >&2
    exit 1
fi
GTUI_GONE="$(grep -aF '[gtui] oltre la cronologia' "$LOG" | tail -1 | sed 's/.*patches=//' || true)"
GTUI_K0="$(printf '%s' "$GTUI_LOG" | sed 's/.*patches=//; s/ .*//')"
GTUI_CRC="$(printf '%s' "$GTUI_LOG" | sed 's/.*crc=//')"
echo "[tui] gtui: patches k0=${GTUI_K0:-?} dopo40=${GTUI_KEEP:-?} oltre=${GTUI_GONE:-?} crc=${GTUI_CRC:-?}"

# 5b. **Split mentre gtui gira** (lo scenario vero): la tile si stringe, la
#     larghezza cambia -> il modello scarta le toppe (il re-wrap puo'
#     spezzare una riga) -> l'app viene **svegliata** dall'hint e ridisegna
#     alla misura nuova (ADR-0098/0099). Se non lo facesse resterebbe un log
#     senza grafica: e' il difetto visto a schermo.
send_mon 'sendkey meta_l-shift-5'
if ! wait_for '[impluvium] split' 40; then
    echo "[tui] FAIL: split non avvenuto" >&2
    exit 1
fi
if ! wait_for '[gtui] resize' 40; then
    echo "[tui] FAIL: gtui non si e' riadattato allo split" >&2
    exit 1
fi
GTUI_RESIZE_LINE="$(grep -aF '[gtui] resize' "$LOG" | tail -1 || true)"
GTUI_RESIZE_SIZE="$(printf '%s' "$GTUI_RESIZE_LINE" | sed 's/.*resize //; s/ .*//')"
GTUI_RESIZE="$(printf '%s' "$GTUI_RESIZE_LINE" | sed 's/.*patches=//; s/ .*//')"
echo "[tui] gtui dopo lo split: $GTUI_RESIZE_SIZE (patches=${GTUI_RESIZE:-?})"
# Torna sulla tile di gtui (dopo lo split il focus e' sulla nuova) e chiudi.
send_mon 'sendkey meta_l-h'
sleep 1
send_mon 'sendkey q'
wait_for '[gtui] exit' 20 || true

# 6. **Seconda tile**: la grafica deve uscire anche li'. Il bug del primo giro
#    era esattamente qui: coordinate schermo su una vista locale -> la toppa
#    cadeva fuori e il clip la buttava via in silenzio.
send_mon 'sendkey meta_l-l'
sleep 1
GTUI_BEFORE="$(grep -acF '[gtui] log' "$LOG" || true)"
send_text $'gtui\n'
# Il secondo giro: la tile ha un'altra sessione, quindi altri marker.
START2=$SECONDS
while [ "$(grep -acF '[gtui] log' "$LOG" || true)" -le "${GTUI_BEFORE:-0}" ]; do
    if [ $((SECONDS - START2)) -ge 60 ]; then
        echo "[tui] FAIL: gtui non e' partito nella seconda tile" >&2
        exit 1
    fi
    sleep 1
done
# Aspetta che finisca (scroll lungo) e chiudi.
START2=$SECONDS
while [ "$(grep -acF '[gtui] oltre la cronologia' "$LOG" || true)" -lt 2 ]; do
    if [ $((SECONDS - START2)) -ge 120 ]; then
        echo "[tui] FAIL: gtui della seconda tile non ha finito" >&2
        exit 1
    fi
    sleep 1
done
GTUI2="$(grep -aF '[gtui] log' "$LOG" | tail -1 || true)"
echo "[tui] seconda tile: $GTUI2"
send_mon 'sendkey q'
wait_for '[gtui] exit' 20 || true
sleep 1
OUTSIDE="$(grep -acF '[console] toppa fuori dalla tile' "$LOG" || true)"

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
if [ -n "$TITLE_PX" ] && [ "$TITLE_PX" -gt 300 ] 2>/dev/null; then ok=1; else ok=0; fi
check "il primo frame e' a video senza tasti ($TITLE_PX pixel chiari in alto a sinistra)" "$ok"
if [ -n "$FRAME_BYTES" ] && [ "$FRAME_BYTES" -lt 200 ] 2>/dev/null && [ "$FRAME_CHUNKS" = "1" ]; then ok=1; else ok=0; fi
check "un tasto = un frame piccolo in una feed ($FRAME_BYTES byte, $FRAME_CHUNKS chunk)" "$ok"
if [ -n "$FIRST_BYTES" ] && [ -n "$FRAME_BYTES" ] && [ "$FIRST_BYTES" -gt "$FRAME_BYTES" ] 2>/dev/null; then ok=1; else ok=0; fi
check "il primo frame e' intero, i successivi differenziali ($FIRST_BYTES -> $FRAME_BYTES byte)" "$ok"
if [ -n "$PATCH_BYTES" ] && [ "$PATCH_BYTES" -gt 0 ] 2>/dev/null; then ok=1; else ok=0; fi
check "toppa di pixel disegnata ($PATCH_BYTES byte)" "$ok"
# Il CRC della riga prima/dopo: i pixel sono arrivati al framebuffer e sono
# **diversi** (una toppa che non disegna sarebbe indistinguibile).
A="${PATCH_CRC%%/*}"; B="${PATCH_CRC##*/}"
if [ -n "$A" ] && [ -n "$B" ] && [ "$A" != "$B" ]; then ok=1; else ok=0; fi
check "la toppa cambia i pixel della riga (crc $PATCH_CRC)" "$ok"
if [ -n "$RESIZED" ] && [ "$RESIZED" != "$BEFORE_RESIZE" ]; then ok=1; else ok=0; fi
check "riadattamento dopo il modeset ($BEFORE_RESIZE -> $RESIZED)" "$ok"
# GTUI: le toppe sono state disegnate, restano nella cronologia e poi se ne
# vanno **con le loro righe** (nessuna eviction separata, ADR-0099).
if [ -n "$GTUI_K0" ] && [ "$GTUI_K0" -gt 0 ] 2>/dev/null; then ok=1; else ok=0; fi
check "gtui: toppe disegnate ($GTUI_K0 byte)" "$ok"
if [ "$GTUI_CRC" != "0" ] && [ -n "$GTUI_CRC" ]; then ok=1; else ok=0; fi
check "gtui: i pixel arrivano al framebuffer (crc $GTUI_CRC)" "$ok"
if [ -n "$GTUI_KEEP" ] && [ "$GTUI_KEEP" -gt 0 ] 2>/dev/null; then ok=1; else ok=0; fi
check "gtui: le toppe restano nella cronologia ($GTUI_KEEP byte)" "$ok"
if [ "$GTUI_GONE" = "0" ]; then ok=1; else ok=0; fi
check "gtui: le toppe muoiono con le loro righe ($GTUI_GONE byte)" "$ok"
# Seconda tile: la grafica si vede anche li' (stessi byte) e **nessuna** toppa
# e' mai caduta fuori dalla vista (la guardia del traduttore di coordinate).
if [ -n "$GTUI_RESIZE" ] && [ "$GTUI_RESIZE" -gt 0 ] 2>/dev/null; then ok=1; else ok=0; fi
check "dopo lo split gtui ridisegna la grafica ($GTUI_RESIZE_SIZE, ${GTUI_RESIZE:-?} byte)" "$ok"
GTUI2_K="$(printf '%s' "${GTUI2:-}" | sed 's/.*patches=//; s/ .*//')"
if [ -n "$GTUI2_K" ] && [ "$GTUI2_K" -gt 0 ] 2>/dev/null; then ok=1; else ok=0; fi
check "gtui nella seconda tile: grafica disegnata ($GTUI2_K byte)" "$ok"
if [ "${OUTSIDE:-0}" = "0" ]; then ok=1; else ok=0; fi
check "nessuna toppa fuori dalla tile ($OUTSIDE avvisi)" "$ok"
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
    echo "[tui] PASS (schermo intero: alt screen, tasti, resize; toppe: pixel, cronologia, morte con la riga)"
else
    echo "[tui] FAIL (log $LOG)" >&2
fi
exit "$fail"
