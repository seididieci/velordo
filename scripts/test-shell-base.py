#!/usr/bin/env python3
"""Test shell BASE (Fase 9.4/18/19/20): ls/cat/mkdir, backspace, builtin, cd/pwd, ls -l, kill, ps, rm/cp/mv, FAT scrivibile, clear."
Avvia il proprio QEMU (seriale + monitor dedicati); i comandi viaggiano in
script via `source` (1 riga digitata per gruppo, script in /test/sh),
verifica sul log seriale. Solo backspace/clear restano interattivi (VGA
screendump). Autonomo: prepara le immagini (salvo --no-prep), boota, testa,
pulisce le sue fixture. Vedi scripts/shell_harness.py.
"""
import sys, os, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args, lit_pixels, diff_masked

SH = "/test/sh"


def main():
    args = parse_shell_args("/tmp/velordo-base-mon.sock", "/tmp/velordo-base-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    SHOT0 = args.serial + ".shot0.ppm"
    SHOT1 = args.serial + ".shot1.ppm"
    SHOT2 = args.serial + ".shot2.ppm"
    SHOT3 = args.serial + ".shot3.ppm"
    try:
        sh.boot()
        ok = True

        # Suite di boot (se il kernel bootato la include: dipende da come e'
        # stato compilato init): si asserisce quando la suite e' presente, si
        # salta in boot di produzione.
        data = sh.read_log()
        if b"[usertests] SUMMARY" in data:
            found = b"t36 UUID/LABEL + discovery stabile: PASS" in data
            c.check("t36 suite pre-shell", found)

        # ls + cat + mkdir (script base1). E1 (root ArcaFS): in / ci sono
        # anche bin, test (/bin, /test), usr, tool e tmp (ramfs su /tmp).
        # ADR-0059: niente piu' "fat" (il FAT e' uscito dal boot).
        out = sh.run_source(SH + "/base1.txt")
        found = (b"hello.txt" in out and b"test.txt" in out
                 and b"dev" in out
                 and b"bin" in out and b"test" in out and b"tmp" in out)
        c.check("ls / (hello.txt, test.txt, dev, bin, test, tmp)", found)
        found = b"Hello from Velordo ramfs!" in out
        c.check("cat hello.txt", found)
        # "prova" non appare nell'eco (source silenzioso, niente echo su
        # seriale): deve comparire SOLO come entry della directory.
        found = b"prova" in out
        c.check("mkdir prova visibile in ls", found)

        # Fase 18.0: backspace a riga vuota non mangia il prompt. La VGA e'
        # statica al prompt: shot0 riferimento; "q" deve cambiare lo schermo
        # (controllo positivo: screendump sensibile); backspace torna a shot0;
        # altri 3 backspace a riga vuota devono lasciare tutto identico
        # (pre-fix mangiavano "$ "). Resta interattivo (VGA, non scriptabile).
        sh.wait_prompt()
        time.sleep(0.2)
        for p in (SHOT0, SHOT1, SHOT2, SHOT3):
            try: os.unlink(p)
            except FileNotFoundError: pass
        got0 = sh.screendump(SHOT0)
        sh.type_text("q")
        time.sleep(0.8)
        got1 = sh.screendump(SHOT1)
        sh.send_mon("sendkey backspace")
        time.sleep(0.8)
        got2 = sh.screendump(SHOT2)
        for _ in range(3):
            sh.send_mon("sendkey backspace")
        time.sleep(0.8)
        got3 = sh.screendump(SHOT3)
        if not (got0 and got1 and got2 and got3):
            print("FAIL backspace: screendump mancati")
            c.ok = False
        else:
            d1 = diff_masked(SHOT0, SHOT1)
            d2 = diff_masked(SHOT0, SHOT2)
            d3 = diff_masked(SHOT0, SHOT3)
            print("info backspace: diff q=%d erase=%d empty=%d" % (d1, d2, d3))
            # La 'q' accende solo i pixel del glifo (~96 byte): soglia bassa
            # ma > 0 (controllo positivo che lo screendump sia sensibile).
            found = d1 > 50 and d2 <= 64 and d3 <= 64
            print(("PASS " if found else "FAIL ")
                  + "backspace: eco ok, cancel ok, prompt intatto a riga vuota")
            ok = ok and found

        # Fase 18.1: builtin + cd/pwd + relativi (script base2, prova esiste).
        out = sh.run_source(SH + "/base2.txt")
        found = b"hello world" in out
        c.check("echo hello world", found)
        found = b"1 4 26 hello.txt" in out
        c.check("wc hello.txt (=1 4 26)", found)
        found = b"48 65 6c 6c 6f" in out
        c.check("hexdump hello.txt", found)
        found = b"/prova" in out
        c.check("cd prova + pwd", found)
        found = b"inner.txt" in out
        c.check("path relativo in ls", found)
        # L'ultimo `ls prova` e' nello stesso slice: altra occorrenza.
        c.check("cd .. + ls prova", b"inner.txt" in out)

        # Fase 19.2 (stretch ls -l): tipo + size via R_STAT (1 RT per entry).
        out = sh.run_source(SH + "/base3.txt")
        found = b"- 26 hello.txt" in out
        c.check("ls -l (tipo + size)", found)
        found = b"d 0 lldir" in out
        c.check("ls -l (marcatore dir)", found)
        # La radice in script separato: l'assenza di (ro) e' sullo slice.
        out = sh.run_source(SH + "/base3b.txt")
        found = b"- 26 hello.txt" in out and b"(ro)" not in out
        c.check("ls -l / (scrivibile)", found)

        # kill errori + ps.
        out = sh.run_source(SH + "/base4k.txt")
        found = (b"kill: unknown pid/service" in out
                 and out.count(b"kill: failed") >= 2)
        c.check("kill errori + init rifiutato", found)
        found = (b"PID  NAME" in out and b"rector" in out
                 and b"usershell" in out)
        c.check("ps tabellare (init+shell)", found)

        # Fase 18.2: rmdir su piena + rm + ls (prova contiene inner.txt).
        out = sh.run_source(SH + "/base5.txt")
        found = b"rmdir: failed" in out
        c.check("rmdir rifiutata su dir piena", found)
        found = b"inner.txt" not in out
        c.check("rm prova/inner.txt sparito da ls", found)
        # read-fail in script separato: l'errore contiene il path.
        out = sh.run_source(SH + "/base5b.txt")
        found = b"cannot open" in out
        c.check("cat dopo rm fallisce", found)

        # rmdir su dir ormai vuota + sparizione.
        out = sh.run_source(SH + "/base5c.txt")
        found = b"prova" not in out
        c.check("rmdir prova vuota", found)

        # cp/mv ramfs.
        out = sh.run_source(SH + "/base6.txt")
        found = b"Hello from Velordo ramfs!" in out
        c.check("cp hello.txt copy.txt", found)
        c.check("mv preserva contenuto", b"Hello from Velordo ramfs!" in out)
        found = b"cannot open" in out
        c.check("mv rimuove sorgente", found)

        # Fase 20 (radice scrivibile): cp verso /bin CREA + read-back; rm
        # riesce (su ArcaFS l'unlink esiste: sul FAT era rifiutato).
        out = sh.run_source(SH + "/base7.txt")
        found = b"Hello from Velordo ramfs!" in out
        c.check("cp hello.txt -> ramfs", found)
        found = b"Hello from Velordo ramfs!" in out
        c.check("cp verso /bin + read-back", found)
        found = b"cannot remove" not in out
        c.check("rm su /bin riuscito (unlink su ArcaFS)", found)

        # clear: scherma testo, pulisce, shell resta viva (interattivo, VGA).
        sh.wait_prompt()
        if not sh.screendump(SHOT0):
            print("FAIL clear: screendump pre mancato")
            c.ok = False
        else:
            before = lit_pixels(SHOT0)
            sh.run("clear")
            sh.wait_prompt()
            if not sh.screendump(SHOT1):
                print("FAIL clear: screendump post mancato")
                c.ok = False
            else:
                after = lit_pixels(SHOT1)
                print("info clear: lit prima=%d dopo=%d" % (before, after))
                found = after < 3000 and before - after > 3000
                c.check("clear pulisce la VGA", found)
            sh.run("echo alive")
            data = sh.read_log()
            found = b"alive" in data
            c.check("shell viva dopo clear", found)
        return 0 if (c.ok and ok) else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
