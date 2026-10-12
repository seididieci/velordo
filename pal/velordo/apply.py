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
import re
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
    "sys/fs/velordo.rs",
    "sys/fs/vfslib/mod.rs",
    "sys/sync/futex/velordo.rs",
    "sys/thread_local/key/velordo.rs",
]

# (sorgente-relativa-a-pal/, destinazione-in-std/src): copie vendored da
# fuori pal/new (source unica nel repo, mai derive silenziose).
COPY_MAP = [
    # Relativi a pal/velordo/new (tre livelli sopra = root repo).
    ("../../../libs/valloc/src/core.rs", "sys/alloc/valloc_core.rs"),
    # vfscli (S2.2): client FS condiviso; `abi.rs` e' GENERATO sotto (stessi
    # nomi della controparte repo, letterali al posto dei re-export).
    ("../../../libs/vfscli/src/base.rs", "sys/fs/vfslib/base.rs"),
    ("../../../libs/vfscli/src/session.rs", "sys/fs/vfslib/session.rs"),
    ("../../../libs/vfscli/src/ring.rs", "sys/fs/vfslib/ring.rs"),
    ("../../../libs/vfscli/src/sync.rs", "sys/fs/vfslib/sync.rs"),
    ("../../../libs/vfscli/src/cwd.rs", "sys/fs/vfslib/cwd.rs"),
    ("../../../libs/vfscli/src/error.rs", "sys/fs/vfslib/error.rs"),
]

# (file-relativo-a-std/src, anchor, inserimento): anchor deve occorrere UNA volta.
ARMS = [
    (
        "sys/pal/mod.rs",
        '    target_os = "uefi" => {\n        mod uefi;\n        pub use self::uefi::*;\n    }\n',
        # pub(crate): altri moduli raggiungono `crate::sys::pal::velordo::numbers`
        # (ABI generati, S2.2); il glob sotto riesporta solo le funzioni.
        '    target_os = "velordo" => {\n        pub(crate) mod velordo;\n        pub use self::velordo::*;\n    }\n',
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
        "sys/sync/futex/mod.rs",
        '    target_os = "hermit" => {\n        mod hermit;\n        pub use hermit::*;\n    }\n',
        '    target_os = "hermit" => {\n        mod hermit;\n        pub use hermit::*;\n    }\n    target_os = "velordo" => {\n        pub(crate) mod velordo;\n        pub use velordo::*;\n    }\n',
    ),
    (
        "sys/sync/mutex/mod.rs",
        '        target_os = "hermit",\n',
        '        target_os = "hermit",\n        target_os = "velordo",\n',
    ),
    (
        "sys/sync/condvar/mod.rs",
        '        target_os = "hermit",\n',
        '        target_os = "hermit",\n        target_os = "velordo",\n',
    ),
    (
        "sys/sync/rwlock/mod.rs",
        '        target_os = "hermit",\n',
        '        target_os = "hermit",\n        target_os = "velordo",\n',
    ),
    (
        "sys/sync/once/mod.rs",
        '        target_os = "hermit",\n',
        '        target_os = "hermit",\n        target_os = "velordo",\n',
    ),
    (
        "sys/sync/thread_parking/mod.rs",
        '        target_os = "hermit",\n',
        '        target_os = "hermit",\n        target_os = "velordo",\n',
    ),
    (
        "sys/sync/mod.rs",
        'mod futex;',
        'pub(crate) mod futex;',
        "inplace",
    ),
    (
        "sys/thread_local/mod.rs",
        '        target_os = "motor" => {\n            mod racy;\n            #[cfg(test)]\n            mod tests;\n            pub(super) use moto_rt::tls::{Key, get, set};\n            use moto_rt::tls::{create, destroy};\n            pub(super) use racy::LazyKey;\n        }',
        '        target_os = "velordo" => {\n            mod racy;\n            mod velordo;\n            pub(super) use racy::LazyKey;\n            pub(crate) use velordo::{destroy_tls, register_slot, release_base};\n            pub(super) use velordo::{Key, get, set};\n            use velordo::{create, destroy};\n        }',
    ),
    (
        "sys/thread/mod.rs",
        '    target_os = "uefi" => {\n        mod uefi;\n        pub use uefi::{available_parallelism, sleep};\n        #[expect(dead_code)]\n        mod unsupported;\n        pub use unsupported::{DEFAULT_MIN_STACK_SIZE, Thread, current_os_id, set_name, yield_now};\n    }\n',
        '    target_os = "velordo" => {\n        mod velordo;\n        pub use velordo::{DEFAULT_MIN_STACK_SIZE, Thread, available_parallelism, current_os_id, init_main_tls, set_name, sleep, yield_now};\n    }\n',
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
        "sys/fs/mod.rs",
        '    target_os = "hermit" => {\n        mod hermit;\n        use hermit as imp;\n    }\n',
        '    target_os = "hermit" => {\n        mod hermit;\n        use hermit as imp;\n    }\n    target_os = "velordo" => {\n        mod velordo;\n        use velordo as imp;\n    }\n',
    ),
    (
        "sys/thread_local/mod.rs",
        '        any(target_os = "hermit", target_os = "xous") => {',
        '        any(target_os = "hermit", target_os = "velordo", target_os = "xous") => {',
        "inplace",
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


def gen_numbers(repo_root: Path) -> str:
    """Estrae TUTTI i `pub const NOME: int = expr` da syscall-numbers e li
    emette come letterali (S2.2: single source vero, mai duplicati a mano).
    Forme supportate: letterali dec/hex, `!0u64 - N`, aritmetica `*+-`,
    `u64::MAX`. Qualunque altra forma = errore LOUD (mai valori skippati
    in silenzio)."""
    src = repo_root / "syscall-numbers" / "src" / "lib.rs"
    assert src.exists(), f"syscall-numbers assente: {src}"
    text = src.read_text()
    rows = re.findall(r"^pub const (\w+): (\w+) = ([^;]+);", text, re.M)
    assert len(rows) > 50, f"syscall-numbers illeggibile? ({len(rows)} const)"
    out = [
        "//! ABI numerica Velordo — GENERATO da apply.py (S2.2).",
        "//! MAI modificare a mano: single source = syscall-numbers/src/lib.rs.",
        "#![allow(dead_code)]",
        "",
    ]
    seen = set()

    def emit_const(name, ty, rhs):
        assert name not in seen, f"const duplicata: {name}"
        seen.add(name)
        e = rhs.strip().replace("u64::MAX", str(2**64 - 1)).replace("!0u64", str(2**64 - 1))
        assert re.fullmatch(r"[0-9xXa-fA-F _+*()\-]+", e), f"forma non supportata: {name} = {rhs}"
        v = eval(e, {"__builtins__": {}}, {})
        assert isinstance(v, int) and v >= 0, f"valore non intero: {name}"
        out.append(f"pub const {name}: {ty} = {v};")

    for name, ty, rhs in rows:
        emit_const(name, ty, rhs)
    # Enum fieldless con discriminanti esplicite (solo `Service`: serve
    # l'id numerico per il lookup raw; qualunque altro enum = LOUD).
    for m in re.finditer(r"pub enum (\w+) \{(.*?)\n\}", text, re.S):
        ename, body = m.group(1), m.group(2)
        assert ename == "Service", f"enum non gestito: {ename}"
        for line in body.splitlines():
            mm = re.match(r"\s*(\w+) = (\d+),?", line)
            if mm:
                emit_const(f"SERVICE_{mm.group(1).upper()}", "u64", mm.group(2))
    # Spot-check fail-loud sui critici (se mancano, la PAL non compila comunque,
    # ma qui il messaggio dice DOVE guardare).
    for must in ("SYS_WRITE", "SYS_MMAP", "SYS_RING_ALLOC", "USER_FS_BUFFER", "R_OPEN"):
        assert must in seen, f"const critica assente: {must}"
    return "\n".join(out) + "\n"


def main() -> None:
    lib = Path(sys.argv[1])
    new = Path(sys.argv[2])
    std = lib / "std" / "src"
    assert (lib / "std" / "Cargo.toml").exists(), f"library-dir non valida: {lib}"
    repo_root = (new / ".." / ".." / "..").resolve()

    # Numeri ABI generati (prima di copiare: i file PAL li usano). La stessa
    # estrazione alimenta `vfslib/abi.rs` (stessi nomi, per le copie vfscli).
    numbers = gen_numbers(repo_root)
    np = std / "sys" / "pal" / "velordo" / "numbers.rs"
    np.parent.mkdir(parents=True, exist_ok=True)
    np.write_text(numbers)
    print(f"[pal] gen numbers.rs ({len(numbers.splitlines())} righe da syscall-numbers)")
    ab = std / "sys" / "fs" / "vfslib" / "abi.rs"
    ab.parent.mkdir(parents=True, exist_ok=True)
    ab.write_text(
        "//! ABI per il client FS — GENERATO come numbers.rs (S2.2).\n"
        "//! Stessi nomi di `libs/vfscli/src/abi.rs` (li' re-export).\n"
        "#![allow(dead_code)]\n\n" + "\n".join(
            l for l in numbers.splitlines()
            if l.startswith("pub const ") and not l.startswith("pub const SYS_")
        ) + "\n"
    )
    print("[pal] gen vfslib/abi.rs (senza SYS_*: il provider usa raw dedicati)")

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
