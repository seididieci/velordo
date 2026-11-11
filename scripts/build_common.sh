#!/usr/bin/env bash
# Funzioni condivise per la build freestanding dei crate utente/test.
#
# Usato SOLO via `source` da scripts/build-userland.sh e scripts/build-tests.sh,
# che si posizionano in CWD=radice del repo prima di chiamare `build_one`.
#
# Per ogni binario:
#   - CWD=radice: il config discovery di Cargo risale e trova la root .cargo
#     (build-std). Le rustflags user sono impostate qui via
#     CARGO_TARGET_*_RUSTFLAGS (unica fonte), NON nei file .cargo dei crate.
#   - Ogni binario ha il proprio linker script (-T) che forza caricamento a
#     USER_CODE e KEEP(.text._start).
#
# Le variabili `BUILD` e `CARGO_TARGET_DIR` devono essere esportate dal
# chiamante (es. userland/build e testland/build).

objcopy_tool() {
    if command -v llvm-objcopy >/dev/null 2>&1; then
        command -v llvm-objcopy
        return
    fi

    if command -v rust-objcopy >/dev/null 2>&1; then
        command -v rust-objcopy
        return
    fi

    local sysroot
    sysroot="$(rustc --print sysroot 2>/dev/null || true)"
    if [ -n "$sysroot" ]; then
        if [ -x "$sysroot/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy" ]; then
            printf '%s\n' "$sysroot/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy"
            return
        fi
        if [ -x "$sysroot/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy" ]; then
            printf '%s\n' "$sysroot/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy"
            return
        fi
    fi

    return 1
}

build_one() {
    local crate="$1"            # path del crate, es. userland/console
    local ld="$2"               # linker script del binario
    local out_name="$3"         # nome del .bin prodotto
    local elfname="$4"          # nome del binario ELF (dal Cargo.toml)
    local extra_flags="${*:5}"  # flag cargo extra opzionali (es. --no-default-features)

    local LD_ABS="$(pwd)/$ld"
    # N6.2.0 (ADR-0071): lo userland usa un target proprio con SSE2
    # (`targets/x86_64-unknown-velordo-user.json`). Il kernel resta su
    # `x86_64-unknown-none` soft-float: la separazione e' voluta. SSE2 qui e'
    # sicuro perche' il kernel salva/ripristina XMM (CR4.OSFXSR +
    # fxsave/fxrstor per pid, S2.5b).
    local USER_TARGET_JSON="$(pwd)/targets/x86_64-unknown-velordo-user.json"
    local USER_TARGET_NAME="x86_64-unknown-velordo-user"
    export CARGO_TARGET_X86_64_UNKNOWN_VELORDO_USER_RUSTFLAGS="\
-C relocation-model=pic \
-C link-arg=-T${LD_ABS} \
-C link-arg=--apply-dynamic-relocs"

    echo "[build] $elfname (freestanding, PIC @ USER_CODE, SSE2)"
    # shellcheck disable=SC2086
    # Niente `compiler-builtins-mem`: con SSE2 LLVM emette chiamate a
    # memcpy/memset/memcmp e i simboli li fornisce `civis::rt` (rep movsb /
    # rep stosq, la stessa forma che LLVM inlinea senza SSE). Il default di
    # compiler_builtins su x86_64 (rep movsb + rep movsq + rep movsb) costa
    # +46% su `testsarca` append 512 KiB sotto QEMU TCG (ADR-0071).
    cargo build --release --manifest-path "$crate/Cargo.toml" \
        --target "$USER_TARGET_JSON" -Zjson-target-spec $extra_flags

    local ELF="$TARGET_DIR/$USER_TARGET_NAME/release/$elfname"
    local OUT="$BUILD/$out_name"
    mkdir -p "$BUILD"
    local OBJCOPY
    if ! OBJCOPY="$(objcopy_tool)"; then
        echo "[build] ERROR: llvm-objcopy/rust-objcopy non trovato nel PATH" >&2
        echo "[build] Installa llvm-tools-preview o aggiungi l'objcopy del toolchain al PATH" >&2
        exit 1
    fi
    # Fase 31: si embedda/inietta l'ELF (stripped), non piu' il flat `-O binary`.
    # Il kernel lo carica per-segmento (`kernel/src/elf.rs`, W^X). `--strip-all`
    # rimuove symtab/debug ma CONSERVA i program header (PT_LOAD). Il nome
    # output resta `.bin` (il loader sniffa il magic ELF).
    "$OBJCOPY" --strip-all "$ELF" "$OUT"

    local SIZE=$(stat -c %s "$OUT")
    echo "[build] $OUT ($SIZE bytes)"
}
