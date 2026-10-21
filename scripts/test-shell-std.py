#!/usr/bin/env python3
"""Test shell STD (S1.3 + S2.0): hello std nativo (println + Vec + Mutex +
HashMap + Instant + argv/env/cwd) via `run`. Fuori dal gate default: gira
solo se il binario esiste (seed condizionale da scripts/build-pal.sh);
altrimenti SKIP rumoroso ma exit 0 (mai rosso per assenza).
Uso: python3 scripts/test-shell-std.py [--no-prep]
"""
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args


def main():
    args = parse_shell_args("/tmp/velordo-std-mon.sock", "/tmp/velordo-std-serial.log")
    if not args.no_prep:
        prep_images()
    # Binario assente = SKIP (non FAIL): la PAL si compila a parte.
    import subprocess
    # ADR-0058: il binario vive nel volume ArcaFS (seed "ns:test/stdhello.bin"),
    # non piu' in un FAT: la presenza si verifica sull'host.
    has = os.path.exists(os.path.join(os.getcwd(), "pal/build/hello-std.bin"))
    if not has:
        print("[std] SKIP: /test/stdhello.bin assente (scripts/build-pal.sh prima)")
        return 0
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # S2.4: l'hello dura ~350ms (pipe+fork) contro T_RUN=0.4s: aspettare
        # DONE esplicito, mai sleep fisso (il troncamento tagliava la coda).
        out = sh.run_until("run /test/stdhello.bin", b"[stdhello] DONE", timeout=60)
        c.check("stdhello println", b"[stdhello] ciao da std su Velordo" in out)
        c.check("stdhello vec", b"[stdhello] vec len=8 sum=140" in out)
        c.check("stdhello mutex", b"[stdhello] mutex=41" in out)
        c.check("stdhello hashmap", b"[stdhello] hashmap[chiave]=7" in out)
        c.check("stdhello instant", b"[stdhello] instant ok" in out)
        # S2.0: argv/env/cwd (export prima, run con argv dopo).
        sh.run_out("export VELORDO_S2=s2ok")
        out = sh.run_until("run /test/stdhello.bin foo bar", b"[stdhello] DONE", timeout=60)
        c.check("stdhello argv", b"[stdhello] argv=foo,bar" in out)
        c.check("stdhello env", b"[stdhello] env VELORDO_S2=s2ok" in out)
        c.check("stdhello set_var", b"[stdhello] set_var STD_SET=42" in out)
        c.check("stdhello cwd", b"[stdhello] cwd=/" in out)
        c.check("stdhello churn", b"[stdhello] alloc churn ok in " in out)
        c.check("stdhello overalign", b"[stdhello] overalign ok=true" in out)
        c.check("stdhello stats", b"[stdhello] alloc stats a=" in out)
        c.check("stdhello fs roundtrip", b"[stdhello] fs roundtrip ok=true" in out)
        c.check("stdhello fs len", b"[stdhello] fs len=13 is_file=true" in out)
        c.check("stdhello fs readdir", b"[stdhello] fs readdir n=1" in out)
        c.check("stdhello fs DONE", b"[stdhello] fs DONE" in out)
        c.check("stdhello pipe", b"[stdhello] pipe ok=true" in out)
        c.check("stdhello cmd status", b"[stdhello] cmd status ok=true code=Some(0)" in out)
        c.check("stdhello cmd fail", b"[stdhello] cmd fail code=Some(3)" in out)
        c.check("stdhello cmd missing", b"[stdhello] cmd missing err=true" in out)
        c.check("stdhello cmd out", b"[stdhello] cmd out code=Some(7) cout=true cerr=true echo=true" in out)
        c.check("stdhello cmd pipe", b"[stdhello] cmd pipe code=Some(7) echo=true cerr=true" in out)
        c.check("stdhello t-empty-join", b"[stdhello] t-empty-join ok" in out)
        c.check("stdhello t-raw", b"[stdhello] t-raw=1" in out)
        c.check("stdhello threads sum", b"[stdhello] threads sum=1000" in out)
        c.check("stdhello condvar", b"[stdhello] condvar ok" in out)
        c.check("stdhello once", b"[stdhello] once n=1" in out)
        c.check("stdhello tls", b"[stdhello] tls main=7 child=0" in out)
        c.check("stdhello threads DONE", b"[stdhello] threads DONE" in out)
        c.check("stdhello DONE", b"[stdhello] DONE" in out)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
