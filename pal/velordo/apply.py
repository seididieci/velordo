#!/usr/bin/env python3
"""Applica la PAL Velordo ai sorgenti della library Rust (S1.3).

Copia i file nuovi (pal/velordo/new/...) e inserisce gli arm
`target_os = "velordo"` nei dispatch esistenti. Ogni inserimento ha un
anchor esatto: a mismatch lo script fallisce LOUD (mai patch parziali
sul sorgente del toolchain). Idempotente: arm gia' presenti = skip.

Uso: apply.py <library-dir> <pal-new-dir>
  library-dir = copia scrivibile di library/ (rust-src)
  pal-new-dir = pal/velordo/new
"""

import os
import shutil
import sys
from pathlib import Path

NEW_FILES = [
    "sys/pal/velordo/mod.rs",
    "sys/alloc/velordo.rs",
    "sys/random/velordo.rs",
    "sys/stdio/velordo.rs",
    "sys/time/velordo.rs",
    "sys/thread/velordo.rs",
    "sys/io/error/velordo.rs",
    "os/velordo/mod.rs",
    "os/velordo/os_str.rs",
    "sys/args/velordo.rs",
    "sys/env/velordo.rs",
    "sys/paths/velordo.rs",
]

# (sorgente-relativa-a-pal/, destinazione-in-std/src): copie vendored da
# fuori pal/new (source unica nel repo, mai derive silenziose).
COPY_MAP = [
    # Relativi a pal/velordo/new (tre livelli sopra = root repo).
    ("../../../libs/valloc/src/core.rs", "sys/alloc/valloc_core.rs"),
]

# (file-relativo-a-std/src, anchor, inserimento): anchor deve occorrere UNA volta.
ARMS = [
    (
        "sys/pal/mod.rs",
        '    target_os = "uefi" => {\n        mod uefi;\n        pub use self::uefi::*;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use self::velordo::*;\n    }\n',
    ),
    (
        "sys/alloc/mod.rs",
        '    target_os = "xous" => {\n        mod xous;\n        use xous as imp;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        use velordo as imp;\n    }\n',
    ),
    (
        "sys/alloc/mod.rs",
        'pub use imp::{alloc, dealloc, realloc};',
        'pub use imp::{alloc, dealloc, realloc, stats};',
        "inplace",
    ),
    (
        "sys/random/mod.rs",
        '    target_os = "zkvm" => {\n        mod zkvm;\n        pub use zkvm::fill_bytes;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use velordo::fill_bytes;\n    }\n',
    ),
    (
        "sys/stdio/mod.rs",
        '    target_os = "xous" => {\n        mod xous;\n        pub use xous::*;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use velordo::*;\n    }\n',
    ),
    (
        "sys/time/mod.rs",
        '    target_os = "xous" => {\n        mod xous;\n        use xous as imp;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        use velordo as imp;\n    }\n',
    ),
    (
        "sys/thread/mod.rs",
        '    target_os = "uefi" => {\n        mod uefi;\n        pub use uefi::{available_parallelism, sleep};\n        #[expect(dead_code)]\n        mod unsupported;\n        pub use unsupported::{DEFAULT_MIN_STACK_SIZE, Thread, current_os_id, set_name, yield_now};\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use velordo::sleep;\n        #[expect(dead_code)]\n        mod unsupported;\n        pub use unsupported::{DEFAULT_MIN_STACK_SIZE, Thread, available_parallelism, current_os_id, set_name, yield_now};\n    }\n',
    ),
    (
        "sys/exit.rs",
        '        target_os = "xous" => crate::os::xous::ffi::exit(code as u32),\n',
        '        target_os = "velordo" => crate::sys::pal::exit(code),\n',
    ),
    (
        "sys/io/error/mod.rs",
        '    target_os = "xous" => {\n        mod xous;\n        pub use xous::*;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use velordo::*;\n    }\n',
    ),
    (
        "sys/thread_local/mod.rs",
        '        target_os = "uefi",\n        target_os = "zkvm",\n        target_os = "trusty",\n        target_os = "vexos",\n    ) => {\n        mod no_threads;',
        '        target_os = "uefi",\n        target_os = "velordo",\n        target_os = "zkvm",\n        target_os = "trusty",\n        target_os = "vexos",\n    ) => {\n        mod no_threads;',
        "tls",
    ),
    (
        "sys/thread_local/mod.rs",
        '            all(target_family = "wasm", not(target_env = "p3")),\n            target_os = "uefi",\n            target_os = "zkvm",\n            target_os = "trusty",\n            target_os = "vexos",\n        ) => {\n            pub(crate) fn enable() {',
        '            all(target_family = "wasm", not(target_env = "p3")),\n            target_os = "uefi",\n            target_os = "velordo",\n            target_os = "zkvm",\n            target_os = "trusty",\n            target_os = "vexos",\n        ) => {\n            pub(crate) fn enable() {',
        "guard",
    ),
    (
        "os/mod.rs",
        '#[cfg(target_os = "uefi")]\npub mod uefi;\n',
        '#[cfg(target_os = "uefi")]\npub mod uefi;\n#[cfg(target_os = "velordo")]\npub mod velordo;\n',
        "inplace",
    ),
    (
        "sys/args/mod.rs",
        '    target_os = "wasi",\n    target_os = "xous",\n))]\nmod common;',
        '    target_os = "wasi",\n    target_os = "xous",\n    target_os = "velordo",\n))]\nmod common;',
        "inplace",
    ),
    (
        "sys/args/mod.rs",
        '    target_os = "xous" => {\n        mod xous;\n        pub use xous::*;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use velordo::*;\n    }\n',
    ),
    (
        "sys/env/mod.rs",
        '    target_os = "wasi",\n    target_os = "xous",\n))]\nmod common;',
        '    target_os = "wasi",\n    target_os = "xous",\n    target_os = "velordo",\n))]\nmod common;',
        "inplace",
    ),
    (
        "sys/env/mod.rs",
        '    target_os = "xous" => {\n        mod xous;\n        pub use xous::*;\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use velordo::*;\n    }\n',
    ),
    (
        "sys/paths/mod.rs",
        '    target_os = "hermit" => {\n        mod hermit;\n        #[expect(dead_code)]\n        mod unsupported;\n        mod imp {\n            pub use super::hermit::{getcwd, temp_dir};\n            pub use super::unsupported::{\n                JoinPathsError, SplitPaths, chdir, current_exe, home_dir, join_paths, split_paths,\n            };\n        }\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        #[expect(dead_code)]\n        mod unsupported;\n        mod imp {\n            pub use super::velordo::{chdir, getcwd, temp_dir};\n            pub use super::unsupported::{\n                JoinPathsError, SplitPaths, current_exe, home_dir, join_paths, split_paths,\n            };\n        }\n    }\n',
    ),
]


def main() -> None:
    lib = Path(sys.argv[1])
    new = Path(sys.argv[2])
    std = lib / "std" / "src"
    assert (lib / "std" / "Cargo.toml").exists(), f"library-dir non valida: {lib}"

    for rel in NEW_FILES:
        src = new / rel
        dst = std / rel
        assert src.exists(), f"file PAL mancante: {src}"
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
        print(f"[pal] new {rel}")

    for src_rel, dst_rel in COPY_MAP:
        src = Path(os.path.normpath(new / src_rel))
        dst = std / dst_rel
        assert src.exists(), f"file condiviso mancante: {src}"
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
        print(f"[pal] copy {src_rel} -> {dst_rel}")

    # Secondo elenco no_threads in thread_local/guard (stesso set, altro punto).
    guard_anchor = None
    for entry in ARMS:
        if len(entry) == 4:
            rel, anchor, insert, _tag = entry
            additive = False  # modifica in-place (lista esistente)
        else:
            rel, anchor, insert = entry
            additive = True  # aggiunta dopo l'anchor (l'anchor resta)
        p = std / rel
        text = p.read_text()
        # Skip per-edit (non per-file: thread_local ha DUE liste): salta
        # solo se l'inserimento esatto e' gia' presente.
        if insert.strip() in text:
            print(f"[pal] skip {rel} (gia' applicato)")
            continue
        n = text.count(anchor)
        assert n == 1, f"{rel}: anchor trovato {n}x (atteso 1)"
        text = text.replace(anchor, anchor + insert if additive else insert)
        p.write_text(text)
        print(f"[pal] arm {rel}")

    print("[pal] applicata: verifica con build")


if __name__ == "__main__":
    main()
