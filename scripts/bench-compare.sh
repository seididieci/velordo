#!/usr/bin/env bash
# Wrapper del comparator userbench (logica in bench-compare.py).
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 scripts/bench-compare.py "$@"
