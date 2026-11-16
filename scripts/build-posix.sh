#!/usr/bin/env bash
# Build dei binari della personalita' POSIX (Fase 58.4, ADR-0041).
#
# Vivono in `flavours/posix/` (server = posix-server, shell, cli = programmi
# lanciabili) e producono `flavours/posix/build/*.bin`. I servizi nativi
# (init, cardo, block, gpu, kbd, vela, porta, vestigia, time, uptime) restano
# in `userland/` e in `userland/build`.
#
# Invocato da build-userland.sh PRIMA di gen-service-hashes.sh: il manifest
# degli hash copre sia userland/build sia flavours/posix/build. Target Cargo
# separato (`flavours/posix/build/target`) per non mescolare gli artefatti.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/build_common.sh

BUILD="flavours/posix/build"
TARGET_DIR="$BUILD/target"
export CARGO_TARGET_DIR="$TARGET_DIR"

# Pulisco i .bin stantii (il manifest hash li vedrebbe doppi).
# ADR-0078 (fase 1): niente `rm -f *.bin` (vedi build-userland.sh): il
# confronto in `build_one` tiene stabile il mtime degli output invariati.

# posix-server (Fase 40.3): supervisionato da init, registra `Service::Posix`.
build_one flavours/posix/server flavours/posix/server/src/posix.ld userposix.bin userposix
# Shell POSIX (personalita'): builtin, parser, job control, redirect.
build_one flavours/posix/shell  flavours/posix/shell/src/shell.ld   usershell.bin usershell
# runhello (Fase 37.2): programma lanciabile dalla shell, non un servizio.
build_one flavours/posix/cli/runhello flavours/posix/cli/runhello/src/runhello.ld userrunhello.bin userrunhello
