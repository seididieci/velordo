#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

echo "[run] Scheduler: RT a 32 priorita' + CBS"

# Build dei binari userspace (userland/ = servizi utente) e della test suite
# (testland/) in pipeline separate, poi il kernel che li embedda entrambi
# (user_binary.rs via include_bytes!).
./scripts/build-userland.sh
./scripts/build-tests.sh

# Diagnostica scheduler/IRQ (feature `sched_debug`): snapshot ready-mask ogni
# 100 tick + log IRQ1. Spenta di default (log puliti); accesa con
# SCHED_DEBUG=1. run-tests.sh la imposta sempre.
if [ "${SCHED_DEBUG:-0}" = "1" ]; then
    echo "[run] sched_debug ATTIVO (log scheduler + IRQ1)"
    cargo build --release --features sched_debug
else
    cargo build --release
fi

# Immagini disco FAT32 (Fase 9.2 + 16d): generate a ogni run.
# fat.img = disco di boot (UUID 4F4C4556, label VELORDO, montato a /fat);
# fat2.img = secondo disco (UUID e label diversi + MARKER.TXT) per i test di
# identità stabile (t36) e il reorder (SWAP_DRIVES=1 inverte l'ordine IDE:
# le lettere sdX si scambiano, UUID=/LABEL= restano validi).
python3 scripts/mkfat.py userland/fs/fat.img
python3 scripts/mkfat.py userland/fs/fat2.img --serial C0FFEE01 --label SECOND --marker "second disk marker"

# Servizi da disco (Fase 21): /bin + /test iniettati via script condiviso
# (stesso usato da test-shell.py) DOPO mkfat. Fail-loud.
bash scripts/inject-bins.sh

# ArcaFS (Fase 54, P5): build del tool host + `arca.img` (superblock LBA0 +
# shadow). Sempre generata (economica); il terzo drive si aggiunge solo con
# ARCA_IMG=1, cosi' il boot default resta a due dischi (sda/sdb stabili per
# t32/t36).
./scripts/arca-tool.sh

# Fase 55, Parte 4 + E2: crea arca-part.img (MBR, LBA63) e arca-gpt.img
# (GPT, LBA64) quando ARCA_IMG=1. Il PC ha solo 4 IDE: come quarto drive si
# attacca la GPT (sdd: serve ai test 12-13); la MBR resta artefatto per run
# manuali (build+self-check la tengono valida, il mount-in-partizione e'
# coperto via GPT — perde solo il parser MBR nel gate, vedi 11-testing).

# Fase 2 (root su volume): assegna UUID distinti alle derivate (MBR/GPT) per
# evitare collisioni con arca.img — il kernel sceglie la root per UUID (mai
# per lettera/scan). Le derivate sono copie di blocchi del volume originale:
# NON vanno seedate a loro volta.
# S1.2: le derivate MBR/GPT sono cablate a 32 MiB (build-arca-part.sh):
# con volumi grandi si saltano (fail-loud dentro gli script, mai immagini
# a meta': i test 12-13 restano adaptively-PASS come senza quarto drive).
if [ "${ARCA_IMG:-0}" = "1" ] && [ "${ARCA_SIZE_MIB:-32}" = "32" ]; then
    ./scripts/build-arca-part.sh
    python3 ./scripts/build-arca-gpt.sh
    # UUID distinti per partizione MBR (LBA63) e GPT (LBA64).
    ARCA_TOOL_BIN="${ARCA_TOOL_BIN:-$ROOT/build-meta/arca}"
    "$ARCA_TOOL_BIN" uuid userland/disk/arca-part.img 4152434100000002 --lba 63
    "$ARCA_TOOL_BIN" uuid userland/disk/arca-gpt.img 4152434100000003 --lba 64
fi

KERNEL=target/x86_64-unknown-none/release/velord
DISPLAY="${RUN_DISPLAY:-none}"   # RUN_DISPLAY=gtk per vedere la VGA in locale

# Boot diretto via protocollo PVH (ELF64 + nota XEN_ELFNOTE_PHYS32_ENTRY):
# QEMU carica il kernel e trasferisce il controllo in protected mode 32-bit.
# Due drive IDE (Fase 16d): block li enumera sda,sdb in ordine di probe
# e cardo monta a /fat per UUID (mai per lettera).
if [ "${SWAP_DRIVES:-0}" = "1" ]; then
    DRIVES="-drive file=userland/fs/fat2.img,format=raw,if=ide -drive file=userland/fs/fat.img,format=raw,if=ide"
else
    DRIVES="-drive file=userland/fs/fat.img,format=raw,if=ide -drive file=userland/fs/fat2.img,format=raw,if=ide"
fi
# Terzo drive ArcaFS SEMPRE attaccato (Fase A: e' la root `/`, scelta per UUID —
# sda/sdb restano i due FAT). Quarto drive (partizione GPT su arca-gpt.img =
# secondary slave) opt-in con ARCA_IMG=1 per i test 12-13 e il mount in
# partizione di 4-8 (E2: il PC ha solo 4 IDE; la MBR resta artefatto per run
# manuali — mount-in-partizione coperto via GPT, vedi 11-testing).
# La suite cerca i volumi via magic/UUID, mai per lettera.
DRIVES="$DRIVES -drive file=userland/disk/arca.img,format=raw,if=ide"
# S1.2: quarto drive solo se la derivata esiste davvero (a size non-default
# non viene generata: vedi sopra; QEMU fallirebbe su file assente).
if [ "${ARCA_IMG:-0}" = "1" ] && [ -f userland/disk/arca-gpt.img ]; then
    DRIVES="$DRIVES -drive file=userland/disk/arca-gpt.img,format=raw,if=ide"
fi
# Fase 2 (root su volume): passa la cmdline `-append` a QEMU. Il kernel legge
# `hvm_start_info.cmdline_paddr` al boot e lo rende a userland via SYS_BOOT_
# CMDLINE (53). cardo parse `root=UUID=<8hex>` dal cmdline: monta il primo
# volume ArcaFS con quell'uuid come `/`, ramfs solo su `/tmp`. Se l'uuid non
# esiste o il parametro e' assente → kernel panic loud.
if [ -f userland/disk/arca.img ]; then
    APPEND="root=UUID=4152434100000001"
    # S2.5d: run dedicato allo staticone (volume + RAM grandi via env).
    if [ "${RUN_S25:-0}" = "1" ]; then
        APPEND="$APPEND s25=1"
    fi
else
    APPEND=""
fi
# shellcheck disable=SC2086
# S-T (T2): FSGSBASE per la TLS user (rdfsbase/wrfsbase da ring 3). Solo
# questo flag oltre qemu64: niente altro cambia per gli altri test.
# S2.5d: +rdrand,+rdseed per il backend getrandom RDRAND dello staticone
# (no-op se qemu64 li ha gia'; il boot fallisce loud se invalidi).
# S1.2: RAM parametrica (default 256M: gate veloce; multi-GB per S1.3).
# S2.5d: `-append` con token multipli (root=UUID + s25=1) — il vecchio
# `$(echo "-append $APPEND")` splittava e QEMU leggeva `s25=1` come drive.
if [ -n "$APPEND" ]; then
    set -- "$@" -append "$APPEND"
fi
exec qemu-system-x86_64 \
    -m ${RUN_MEM:-256M} \
    -cpu qemu64,+fsgsbase,+rdrand,+rdseed \
    -display "$DISPLAY" \
    -serial stdio \
    -no-reboot \
    -kernel "$KERNEL" \
    $DRIVES \
    "$@"
