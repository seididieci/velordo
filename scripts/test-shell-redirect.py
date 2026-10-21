#!/usr/bin/env python3
"""Test shell REDIRECT (Fase 40.4): >, >>, <, 2>, 2>>, 2>&1 ordinati + run redirectato."
Avvia il proprio QEMU (seriale + monitor dedicati); i comandi viaggiano in
script via `source` (gruppi separati dove gli assert di assenza lo
richiedono). Verifica sul log seriale. Autonomo: prepara le immagini
(salvo --no-prep), boota, testa, pulisce le sue fixture (rm in red4).
Vedi scripts/shell_harness.py.
"""
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args

SH = "/test/sh"


def main():
    args = parse_shell_args("/tmp/velordo-redir-mon.sock", "/tmp/velordo-redir-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # Fase 40.4: redirect shell (bash-like: ultimo vince per slot).
        # Builtin stdout: > crea/tronca, >> appende.
        out = sh.run_source(SH + "/red1.txt")
        found = b"hello redir" in out
        c.check("echo > file + cat", found)
        found = b"hello redir" in out and b"second" in out
        c.check(">> appende", found)

        # > tronca + stdin builtin (script separato: "second" assente).
        out = sh.run_source(SH + "/red2.txt")
        found = b"solo" in out and b"second" not in out
        c.check("> tronca", found)
        found = b"solo" in out
        c.check("cat < file", found)
        found = b"1 1 5 -" in out
        c.check("wc < file (=1 1 5 -)", found)
        found = b"73 6f 6c 6f" in out
        c.check("hexdump < file", found)
        found = b"no such file or directory" in out
        c.check("< missing (ENOENT distinto)", found)

        # Separazione stdout/stderr: l'errore non inquina >.
        out = sh.run_source(SH + "/red3a.txt")
        found = b"- 0 o404.txt" in out
        c.check("errore non inquina > (file vuoto)", found)
        out = sh.run_source(SH + "/red3b.txt")
        found = b"cannot open missing404" in out
        c.check("2> cattura errore builtin", found)
        out = sh.run_source(SH + "/red3c.txt")
        found = b"cannot open missing404" in out
        c.check("2>> appende errore builtin", found)
        out = sh.run_source(SH + "/red3d.txt")
        found = b"cannot open missing404" in out
        c.check("2>&1 dopo >: errore nel file", found)
        out = sh.run_source(SH + "/red3e.txt")
        found = b"cannot open missing404" in out
        c.check("2>&1 prima di >: errore su terminale", found)
        found = b"- 0 o404c.txt" in out
        c.check("2>&1 prima di >: file vuoto", found)
        found = b"missing target" in out
        c.check("redirect senza target", found)

        # run con redirect (handoff grant via argv-magic, claim nello startup).
        # red4: 3 run da disco + job bg + wait: sleep esplicito (il bg
        # segue il fork da disco, piu' lento dello sleep fisso).
        out = sh.run_source(SH + "/red4.txt", sleep=3.0)
        found = b"runhello: hello" in out and b"non utf8" not in out
        c.check("run > file (magic nascosto)", found)
        found = b"[exit 3]" in out
        c.check("run > file + exit code", found)
        found = b"runhello: stdin:solo" in out
        c.check("run < > : stdin nel file", found)
        found = b"[bg pid" in out
        c.check("run bg + redirect", found)
        found = b"exit" in out
        c.check("wait chiude bg redirectato", found)
        found = b"runhello: bgx" in out
        c.check("output bg nel file", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
