#!/usr/bin/env bash
# Crea un'immagine MBR con superblock ArcaFS scritto a offset partizione LBA63.
#
# Uso: scripts/build-arca-part.sh [out_img]   (default userland/disk/arca-part.img)
# Requisiti: dd, mkfs.vfat (opzionale per formattare la partizione).
#
# Produce un disco con:
#   - MBR valido con prima partizione a LBA63 (offset 32256 B)
#   - Superblock ArcaFS scritto all'inizio della partizione (LBA63)

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/userland/disk/arca-part.img}"
ARCA_IMG_FILE="$ROOT/userland/disk/arca.img"

# Verifica che arca.img esista (contiene il superblock da copiare)
if [ ! -f "$ARCA_IMG_FILE" ]; then
    echo "[arca-part] errore: $ARCA_IMG_FILE non trovato (creare prima con scripts/arca-tool.sh)"
    exit 1
fi

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"

# Dimensioni dal volume sorgente (S1.2/W6: non piu' cablate a 32 MiB).
VOL_SIZE="$(wc -c < "$ARCA_IMG_FILE")"
if [ "$VOL_SIZE" -le 0 ] || [ $((VOL_SIZE % 512)) -ne 0 ]; then
    echo "[arca-part] errore: volume di $VOL_SIZE byte (non multiplo di 512), abort"
    exit 1
fi
VOL_SECTORS=$((VOL_SIZE / 512))
TOTAL_SECTORS=$((63 + VOL_SECTORS))
OUT_BYTES=$((TOTAL_SECTORS * 512))

# 1. Crea immagine: offset 63 settori + volume ArcaFS.
dd if=/dev/zero of="$OUT" bs=512 count="$TOTAL_SECTORS" 2>/dev/null

# 2. Scrivi MBR manualmente (512 byte esatti).
#    Struttura MBR:
#      [0x000..0x1BD] Bootloader vuoto (446 B, fino a 0x1BE)
#      [0x1BE..0x1FD] Tabella partizioni (64 B, 4 voci da 16 B)
#      [0x1FE..0x1FF] Signature (2 B = 0x55AA)
#
#    Voce partizione primaria (16 B a offset 0x1BE):
#      [0] boot flag (0x80 = active)
#      [1-3] CHS start (testa=1, settore=1, cilindro=0)
#      [4] tipo (0x83 = Linux)
#      [5-7] CHS end (stesso)
#      [8-11] LBA start LE u32 (63)
#      [12-15] settori LE u32 (VOL_SECTORS: copre tutto il volume)
#
#    BUG STORICO (Fase 55): la tabella partiva a offset 440 invece di 0x1BE
#    e il file MBR era di 506 byte (short write su dd bs=512): la signature
#    finiva a offset 504 invece di 0x1FE e block leggeva sig=[0x0,0x0]
#    senza esporre sdc1. Ora offset e dimensione sono esatti e verificati
#    sotto (fail-loud).

{
    # Bootloader vuoto (446 byte a zero, fino a 0x1BE)
    dd if=/dev/zero bs=1 count=446 2>/dev/null

    # Tabella partizioni: prima voce + padding fino a offset 510
    printf '\x80\x01\x01\x00\x83\x01\x01\x00'   # boot, CHS start, tipo=Linux, CHS end
    printf '\x3f\x00\x00\x00'                   # LBA start = 63 (LE u32)
    SECT_LE="$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' \
        $((VOL_SECTORS & 0xff)) $(((VOL_SECTORS >> 8) & 0xff)) \
        $(((VOL_SECTORS >> 16) & 0xff)) $(((VOL_SECTORS >> 24) & 0xff)))"
    printf '%b' "$SECT_LE"                      # settori = VOL_SECTORS (LE u32)
    dd if=/dev/zero bs=1 count=48 2>/dev/null   # padding: 48 byte a zero (fino a offset 510)

    # Signature MBR (2 byte a offset 0x1FE)
    printf '\x55\xaa'
} > /tmp/mbr.bin

# Sovrascrive i primi 512 byte con il MBR (il file DEVE essere di 512 byte)
MBR_SIZE="$(wc -c < /tmp/mbr.bin)"
if [ "$MBR_SIZE" != "512" ]; then
    echo "[arca-part] errore: mbr.bin di $MBR_SIZE byte (attesi 512), abort"
    rm -f /tmp/mbr.bin
    exit 1
fi
dd if=/tmp/mbr.bin of="$OUT" bs=512 count=1 conv=notrunc 2>/dev/null
rm -f /tmp/mbr.bin

# 3. Copia l'intero volume ArcaFS all'inizio della partizione (LBA63).
dd if="$ARCA_IMG_FILE" of="$OUT" bs=512 seek=63 conv=notrunc 2>/dev/null
# Fail-loud se il volume sborda dalla partizione (dd estenderebbe il file oltre
# il layout dichiarato sopra).
OUT_SIZE="$(wc -c < "$OUT")"
if [ "$OUT_SIZE" != "$OUT_BYTES" ]; then
    echo "[arca-part] errore: size $OUT_SIZE (attesi $OUT_BYTES), abort"
    exit 1
fi

# 4. Verifiche fail-loud (mai un'immagine muta in QEMU).
SIG="$(dd if="$OUT" bs=1 skip=510 count=2 2>/dev/null | od -An -tx1 | tr -d ' \n')"
if [ "$SIG" != "55aa" ]; then
    echo "[arca-part] errore: signature MBR a 0x1FE assente (got $SIG), abort"
    exit 1
fi
MAGIC="$(dd if="$OUT" bs=1 skip=$((63 * 512)) count=4 2>/dev/null)"
if [ "$MAGIC" != "ACFS" ]; then
    echo "[arca-part] errore: magic ArcaFS a LBA63 assente (got $MAGIC), abort"
    exit 1
fi

echo "[arca-part] creata $OUT (MBR + ArcaFS a offset LBA63, sig + magic verificati)"
