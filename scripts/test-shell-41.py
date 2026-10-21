#!/usr/bin/env python3
"""Test shell PARSER (Fase 41): quote/escape/commenti, ; && ||, $VAR/$?/~/$$, export, field-split, glob, errori."
Avvia il proprio QEMU (seriale + monitor dedicati); i comandi viaggiano in
script via `source` (gruppi separati dove gli assert di assenza/posizione lo
richiedono). Verifica sul log seriale. Autonomo: prepara le immagini
(salvo --no-prep), boota, testa, pulisce le sue fixture. Vedi
scripts/shell_harness.py.
"""
import sys, os, re
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args, has_line

SH = "/test/sh"


def main():
    args = parse_shell_args("/tmp/velordo-41-mon.sock", "/tmp/velordo-41-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # Fase 41: parser quote-aware (quote/escape/commenti, ; && || &,
        # $VAR ${VAR} $? $$ ~, glob * ?; pipe rifiutata verso Fase 42).
        # Quote: singolo raggruppa e inibisce tutto, doppio solo $.
        out = sh.run_source(SH + "/p41a.txt")
        found = b"a   b" in out
        c.check("41 single-quote raggruppa", found)
        found = b">" in out
        c.check("41 redirect in quote letterale", found)
        found = b"a#b" in out
        c.check("41 # in quote non e' commento", found)
        found = b"$Q41" in out and b"vv" not in out
        c.check("41 $ in single-quote letterale", found)
        found = b"*" in out
        c.check("41 glob in quote inibito", found)

        out = sh.run_source(SH + "/p41b.txt")
        found = b"a   b" in out
        c.check("41 double-quote raggruppa", found)
        found = b"v=vv" in out
        c.check("41 $ in double-quote espande", found)
        found = b"a$B" in out
        c.check("41 escape in double-quote", found)

        # Escape fuori quote + commenti.
        found = b"a b" in out
        c.check("41 escape spazio", found)
        found = b"a;b" in out
        c.check("41 escape punto-e-virgola", found)
        found = b"hi" in out
        c.check("41 commento trailing", found)

        # Variabili: bare-assign, ${}, unset, export lista/errori.
        out = sh.run_source(SH + "/p41c1.txt")
        # Riga intera "] pre" (non prima-riga dello slice: i burst [blkdbg]/
        # [irq1] del kernel si intercalano ai bordi — ancoraggio al formato).
        found = b"UNSET41X" not in out and has_line(out, b"pre")
        c.check("41 $UNSET sparisce", found)
        found = b"zzz" in out
        c.check("41 bare NAME=valore", found)
        found = b"zzz!" in out
        c.check("41 ${VAR}", found)
        found = b"BARE41=zzz" in out
        c.check("41 export lista", found)
        found = b"bad name" in out
        c.check("41 export nome invalido", found)
        # 43a: il prefisso mono-comando ora funziona (builtin: save/set/
        # restore — "hi" stampato, F41X non persiste; il set e' provato in 43).
        found = has_line(out, b"hi")
        c.check("41 VAR=v cmd mono-comando (43a)", found)

        # $$ e ~: assert di posizione, script dedicati.
        out = sh.run_source(SH + "/p41c3.txt")
        found = re.search(rb"\d+", out) is not None and b"$$" not in out
        c.check("41 $$ numerico", found)
        out = sh.run_source(SH + "/p41c2.txt")
        found = has_line(out, b"/")
        c.check("41 tilde -> /", found)

        # Field-split: una variabile con spazio diventa DUE argv (osservabile
        # via `cp src dst`: senza split sarebbe un'unica sorgente inesistente).
        out = sh.run_source(SH + "/p41d1.txt")
        found = b"spcontent" in out
        c.check("41 field-split non quotato", found)

        # Connettori: ; && ||, short-circuit, catene, $?, ignoto=127.
        out = sh.run_source(SH + "/p41e1.txt")
        found = b"c41a" in out and b"c41b" in out
        c.check("41 ; sequenza", found)
        found = b"after41" in out
        c.check("41 ; ignora lo status", found)
        found = b"ok41" in out and b"yes41" in out
        c.check("41 && catena", found)
        found = b"no41" not in out
        c.check("41 && short-circuit", found)
        out = sh.run_source(SH + "/p41e2.txt")
        found = b"or41" in out
        c.check("41 || scatta", found)
        found = b"ok41b" in out and b"no41b" not in out
        c.check("41 || salta a successo", found)
        found = b"deep41" in out
        c.check("41 catena || profonda", found)
        out = sh.run_source(SH + "/p41f2.txt")
        found = has_line(out, b"1")
        c.check("41 $? dopo errore", found)
        found = b"unknown command" in out
        c.check("41 comando ignoto", found)
        found = has_line(out, b"127")
        c.check("41 $? dopo ignoto (=127)", found)
        found = b"comb41" in out
        c.check("41 redirect + &&", found)

        # Glob via readdir: *, ?, no-match letterale, dotfile esclusi.
        out = sh.run_source(SH + "/p41g1.txt")
        found = b"g41a1" in out and b"g41a2" in out and b"g41b1" in out
        c.check("41 glob *", found)
        out = sh.run_source(SH + "/p41g2.txt")
        found = b"g41a1" in out and b"g41a2" in out and b"g41b1" not in out
        c.check("41 glob ?", found)
        out = sh.run_source(SH + "/p41g3.txt")
        found = b"g41nomatch*.zzz" in out
        c.check("41 glob no-match letterale", found)
        out = sh.run_source(SH + "/p41g4.txt")
        found = b"h41" not in out
        c.check("41 glob esclude dotfile", found)
        out = sh.run_source(SH + "/p41g5.txt")
        found = b".h41" in out
        c.check("41 glob dotfile con punto", found)
        found = b"abc" in out
        c.check("41 quote non chiusa letterale", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
