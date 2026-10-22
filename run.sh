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

# ADR-0059: **il FAT esce dal boot**. I dischi FAT non vengono piu'
# generati ne' attaccati: il boot usa solo ArcaFS (root su arca.img + le
# derivate opzionali). `scripts/mkfat.py` resta solo per la suite FAT
# opt-in e per il confronto prestazionale ArcaFS-vs-FAT (run dedicati, fuori
# gate).
#
# ArcaFS (Fase 54, P5): build del tool host + `arca.img` (superblock LBA0 +
# shadow). Sempre generata (economica); le derivate si aggiungono solo con
# ARCA_IMG=1 (test 12-13 e i volumi secondari).
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
# S1.2/W6: le derivate MBR/GPT seguono la dimensione di arca.img (nessun
# limite cablato a 32 MiB: i test 12-13 restano nel gate anche col volume
# piu' grande richiesto dai blocchi da 64 KiB).
if [ "${ARCA_IMG:-0}" = "1" ]; then
    ./scripts/build-arca-part.sh
    python3 ./scripts/build-arca-gpt.sh
    # UUID distinti per partizione MBR (LBA63) e GPT (LBA64).
    ARCA_TOOL_BIN="${ARCA_TOOL_BIN:-$ROOT/build-meta/arca}"
    "$ARCA_TOOL_BIN" uuid userland/disk/arca-part.img 4152434100000002 --lba 63
    "$ARCA_TOOL_BIN" uuid userland/disk/arca-gpt.img 4152434100000003 --lba 64
    # ADR-0059 (v4): i volumi derivati si presentano con una label (il loro
    # nome: la discovery by-label arriva con la fase USB).
    "$ARCA_TOOL_BIN" set userland/disk/arca-part.img --label PART --lba 63
    "$ARCA_TOOL_BIN" set userland/disk/arca-gpt.img --label GPT --lba 64
fi

KERNEL=target/x86_64-unknown-none/release/velord
DISPLAY="${RUN_DISPLAY:-none}"   # RUN_DISPLAY=gtk per vedere la VGA in locale

# Boot diretto via protocollo PVH (ELF64 + nota XEN_ELFNOTE_PHYS32_ENTRY):
# QEMU carica il kernel e trasferisce il controllo in protected mode 32-bit.
# Drive IDE: il volume ArcaFS root (sda) SEMPRE; le derivate (GPT/MBR)
# opzionali con ARCA_IMG=1. `SWAP_DRIVES=1` inverte l'ordine IDE: le lettere
# sdX si scambiano, ma la root e' scelta per **UUID** (mai per lettera) —
# e' la prova che l'identita' del volume non dipende dalla posizione.
if [ "${SWAP_DRIVES:-0}" = "1" ] && [ -f userland/disk/arca-gpt.img ]; then
    DRIVES="-drive file=userland/disk/arca-gpt.img,format=raw,if=ide -drive file=userland/disk/arca.img,format=raw,if=ide"
else
    DRIVES="-drive file=userland/disk/arca.img,format=raw,if=ide"
fi
# Drive secondario (partizione GPT su arca-gpt.img) opt-in con ARCA_IMG=1:
# serve ai test 12-13 e al mount in partizione (E2: il PC ha solo 4 IDE; la
# MBR resta artefatto per run manuali — mount-in-partizione coperto via GPT,
# vedi 11-testing). La suite cerca i volumi via magic/UUID, mai per lettera.
if [ "${ARCA_IMG:-0}" = "1" ] && [ -f userland/disk/arca-gpt.img ]; then
    DRIVES="$DRIVES -drive file=userland/disk/arca-gpt.img,format=raw,if=ide"
fi
# FAT opzionale (ADR-0059): il boot NON usa il FAT, ma i run opt-in sì
# (suite FAT `run-fat-tests.sh`, confronto prestazionale ArcaFS-vs-FAT in
# scripts/bench.sh). Con FAT_IMG=1 le immagini FAT vengono create e
# attaccate; senza, il sistema parte solo con ArcaFS.
if [ "${FAT_IMG:-0}" = "1" ]; then
    python3 scripts/mkfat.py userland/fs/fat.img
    python3 scripts/mkfat.py userland/fs/fat2.img --serial C0FFEE01 \
        --label SECOND --marker "second disk marker"
    DRIVES="$DRIVES -drive file=userland/fs/fat.img,format=raw,if=ide"
    DRIVES="$DRIVES -drive file=userland/fs/fat2.img,format=raw,if=ide"
    echo "[run] FAT_IMG=1: dischi FAT attaccati (run opt-in)"
fi
# Fase 2 (root su volume): passa la cmdline `-append` a QEMU. Il kernel legge
# `hvm_start_info.cmdline_paddr` al boot e lo rende a userland via SYS_BOOT_
# CMDLINE (53). cardo parse `root=UUID=<8hex>` dal cmdline: monta il primo
# volume ArcaFS con quell'uuid come `/`, ramfs solo su `/tmp`. Se l'uuid non
# esiste o il parametro e' assente → kernel panic loud.
if [ -f userland/disk/arca.img ]; then
    APPEND="root=UUID=4152434100000001"
    # S2.5d/F1: run dedicato allo staticone (volume + RAM grandi via env).
    # Il payload e' seedato in ArcaFS da scripts/arca-tool.sh; il volume
    # default sale a 384 MiB quando RUN_S25=1 (vedi arca-tool.sh).
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

# L0b (ADR-0057): virtio-gpu-pci in modalita' modern (MSI-X). D3: il device
# e' il display del terminale (i pixel sono della console server, il driver
# e' solo il device); VIRTIO_GPU=0 = path di fallback (device assente,
# console su VGA testuale).
GPU_DEV=""
if [ "${VIRTIO_GPU:-1}" = "1" ]; then
    GPU_DEV="-device virtio-gpu-pci,disable-legacy=on"
fi
# Il **display di default** del sistema e' il terminale pixel (virtio-gpu).
# Il UI GTK di QEMU mostra la **console 0**: con la VGA presente la console 0
# e' la VGA, e il terminale pixel (console 1) resta senza finestra (la VGA
# mostra il testo di boot stantio: nel path pixel la console non ci scrive
# piu'). Tolta la VGA, il virtio-gpu diventa la console 0 e la finestra
# mostra il terminale.
#
#   FB_WINDOW=auto (default) -vga none se si vuole un display GRAFICO
#                             (RUN_DISPLAY != none) e il device virtio-gpu
#                             e' presente
#   FB_WINDOW=1             -vga none sempre (forza)
#   FB_WINDOW=0             VGA presente (forza: fallback testuale della
#                             console e dump screendump della VGA)
#
# NB: senza il device VGA il fallback "VGA testuale" della console non ha
# dove scrivere (il device e' assente, non e' solo vuoto) — il percorso
# automatico lo toglie solo con il device virtio-gpu presente.
VGA_DEV=""
case "${FB_WINDOW:-auto}" in
    1) VGA_DEV="-vga none" ;;
    0) VGA_DEV="" ;;
    *)
        if [ "$DISPLAY" != "none" ] && [ "${VIRTIO_GPU:-1}" = "1" ]; then
            VGA_DEV="-vga none"
        fi
        ;;
esac
if [ -n "$VGA_DEV" ]; then
    GPU_DEV="$GPU_DEV $VGA_DEV"
fi
# shellcheck disable=SC2086
exec qemu-system-x86_64 \
    -m ${RUN_MEM:-256M} \
    -cpu qemu64,+fsgsbase,+rdrand,+rdseed \
    -display "$DISPLAY" \
    -serial stdio \
    -no-reboot \
    -kernel "$KERNEL" \
    $DRIVES \
    $GPU_DEV \
    "$@"
