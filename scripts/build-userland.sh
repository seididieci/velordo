#!/usr/bin/env bash
# Build dei binari userspace in modalita' freestanding.
#
# Qui stanno SOLO i servizi utente (userland/): init, gpu, fs, vela,
# disk, shell, uptime, kbd, tty, posix, time, log — i binari "ad uso utente" dell'OS. I binari della test suite
# (testland/) sono compilati da scripts/build-tests.sh.
#
# Prodotto: userland/build/*.bin, codice raw caricato a USER_CODE dai processi
# user; solo init/disk/fs sono embedded nel kernel, il resto va in /fat/bin
# via inject-bins.sh (servizi da disco, Fase 21).
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/build_common.sh

BUILD="userland/build"
TARGET_DIR="$BUILD/target"
export CARGO_TARGET_DIR="$TARGET_DIR"

# Fase 58.4: i .bin sono per-dir (nativi qui, POSIX in flavours/posix/build).
# Pulisco i .bin stantii (es. binari POSIX prima dello spostamento) cosi' il
# manifest degli hash non li vede due volte (E0428 duplicate const).
rm -f "$BUILD"/*.bin

# Suite di test: di default init la SALTA (feature `skip_tests`, boot di
# produzione dritto alla shell). Con RUN_TESTS=1 (run-tests.sh) init
# viene compilato con `--no-default-features` ed esegue la suite completa.
if [ "${RUN_TESTS:-0}" = "1" ]; then
    RECTOR_FEATURES="--no-default-features"
    echo "[build] rector CON test suite (RUN_TESTS=1)"
else
    RECTOR_FEATURES=""
    echo "[build] rector production (test saltati)"
fi

# Bench throughput (Fase 23, scripts/bench.sh): feature `bench` ortogonale a
# skip_tests (il bench gira anche senza suite, mai nel gate).
if [ "${RUN_BENCH:-0}" = "1" ]; then
    RECTOR_FEATURES="$RECTOR_FEATURES --features bench"
    echo "[build] rector CON bench (RUN_BENCH=1)"
fi

build_one userland/block   userland/block/src/block.ld     block.bin       block
build_one userland/vela    userland/vela/src/vela.ld       vela.bin        vela
build_one userland/gpu     userland/gpu/src/gpu.ld         gpu.bin         gpu
build_one userland/kbd     userland/kbd/src/kbd.ld         kbd.bin         kbd
build_one userland/porta   userland/porta/src/porta.ld     porta.bin       porta
build_one userland/uptime  userland/uptime/src/uptime.ld   useruptime.bin  useruptime
# Servizio identita'/autenticazione utenti (ADR-0055): identis gestisce il
# bucket `identis:config` (nome -> hash password). identis-login e' il primo
# programma interattivo al posto della shell (chiede utente/password, stampa SUCCESS/FAIL).
build_one userland/identis  userland/identis/src/identis.ld identis.bin   identis
build_one userland/identis-login userland/identis-login/src/login.ld login.bin   identis-login
# create-user (ADR-0055): client che chiede a Identis di creare un utente
# (token di sessione + nome + hash BLAKE2s-256). Non e' ancora lanciato da
# nessuno: la shell nativa (Fase 3.2) lo invocera' come comando.
build_one userland/create-user userland/create-user/src/create-user.ld create-user.bin create-user
# Fornitore di data/ora (Fase 50, P1 orologio): prima di gen-service-hashes
# cosi' il manifest Strato 2 lo copre (HASH_USERTIME).
build_one userland/time     userland/time/src/time.ld       usertime.bin    usertime
# Gateway centrale di logging L1 (Fase 57, ADR-0039): prima di
# gen-service-hashes cosi' il manifest Strato 2 lo copre (HASH_VESTIGIA).
build_one userland/vestigia userland/vestigia/src/vestigia.ld vestigia.bin   vestigia
# Tool guest ArcaFS (Fase 54, P5; Fase 58.4: dir `userland/tools/`): `list`/
# `stat` dalla shell, in /bin. Nativo (solo meccanismo) ma usa `libr::entry!`
# per onorare il redirect. Prima di gen-service-hashes (policy restrittiva).
build_one userland/tools/arca userland/tools/arca/src/arca.ld userarca.bin userarca

# Personalita' POSIX (Fase 58.4, ADR-0041): server/shell/cli vivono in
# flavours/posix/ e producono flavours/posix/build/*.bin. Costruiti PRIMA di
# gen-service-hashes cosi' il manifest Strato 2 li copre (HASH_USERPOSIX,
# HASH_USERSHELL, HASH_USERRUNHELLO).
bash scripts/build-posix.sh

# Manifest degli hash dei servizi (Fase 36, Strato 2): FNV-1a sui `.bin`
# appena prodotti. fs e init vengono DOPO perche' lo includono a compile time
# (cardo: policy FS_REGISTER su identita'; rector: manifest pre-spawn) via
# `include!(env!("VELORDO_SERVICE_HASHES"))` — senza la variabile la loro
# compilazione fallisce loud (mai manifest stale silenzioso).
bash scripts/gen-service-hashes.sh
export VELORDO_SERVICE_HASHES="$(pwd)/build-meta/service_hashes.rs"
# Tabella policy servizi (Fase 45, sandbox build): emessa dallo stesso script.
# TEST_POLICY ancora assente qui (i .bin test non esistono): placeholder
# vuoto per la prima build di cardo, SOSTITUITO dal rebuild in coda a
# build-tests.sh (unico cardo.bin che conta: kernel+inject vengono dopo).
# Placeholder MAI usato a runtime: ogni binario in tabella servizi o ignoto
# ha comunque un tetto (il lookup cade sul default a tabella vuota).
if [ ! -f "build-meta/test_policy.rs" ]; then
    printf '// Placeholder pre-test (Fase 45): sostituito dal rebuild in build-tests.sh.\n// A tabella vuota ogni hash test cade nel default restrittivo.\npub const TEST_POLICY: &[(u64, u32)] = &[];\n' > build-meta/test_policy.rs
fi
export VELORDO_SERVICE_POLICY="$(pwd)/build-meta/service_policy.rs"
export VELORDO_TEST_POLICY="$(pwd)/build-meta/test_policy.rs"

build_one userland/cardo   userland/cardo/src/cardo.ld     cardo.bin       cardo
build_one userland/rector  userland/rector/src/rector.ld   rector.bin      rector $RECTOR_FEATURES
