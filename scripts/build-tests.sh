#!/usr/bin/env bash
# Build dei binari della TEST SUITE in modalita' freestanding.
#
# testland/ contiene ogni binario NON "ad uso utente": demo, i test
# ramfs/fat (usertestfs/usertestfat), gli strumenti di stress (hogheap,
# gli strumenti di stress (hogheap, devreader) e la suite di regressione
# completa (usertests + helper client/spin).
#
# Prodotto: testland/build/*.bin, iniettati in /fat/test via inject-bins.sh
# e spawnati da disco (spawn_image); solo init/disk/fs restano embedded.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/build_common.sh

BUILD="testland/build"
TARGET_DIR="$BUILD/target"
export CARGO_TARGET_DIR="$TARGET_DIR"

# Fase 58.5: i .bin test stanno in testland/build (meccanismo) e
# flavours/posix/tests/build (personalita' POSIX). Pulisco i .bin stantii.
rm -f "$BUILD"/*.bin flavours/posix/tests/build/*.bin

# Manifest hash dei servizi (Fase 36): generato da build-userland.sh (che gira
# sempre prima, vedi run.sh). Riesportato qui per i crate test che lo
# includono (usertests t51: peer_info atteso); se manca, fail loud subito.
if [ ! -f "build-meta/service_hashes.rs" ]; then
    echo "[build-tests] ERROR: build-meta/service_hashes.rs mancante (build-userland.sh prima)" >&2
    exit 1
fi
export VELORDO_SERVICE_HASHES="$(pwd)/build-meta/service_hashes.rs"

build_one testland/demo    testland/demo/src/demo.ld         userdemo.bin      userdemo
build_one testland/testfs  testland/testfs/src/testfs.ld     usertestfs.bin    usertestfs
build_one testland/testfat testland/testfat/src/testfat.ld   usertestfat.bin   usertestfat
# ArcaFS P5 (Fase 54): BLAKE2s + content_hash + volume (terzo drive opt-in).
build_one testland/testsarca testland/testsarca/src/testsarca.ld usertestsarca.bin usertestsarca
build_one testland/hogheap testland/hogheap/src/hogheap.ld   userhogheap.bin   userhogheap
build_one testland/devreader testland/devreader/src/devreader.ld userdevreader.bin userdevreader
build_one testland/usertests testland/usertests/src/usertests.ld usertests.bin usertests
# Thread 1:1 (S-T, ADR-0046): suite dedicata (create/exit/stack/TLS/futex).
build_one testland/threadtest testland/threadtest/src/threadtest.ld threadtest.bin userthreadtest
build_one testland/usertest-client testland/usertest-client/src/client.ld usertestcli.bin usertestcli
build_one testland/usertest-spin testland/usertest-spin/src/spin.ld usertestspin.bin usertestspin
build_one testland/utcbstest testland/utcbstest/src/utcbstest.ld utcbstest.bin utcbstest
# Bench: BENCH_REDUCED=1 compila il profilo ridotto (feature `reduced`), per
# la validazione rapida su host lenti. Mai nel gate.
BENCH_FEATURES=""
if [ "${BENCH_REDUCED:-0}" = "1" ]; then
    BENCH_FEATURES="--features reduced"
    echo "[build] userbench CON profilo reduced"
fi
# shellcheck disable=SC2086
build_one testland/bench testland/bench/src/bench.ld userbench.bin userbench $BENCH_FEATURES
# Attore "ignoto" di t57 (Fase 45): binario testland ESCLUSO dalla tabella
# policy (gen-test-policy.sh lo salta per nome) per provare il default
# restrittivo fail-closed sugli hash fuori manifest.
build_one testland/foreign testland/foreign/src/foreign.ld userforeign.bin userforeign
# Fuse staticone (S2.5d): ricompone i chunk rustc e lo spawna bigstack.
# Mai nel gate (run dedicato RUN_S25=1).
build_one testland/s25fuse testland/s25fuse/src/s25fuse.ld users25fuse.bin users25fuse

# Suite test della personalita' POSIX (Fase 58.5, ADR-0041): binario in
# flavours/posix/tests, output in flavours/posix/tests/build (BUILD override;
# CARGO_TARGET_DIR resta quello testland). Coperta da gen-test-policy.sh.
BUILD="flavours/posix/tests/build" build_one flavours/posix/tests flavours/posix/tests/src/posixtests.ld userposixtests.bin userposixtests

# Tabella policy test (Fase 45, sandbox build): hash testland -> ALL, inclusa
# SOLO da cardo (niente ciclo: i test non la includono). Segue il rebuild di
# cardo (unico consumatore): cardo.bin in userland/build viene rigenerato
# QUI (prima del kernel che lo embedda e di inject-bins.sh che lo copia su
# /fat per i restart da disco). Fixpoint in un passaggio (cardo e' fuori da
# entrambe le tabelle, i test non dipendono da questa).
bash scripts/gen-test-policy.sh
export VELORDO_SERVICE_POLICY="$(pwd)/build-meta/service_policy.rs"
export VELORDO_TEST_POLICY="$(pwd)/build-meta/test_policy.rs"
# Rebuild mirato in userland/build (BUILD override: build_one scrive in
# $BUILD/$out e qui $BUILD e' testland/build — il kernel embedda e inject
# copiano userland/build). Rimuove anche l'eventuale cardo.bin stale in
# testland/build (artefatto di run precedenti: NON deve finire in tabella).
rm -f testland/build/cardo.bin
BUILD="userland/build" build_one userland/cardo userland/cardo/src/cardo.ld cardo.bin cardo
