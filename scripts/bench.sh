#!/usr/bin/env bash
# Bench throughput client→FS su KVM (Fase 23; A/B ArcaFS/FAT + nativo).
#
# Esegue N boot con userbench (RUN_BENCH=1: init lo spawna prima della shell,
# senza suite) e raccoglie le righe `[bench]`. Dopo il bench QEMU resta sulla
# shell: `bench.sh` rileva `DONE` e chiude il process group (early-stop); il
# `timeout` resta come rete di sicurezza per build/boot bloccati.
# Piattaforma di riferimento KVM (`-accel kvm -cpu host`); senza /dev/kvm
# ripiega su TCG con warning (tempi NON di riferimento).
# Al termine stampa il confronto ArcaFS/FAT e nativo/POSIX (bench-compare).
set -euo pipefail
cd "$(dirname "$0")/.."

# Profili (C): il `full` e' la campagna di riferimento; il `reduced` serve
# alla validazione rapida su host lenti. Override espliciti restano possibili
# (es. ARCA_SIZE_MIB=2048 RUNS=1 BENCH_PROFILE=full ./scripts/bench.sh).
PROFILE="${BENCH_PROFILE:-full}"
case "$PROFILE" in
    split)
        # La matrice per-size vive in bench-split.sh: un boot per gruppo con
        # timeout proprio; evita di perdere tutta la campagna su un timeout.
        exec ./scripts/bench-split.sh
        ;;
    reduced)
        export BENCH_REDUCED=1
        : "${RUNS:=1}"
        : "${TIMEOUT_S:=900}"
        : "${ARCA_SIZE_MIB:=512}"
        ;;
    full)
        export BENCH_REDUCED=0
        : "${RUNS:=3}"
        # 5400 s: 1M/4M (cold compreso) e PUT nativo 1M; la prima run paga
        # anche la build e il boot. L'early-stop chiude appena arriva DONE,
        # quindi il timeout e' solo la rete di sicurezza (3600 era troppo
        # corto su host lenti: build ~6 min + matrice ~55 min).
        : "${TIMEOUT_S:=5400}"
        # ArcaFS: la orphan-GC gira solo al bind (56.2c), quindi i COW orfani
        # di una sessione restano allocati fino al remount. Il gate usa 128 MiB
        # (basta alla suite); la campagna full fa molti piu' commit (small_ow,
        # cold, PUT nativo) e con 512 MiB arriva a NOSPC prima della fine.
        # 4 GiB = headroom per la matrice completa, senza cambiare il costo per
        # operazione. run.sh rigenera l'immagine a ogni run.
        : "${ARCA_SIZE_MIB:=4096}"
        ;;
    *)
        echo "[bench] BENCH_PROFILE non valido: $PROFILE (usa reduced|full|split)" >&2
        exit 2
        ;;
esac
export ARCA_SIZE_MIB
echo "[bench] profilo: $PROFILE (RUNS=$RUNS, TIMEOUT_S=$TIMEOUT_S, ARCA_SIZE_MIB=$ARCA_SIZE_MIB)"

ACCEL=()
if [ -e /dev/kvm ]; then
    ACCEL=(-accel kvm -cpu host)
    echo "[bench] piattaforma: KVM (riferimento)"
else
    echo "[bench] WARNING: /dev/kvm assente, TCG (tempi non di riferimento)" >&2
fi

# Chiude timeout+QEMU (setsid: il pid e' anche pgid del nuovo gruppo). Il TERM
# basta a QEMU; il KILL e' solo una rete di sicurezza. Mai pkill globale.
kill_run() {
    local pid="$1"
    kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
    local n=0
    while kill -0 "$pid" 2>/dev/null && [ "$n" -lt 8 ]; do
        sleep 0.25
        n=$((n + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill -KILL -- "-$pid" 2>/dev/null || true
    fi
}

LOG_FILES=()
for i in $(seq 1 "$RUNS"); do
    log="/tmp/bench-run$i.log"
    LOG_FILES+=("$log")
    echo "[bench] run $i/$RUNS (log $log)"
    code=0
    # setsid: nuovo gruppo; al DONE chiudiamo noi QEMU invece di attendere il
    # timeout (QEMU resta sulla shell dopo il bench). `timeout` resta la rete
    # di sicurezza per build/boot bloccati.
    RUN_BENCH=1 setsid timeout "$TIMEOUT_S" ./run.sh "${ACCEL[@]}" >"$log" 2>&1 &
    pid=$!
    deadline=$((SECONDS + TIMEOUT_S))
    timed_out=0
    while kill -0 "$pid" 2>/dev/null; do
        if grep -q '\[bench\] DONE ok=' "$log" 2>/dev/null; then
            # Lascia svuotare il seriale (la riga DONE e' gia' nel file).
            sleep 0.3
            kill_run "$pid"
            break
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            timed_out=1
            kill_run "$pid"
            break
        fi
        sleep 0.25
    done
    wait "$pid" 2>/dev/null || code=$?
    if ! grep -q '\[bench\] DONE ok=1' "$log"; then
        if grep -q '\[bench\] DONE ok=0' "$log"; then
            echo "[bench] run $i FAILED (DONE ok=0)" >&2
        elif [ "$timed_out" = "1" ]; then
            echo "[bench] run $i FAILED (timeout ${TIMEOUT_S}s)" >&2
        else
            echo "[bench] run $i FAILED (bench incompleto, exit $code)" >&2
        fi
        grep '\[bench\]' "$log" >&2 || tail -20 "$log" >&2
        exit 1
    fi
    grep '\[bench\]' "$log" | sed "s/^/[run$i] /"
done
# In modalita' split il comparator gira una sola volta su tutti i log: un
# gruppo (es. `native`) non ha coppie A/B e farebbe fallire il compare locale.
if [ "${BENCH_NO_COMPARE:-0}" != "1" ]; then
    python3 scripts/bench-compare.py "${LOG_FILES[@]}"
fi
