#!/usr/bin/env bash
# Gate di regressione Velordo: boot con la test suite completa.
#
# `./run.sh` di default e' produzione (init salta i test, shell subito).
# Questo wrapper imposta RUN_TESTS=1 (init compilato con --no-default-features
# ed esegue usertestfs/usertestfat/usertests/testsarca in sequenza prima della
# shell) e rimanda a run.sh. Righe attese + zero FAIL/PANIC/FAULT (ADR-0059:
# il FAT e' fuori dal gate — la suite FAT32 sta in scripts/run-fat-tests.sh,
# run dedicato con un disco FAT attaccato):
#   [testfs] PASS 5/5
#   [testsarca] PASS 64/64
#   [posixtests] PASS 4/4
#   [identistest] PASS 11/11
#   [usertests] PASS 59/59
#   [threadtest] PASS 12/12
# (D4: la suite e' cresciuta di una riga: [usertests] 59/59 con t63)
# L0b (virtio-gpu + MSI-X, ADR-0057): nessuna riga nuova; il boot verifica i
# marker `[gpu] virtio-gpu ...` / `[gpu] display info: 1280x800 enabled=1`.
# D3 (console virtuale, ADR-0060): nessuna riga nuova; i marker sono
# `[console] surface: pixel 1280x800 grid=128x36` e `[gpu] scanout: ...`
# (con VIRTIO_GPU=0: `[console] surface: vga-text 80x25`).
set -euo pipefail
cd "$(dirname "$0")"
# Diagnostica scheduler/IRQ attiva nei run di test (feature `sched_debug`).
# ARCA_IMG=1: quarto drive ArcaFS (partizione GPT su arca-gpt.img, sdd/sdd1)
# per testsarca 12-13 (il terzo, arca.img=root sdc, e' sempre attaccato da
# run.sh; 22-40 girano sul root vivo in bucket isolati, 41-50 su mount root,
# 51-54 quota A3 in bucket q5*).
RUN_TESTS=1 SCHED_DEBUG=1 ARCA_IMG=1 exec ./run.sh "$@"
