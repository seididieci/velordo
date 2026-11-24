#!/usr/bin/env bash
# Gate di regressione Velordo: boot con la test suite completa **e** la shell
# nativa, nello stesso boot (ADR-0081).
#
# `./run.sh` di default e' produzione (init salta i test, shell subito).
# Questo wrapper imposta RUN_TESTS=1 (init compilato con --no-default-features
# ed esegue usertestfs/usertestfat/usertests/testsarca in sequenza prima della
# shell) e mette il seriale su file + il monitor su unix socket: cosi' la
# stessa QEMU e' pilotabile via `sendkey` e, appena il login chiede le
# credenziali, il gate si attacca (`--attach-pid`) e fa girare i 33 check
# della shell nativa (`scripts/test-shell-native.py`, gli stessi di
# `scripts/test-shell-all.sh native`). Righe attese + zero FAIL/PANIC/FAULT:
#   [testfs] PASS 5/5
#   [testsarca] PASS 64/64
#   [posixtests] PASS 4/4
#   [identistest] PASS 11/11
#   [usertests] PASS 70/70
#   [threadtest] PASS 12/12
#   [shell-native] PASS 33/33
# (Fase 30.1: t65 shm; ADR-0078 fase 2: t66 firma; ADR-0079 fase 3: t67 classe;
#  ADR-0079 fase 4: t68 porte I/O dal manifest; ADR-0080 fase 6: t70 consenso
#  per immagine + broker; ADR-0081: fase shell nativa nel gate)
# N7.0 (ADR-0074): nessuna riga nuova — il client di rete vive in `civis::net`
# e il formato degli slot in `libs/net-ep`; i percorsi di rete si provano nei
# run dedicati (scripts/run-net-tests.sh, scripts/run-tls-tests.sh).
# D5/ADR-0062: dopo la suite il boot finisce al **prompt del login** (il
# login e' il gate: la shell parte solo dopo l'autenticazione). Il gate non
# aspetta piu' il timeout esterno: il prompt e' il segnale di fine suite, e
# da li' si fa login e si provano i programmi da `usr` (il bug della shell
# non verificata non era coperto da nessun gate: ADR-0080 fase 6).
# L0b (virtio-gpu + MSI-X, ADR-0057): il boot verifica i marker
# `[gpu] virtio-gpu ...` / `[gpu] display info: 1280x800 enabled=1`.
# D3 (console virtuale, ADR-0060): i marker sono
# `[console] surface: pixel 1280x800 grid=128x36` e `[gpu] scanout: ...`
# (con VIRTIO_GPU=0: `[console] surface: vga-text 80x25`).
#
# Valvole: GATE_SHELL=0 salta la fase shell (iterazione veloce sulla suite);
# GATE_TIMEOUT (default 600) e' l'attesa del prompt del login;
# SHELL_TIMEOUT (default 900) limita la fase shell.
set -euo pipefail
cd "$(dirname "$0")"

LOG="${GATE_LOG:-/tmp/velordo-gate.log}"
MON="${GATE_MON:-/tmp/velordo-gate.sock}"
SHELL_LOG="${SHELL_LOG:-/tmp/velordo-gate-shell.log}"
TIMEOUT="${GATE_TIMEOUT:-600}"
SHELL_TIMEOUT="${SHELL_TIMEOUT:-900}"

QEMU_PID=""
TAIL_PID=""
cleanup() {
    [ -n "$TAIL_PID" ] && kill "$TAIL_PID" 2>/dev/null || true
    [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null || true
}
trap cleanup EXIT

rm -f "$LOG" "$MON"
: > "$LOG"
# Output live: il seriale non e' piu' stdio, quindi lo seguiamo dal file.
tail -n +1 -F "$LOG" &
TAIL_PID=$!

echo "[gate] boot: suite + shell nativa (log $LOG)"
# Diagnostica scheduler/IRQ attiva nei run di test (feature `sched_debug`).
# ARCA_IMG=1: quarto drive ArcaFS (partizione GPT su arca-gpt.img, sdd/sdd1)
# per testsarca 12-13 (il terzo, arca.img=root sdc, e' sempre attaccato da
# run.sh; 22-40 girano sul root vivo in bucket isolati, 41-50 su mount root,
# 51-54 quota A3 in bucket q5*). SERIAL/MONITOR: il boot diventa pilotabile.
RUN_TESTS=1 SCHED_DEBUG=1 ARCA_IMG=1 \
    SERIAL="file:$LOG" MONITOR="unix:$MON,server=on,wait=off" \
    ./run.sh "$@" &
QEMU_PID=$!

# Il prompt del login compare SOLO dopo la suite (init spawa il login per
# ultimo): e' il segnale di fine boot, e sostituisce il vecchio timeout
# esterno che chiudeva il gate.
boot_ok=1
start=$SECONDS
while ! rg -qF '[login] Nome utente:' "$LOG" 2>/dev/null; do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "[gate] QEMU terminato prima del prompt del login" >&2
        boot_ok=0
        break
    fi
    if [ $((SECONDS - start)) -ge "$TIMEOUT" ]; then
        echo "[gate] timeout ${TIMEOUT}s: prompt del login mai comparso" >&2
        boot_ok=0
        break
    fi
    sleep 1
done

fail=0
[ "$boot_ok" -eq 1 ] || fail=1

# Contratto della suite (le stesse righe di prima).
if [ "$fail" -eq 0 ]; then
    for want in \
        '[testfs] PASS 5/5' \
        '[testsarca] PASS 64/64' \
        '[posixtests] PASS 4/4' \
        '[identistest] PASS 11/11' \
        '[usertests] PASS 70/70' \
        '[threadtest] PASS 12/12'
    do
        if ! rg -qF -- "$want" "$LOG"; then
            echo "[gate] manca: $want" >&2
            fail=1
        fi
    done
    if rg -q 'FAIL|PANIC|#.* FAULT' "$LOG"; then
        echo "[gate] FAIL/PANIC/FAULT durante la suite:" >&2
        rg -n 'FAIL|PANIC|#.* FAULT' "$LOG" | head -5 || true
        fail=1
    fi
    # `munmap` con geometria non di pagina (addr disallineato o len non
    # multiplo): il kernel lo logga loud perche' il chiamante di solito
    # ignora il -1 e il ref shm resta appeso (il leak che ha rotto tlsprobe).
    # La forma malformata non e' mai legittima: zero righe.
    if rg -q '\[munmap\]' "$LOG"; then
        echo "[gate] FAIL: munmap con geometria non di pagina (leak shm):" >&2
        rg -n '\[munmap\]' "$LOG" | head -5 || true
        fail=1
    fi
fi

# Fase shell nativa: stessa QEMU, login gia' al prompt. Gli stessi 33 check
# di `scripts/test-shell-all.sh native`, ma in attach (niente secondo boot,
# niente secondo seed: il volume e' quello su cui ha girato la suite).
if [ "$fail" -eq 0 ] && [ "${GATE_SHELL:-1}" = "1" ]; then
    echo "[gate] shell nativa (33 check, --attach sul boot della suite)"
    if ! SHELL_KVM="${KVM:-0}" timeout "$SHELL_TIMEOUT" \
            python3 scripts/test-shell-native.py \
                --attach-pid "$QEMU_PID" --mon "$MON" --serial "$LOG" \
                --no-prep 2>&1 | tee "$SHELL_LOG"
    then
        echo "[gate] fase shell nativa: FAIL (log $SHELL_LOG)" >&2
        fail=1
    fi
    # Il log e' lo stesso: la shell non deve sporcarlo (panic o FAIL in coda).
    if rg -q 'FAIL|PANIC|#.* FAULT|\[munmap\]' "$LOG"; then
        echo "[gate] FAIL/PANIC/FAULT/munmap dopo la fase shell:" >&2
        rg -n 'FAIL|PANIC|#.* FAULT|\[munmap\]' "$LOG" | head -5 || true
        fail=1
    fi
fi

if [ "$fail" -eq 0 ]; then
    echo "[gate] PASS (6 righe di suite + shell nativa, zero FAIL/PANIC/FAULT)"
else
    echo "[gate] FAIL (log $LOG, shell $SHELL_LOG)" >&2
fi
exit "$fail"
