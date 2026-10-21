#!/usr/bin/env python3
"""Test shell JOB CONTROL (Fase 44a): fg/bg, Ctrl-Z su run singolo, jobs/ps
con stopped, selettivita' (il bg intatto mentre il fg si sospende)."
Avvia il proprio QEMU (seriale + monitor dedicati); Ctrl-Z via sendkey
(`ctrl-z` → 0x1a, intercettato da `wait_fg` solo durante un fg singolo).
Robusto alla latenza sotto carico: ogni comando attende il proprio output
(`run_until` su pattern), mai sleep fissi (il fork da disco e la delivery
input superano gli sleep fissi sotto `--jobs`). Gli assert sono sul log
seriale (output + annunci `[N]+ Stopped`). Pulisce con kill+wait finali.
Vedi scripts/shell_harness.py.
"""
import sys, os, re, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args


def fg_ctrl_z(sh):
    """Premi Ctrl-Z e attendi il ritorno al prompt (job sospeso)."""
    sh.press("ctrl-z")
    time.sleep(1.2)
    sh._need_sync = True


def job_state(out, pid):
    """Stato del job con quel pid dalla tabella `jobs` (`run`/`stopped`/
    `done`/b"" se assente). Match preciso `[N] pid P STATO` (i pid appaiono
    anche nelle righe kernel `[fork]/[job]/[proc]`: il semplice `in` confonde).
    """
    m = re.search(rb"\[\d+\] pid %s (run|stopped|done) " % pid.encode(), out)
    return m.group(1) if m else b""


def main():
    args = parse_shell_args("/tmp/velordo-44-mon.sock", "/tmp/velordo-44-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # 1. bg veloce + fg su Done (report + remove, deterministico).
        out = sh.run_until("run /usr/bin/runhello.bin hello &", b"[bg pid")
        m = re.search(rb"\[bg pid (\d+)\]", out)
        found = m is not None
        c.check("44 bg annuncia pid", found)
        pid = m.group(1).decode() if m else "0"
        time.sleep(1.0)  # runhello e' gia' Done: fg fa report, non attesa
        out = sh.run_until("fg %0", ("pid %s: exit" % pid).encode())
        found = ("pid %s: exit 0" % pid).encode() in out
        c.check("44 fg su Done riporta", found)
        out = sh.run_until("jobs", b"no jobs")
        found = b"no jobs" in out
        c.check("44 jobs vuota dopo fg", found)

        # 2. Due bg longevi (uptime): jobs li mostra run.
        out = sh.run_until("run /bin/uptime.bin &", b"[bg pid")
        m0 = re.search(rb"\[bg pid (\d+)\]", out)
        out = sh.run_until("run /bin/uptime.bin &", b"[bg pid")
        m1 = re.search(rb"\[bg pid (\d+)\]", out)
        found = m0 is not None and m1 is not None
        c.check("44 due bg annunciati", found)
        pa = m0.group(1).decode() if m0 else "0"
        pb = m1.group(1).decode() if m1 else "0"
        # La riga del SECONDO job e' stampata per ultima: attenderla copre
        # entrambe (jobs elenca in ordine).
        out = sh.run_until("jobs", ("pid %s run" % pb).encode())
        found = job_state(out, pa) == b"run" and job_state(out, pb) == b"run"
        c.check("44 jobs mostra 2 run", found)

        # 3. fg %1 (Running) + Ctrl-Z: solo B si sospende (selettivita').
        sh.wait_prompt()
        sh.type_text("fg %1")
        sh.send_mon("sendkey ret")
        time.sleep(1.5)  # fg partito (wait_fg), prompt non tornato
        fg_ctrl_z(sh)
        sh.wait_prompt()
        data = sh.read_log()
        found = b"[1]+ Stopped" in data
        c.check("44 Ctrl-Z annuncia stop", found)
        out = sh.run_until("jobs", ("pid %s stopped" % pb).encode())
        # A run, B stopped: due righe distinte.
        found = job_state(out, pa) == b"run"
        c.check("44 bg A intatto", found)
        found = job_state(out, pb) == b"stopped"
        c.check("44 fg B stopped", found)

        # 4. ps mostra stopped.
        out = sh.run_until("ps", b"stopped")
        found = b"stopped" in out
        c.check("44 ps mostra stopped", found)

        # 5. bg %1 riprende: entrambi run.
        out = sh.run_until("bg %1", ("pid %s run" % pb).encode())
        out = sh.run_until("jobs", ("pid %s run" % pb).encode())
        found = job_state(out, pa) == b"run" and job_state(out, pb) == b"run"
        c.check("44 bg riprende", found)

        # 6. fg %1 + Ctrl-Z di nuovo (loop resume/wait/suspend).
        sh.wait_prompt()
        sh.type_text("fg %1")
        sh.send_mon("sendkey ret")
        time.sleep(1.5)
        fg_ctrl_z(sh)
        sh.wait_prompt()
        data = sh.read_log()
        found = data.count(b"[1]+ Stopped") >= 2
        c.check("44 fg/bg/fg loop", found)

        # 7. Errori: fg ignoto, poi cleanup e bg senza job.
        out = sh.run_until("fg %9", b"no such job")
        found = b"no such job" in out
        c.check("44 fg ignoto", found)
        out = sh.run_until("kill %s" % pa, b"")
        out = sh.run_until("kill %s" % pb, b"")
        out = sh.run_until("wait %s" % pa, ("pid %s:" % pa).encode())
        out = sh.run_until("wait %s" % pb, ("pid %s:" % pb).encode())
        out = sh.run_until("jobs", b"no jobs")
        found = b"no jobs" in out
        c.check("44 cleanup finale", found)
        out = sh.run_until("bg", b"no jobs")
        found = b"no jobs" in out
        c.check("44 bg senza job", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
