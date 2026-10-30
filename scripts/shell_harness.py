#!/usr/bin/env python3
"""Harness comune dei test interattivi di shell (estratto da test-shell.py).

Un boot QEMU per file di test: seriale su file + monitor su unix socket,
comandi via sendkey, assert sul log seriale (la shell specchia l'output).
Ogni file di fase (test-shell-*.py) e' autonomo: prepara le immagini, boota,
digita, verifica, pulisce. Il runner test-shell-all.sh li orchestra
(sequenziale o --jobs N con overlay qcow2 per istanza).

I nomi sendkey sono VERIFICATI su QEMU 10.2.2 (vedi KEYMAP sotto; dettagli e
sonde in smoke41.py).
"""
import socket, subprocess, sys, time, os, re

KERNEL_DFLT = "target/x86_64-unknown-none/release/velord"
# ADR-0059: i dischi FAT non esistono piu' nel boot (FAT_DFLT/FAT2_DFLT
# rimossi: il boot e' solo ArcaFS).
# Secondo disco (stessa fixture di run.sh: UUID C0FFEE01, label SECOND):
# senza, t36 fallisce per ambiente (mount UUID impossibile).
# Terzo disco (Fase A/E1: root ArcaFS — senza, cardo va in panic loud per
# root=UUID assente e la shell non parte mai).
ARCA_DFLT = "userland/disk/arca.img"

# Nomi sendkey VERIFICATI su QEMU 10.2.2 ('period' NON esiste: il punto e'
# 'dot'; MAIUSCOLE come combo 'shift-x'; '>' e '<' = shift-dot/shift-comma;
# nomi parser Fase 41 sondati live in smoke41.py; '\\' e '|' richiedono il
# fix Us104Fix in porta).
KEYMAP = {" ": "spc", "\n": "ret", ".": "dot", "-": "minus", "/": "slash", "&": "shift-7",
          ">": "shift-dot", "<": "shift-comma", "!": "shift-1",
          "'": "apostrophe", '"': "shift-apostrophe",
          "\\": "backslash", "|": "shift-backslash",
          ";": "semicolon", "$": "shift-4", "*": "shift-8",
          "%": "shift-5",
          "?": "shift-slash", "#": "shift-3", "~": "shift-grave_accent",
          "=": "equal", "_": "shift-minus", ":": "shift-semicolon",
          "{": "shift-bracket_left", "}": "shift-bracket_right",
          "`": "grave_accent"}
KEYMAP.update({chr(c): "shift-%s" % chr(c).lower() for c in range(ord("A"), ord("Z") + 1)})

# ── Timing adattivi KVM/TCG (velocizzazione test) ─────────────────────
# Su KVM la latenza IRQ e' sub-tick (Fase 38.3): gli sleep conservativi
# pensati per TCG/overrun PS/2 (buffer a 1 byte, drain ~40-60ms) si possono
# stringere senza flaky. Su TCG restano i valori storici. Rilevato una volta
# a import (coerente con Shell.boot che usa /dev/kvm per -accel kvm).
FAST = os.path.exists("/dev/kvm")

# Per-tasto sendkey + pausa di drain ogni 12 tasti (overrun PS/2).
T_TYPE = 0.06 if FAST else 0.18
T_DRAIN_EVERY = 12
T_DRAIN = 0.25 if FAST else 1.0
# Post-sleep monitor, coda/poll wait_prompt, sleep run/run_out, heredoc.
T_MON = 0.05 if FAST else 0.12
T_MON_QUERY = 0.15 if FAST else 0.3
T_WAIT_TAIL = 0.1 if FAST else 0.3
T_WAIT_POLL = 0.05 if FAST else 0.2
T_RUN = 0.4 if FAST else 1.0
T_HDR_FIRST = 0.3 if FAST else 0.8
T_HDR_LINE = 0.2 if FAST else 0.5
T_HDR_LAST = 0.5 if FAST else 1.5
T_BOOT_SLEEP = 0.3 if FAST else 1.0
T_BOOT_POLL = 0.1 if FAST else 0.4
T_DUMP = 0.3 if FAST else 0.6


def parse_shell_args(mon_dflt, serial_dflt):
    """CLI minima dei file di fase: --mon/--serial/--arca/--kernel/
    --arca-format/--no-prep (il runner parallelo li passa per isolare le
    istanze; in locale valgono i default). ADR-0059: i dischi FAT sono
    spariti (il boot e' solo ArcaFS)."""
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--mon", default=mon_dflt)
    ap.add_argument("--serial", default=serial_dflt)
    ap.add_argument("--arca", default=ARCA_DFLT)
    ap.add_argument("--kernel", default=KERNEL_DFLT)
    ap.add_argument("--arca-format", default="raw",
                    help="raw (default) o qcow2 per gli overlay paralleli")
    ap.add_argument("--no-prep", action="store_true",
                    help="salta il seed ArcaFS (il runner lo fa una volta sola)")
    return ap.parse_args()


def prep_images(arca=ARCA_DFLT):
    """Seed fresco del volume ArcaFS (come run.sh). ADR-0059: niente piu'
    immagini FAT ne' iniettore — i binari vivono nel volume. La root e' sul
    volume: senza seed fresco la shell parte su dati stantii (E1:
    persistenza root tra fasi)."""
    subprocess.run(["bash", "scripts/arca-tool.sh",
                    os.path.join(os.getcwd(), arca)
                    if not os.path.isabs(arca) else arca], check=True)


class Checker:
    """Raccoglie PASS/FAIL con stampa immediata (come il vecchio main)."""
    def __init__(self):
        self.ok = True

    def check(self, name, cond):
        print(("PASS " if cond else "FAIL ") + name, flush=True)
        self.ok = self.ok and bool(cond)
        return bool(cond)


def has_line(out: bytes, text: bytes) -> bool:
    """Vero se `text` appare come RIGA INTERA nello slice seriale, in una
    qualunque delle tre forme in cui l'output puo' presentarsi (dipende solo
    da cosa precede sul seriale, mai dal contenuto):
    - `] text` (dopo timestamp: output a inizio riga, tipico `source`),
    - `$ text` (incollato al prompt senza newline, tipico digitato),
    - a inizio slice (prompt stampato prima del mark).
    Senza, gli assert posizionali dipendono dagli interleave kernel/timing
    (osservato: burst [blkdbg]/[irq1] ai bordi spostano gli anchor)."""
    return (b"] " + text + b"\n" in out
            or b"$ " + text + b"\n" in out
            or out.startswith(text + b"\n"))


def count_lines(out: bytes, text: bytes) -> int:
    """Conta le righe intere `text` in qualunque forma (vedi `has_line`):
    per i prima/dopo sui replay da history."""
    n = out.count(b"] " + text + b"\n") + out.count(b"$ " + text + b"\n")
    if out.startswith(text + b"\n"):
        n += 1
    return n


class Shell:
    """Una istanza QEMU + shell Velordo. I path distinguono le istanze
    parallele (il runner li assegna per fase)."""

    def __init__(self, mon, serial, kernel=KERNEL_DFLT, arca_img=None,
                 arca_format="raw"):
        self.mon = mon
        self.serial = serial
        self.kernel = kernel
        self.arca_img = ARCA_DFLT if arca_img is None else arca_img
        self.arca_format = arca_format
        self.q = None
        # wait_prompt event-driven sul conteggio prompt CONSUMATO.
        self._prompt_seen = 0
        self._need_sync = False

    # ── Monitor / typing ──────────────────────────────────────────

    def send_mon(self, cmd: str, sleep=None):
        if sleep is None:
            sleep = T_MON
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(self.mon)
        s.sendall(cmd.encode() + b"\n")
        time.sleep(0.03)
        try:
            s.recv(4096)
        except Exception:
            pass
        s.close()
        time.sleep(sleep)

    def mon_query(self, cmd: str) -> str:
        """Interroga il monitor HMP e ritorna la risposta (diagnostica)."""
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(self.mon)
            s.sendall(cmd.encode() + b"\n")
            time.sleep(T_MON_QUERY)
            try:
                data = s.recv(65536)
            except Exception:
                data = b""
            s.close()
            return data.decode(errors="replace")
        except Exception as e:
            return "<mon query failed: %s>" % e

    def type_text(self, text: str, sleep=None):
        # Su TCG sleep generoso: a 8 tasti/s il guest perde scancode sotto
        # carico (buffer PS/2 a 1 byte, drain IRQ1+schedule ~40-60ms —
        # overrun). Su KVM (FAST) latenza sub-tick: 0.06/tasto + drain corto.
        # Pausa di drain ogni 12 tasti (i comandi lunghi troncavano la coda).
        if sleep is None:
            sleep = T_TYPE
        for i, ch in enumerate(text):
            self.send_mon("sendkey %s" % KEYMAP.get(ch, ch), sleep=sleep)
            if (i + 1) % T_DRAIN_EVERY == 0:
                time.sleep(T_DRAIN)

    def read_log(self):
        try:
            with open(self.serial, "rb") as f:
                return f.read()
        except FileNotFoundError:
            return b""

    # ── Prompt sync ───────────────────────────────────────────────

    def wait_prompt(self, timeout=8):
        t0 = time.time()
        if self._need_sync:
            deadline = time.time() + timeout
            while (self.read_log().count(b"$ ") <= self._prompt_seen
                    and time.time() < deadline):
                time.sleep(T_WAIT_POLL)
            self._need_sync = False
        time.sleep(T_WAIT_TAIL)
        self._prompt_seen = self.read_log().count(b"$ ")
        dt = time.time() - t0
        if dt > 3:
            print("info slow wait_prompt %.1fs" % dt, flush=True)

    def run(self, cmd: str, sleep=None):
        if sleep is None:
            sleep = T_RUN
        t0 = time.time()
        self.wait_prompt()
        self.type_text(cmd)
        self.send_mon("sendkey ret")
        time.sleep(sleep)
        self._need_sync = True  # comando in volo: il prossimo wait sincronizza
        dt = time.time() - t0
        if dt > 10:
            print("info slow run %.1fs: %s" % (dt, cmd), flush=True)

    def run_out(self, cmd: str, sleep=None):
        """Esegue e ritorna SOLO l'output nuovo (coda del log): serve per
        gli assert di assenza (il log cumulativo contiene gia' tutto)."""
        if sleep is None:
            sleep = T_RUN
        t0 = time.time()
        self.wait_prompt()
        mark = len(self.read_log())
        self.type_text(cmd)
        self.send_mon("sendkey ret")
        time.sleep(sleep)
        self._need_sync = True  # come run()
        dt = time.time() - t0
        if dt > 10:
            print("info slow run_out %.1fs: %s" % (dt, cmd), flush=True)
        return self.read_log()[mark:]

    def run_source(self, path: str, sleep=None):
        """Esegue uno script via `source` (1 riga digitata invece di N
        comandi): ritorna l'output nuovo come run_out. Gli sleep espliciti
        passati restano rispettati (es. job bg che richiedono attesa). E1:
        dopo lo sleep si sincronizza sul prompt rientrato (mai slice
        troncata: sotto `--jobs N` il guest impiega piu' dello sleep a
        flussare script multi-comando). Timeout 8s di wait_prompt, mai hang.
        """
        if sleep is None:
            sleep = T_RUN
        t0 = time.time()
        self.wait_prompt()
        mark = len(self.read_log())
        self.type_text("source %s" % path)
        self.send_mon("sendkey ret")
        time.sleep(sleep)
        self._need_sync = True
        self.wait_prompt()
        dt = time.time() - t0
        if dt > 10:
            print("info slow run_source %.1fs: %s" % (dt, path), flush=True)
        return self.read_log()[mark:]

    def press(self, key: str, sleep=0.5):
        """Un tasto speciale via sendkey (43b: up/down/left/right/home/end/
        delete/esc/backspace, 44: ctrl-z). Niente Enter: lo manda il chiamante."""
        self.send_mon("sendkey %s" % key)
        time.sleep(sleep)

    def run_until(self, cmd: str, pattern: bytes, timeout=10, sleep=None):
        """Esegue e attende che `pattern` appaia nell'output nuovo (44: gli
        annunci `[bg pid N]` seguono il fork da disco, piu' lento dello sleep
        fisso di `run`). Ritorna lo slice comunque (vuoto di pattern a timeout:
        l'assert fallisce rumoroso, mai hang)."""
        if sleep is None:
            sleep = T_RUN
        self.wait_prompt()
        mark = len(self.read_log())
        self.type_text(cmd)
        self.send_mon("sendkey ret")
        time.sleep(sleep)
        self._need_sync = True
        deadline = time.time() + timeout
        while time.time() < deadline:
            out = self.read_log()[mark:]
            if pattern in out:
                return out
            time.sleep(0.2)
        return self.read_log()[mark:]

    def run_heredoc(self, first: str, lines, delim: str, sleep=None):
        """Heredoc Fase 42: prima riga, corpo riga per riga (prompt secondario
        `> `, non contato dai prompt), delimitatore."""
        if sleep is None:
            sleep = T_HDR_LAST
        self.wait_prompt()
        mark = len(self.read_log())
        self.type_text(first)
        self.send_mon("sendkey ret")
        time.sleep(T_HDR_FIRST)
        for ln in lines:
            self.type_text(ln)
            self.send_mon("sendkey ret")
            time.sleep(T_HDR_LINE)
        self.type_text(delim)
        self.send_mon("sendkey ret")
        time.sleep(sleep)
        self._need_sync = True
        return self.read_log()[mark:]

    # ── Boot / teardown ───────────────────────────────────────────

    def login(self, user="root", pwd="root", timeout=40):
        """Login al boot (ADR-0055): il login e' l'entry point nativo —
        chiede utente e password, poi avvia la shell. Aspetta il prompt
        'Nome utente:', digita le credenziali e lascia che la shell
        prenda il console."""
        t0 = time.time()
        while b"Nome utente:" not in self.read_log():
            if time.time() - t0 > timeout:
                raise RuntimeError("prompt di login mai comparso")
            time.sleep(T_WAIT_POLL)
        self.type_text(user + "\n")
        time.sleep(0.3)
        self.type_text(pwd + "\n")

    def boot(self, timeout=60):
        """Avvia QEMU, fa il login e attende la shell **POSIX** pronta
        (personality: prompt `$ `). Ritorna il Popen (anche in self.q)."""
        self._boot_start()
        deadline = time.time() + timeout
        while time.time() < deadline:
            data = self.read_log()
            if (b"[shell] starting" in data
                    and b"registered mount '/dev/input'" in data
                    and b"$ " in data):
                self.wait_prompt()
                return self.q
            time.sleep(T_BOOT_POLL)
        raise RuntimeError("shell non pronta")

    def boot_native(self, timeout=60):
        """Avvia QEMU, fa il login e attende la **shell nativa** (default dal
        login, ADR-0062): banner `[shell] Velordo shell nativa` + prompt
        `> `. Ritorna il Popen (anche in self.q).

        Il prompt nativo contiene ANSI truecolor e glifi Nerd Font: si
        sincronizza sul banner, non sul prompt (che non e' confrontabile
        come stringa). Il `_prompt_seen` di `wait_prompt` riguarda `$ ` e
        non si usa qui (i test nativi attendono pattern espliciti)."""
        self._boot_start()
        deadline = time.time() + timeout
        while time.time() < deadline:
            data = self.read_log()
            if (b"[shell] Velordo shell nativa" in data
                    and b"> " in data):
                time.sleep(T_WAIT_TAIL)
                return self.q
            time.sleep(T_BOOT_POLL)
        raise RuntimeError("shell nativa non pronta")

    def _boot_start(self):
        """Parte QEMU e fa il login (comune a `boot` e `boot_native`)."""
        for p in (self.serial, self.mon):
            try:
                os.unlink(p)
            except FileNotFoundError:
                pass
        args = [
            "qemu-system-x86_64", "-m", "256M", "-display", "none",
            "-serial", "file:" + self.serial,
            "-monitor", "unix:%s,server=on,wait=off" % self.mon,
            "-no-reboot", "-kernel", self.kernel,
        ]
        # E1: il drive ArcaFS (root obbligatoria, come run.sh) + cmdline
        # root=UUID. Senza, cardo va in panic loud e niente shell. ADR-0059:
        # i dischi FAT non vengono piu' attaccati.
        args.append("-drive")
        args.append("file=%s,format=%s,if=ide" % (self.arca_img, self.arca_format))
        args.append("-append")
        args.append("root=UUID=4152434100000001")
        # KVM se disponibile: abbatte la tassa di emulazione sui round-trip
        # tastiera (IRQ1→kbd→tty→shell→prompt). `-cpu host` come bench.sh
        # (TSC/round-trip a velocita' nativa invece che emulata).
        if FAST:
            args[1:1] = ["-accel", "kvm", "-cpu", "host"]
        self.q = subprocess.Popen(args)
        time.sleep(T_BOOT_SLEEP)
        kvm_info = self.mon_query("info kvm").replace("\r", "").strip().split("\n")
        kvm_tail = [l for l in kvm_info if "kvm" in l.lower()][-1:]
        print("info accel: %s (timing %s)" % (" | ".join(kvm_tail) if kvm_tail else kvm_info[-1:],
              "fast" if FAST else "slow"), flush=True)
        # Il login parte col boot (ADR-0055): credenziali e poi la shell.
        self.login()

    def terminate(self):
        if self.q is not None:
            self.q.terminate()
            try:
                self.q.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.q.kill()
            self.q = None

    # ── VGA screendump (test backspace/clear) ─────────────────────

    def screendump(self, path: str):
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        self.send_mon("screendump %s" % path, sleep=T_DUMP)
        deadline = time.time() + 10
        while time.time() < deadline:
            if os.path.exists(path) and os.path.getsize(path) > 1000:
                time.sleep(0.3)
                return True
            time.sleep(0.2)
        return False


def load_ppm(path: str):
    with open(path, "rb") as f:
        data = f.read()
    assert data[:2] == b"P6", "screendump non PPM"
    # Header: P6 + commenti + "W H" + maxval, poi byte raw RGB.
    parts = []
    i = 2
    while len(parts) < 3:
        while data[i:i+1].isspace():
            i += 1
        if data[i:i+1] == b"#":
            while data[i:i+1] != b"\n":
                i += 1
            continue
        j = i
        while not data[j:j+1].isspace():
            j += 1
        parts.append(data[i:j])
        i = j
    while data[i:i+1].isspace():
        i += 1
    w, h = int(parts[0]), int(parts[1])
    return w, h, data[i:i + w * h * 3]


def lit_pixels(path: str):
    """Byte accesi nello screendump (testo bianco su nero), escluse le
    ultime 3 scanline (cursore lampeggiante)."""
    w, h, p = load_ppm(path)
    row = w * 3
    lit = 0
    for y in range(h - 3):
        off = y * row
        for k in range(row):
            if p[off + k] != 0:
                lit += 1
    return lit


def diff_masked(a: str, b: str):
    """Byte diversi tra due screendump, mascherando le ultime 3 scanline
    (cursore hardware lampeggiante: non e' contenuto)."""
    wa, ha, pa = load_ppm(a)
    wb, hb, pb = load_ppm(b)
    if (wa, ha, len(pa)) != (wb, hb, len(pb)):
        return -1
    row = wa * 3
    diff = 0
    for y in range(ha - 3):
        off = y * row
        for k in range(row):
            if pa[off + k] != pb[off + k]:
                diff += 1
    return diff
