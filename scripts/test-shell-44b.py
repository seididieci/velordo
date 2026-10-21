#!/usr/bin/env python3
"""Test shell SEGNALI (Fase 44b): Ctrl-C selettivo su fg non cooperante
(cancel + escalation kill(130)), bg intatto, cleanup."
Avvia il proprio QEMU (seriale + monitor dedicati); Ctrl-C via sendkey
(`ctrl-c` → 0x03, intercettato da `wait_fg`). Il catch cooperativo e' provato
in-guest (t56: helper esce 42); qui la causa di morte 130 + selettivita'.
Comandi robusti alla latenza (`run_until` su pattern). Pulisce con kill+wait.
Vedi scripts/shell_harness.py.
"""
import sys, os, re, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args


def job_state(out, pid):
    """Come in test-shell-44.py: stato dalla tabella `jobs`."""
    m = re.search(rb"\[\d+\] pid %s (run|stopped|done) " % pid.encode(), out)
    return m.group(1) if m else b""


def main():
    args = parse_shell_args("/tmp/velordo-44b-mon.sock", "/tmp/velordo-44b-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # bg longevo (A) + fg longevo (B, non cooperante: uptime non legge
        # mai il canale, il cancel resta in coda e scatta l'escalation).
        out = sh.run_until("run /bin/uptime.bin &", b"[bg pid")
        m = re.search(rb"\[bg pid (\d+)\]", out)
        found = m is not None
        c.check("44b bg annuncia pid", found)
        pa = m.group(1).decode() if m else "0"
        sh.wait_prompt()
        sh.type_text("run /bin/uptime.bin")
        sh.send_mon("sendkey ret")
        time.sleep(1.5)  # fg partito (wait_fg), prompt non tornato
        sh.press("ctrl-c")
        time.sleep(3.0)  # grace (20 tick) + kill + reclaim + EXIT + prompt
        sh._need_sync = True
        sh.wait_prompt()
        data = sh.read_log()
        found = b"[exit 130]" in data
        c.check("44b Ctrl-C causa 130", found)
        # Selettivita': A run, B sparito dalla tabella (rimosso all'uscita).
        out = sh.run_until("jobs", ("pid %s run" % pa).encode())
        found = job_state(out, pa) == b"run"
        c.check("44b bg A intatto", found)
        # Cleanup: kill+wait di A, tabella vuota.
        out = sh.run_until("kill %s" % pa, b"")
        out = sh.run_until("wait %s" % pa, ("pid %s:" % pa).encode())
        found = ("pid %s: exit" % pa).encode() in out
        c.check("44b wait chiude A", found)
        out = sh.run_until("jobs", b"no jobs")
        found = b"no jobs" in out
        c.check("44b jobs vuota finale", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
