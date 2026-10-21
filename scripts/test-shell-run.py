#!/usr/bin/env python3
"""Test shell RUN (Fase 37.2): run fg con argv/exit-code/errori, bg + jobs/kill/wait."
Avvia il proprio QEMU (seriale + monitor dedicati); i comandi viaggiano in
script via `source` (il pid del bg per kill/wait lo estrae il .py dallo
slice). Verifica sul log seriale. Autonomo: prepara le immagini
(salvo --no-prep), boota, testa, pulisce le sue fixture.
Vedi scripts/shell_harness.py.
"""
import sys, os, re
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args

SH = "/test/sh"


def main():
    args = parse_shell_args("/tmp/velordo-run-mon.sock", "/tmp/velordo-run-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # Fase 37.2: run/jobs/wait (fork+exec, EXIT_NOTIFY) in un solo script.
        out = sh.run_source(SH + "/run1.txt")
        # Foreground veloce con argv: runhello stampa gli argv su seriale.
        found = b"runhello: hello" in out and b"runhello: world" in out
        c.check("run fg con argv (echo)", found)

        # Exit code != 0 annunciato dal fg come [exit N] (runhello esce 3
        # se un argv e' "fail").
        found = b"[exit 3]" in out
        c.check("run fg exit code ([exit 3])", found)

        # Errori: path inesistente, wait su job ignoto.
        found = b"run: cannot load" in out
        c.check("run su path ignoto", found)
        found = b"wait: no such job" in out
        c.check("wait su job ignoto", found)

        # Background: uptime longevo, jobs lo elenca (il pid per kill/wait
        # si estrae dallo slice: l'annuncio [bg pid N] e' nello slice).
        m = re.search(rb"\[bg pid (\d+)\]", out)
        found = m is not None
        c.check("run bg annuncia pid", found)
        pid = m.group(1).decode() if m else "0"
        found = b"/bin/uptime.bin" in out and b"run" in out
        c.check("jobs elenca il bg", found)
        # kill+wait restano digitati (servono il pid appena estratto).
        # kill parent-scoped: la shell E' il parent, consentito.
        sh.run_out("kill %s" % pid)
        out = sh.run_out("wait %s" % pid)
        found = ("pid %s: exit" % pid).encode() in out
        c.check("wait chiude il bg con codice", found)
        out = sh.run_out("jobs")
        found = b"no jobs" in out
        c.check("jobs vuota dopo wait", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
