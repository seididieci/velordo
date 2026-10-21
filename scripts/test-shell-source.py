#!/usr/bin/env python3
"""Test shell SOURCE (builtin `source <file>`): 1 riga digitata per script
invece di N comandi via sendkey (velocizzazione test + pilota del builtin
permanente, anticipa Fase 43). Avvia il proprio QEMU (seriale + monitor
dedicati), verifica sul log seriale. Autonomo: prepara le immagini
(salvo --no-prep), boota, testa, pulisce. Vedi scripts/shell_harness.py.

Gli script vivono in /test/sh (iniettati da scripts/inject-bins.sh).
"""
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args

SH = "/test/sh"


def main():
    args = parse_shell_args("/tmp/velordo-source-mon.sock", "/tmp/velordo-source-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # smoke: 15+ comandi in 1 riga digitata (builtin, redirect, pipe,
        # heredoc, run, vars, $?, mkdir/cd/rmdir).
        # smoke: ~20 comandi + 1 run da disco (lento a freddo): sleep
        # esplicito, altrimenti lo slice si chiude a meta' (flaky su KVM).
        out = sh.run_source(SH + "/smoke.txt", sleep=2.0)
        c.check("source smoke (echo)", b"src-smoke-hi" in out)
        c.check("source smoke (vars)", b"val-abc" in out)
        c.check("source smoke (;)", b"one" in out and b"two" in out)
        c.check("source smoke (&&)", b"ok-a" in out and b"ok-b" in out)
        c.check("source smoke (||)", b"or-took" in out)
        c.check("source smoke ($?)", b"after-0" in out)
        c.check("source smoke (cat)", b"Hello from Velordo ramfs!" in out)
        # cat in pipe aggiunge un \n in coda (come in test-shell-42.py:
        # hello.txt 26B/1 riga -> la pipe vede 2 linee, 27 byte).
        c.check("source smoke (pipe)", b"2 4 27 -" in out)
        c.check("source smoke (heredoc)", b"hd-line-one" in out)
        c.check("source smoke (redirect)", b"payload" in out)
        c.check("source smoke (run)", b"runhello: hello" in out)
        c.check("source smoke (run $?)", b"run-code-0" in out)
        c.check("source smoke (cd/pwd)", b"/srcdir" in out)

        # Pulizia dello script verificata da tastiera (file spariti davvero).
        out = sh.run_out("cat /srcpay.txt")
        c.check("source smoke (rm file)", b"cannot open" in out)
        out = sh.run_out("cd /srcdir")
        c.check("source smoke (rmdir)", b"no such directory" in out)

        # exit in script termina lo script (mai la shell); esecuzione
        # silenziosa (niente eco delle righe: l'assenza e' asseribile).
        out = sh.run_source(SH + "/exit.txt")
        c.check("source exit (prima)", b"before-exit" in out)
        c.check("source exit (dopo mai eseguito)", b"never-printed" not in out)
        out = sh.run_out("echo after-$?")
        c.check("source exit (code 3)", b"after-3" in out)

        # Self-source: la guardia di annidamento (max 4) lo ferma.
        out = sh.run_source(SH + "/loop.txt")
        c.check("source loop (nesting)", b"nesting too deep" in out)

        # Error paths.
        out = sh.run_out("source %s/nope.txt" % SH)
        c.check("source ignoto", b"cannot load" in out)
        out = sh.run_out("source")
        c.check("source senza arg", b"usage" in out)

        # File vuoto = no-op riuscita ($? = 0).
        sh.run("> /srcempty.txt")
        sh.run_source("/srcempty.txt")
        out = sh.run_out("echo empty-$?")
        c.check("source vuoto ($? 0)", b"empty-0" in out)
        sh.run("rm /srcempty.txt")
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
