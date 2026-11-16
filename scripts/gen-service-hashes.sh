#!/usr/bin/env bash
# Genera il manifest degli hash dei servizi (Fase 36, Strato 2 di ADR-0026).
#
# Calcola FNV-1a a 64 bit (stesso algoritmo di `syscall_numbers::image_hash`,
# single source dell'identita' misurata) sui `.bin` finali dei servizi nativi
# (userland/build) e della personalita' POSIX (flavours/posix/build, Fase 58.4)
# e scrive `build-meta/manifest.txt` + `build-meta/policy.txt`: **dati**
# seedati in `sys:manifest` / `sys:policy` da arca-tool.sh (ADR-0078, fase 1).
# init, cardo e usertests li leggono a runtime, quindi cambiare un servizio non
# ricompila piu' il kernel.
#
# Correttezza dei byte misurati: gli stessi file vengono (a) embeddati nel
# kernel via `include_bytes!` (init/disk/fs) e (b) copiati verbatim su /fat
# via `inject-bins.sh` (mcopy non tocca il contenuto): l'hash qui calcolato e'
# quello dei byte effettivamente caricati dal loader ELF in entrambi i path.
#
# Oltre agli hash emette `build-meta/service_policy.rs` (Fase 45, sandbox
# build): tabella `SERVICE_POLICY: &[(hash, ops_mask)]` con la mask dei
# diritti FS consentiti a CIASCUN servizio noto (cardo la applica come
# tetto via `peer_info`, vedi `userland/fs/src/policy.rs`). I servizi TCB
# hanno ALL (fiducia per parentela+hash, Strato 1+2); i programmi di terzi
# hanno righe restrittive esplicite (sotto: solo runhello oggi — il futuro
# toolchain avra' le sue righe, zero redesign). Gli hash ignoti al manifest
# cadono nel default restrittivo di cardo (niente MOUNT/UMOUNT/GRANT/PIPE).
#
# Fail-loud (set -euo + check espliciti): binari mancanti/vuoti o output non
# scrivibile = build interrotta, mai manifest stale silenzioso. Lo script
# rigenera SEMPRE da zero (niente append): rieseguire e' idempotente.
set -euo pipefail
cd "$(dirname "$0")/.."

# Fase 58.4: i servizi nativi stanno in userland/build, la personalita' POSIX
# in flavours/posix/build (server/shell/cli). Il manifest li copre entrambi.
BUILD_DIRS=(userland/build flavours/posix/build)
OUT_DIR="build-meta"
OUT="$OUT_DIR/manifest.txt"
POL_OUT="$OUT_DIR/policy.txt"

for d in "${BUILD_DIRS[@]}"; do
    if [ ! -d "$d" ]; then
        echo "[gen-hashes] ERROR: $d mancante (build-userland.sh/build-posix.sh prima)" >&2
        exit 1
    fi
done

mkdir -p "$OUT_DIR"
man_tmp="$OUT.tmp"
pol_tmp="$POL_OUT.tmp"

python3 - "${BUILD_DIRS[@]}" "$man_tmp" "$pol_tmp" <<'EOF'
import glob, hashlib, os, sys

def fnv1a(data: bytes) -> int:
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h

dirs = sys.argv[1:-2]
man_tmp, pol_tmp = sys.argv[-2], sys.argv[-1]
bins = sorted(p for d in dirs for p in glob.glob(os.path.join(d, "*.bin")))
if not bins:
    sys.exit("nessun .bin in %s" % ", ".join(dirs))
# rector.bin e cardo.bin ESCLUSI: entrambi includono il manifest a compile
# time (rector: expected_hash; cardo: driver_name_of), quindi il loro hash nel
# manifest sarebbe stale-by-construction E instabile (ciclo: il binario
# incorpora l'hash di se stesso → ogni build lo cambia → la successiva lo
# ricambia, mai fixpoint — osservato: HASH_CARDO flippa a ogni run).
# Non servono: init non verifica se stesso (impossibile per costruzione, lo
# misura il kernel) e non controlla fs (embedded, TCB); cardo non pinna se
# stesso (la regola same-image confronta due peer vivi, niente manifest).
# Il manifest copre esattamente i servizi caricati da disco + disk (embedded
# ma senza ciclo: block non include il manifest) — per questi il fixpoint
# e' raggiunto in UN passaggio (i loro binari non incorporano alcun hash).
bins = [p for p in bins if os.path.basename(p) not in ("rector.bin", "cardo.bin")]

# ADR-0078 (fase 1): il manifest e la policy sono **dati**, non codice.
# Emettiamo due file seedati in `sys:` da arca-tool.sh; init e cardo li
# leggono a runtime, quindi cambiare un servizio NON ricompila piu' il kernel.
#
#   manifest.txt  -> sys:manifest     `nome <blake2s-256 hex>` per riga
#   policy.txt    -> sys:policy       `<hi16> <lo16> <mask3> <nome>` per riga
#
# L'identita' e' `ImageId` (BLAKE2s-128, primi 16 byte del digest): init la
# ricava dal digest stesso, quindi una sola fonte copre identita' e
# content-hash.
POLICY = {
    "userrunhello": 0x00F,  # OPEN|READ|WRITE|READDIR (programma di terzi)
    "userarca": 0x00B,      # OPEN|READ|READDIR
    "netdev": 0x000,
    "net": 0x000,
    "netecho": 0x000,
}
DEFAULT_MASK = 0x1FFF  # ALL (servizi TCB, incluso EXECUTE)

manifest = []
policy = []
for p in bins:
    with open(p, "rb") as f:
        data = f.read()
    if not data:
        sys.exit("binario vuoto: %s" % p)
    stem = os.path.splitext(os.path.basename(p))[0]
    digest = hashlib.blake2s(data).digest()
    id_lo = int.from_bytes(digest[0:8], "little")
    id_hi = int.from_bytes(digest[8:16], "little")
    manifest.append("%s %s" % (stem, digest.hex()))
    mask = POLICY.get(stem, DEFAULT_MASK)
    policy.append("%016x %016x %03x %s" % (id_hi, id_lo, mask, stem))

with open(man_tmp, "w") as f:
    f.write("# Generato da scripts/gen-service-hashes.sh — non modificare a mano.\n")
    f.write("\n".join(manifest) + "\n")
print("[gen-hashes] manifest %d righe -> %s" % (len(manifest), man_tmp))

with open(pol_tmp, "w") as f:
    f.write("# Generato da scripts/gen-service-hashes.sh — non modificare a mano.\n")
    f.write("\n".join(policy) + "\n")
print("[gen-hashes] policy %d righe -> %s" % (len(policy), pol_tmp))
EOF

mv "$man_tmp" "$OUT"
echo "[gen-hashes] scritto $OUT"
mv "$pol_tmp" "$POL_OUT"
echo "[gen-hashes] scritta $POL_OUT"
