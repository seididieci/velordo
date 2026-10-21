#!/usr/bin/env bash
# Gate di regressione Velordo: boot con la test suite completa.
#
# `./run.sh` di default e' produzione (init salta i test, shell subito).
# Questo wrapper imposta RUN_TESTS=1 (init compilato con --no-default-features
# ed esegue usertestfs/usertestfat/usertests/testsarca in sequenza prima della
# shell) e rimanda a run.sh. Righe attese + zero FAIL/PANIC/FAULT:
#   [testfs] PASS 5/5
#   [testfat] PASS 7/7
#   [testsarca] PASS 62/62
#   [posixtests] PASS 4/4
#   [usertests] PASS 58/58
#   [threadtest] PASS 12/12
# L0b (virtio-gpu + MSI-X, ADR-0057): nessuna riga nuova; il boot verifica i
# marker `[gpu] virtio-gpu ...` / `[gpu] display info: 1280x800 enabled=1`.
set -euo pipefail
cd "$(dirname "$0")"
# Diagnostica scheduler/IRQ attiva nei run di test (feature `sched_debug`).
# ARCA_IMG=1: quarto drive ArcaFS (partizione GPT su arca-gpt.img, sdd/sdd1)
# per testsarca 12-13 (il terzo, arca.img=root sdc, e' sempre attaccato da
# run.sh; 22-40 girano sul root vivo in bucket isolati, 41-50 su mount root,
# 51-54 quota A3 in bucket q5*).
RUN_TESTS=1 SCHED_DEBUG=1 ARCA_IMG=1 exec ./run.sh "$@"
