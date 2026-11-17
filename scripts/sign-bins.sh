#!/usr/bin/env bash
# Firma delle immagini (ADR-0078, fase 2).
#
# Il sidecar e' indicizzato dal **contenuto**, non dal path: una firma vale per
# tutti gli oggetti con quegli stessi byte (`sys`, `vela`, `usr`, il mirror
# `ns`). Quindi qui si firma un file, non una destinazione:
#
#   sig:<blake2s-256 hex>  ->  [VSIG][key_id:8][sig:64]     (76 byte)
#
# La chiave dipende dal **bucket di spedizione** (chi decide la classe):
# `usr` = programmi utente -> chiave `thirdparty`; `sys`/`vela` = sistema ->
# chiave `system`. Il chiamante e' `scripts/arca-tool.sh` (seed), che conosce
# gia' la mappa file->bucket: il formato non viene duplicato qui.
#
# Uso:
#   scripts/sign-bins.sh build                    # costruisce il tool host
#   scripts/sign-bins.sh one <bucket> <file>      # stampa "<digest> <sidecar>"
#   scripts/sign-bins.sh digest <file>            # BLAKE2s-256 hex
#   scripts/sign-bins.sh anchor                   # rigenera build-meta/anchor.txt
#
# Chiavi: `tools/sign/keys/<classe>-<nome>.key` (seed Ed25519 in hex). Le due
# chiavi nel repo sono di **test**, non di produzione; `VELORDO_SIGN_KEY_SYSTEM`
# e `VELORDO_SIGN_KEY_THIRDPARTY` le sostituiscono (release) e
# `VELORDO_SIGN_KEYS_DIR` cambia la directory dell'anchor.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

SIGN_BIN="${SIGN_TOOL_BIN:-$ROOT/build-meta/sign-tool}"
KEYS_DIR="${VELORDO_SIGN_KEYS_DIR:-$ROOT/tools/sign/keys}"

# Rebuild del tool se le sue sorgenti (o `libs/sig`) sono piu' nuove del
# binario. Il `.cargo/config.toml` della radice forza il target bare-metal e
# Cargo lo legge dalla CWD: si compila da una directory neutra (come
# `arca-tool.sh`), col target dir del manifest.
ensure_tool() {
    local src="$ROOT/tools/sign/Cargo.toml $ROOT/libs/sig/Cargo.toml"
    local newer=0
    if [ ! -x "$SIGN_BIN" ]; then
        newer=1
    else
        for f in $src "$ROOT"/tools/sign/src/*.rs "$ROOT"/libs/sig/src/*.rs; do
            [ "$f" -nt "$SIGN_BIN" ] && newer=1
        done
    fi
    [ "$newer" = "0" ] && return 0
    mkdir -p "$(dirname "$SIGN_BIN")"
    ( cd "${TMPDIR:-/tmp}" && cargo build --release \
        --manifest-path "$ROOT/tools/sign/Cargo.toml" >/dev/null )
    cp "$ROOT/tools/sign/target/release/sign" "$SIGN_BIN"
    echo "[sign] tool in $SIGN_BIN" >&2
}

key_for_bucket() {
    case "$1" in
        # Programmi utente: chiave di terze parti (classe ThirdParty).
        usr) echo "${VELORDO_SIGN_KEY_THIRDPARTY:-$KEYS_DIR/thirdparty-test-only.key}" ;;
        # Codice di sistema e driver: chiave di sistema (classe System).
        sys|vela) echo "${VELORDO_SIGN_KEY_SYSTEM:-$KEYS_DIR/system-test-only.key}" ;;
        *)
            echo "[sign] errore: bucket '$1' non ammette firme (solo sys/vela/usr)" >&2
            exit 1
            ;;
    esac
}

out_dir() {
    mkdir -p "$ROOT/build-meta/sig"
    echo "$ROOT/build-meta/sig"
}

case "${1:-}" in
build)
    ensure_tool
    ;;
digest)
    [ $# -eq 2 ] || { echo "uso: sign-bins.sh digest <file>" >&2; exit 2; }
    ensure_tool
    "$SIGN_BIN" digest "$2"
    ;;
one)
    [ $# -eq 3 ] || { echo "uso: sign-bins.sh one <bucket> <file>" >&2; exit 2; }
    bucket="$2"; file="$3"
    [ -f "$file" ] || { echo "[sign] errore: $file mancante" >&2; exit 1; }
    key="$(key_for_bucket "$bucket")"
    [ -f "$key" ] || { echo "[sign] errore: chiave $key mancante (bucket $bucket)" >&2; exit 1; }
    ensure_tool
    digest="$("$SIGN_BIN" digest "$file")"
    sidecar="$(out_dir)/$digest.sig"
    # Firma sempre (deterministica): se il contenuto non cambia il sidecar e'
    # identico byte per byte, quindi l'idempotenza e' nel merito, non nel mtime.
    "$SIGN_BIN" sign "$key" "$file" "$sidecar" >/dev/null
    echo "$digest $sidecar"
    ;;
anchor)
    ensure_tool
    mkdir -p "$ROOT/build-meta"
    out="$ROOT/build-meta/anchor.txt"
    : > "$out"
    shopt -s nullglob
    for k in "$KEYS_DIR"/*.key; do
        base="$(basename "$k" .key)"
        cls="${base%%-*}"
        case "$cls" in
            system|thirdparty) ;;
            *)
                echo "[sign] errore: $k: prefisso di classe ignoto (system|thirdparty)" >&2
                exit 1
                ;;
        esac
        read -r kid pk < <("$SIGN_BIN" pubkey "$k")
        printf '%s %s %s %s\n' "$kid" "$pk" "$cls" "$base" >> "$out"
    done
    if [ ! -s "$out" ]; then
        echo "[sign] errore: nessuna chiave in $KEYS_DIR (anchor vuoto)" >&2
        exit 1
    fi
    echo "[sign] anchor: $(wc -l < "$out") chiavi ($out)" >&2
    ;;
*)
    echo "uso: sign-bins.sh build|one <bucket> <file>|digest <file>|anchor" >&2
    exit 2
    ;;
esac
