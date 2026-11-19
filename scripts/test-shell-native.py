#!/usr/bin/env python3
"""Test shell NATIVA + bucket `binary` (S3.2b-4, ADR-0064).

La shell nativa e' l'entry point del login (ADR-0062): cerca i comandi
built-in, poi prova a eseguire il nome digitato come programma in un bucket
di tipo `binary` (namespace piatto, niente sottocartelle). Il test copre il
percorso completo: `BIN_LIST` -> `obj_get` -> `R_RIGHTS_EXECUTE` ->
`spawn_image` con argv -> exit notificato al padre, e verifica che il
response ring resti sincronizzato (una op FS dopo lo spawn: `users`).

Due modi, stessi 33 check:

- **autonomo** (default, `scripts/test-shell-all.sh native`): prepara il seed
  ArcaFS, boota QEMU, fa il login (`boot_native`), digita.
- **attach** (`--attach-pid`, ADR-0081): si attacca al QEMU che
  `run-tests.sh` ha gia' avviato per la suite (seriale e monitor passati dal
  gate). Niente spawn e niente seed: il volume e' montato dal guest. Il
  prompt del login arriva DOPO la suite, quindi `--login-timeout` copre
  l'attesa (il gate lo attende gia' in bash: qui e' solo una rete di
  sicurezza).

Vedi scripts/shell_harness.py.
"""
import sys, os, time, re
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import (Shell, Checker, prep_images, parse_shell_args,
                           T_WAIT_POLL, T_WAIT_TAIL, T_RUN)


def run_cmd(sh, cmd, pattern=None, timeout=30):
    """Digita `cmd` + Enter e ritorna lo slice nuovo di log. Se `pattern` e'
    dato attende che compaia (spawn da disco + exit: piu' lento di uno sleep
    fisso)."""
    mark = len(sh.read_log())
    sh.type_text(cmd)
    sh.send_mon("sendkey ret")
    if pattern is not None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            out = sh.read_log()[mark:]
            if pattern in out:
                break
            time.sleep(T_WAIT_POLL)
    time.sleep(T_WAIT_TAIL)
    return sh.read_log()[mark:]


def _native_args(ap):
    """Opzioni del modo attach (gate end-to-end, ADR-0081)."""
    ap.add_argument("--attach-pid", type=int, default=0,
                    help="pid di un QEMU gia' avviato: niente spawn ne' seed")
    ap.add_argument("--login-timeout", type=float, default=300.0,
                    help="attesa del prompt del login in modo attach "
                         "(dopo la suite del gate)")


def run_checks(sh, c):
    """I 33 check della shell nativa (comuni a boot autonomo e gate)."""
    log = sh.read_log()
    banner = b"[shell] Velordo shell nativa"
    c.check("login -> shell nativa (banner)", banner in log)
    # Solo DOPO il banner: nel gate il log contiene gia' l'output della suite
    # (un `> ` sparso nei test non deve contare come prompt nativo).
    after_banner = log[log.rindex(banner):] if banner in log else b""
    c.check("prompt nativo pronto", b"> " in after_banner)

    # E0 (ADR-0082): al prompt la shell dorme in `recv()` su un evento di
    # `porta`, non si risveglia ogni tick. Un ritorno al polling si vede
    # subito qui (prima: futex a 1 tick; con uno spin: ~100%).
    time.sleep(T_WAIT_TAIL)
    cpu_main = sh.cpu_percent(1.0)
    print("info CPU prompt principale: %.1f%%" % cpu_main, flush=True)
    c.check("prompt principale idle su eventi (CPU < 5%)", cpu_main < 5.0)

    # E1 (ADR-0083): l'orologio del prompt si aggiorna in-place mentre la
    # shell e' ferma al prompt, senza ridisegnare il prompt: il repaint e'
    # `ESC[D` x13 (8 cifre dell'ora + 5 celle del suffisso " ✓ > ") + l'ora
    # + `ESC[C` x5, con **saldo zero** delle celle (un saldo positivo farebbe
    # crescere la riga di 8 celle al secondo). Il console server muove di UNA
    # cella per sequenza (ignora i parametri CSI), quindi il repaint e'
    # ripetuto, non `ESC[13D`. Si aspetta al prompt senza digitare e si
    # verifica che l'ora compaia e avanzi fra due campioni.
    clock_re = rb"(?:\x1b\[D){13}\x1b\[[0-9;]*m(\d\d:\d\d:\d\d)\x1b\[0m(?:\x1b\[C){5}"
    time.sleep(1.5)
    ticks1 = re.findall(clock_re, sh.read_log())
    first = ticks1[-1] if ticks1 else None
    time.sleep(1.5)
    ticks2 = re.findall(clock_re, sh.read_log())
    second = ticks2[-1] if ticks2 else None
    c.check("orologio prompt aggiorna in-place (E1)",
            first is not None and second is not None and second != first)

    # `help` e' un built-in: prova che il REPL risponde prima di toccare
    # il percorso esterno.
    out = run_cmd(sh, "help", b"<programma>")
    c.check("help built-in", b"Comandi disponibili" in out
            and b"<programma>" in out)

    # Percorso completo: BIN_LIST -> oggetto piatto in `usr` -> EXECUTE ->
    # spawn_image. Il nome NON ha path ne' suffisso `.bin`.
    mark = len(sh.read_log())
    out = run_cmd(sh, "runhello hello", b"Processo terminato")
    c.check("bucket binary scoperto (usr)",
            b"[run] Esecuzione di 'runhello' dal bucket 'usr'" in out)
    c.check("runhello eseguito con argv", b"runhello: hello" in out)
    c.check("exit notificato al padre", b"[run] Processo terminato" in out)

    # Il prompt e' disegnato UNA volta per riga: il vecchio editor lo
    # riscriveva a ogni tasto (~1 prompt per carattere). Attende il prompt
    # successivo, poi conta quelli nel tratto digitato + comando.
    deadline = time.time() + 5
    while sh.read_log()[mark:].count(b" > ") < 1 and time.time() < deadline:
        time.sleep(T_WAIT_POLL)
    typed = sh.read_log()[mark:]
    c.check("prompt una volta per riga (no redraw per tasto)",
            1 <= typed.count(b" > ") <= 2)

    # argv multiplo: la shell passa i token via pack_args (prima venivano
    # ignorati: il figlio vedeva solo argv[0]).
    out = run_cmd(sh, "runhello a b", b"Processo terminato")
    c.check("argv multiplo inoltrato", b"runhello: a" in out
            and b"runhello: b" in out)

    # Op FS dopo lo spawn: il response ring non e' desincronizzato (il
    # doppio frame di RIGHTS_EXECUTE lo spostava e `users` falliva).
    out = run_cmd(sh, "users", b"Utenti registrati")
    c.check("users dopo lo spawn (ring sincronizzato)",
            b"Utenti registrati" in out and b"root" in out)
    # Regressione (ADR-0080 fase 6): la shell spawnata dal login senza
    # verifica era `Untrusted`, quindi con un tetto `DEFAULT_UNKNOWN_OPS`
    # che non include `RIGHTS_EXECUTE`: ogni programma di `usr` falliva.
    # Un diniego ora e' tipizzato (`EXECUTE negato`), quindi qui non deve
    # comparire ne' quello ne' un errore opaco: `users` deve funzionare.
    c.check("nessun diniego/errore di permesso su usr",
            b"ERRORE rights_execute" not in out
            and b"EXECUTE negato" not in out)

    # Suggerimento inline (stile oh-my-zsh): digitando `us` la history
    # propone il resto di `users` (`ers`) in Slate, poi riporta il cursore
    # a fine buffer. Poi svuota la riga con due backspace + Invio.
    mark = len(sh.read_log())
    sh.type_text("us")
    time.sleep(T_WAIT_TAIL)
    hint = sh.read_log()[mark:]
    c.check("suggerimento inline dalla history",
            b"\x1b[38;2;141;150;168mers\x1b[0m" in hint)
    sh.send_mon("sendkey backspace")
    sh.send_mon("sendkey backspace")
    sh.send_mon("sendkey ret")
    time.sleep(T_RUN)

    # Up richiama l'ultimo comando (redraw completo della riga) e Invio
    # lo esegue: prova che la history sia davvero alimentata.
    mark = len(sh.read_log())
    sh.press("up", sleep=T_WAIT_TAIL)
    nav = sh.read_log()[mark:]
    c.check("history Up richiama l'ultimo comando", b"> users" in nav)
    sh.send_mon("sendkey ret")
    time.sleep(T_RUN)

    # Comando inesistente: la ricerca nei bucket binary non deve produrre
    # un falso positivo ne' un hang.
    out = run_cmd(sh, "nosuchprog", b"Comando non trovato")
    c.check("programma ignoto", b"Comando non trovato: nosuchprog" in out)
    # ----------------------------
    # INTERPRETER TESTS
    # ----------------------------
    # Persistenza variabile
    out = run_cmd(sh, "let a = 1;", b"")
    c.check("persistenza a", b"" in out)  # no output
    out = run_cmd(sh, "dump(a)", b"1")
    c.check("dump a", b"1" in out)

    # Aritmetica
    out = run_cmd(sh, "let x = 1; let y = 2; dump(x + y)", b"3")
    c.check("aritmetica", b"3" in out)

    # Concatenazione stringhe usando variabili
    out = run_cmd(sh, 'let a = "hello" + " world"', b"")
    c.check("concat stringa assign", b"" in out)
    out = run_cmd(sh, 'dump(a)', b'\"hello world\"')
    c.check("concat stringa dump", b'\"hello world\"' in out)

    # If/else
    out = run_cmd(sh, "if 1 < 2 { dump(\"yes\") } else { dump(\"no\") }", b"\"yes\"")
    c.check("if/else", b"yes" in out)

    # While loop (timeout 60s)
    out = run_cmd(sh, "let i = 0; while i < 3 { dump(i); i = i + 1; }", b"\"0\"", timeout=60)
    ok_while = b"0" in out and b"1" in out and b"2" in out
    c.check("while", ok_while)

    # Error handling (unknown var)
    out = run_cmd(sh, "dump(unknown)", b"variabile non trovata")
    c.check("errore variabile", b"variabile non trovata" in out)

    # Block assignment
    out = run_cmd(sh, "let a = 1; { a = a + 1; }; dump(a)", b"2")
    c.check("assign block", b"2" in out)

    # For loop (range escludente 0..3 -> stampa 0, 1, 2)
    out = run_cmd(sh, "for i in 0..3 { dump(i); }", b"\"0\"", timeout=30)
    ok_for = b"0" in out and b"1" in out and b"2" in out
    c.check("for loop", ok_for)

    # Commento di linea //
    out = run_cmd(sh, "// questo e' un commento\ndump(1)", b"1")
    c.check("commento linea", b"1" in out)

    # Commento blocco /* */
    out = run_cmd(sh, "/* commento blocco */ dump(2)", b"2")
    c.check("commento blocco", b"2" in out)

    # Null uguaglianza
    out = run_cmd(sh, "dump(null == null)", b"true")
    c.check("null uguale null", b"true" in out)

    # type() valuta l'espressione (stesso difetto di dump: prima "expr")
    out = run_cmd(sh, "type(null == null)", b"bool")
    c.check("type valuta espressione", b"bool" in out)

    # Operatore unario !
    out = run_cmd(sh, "dump(!true)", b"false")
    c.check("not true", b"false" in out)

    # Scope if coerente con blocco { }: let dentro if si propaga al parent
    out = run_cmd(sh, "if 1 < 2 { let x = 42; } dump(x)", b"42")
    c.check("scope if propagato", b"42" in out)

    # Regressione "CPU 100% al prompt" (ADR-0067): `create-user` senza
    # argomento entra in `read_line_from_keyboard`, che deve dormire su un
    # evento di input (E0, ADR-0082; prima era un futex a 1 tick, e in questa
    # versione del binario uno spin nudo). Poi Invio a vuoto (nome vuoto) per
    # tornare al prompt.
    mark = len(sh.read_log())
    sh.type_text("create-user")
    sh.send_mon("sendkey ret")
    deadline = time.time() + 10
    while (b"Nome utente:" not in sh.read_log()[mark:]
           and time.time() < deadline):
        time.sleep(T_WAIT_POLL)
    time.sleep(T_WAIT_TAIL)
    cpu = sh.cpu_percent(1.0)
    print("info CPU prompt create-user: %.1f%%" % cpu, flush=True)
    c.check("prompt create-user idle senza busy-wait (CPU < 5%)",
            cpu < 5.0)
    sh.send_mon("sendkey ret")
    time.sleep(T_RUN)
    c.check("nome vuoto rifiutato (ritorno al prompt)",
            b"nome vuoto" in sh.read_log()[mark:])


def main():
    args = parse_shell_args("/tmp/velordo-native-mon.sock",
                            "/tmp/velordo-native-serial.log",
                            extra=_native_args)
    attached = args.attach_pid != 0
    if attached:
        # Il volume e' montato dal guest del gate: MAI ri-seedare.
        sh = Shell.attached(args.mon, args.serial, args.attach_pid)
    else:
        if not args.no_prep:
            prep_images()
        sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel,
                   arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        if attached:
            sh.attach_native(login_timeout=args.login_timeout)
        else:
            sh.boot_native()
        run_checks(sh, c)
        c.summary()
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        c.summary()
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
