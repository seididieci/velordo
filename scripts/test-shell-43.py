#!/usr/bin/env python3
"""Test shell ENV (Fase 43a): export ereditato, VAR=v mono-comando, PWD,
PATH + bare word, shebang, env negli stadi pipe."
Avvia il proprio QEMU (seriale + monitor dedicati); i comandi viaggiano in
script via `source`. Verifica sul log seriale. Autonomo: prepara le immagini
(salvo --no-prep), boota, testa, pulisce. Vedi scripts/shell_harness.py.
"""
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args, has_line

SH = "/test/sh"


def main():
    args = parse_shell_args("/tmp/velordo-43-mon.sock", "/tmp/velordo-43-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # Env persistente ereditato + PWD automatico.
        out = sh.run_source(SH + "/p43a.txt")
        found = b"bar" in out
        c.check("43 assign persiste", found)
        found = b"runhello: env:E43=hello" in out
        c.check("43 export ereditato", found)
        found = b"runhello: env:PWD=/" in out
        c.check("43 PWD automatico (/)", found)
        found = b"runhello: env:PWD=/usr" in out
        c.check("43 PWD segue cd", found)

        # VAR=v mono-comando (esterni e builtin): E43 resta base.
        out = sh.run_source(SH + "/p43b.txt")
        found = b"runhello: env:E43=one" in out
        c.check("43 VAR=v su run", found)
        found = b"hi-two" in out
        c.check("43 VAR=v su builtin", found)
        found = has_line(out, b"base")
        c.check("43 mono non persiste", found)

        # PATH + bare word + ignoto.
        out = sh.run_source(SH + "/p43c.txt")
        found = b"runhello: hello" in out
        c.check("43 bare word via PATH", found)
        found = b"runhello: hello2" in out
        c.check("43 run cerca in PATH", found)
        found = b"unknown command: nosuchprog43" in out
        c.check("43 ignoto resta 127", found)
        found = has_line(out, b"127")
        c.check("43 $? dopo ignoto", found)

        # Shebang (kernel mai coinvolto: solo shell+civis).
        out = sh.run_source(SH + "/p43d.txt")
        found = b"runhello: /test/sh/h43.sh" in out and b"runhello: HI" in out
        c.check("43 shebang argv", found)
        found = b"runhello: EXTRA" in out
        c.check("43 shebang con arg", found)

        # Env negli stadi + stadio esterno bare-word + stadio ignoto.
        out = sh.run_source(SH + "/p43e.txt")
        found = b"runhello: env:E43=seven" in out
        c.check("43 env stadio run", found)
        found = b"STAGE" in out
        c.check("43 stadio bare-word", found)
        found = b"unknown command: nosuchst43" in out
        c.check("43 stadio ignoto", found)
        found = has_line(out, b"base")
        c.check("43 env stadio non leak", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
