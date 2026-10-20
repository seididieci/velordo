#!/usr/bin/env bash
# Inietta i binari servizi/test in /bin e /test su fat.img (Fase 21).
# Single source of truth usata da run.sh e scripts/test-shell.py: DOPO mkfat
# (che rigenera l'immagine) e DOPO build-userland/build-tests. Fail-loud
# (set -e + || exit): senza binari init fallisce loud a boot.
#
# Nomi DESTINAZIONE in 8.3 (il FAT non ha LFN e mcopy troncherebbe in
# USERCO~1.BIN, irrisolvibili dal nostro reader): si spella il prefisso
# "user". I nomi display (ps) restano lunghi: viaggiano in SpawnMeta.
set -euo pipefail
cd "$(dirname "$0")/.."

IMG="userland/fs/fat.img"
mmd -i "$IMG" ::/bin ::/test || exit 1
mcopy -i "$IMG" userland/build/gpu.bin ::/bin/gpu.bin || exit 1
mcopy -i "$IMG" userland/build/useruptime.bin  ::/bin/uptime.bin  || exit 1
mcopy -i "$IMG" userland/build/vela.bin      ::/bin/vela.bin   || exit 1
mcopy -i "$IMG" userland/build/kbd.bin     ::/bin/kbd.bin     || exit 1
mcopy -i "$IMG" userland/build/porta.bin       ::/bin/porta.bin     || exit 1
# Personalita' POSIX (Fase 58.4, ADR-0041): binari in flavours/posix/build.
mcopy -i "$IMG" flavours/posix/build/usershell.bin   ::/bin/shell.bin   || exit 1
# Server di personalita' POSIX (Fase 40.3, P1): skeleton supervisionato.
mcopy -i "$IMG" flavours/posix/build/userposix.bin   ::/bin/posix.bin   || exit 1
# Fornitore di data/ora (Fase 50, P1 orologio).
mcopy -i "$IMG" userland/build/usertime.bin    ::/bin/time.bin    || exit 1
# Gateway centrale di logging L1 (Fase 57, ADR-0039).
mcopy -i "$IMG" userland/build/vestigia.bin     ::/bin/vestigia.bin     || exit 1
mcopy -i "$IMG" flavours/posix/build/userrunhello.bin ::/bin/runhello.bin || exit 1
# Tool guest ArcaFS (Fase 54, P5; Fase 58.4: dir `userland/tools/`): `list`/`stat`.
mcopy -i "$IMG" userland/build/userarca.bin   ::/bin/arca.bin    || exit 1
mcopy -i "$IMG" testland/build/usertestfs.bin   ::/test/testfs.bin   || exit 1
mcopy -i "$IMG" testland/build/usertestfat.bin  ::/test/testfat.bin  || exit 1
mcopy -i "$IMG" testland/build/usertestsarca.bin ::/test/testarca.bin || exit 1
mcopy -i "$IMG" testland/build/usertests.bin    ::/test/tests.bin    || exit 1
mcopy -i "$IMG" testland/build/threadtest.bin   ::/test/thread.bin   || exit 1
mcopy -i "$IMG" testland/build/usertestcli.bin  ::/test/testcli.bin  || exit 1
mcopy -i "$IMG" testland/build/usertestspin.bin ::/test/testspin.bin || exit 1
mcopy -i "$IMG" testland/build/utcbstest.bin    ::/test/cbstest.bin  || exit 1
mcopy -i "$IMG" testland/build/userhogheap.bin  ::/test/hogheap.bin  || exit 1
mcopy -i "$IMG" testland/build/userdevreader.bin ::/test/devreadr.bin || exit 1
mcopy -i "$IMG" testland/build/userdemo.bin     ::/test/demo.bin     || exit 1
mcopy -i "$IMG" testland/build/userbench.bin    ::/test/bench.bin    || exit 1
# Attore "ignoto" di t57 (Fase 45): fuori da ogni tabella policy (il nome
# 8.3 resta lungo: "foreign" sta in 8 caratteri, niente spelling).
mcopy -i "$IMG" testland/build/userforeign.bin  ::/test/foreign.bin  || exit 1
# Suite della personalita' POSIX (Fase 58.5, ADR-0041): binario separato in
# flavours/posix/tests/build (dest 8.3 "posixtst").
mcopy -i "$IMG" flavours/posix/tests/build/userposixtests.bin ::/test/posixtst.bin || exit 1
# Hello std nativo (S1.3): solo se compilato via scripts/build-pal.sh.
if [ -f pal/build/hello-std.bin ]; then
    mcopy -i "$IMG" pal/build/hello-std.bin ::/test/stdhello.bin || exit 1
fi
# Staticone rustc: NON piu' su FAT (S2.5f/F1). Il payload (~100 MB) vive nel
# bucket `ns` di ArcaFS (`tool/rustc.bin`, seed condizionale in
# scripts/arca-tool.sh) e s25fuse lo legge a slice via range read F0. Qui
# resta solo il helper (sotto, in /test), non il payload.
# Script shell per `source` (velocizzazione test: 1 riga digitata invece di N
# comandi via sendkey) + pilota del builtin permanente. Nomi 8.3 come i .bin
# (il FAT non ha LFN: oltre 8+3 mcopy fallisce loud, mai nomi troncati in
# silenzio). Tutti gli script di fase stanno in scripts/sh/.
mmd -i "$IMG" ::/test/sh || exit 1
for s in scripts/sh/*.txt scripts/sh/*.sh; do
    b="$(basename "$s")"
    mcopy -i "$IMG" "$s" "::/test/sh/$b" || exit 1
done

# Fixture grandi read-only anche su FAT (solo profilo bench full/split): la
# creazione guest-side dei file da 4M e' patologicamente lenta e faceva
# scadere il timeout monolitico. Sono file di sola lettura per le righe
# `*_read_hot`/`cold`: i fixture scrivibili restano creati dal guest.
if [ "${RUN_BENCH:-0}" = "1" ] && [ "${BENCH_REDUCED:-0}" != "1" ]; then
    BENCH_FIX="testland/build/bench-fixtures"
    mkdir -p "$BENCH_FIX"
    group="${BENCH_GROUP:-all}"
    if [ "$group" = "1m" ] || [ "$group" = "all" ]; then
        python3 -c "import sys; sys.stdout.buffer.write(bytes((i % 251) for i in range(1048576)))" > "$BENCH_FIX/1m.bin"
        mcopy -i "$IMG" "$BENCH_FIX/1m.bin" ::/R1M00.BIN || exit 1
        mcopy -i "$IMG" "$BENCH_FIX/1m.bin" ::/R1M01.BIN || exit 1
        echo "[inject] + fixture bench FAT 1M cold-read"
    fi
    if [ "$group" = "4m" ] || [ "$group" = "all" ]; then
        python3 -c "import sys; sys.stdout.buffer.write(bytes((i % 251) for i in range(4194304)))" > "$BENCH_FIX/4m.bin"
        mcopy -i "$IMG" "$BENCH_FIX/4m.bin" ::/AB_4M.BIN || exit 1
        mcopy -i "$IMG" "$BENCH_FIX/4m.bin" ::/R4M00.BIN || exit 1
        echo "[inject] + fixture bench FAT 4M read-only"
    fi
fi
echo "[inject] servizi in /bin + /test su $IMG"
