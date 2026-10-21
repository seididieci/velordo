#!/usr/bin/env python3
"""Test shell HISTORY/EDITING (Fase 43b): readline nella shell su tty raw
(Up/Down history, Left/Right/Home/End/Delete, Esc ignorato)."
Avvia il proprio QEMU (seriale + monitor dedicati); digitazione reale via
sendkey (le frecce arrivano come ESC[D/C/A/B dal tty). Gli assert sono sul
log seriale tramite l'EFFETTO dei comandi (l'eco e' solo VGA, mai seriale;
il prompt `$ ` e l'output si incollano sulla stessa riga seriale, a
differenza di `source` dove l'output apre righe fresche).
Autonomo: prepara le immagini (salvo --no-prep), boota, testa. Vedi
scripts/shell_harness.py.
"""
import sys, os, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args, has_line, count_lines


def recall(sh, keys, sleep=1.2):
    """Premi `keys` speciali + Enter su riga vuota (history/editing)."""
    sh.wait_prompt()
    for k in keys:
        sh.press(k)
    sh.send_mon("sendkey ret")
    time.sleep(sleep)
    sh._need_sync = True


def main():
    args = parse_shell_args("/tmp/velordo-43b-mon.sock", "/tmp/velordo-43b-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # History Up riesegue l'ultimo comando (conteggio prima/dopo).
        sh.run("echo one")
        n0 = count_lines(sh.read_log(), b"one")
        recall(sh, ["up"])
        n1 = count_lines(sh.read_log(), b"one")
        c.check("43b up riesegue", n0 >= 1 and n1 == n0 + 1)

        # History Up + coda digitata.
        sh.run("echo two")
        sh.wait_prompt()
        sh.press("up")
        sh.type_text("!")
        sh.send_mon("sendkey ret")
        time.sleep(1.2)
        sh._need_sync = True
        found = has_line(sh.read_log(), b"two!")
        c.check("43b up + edit coda", found)

        # Up/Up/Down naviga e ripristina (BBB +1, AAA invariato).
        sh.run("echo AAA")
        sh.run("echo BBB")
        a0 = count_lines(sh.read_log(), b"AAA")
        b0 = count_lines(sh.read_log(), b"BBB")
        recall(sh, ["up", "up", "down"])
        data = sh.read_log()
        found = count_lines(data, b"AAA") == a0 and count_lines(data, b"BBB") == b0 + 1
        c.check("43b up/up/down", found)

        # Left + inserimento mid-line.
        sh.wait_prompt()
        sh.type_text("echo ab")
        sh.press("left")
        sh.type_text("X")
        sh.send_mon("sendkey ret")
        time.sleep(1.2)
        sh._need_sync = True
        found = has_line(sh.read_log(), b"aXb")
        c.check("43b left + insert", found)

        # Home: inizio riga.
        h0 = count_lines(sh.read_log(), b"hi")
        sh.wait_prompt()
        sh.type_text("cho hi")
        sh.press("home")
        sh.type_text("e")
        sh.send_mon("sendkey ret")
        time.sleep(1.2)
        sh._need_sync = True
        found = count_lines(sh.read_log(), b"hi") == h0 + 1
        c.check("43b home", found)

        # End: home poi end torna in coda.
        sh.wait_prompt()
        sh.type_text("echo ok")
        sh.press("home")
        sh.press("end")
        sh.send_mon("sendkey ret")
        time.sleep(1.2)
        sh._need_sync = True
        found = has_line(sh.read_log(), b"ok")
        c.check("43b home + end", found)

        # Delete cancella sotto cursore.
        h0 = count_lines(sh.read_log(), b"hi")
        sh.wait_prompt()
        sh.type_text("echo hXi")
        sh.press("left")
        sh.press("left")
        sh.press("delete")
        sh.send_mon("sendkey ret")
        time.sleep(1.2)
        sh._need_sync = True
        found = count_lines(sh.read_log(), b"hi") == h0 + 1
        c.check("43b delete", found)

        # Esc solitario: ignorato, mai hang.
        sh.wait_prompt()
        sh.press("esc")
        sh.type_text("echo escok")
        sh.send_mon("sendkey ret")
        time.sleep(1.2)
        sh._need_sync = True
        found = has_line(sh.read_log(), b"escok")
        c.check("43b esc ignorato", found)

        # Shell viva e history con $?: richiamo + status.
        out = sh.run_out("echo $?")
        found = b"0\n" in out
        c.check("43b shell viva", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
