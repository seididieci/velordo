#!/usr/bin/env python3
"""Confronto campagne userbench: ArcaFS vs FAT e ArcaFS nativo vs POSIX.

Legge i log passati (uno per run, righe `[bench] <nome> iters=... cyc_op=...`)
e stampa due tabelle con medie, rapporto e spread tra run.

Convenzioni (single source in testland/bench):
  - `fat_*` / `bulk_fat_*`  ↔  `arca_*` / `bulk_arca_*`   (POSIX, stesso client)
  - `<base>_native`         ↔  `<base>`                  (nativo R_OBJ_* vs POSIX)

Uso: scripts/bench-compare.py /tmp/bench-run1.log /tmp/bench-run2.log ...
"""

import re
import sys

LINE = re.compile(
    r"^\[bench\] (\S+) iters=(\d+) warm=(\d+) cyc_op=(\d+) max_cyc=(\d+) kb_s=(\d+)$"
)
HZ = re.compile(r"^\[bench\] tsc_hz=(\d+)$")
DONE = "[bench] DONE ok=1"


def read_logs(paths):
    rows = {}
    hz = []
    done = 0
    for path in paths:
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as f:
                for line in f:
                    line = line.strip()
                    # Il seriale del kernel prefissa ogni riga con `[<sec>]`;
                    # il formato delle righe bench resta senza timestamp.
                    line = re.sub(r"^\[\d+(?:\.\d+)?\]\s+", "", line)
                    if DONE in line:
                        done += 1
                    m = HZ.match(line)
                    if m:
                        hz.append(int(m.group(1)))
                        continue
                    m = LINE.match(line)
                    if not m:
                        continue
                    name, iters, _warm, cyc, _mx, kbs = m.groups()
                    r = rows.setdefault(
                        name, {"iters": int(iters), "cyc": [], "kb": []}
                    )
                    r["cyc"].append(int(cyc))
                    r["kb"].append(int(kbs))
        except OSError as e:
            print(f"[compare] errore: {e}", file=sys.stderr)
    return rows, hz, done


def summary(vals):
    if not vals:
        return 0.0, 0.0, 0.0, 0.0
    avg = sum(vals) / len(vals)
    lo, hi = min(vals), max(vals)
    spread = 0.0 if len(vals) < 2 or avg == 0 else (hi - lo) / avg * 100.0
    return avg, lo, hi, spread


def partner(name):
    if name.endswith("_native"):
        return name[: -len("_native")]
    if name.startswith("bulk_fat_"):
        return "bulk_arca_" + name[len("bulk_fat_") :]
    if name.startswith("fat_"):
        return "arca_" + name[len("fat_") :]
    return None


def fmt_num(x):
    if x >= 1_000_000:
        return f"{x / 1_000_000:.2f}M"
    if x >= 1_000:
        return f"{x / 1_000:.1f}K"
    return f"{x:.0f}"


def table(title, pairs, left, right):
    print()
    print(title)
    print(
        f"{'caso':<34} {'iters':>5} "
        f"{left + ' cyc/op':>14} {right + ' cyc/op':>14} "
        f"{'A/B':>7} {left + ' KiB/s':>12} {right + ' KiB/s':>12} "
        f"{'spread%':>8}"
    )
    print("-" * 120)
    for case, a, b in pairs:
        ac, _, _, asp = summary(a["cyc"])
        bc, _, _, bsp = summary(b["cyc"])
        ak, _, _, _ = summary(a["kb"])
        bk, _, _, _ = summary(b["kb"])
        ratio = (ac / bc) if bc else 0.0
        print(
            f"{case:<34} {a['iters']:>5} "
            f"{fmt_num(ac):>14} {fmt_num(bc):>14} "
            f"{ratio:>6.2f}x {fmt_num(ak):>12} {fmt_num(bk):>12} "
            f"{asp:>3.0f}/{bsp:<3.0f}"
        )


def main(argv):
    if len(argv) < 2:
        print("uso: bench-compare.py <log-run1> [log-run2 ...]", file=sys.stderr)
        return 2
    rows, hz, done = read_logs(argv[1:])
    if not rows:
        print("[compare] nessuna riga [bench] nei log", file=sys.stderr)
        return 1
    if done < len(argv) - 1:
        print(
            f"[compare] WARNING: DONE ok=1 in {done}/{len(argv) - 1} run",
            file=sys.stderr,
        )

    ab = []
    native = []
    for name, r in rows.items():
        p = partner(name)
        if p is None or p not in rows:
            continue
        if name.endswith("_native"):
            native.append((p, r, rows[p]))  # A = nativo, B = POSIX
        elif name.startswith("fat_") or name.startswith("bulk_fat_"):
            ab.append((p, rows[p], r))  # A = arca, B = fat
    ab.sort(key=lambda x: x[0])
    native.sort(key=lambda x: x[0])

    if hz:
        print(f"[compare] tsc_hz medio: {sum(hz) // len(hz)}")
    if not ab and not native:
        print("[compare] nessuna coppia Arca/FAT o nativo/POSIX", file=sys.stderr)
        return 1
    table("ArcaFS (A) vs FAT (B) — stesso client POSIX", ab, "arca", "fat")
    table(
        "ArcaFS nativo R_OBJ_* (A) vs ArcaFS POSIX (B) — stesso volume",
        native,
        "native",
        "posix",
    )
    print()
    print("A/B > 1 = A piu' lento (piu' cicli per op); spread = (max-min)/media")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
