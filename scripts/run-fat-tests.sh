#!/usr/bin/env bash
# Run dedicato della suite FAT32 (ADR-0058).
#
# Il FAT **non e' piu' il filesystem di sistema**: il boot e' solo ArcaFS e la
# suite FAT32 e' fuori dal gate (il gate e' 6 righe, vedi 11-testing.md).
# Questo script la esegue comunque, in un run separato con un disco FAT
# attaccato (FAT_IMG=1) e init compilato con il feature `fat_tests`.
#
# Uso: ./scripts/run-fat-tests.sh        (1 run, suite FAT + gli altri test)
set -euo pipefail
cd "$(dirname "$0")/.."

export RUN_TESTS=1
export RUN_FAT_TESTS=1
export FAT_IMG=1
# La suite FAT vuole il volume GPT derivato (i test 12-13 di testsarca):
# ARCA_IMG=1 lo attacca.
export ARCA_IMG=1

echo "[fat-tests] run con FAT: suite FAT32 + le altre suite (fuori gate)"
exec ./run-tests.sh "$@"
