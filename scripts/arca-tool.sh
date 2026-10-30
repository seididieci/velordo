#!/usr/bin/env bash
# Build del tool host `arca` (Fase 54, P5) e creazione di `arca.img` con seed.
#
# Il tool e' un binario HOST (std), ma il `.cargo/config.toml` della radice
# forza `target = x86_64-unknown-none` + `build-std` per tutti i crate del
# repo. Il config discovery di Cargo parte dalla CWD (non dal manifest), quindi
# si compila da una directory neutra con `--manifest-path`: il tool prende la
# std del toolchain, i crate bare-metal restano col loro config.
#
# Nota: --manifest-path fa usare a Cargo target dir relativa al manifest, non
# alla CWD. Il binario finisce in $ROOT/tools/arca/target/release/ (non in
# ${TMPDIR}/target).
#
# Uso: scripts/arca-tool.sh [out_img]   (default userland/disk/arca.img)
#
# Fase 2 (root su volume): `arca.img` ha uuid 4152434100000001, viene seedato
# con i file di boot (`/bin`, `/test`, fixture). Derivate (partizione MBR +
# GPT) ricevono UUID diversi via `arca uuid` per evitare collisioni — il kernel
# sceglie la root per UUID (mai per lettera/scan).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/userland/disk/arca.img}"
TOOL_BIN="${ARCA_TOOL_BIN:-$ROOT/build-meta/arca}"

mkdir -p "$(dirname "$TOOL_BIN")" "$(dirname "$OUT")"
( cd "${TMPDIR:-/tmp}" && cargo build --release --manifest-path "$ROOT/tools/arca/Cargo.toml" )
cp "$ROOT/tools/arca/target/release/arca" "$TOOL_BIN"
# Crea il volume con uuid fisso (Fase 2: la root si sceglie per UUID, mai
# prima voce trovata). Derivate (arca-part.img / arca-gpt.img) usano `arca
# uuid` per ottenere UUID distinti senza resettare il volume.
# S1.2/W6: dimensione parametrica (default 128 MiB = 2048 blocchi 64 KiB:
# il gate scrive > 512 blocchi e la vecchia immagine da 32 MiB finiva lo
# spazio). Le derivate MBR/GPT seguono la dimensione del volume.
# S2.5f/F1: il run dedicato allo staticone (RUN_S25=1) alza il default a
# 384 MiB — 100 MB di payload + seed di boot + (F2) mini-sysroot. Il gate
# resta a 128 MiB: ARCA_SIZE_MIB esplicito vince sempre.
if [ "${RUN_S25:-0}" = "1" ] && [ -z "${ARCA_SIZE_MIB:-}" ]; then
    ARCA_SIZE_MIB=384
fi
# ADR-0059 (v4): il volume si presenta — label (il nome) + mountpoint
# dichiarato (vuoto: la root la sceglie la cmdline, non il volume).
$TOOL_BIN create "$OUT" --uuid 4152434100000001 --size-mib "${ARCA_SIZE_MIB:-128}" \
    --label SYSTEM --mountpoint ""
echo "[arca] tool in $TOOL_BIN, immagine $OUT"

# Seed del volume con i file di boot (Fase 2 + D1): `/bin` + `/bin/posix` +
# `/usr/bin` + `/test` + fixture. Il seed e' un build-time: il disco host legge
# i binari gia' compilati e li scrive nel bucket `ns` (percorso, vista POSIX
# `/`) e — per i servizi che init carica per object_id — nel bucket di classe
# (stessi byte, doppia chiave): `vela` = driver (toccano HW), `sys` = servizi,
# `usr` = programmi utente (reserved D1: nessun reader via obj ancora).
# Regola mirror: la chiave oggetto rispecchia il path VFS. Solo ArcaFS ha il
# layout nuovo (FAT resta piatta fino a E); solo volumi freschi (secondary
# root = 0): le derivate MBR/GPT copiano blocchi dal volume originale.
# Fail-loud: file mancante o seed fallito = exit 1, mai `|| true`.
SEED_NS=()
# seed_bin <host-rel> <ns-key> [obj-bucket obj-key]: ns sempre, obj per classi.
seed_bin() {
    if [ ! -f "$ROOT/$1" ]; then
        echo "[arca] errore: $1 mancante (build-userland.sh/build-tests.sh prima)" >&2
        exit 1
    fi
    SEED_NS+=( "ns:$2=$ROOT/$1" )
    if [ -n "${3:-}" ] && [ -n "${4:-}" ]; then
        SEED_NS+=( "$3:$4=$ROOT/$1" )
    fi
}
# Servizi /bin: driver in `vela`, resto in `sys` (D1).
seed_bin userland/build/gpu.bin bin/gpu.bin vela bin/gpu.bin
# Console server (D3, ADR-0060): servizio di sistema (bucket `sys`).
seed_bin userland/build/console.bin bin/console.bin sys bin/console.bin
seed_bin userland/build/useruptime.bin bin/uptime.bin sys bin/uptime.bin
seed_bin userland/build/vela.bin bin/vela.bin sys bin/vela.bin
seed_bin userland/build/kbd.bin bin/kbd.bin vela bin/kbd.bin
seed_bin userland/build/porta.bin bin/porta.bin sys bin/porta.bin
seed_bin userland/build/usertime.bin bin/time.bin vela bin/time.bin
seed_bin userland/build/vestigia.bin bin/vestigia.bin sys bin/vestigia.bin
# Servizio identita'/autenticazione utenti (ADR-0055) + il suo client login
# (ADR-0059: il boot non carica piu' da /fat, tutto da ArcaFS).
seed_bin userland/build/identis.bin bin/identis.bin sys bin/identis.bin
seed_bin userland/build/identis-login.bin bin/identis-login.bin sys bin/identis-login.bin
# Personalita' POSIX in `/bin/posix` (D1: MOVE da /bin, solo ArcaFS).
seed_bin flavours/posix/build/usershell.bin bin/posix/shell.bin sys bin/posix/shell.bin
seed_bin flavours/posix/build/userposix.bin bin/posix/posix.bin sys bin/posix/posix.bin
# Programmi utente in `/usr/bin` (D1: MOVE da /bin, solo ArcaFS).
# Bucket `usr` = tipo BINARY (ADR-0064): namespace PIATTO, solo eseguibili,
# niente sottocartelle. Le chiavi oggetto NON hanno suffisso `.bin`; il mirror
# `ns` mantiene i path VFS (`usr/bin/...`).
seed_bin flavours/posix/build/userrunhello.bin usr/bin/runhello.bin usr runhello
# Personalita' POSIX come PROGRAMMA (ADR-0064 addendum): la shell nativa la
# esegue dalla chiave piatta `usershell`. Il nome processo resta `usershell`
# (meta name = chiave): il `ps` della POSIX se stessa vede ancora `usershell`,
# come quando la spawava init.
seed_bin flavours/posix/build/usershell.bin usr/bin/usershell.bin usr usershell
seed_bin userland/build/userarca.bin usr/bin/arca.bin usr arca
# create-user (ADR-0055): programma utente (l'interprete nativo lo invoca come
# comando, Fase 3.2).
seed_bin userland/build/create-user.bin usr/bin/cuuser.bin usr cuuser
# Interprete nativa (Fase 3.2, S3.2a): REPL minima, spawnata dal login (identis-login).
seed_bin userland/build/shell.bin bin/interpres.bin sys bin/interpres.bin
# Test suite /test (stessi dest 8.3 di inject-bins.sh). D2: le 6 suite che
# init carica per object_id hanno anche l'oggetto in `tst`; gli helper
# (testcli, testspin, …) vivono solo come path (dogfood VFS, D2.4).
seed_bin testland/build/usertestfs.bin test/testfs.bin tst test/testfs.bin
seed_bin testland/build/usertestfat.bin test/testfat.bin tst test/testfat.bin
seed_bin testland/build/usertestsarca.bin test/testarca.bin tst test/testarca.bin
seed_bin testland/build/usertests.bin test/tests.bin tst test/tests.bin
seed_bin testland/build/threadtest.bin test/thread.bin tst test/thread.bin
seed_bin testland/build/usertestcli.bin test/testcli.bin
seed_bin testland/build/usertestspin.bin test/testspin.bin
seed_bin testland/build/utcbstest.bin test/cbstest.bin
seed_bin testland/build/userhogheap.bin test/hogheap.bin
seed_bin testland/build/userdevreader.bin test/devreadr.bin
seed_bin testland/build/userdemo.bin test/demo.bin
seed_bin testland/build/userbench.bin test/bench.bin tst test/bench.bin
seed_bin testland/build/identistest.bin test/idtest.bin tst test/idtest.bin
seed_bin testland/build/userforeign.bin test/foreign.bin
seed_bin flavours/posix/tests/build/userposixtests.bin test/posixtst.bin tst test/posixtst.bin
# Hello std nativo (S1.3): solo se compilato via scripts/build-pal.sh (fuori
# dal gate default: rust-lang/rust + ~10 min di build). Niente fail-loud qui:
# il gate non dipende dalla PAL.
if [ -f "$ROOT/pal/build/hello-std.bin" ]; then
    SEED_NS+=( "ns:test/stdhello.bin=$ROOT/pal/build/hello-std.bin" )
    echo "[arca] + stdhello (PAL)"
fi
# Fuse s25 (S2.5d/F1): il binario che carica lo staticone e lo spawna. Solo
# chiave obj `tst` (+ ns per ispezione): init lo carica per object_id come le
# altre suite. Il payload (~100 MB) vive su ArcaFS (bucket `ns`): la read a
# offset usa F0 (`get_range` con cursore), O(1) per slice sequenziale.
if [ -f "$ROOT/testland/build/users25fuse.bin" ]; then
    SEED_NS+=( "ns:tool/s25fuse.bin=$ROOT/testland/build/users25fuse.bin" )
    SEED_NS+=( "tst:tool/s25fuse.bin=$ROOT/testland/build/users25fuse.bin" )
    echo "[arca] + s25fuse"
fi
# Staticone rustc (S2.5f/F1): payload del run dedicato (RUN_S25=1), solo se
# costruito (scripts/build-velordo-rustc.sh velordo + copia in build-meta/).
# Condizionale come stdhello: il gate default resta byte-identico e il fs non
# paga 100 MB di seed che nessuno legge. Fail-loud se richiesto ma assente:
# un run S25 senza payload e' rosso subito, non un FAIL criptico in guest.
if [ "${RUN_S25:-0}" = "1" ]; then
    if [ ! -f "$ROOT/build-meta/rustc-velordo.bin" ]; then
        echo "[arca] errore: RUN_S25=1 ma build-meta/rustc-velordo.bin assente" >&2
        exit 1
    fi
    SEED_NS+=( "ns:tool/rustc.bin=$ROOT/build-meta/rustc-velordo.bin" )
    echo "[arca] + staticone rustc (RUN_S25=1)"
fi
# Mini-sysroot F2 (S2.5f/F2): rlib+rmeta del target, target JSON e fixture di
# compilazione. Solo nel run del toolchain (RUN_S25=1) e solo se la PAL e'
# stata costruita (`scripts/build-pal.sh`, `$PAL_WORK`): senza sysroot il run
# resta quello S2.5d (`--version`), quindi qui non si fallisce loud.
if [ "${RUN_S25:-0}" = "1" ]; then
    PAL_WORK="${PAL_WORK:-/tmp/opencode-pal}"
    VE_LIB="$PAL_WORK/sysroot-velordo/lib/rustlib/x86_64-unknown-velordo/lib"
    if [ -d "$VE_LIB" ]; then
        n_sys=0
        for f in "$VE_LIB"/lib*.rlib "$VE_LIB"/lib*.rmeta; do
            [ -f "$f" ] || continue
            SEED_NS+=( "ns:usr/lib/velordo/lib/rustlib/x86_64-unknown-velordo/lib/$(basename "$f")=$f" )
            n_sys=$((n_sys + 1))
        done
        SEED_NS+=( "ns:usr/lib/velordo/x86_64-unknown-velordo.json=$ROOT/targets/x86_64-unknown-velordo.json" )
        # Fallback host: rustc risolve anche il target HOST
        # (`x86_64-unknown-velordo`, non builtin) da
        # `<sysroot>/lib/rustlib/<tuple>/target.json`.
        SEED_NS+=( "ns:usr/lib/velordo/lib/rustlib/x86_64-unknown-velordo/target.json=$ROOT/targets/x86_64-unknown-velordo.json" )
        SEED_NS+=( "ns:tool/rustc.args=$ROOT/scripts/f2/rustc.args" )
        SEED_NS+=( "ns:test/f2/hello.rs=$ROOT/scripts/f2/hello.rs" )
        echo "[arca] + mini-sysroot F2 ($n_sys file) + target JSON + fixture"
    else
        echo "[arca] mini-sysroot F2 assente ($VE_LIB): run S25 solo --version"
    fi
fi
# Script shell /test/sh (stessi di inject-bins.sh: *.txt + *.sh).
for s in scripts/sh/*.txt scripts/sh/*.sh; do
    if [ ! -f "$ROOT/$s" ]; then
        echo "[arca] errore: glob $s senza match ($ROOT/$s assente)" >&2
        exit 1
    fi
    b="$(basename "$s")"
    SEED_NS+=( "ns:test/sh/$b=$ROOT/$s" )
done
# Fixture ex-ramfs (cardo/src/server.rs populate): hello.txt + test.txt.
seed_bin userland/disk/hello.txt hello.txt
seed_bin userland/disk/test.txt test.txt
# Fixture range S2.5f/F0 (100 KiB pattern, test57 testsarca): generata qui
# in UN put (mai in repo; gli append guest-side sono O(n^2) per design di
# `put_chunk` — i file grandi si seedano da host, il guest li legge).
mkdir -p "$ROOT/testland/build"
R57_BIN="$ROOT/testland/build/r57.bin"
python3 -c "import sys; sys.stdout.buffer.write(bytes((i % 251) for i in range(102400)))" > "$R57_BIN"
SEED_NS+=( "ns:test/r57.bin=$R57_BIN" )
echo "[arca] + r57 (range fixture)"
# Fixture grandi del bench (profili full): seed host-side SOLO i file
# READ-ONLY (4M hot/cold, 1M cold-read), perche' crearli guest-side costa
# 1024/262 chunk su ArcaFS. I fixture scrivibili (AB_1M hot, W1M* cold-write,
# bch:k1m nativo) restano creati dal guest: scrivere su un file seedato da
# host fallisce dopo ~28 KiB con `ns_put` patch (bug del motore da
# investigare separatamente), mentre il file creato guest-side funziona.
# Solo con RUN_BENCH=1 (gate invariato) e solo full; il reduced non tocca
# 1M/4M.
if [ "${RUN_BENCH:-0}" = "1" ] && [ "${BENCH_REDUCED:-0}" != "1" ]; then
    BENCH_FIX="$ROOT/testland/build/bench-fixtures"
    mkdir -p "$BENCH_FIX"
    python3 -c "import sys; sys.stdout.buffer.write(bytes((i % 251) for i in range(1048576)))" > "$BENCH_FIX/1m.bin"
    python3 -c "import sys; sys.stdout.buffer.write(bytes((i % 251) for i in range(4194304)))" > "$BENCH_FIX/4m.bin"
    case "${BENCH_GROUP:-all}" in
        1m|all)
            SEED_NS+=( "ns:R1M00.BIN=$BENCH_FIX/1m.bin" "ns:R1M01.BIN=$BENCH_FIX/1m.bin" )
            ;;
    esac
    case "${BENCH_GROUP:-all}" in
        4m|all)
            SEED_NS+=( "ns:AB_4M.BIN=$BENCH_FIX/4m.bin" "ns:R4M00.BIN=$BENCH_FIX/4m.bin" )
            ;;
    esac
    echo "[arca] + fixture bench full read-only (gruppo ${BENCH_GROUP:-all})"
fi
# Gruppo per-size del profilo split: il guest legge il marker da ArcaFS
# (`/test/bench-group.txt`) e salta le righe degli altri gruppi.
if [ -n "${BENCH_GROUP:-}" ]; then
    BENCH_GROUP_FILE="$ROOT/testland/build/bench-group.txt"
    printf '%s\n' "$BENCH_GROUP" > "$BENCH_GROUP_FILE"
    SEED_NS+=( "ns:test/bench-group.txt=$BENCH_GROUP_FILE" )
    echo "[arca] + bench group $BENCH_GROUP"
fi
# Font TrueType per la console grafica (D3, ADR-0060): Maple Mono NF
# (SIL OFL 1.1, attribuzione in third_party/LICENSE.txt). Va nel bucket `ns`
# (path VFS `/font.ttf`: la console lo legge a pezzi con seek) e nel bucket
# `sys` (oggetto: chi lo cerca per id).
if [ -f "$ROOT/third_party/MapleMono-NF-Regular.ttf" ]; then
    SEED_NS+=( "ns:font.ttf=$ROOT/third_party/MapleMono-NF-Regular.ttf" )
    SEED_NS+=( "sys:font.ttf=$ROOT/third_party/MapleMono-NF-Regular.ttf" )
    echo "[arca] + font.ttf (Maple Mono NF)"
else
    echo "[arca] ATTENZIONE: third_party/MapleMono-NF-Regular.ttf mancante (console senza font: fallback VGA)" >&2
fi

# Sfondo immagine per la console grafica (D3/D4): BMP 32-bit non compresso,
# 585x135 pixel, ordine BGRA nativo del framebuffer. La console lo legge da
# `/bg.bmp` all'avvio (prima del banner).
if [ -f "$ROOT/assets/bg.bmp" ]; then
    SEED_NS+=( "ns:bg.bmp=$ROOT/assets/bg.bmp" )
    echo "[arca] + bg.bmp (sfondo immagine)"
else
    echo "[arca] ATTENZIONE: assets/bg.bmp mancante (console senza sfondo immagine)" >&2
fi

# Seed bucket Identis (ADR-0055): utente root con hash BLAKE2s-256 di "root".
# Seedato sempre (gate inclusi): il volume parte con un utente valido.
IDENTIS_CFG="$(mktemp)"
printf 'root:%s\n' "0cb8a879dc5d94ed67eb49cc7a4d3052d6a346f2e28fcd9e10e5822aba92eb39" > "$IDENTIS_CFG"
SEED_NS+=( "ns:identis:config=$IDENTIS_CFG" )
echo "[arca] + identis:config (root/root)"
$TOOL_BIN seed "$OUT" "${SEED_NS[@]}"
rm -f "$IDENTIS_CFG"
echo "[arca] seed completato su $OUT"
