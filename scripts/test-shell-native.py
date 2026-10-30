#!/usr/bin/env python3
"""Test shell NATIVA + bucket `binary` (S3.2b-4, ADR-0064).

La shell nativa e' l'entry point del login (ADR-0062): cerca i comandi
built-in, poi prova a eseguire il nome digitato come programma in un bucket
di tipo `binary` (namespace piatto, niente sottocartelle). Il test copre il
percorso completo: `BIN_LIST` -> `obj_get` -> `R_RIGHTS_EXECUTE` ->
`spawn_image` con argv -> exit notificato al padre, e verifica che il
response ring resti sincronizzato (una op FS dopo lo spawn: `users`).

Autonomo come le altre fasi: prepara il seed ArcaFS (salvo --no-prep), boota,
digita, verifica sul log seriale, pulisce. Vedi scripts/shell_harness.py.
"""
import sys, os, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import (Shell, Checker, prep_images, parse_shell_args,
                           T_WAIT_POLL, T_WAIT_TAIL)


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


def main():
    args = parse_shell_args("/tmp/velordo-native-mon.sock", "/tmp/velordo-native-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel,
               arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot_native()
        log = sh.read_log()
        c.check("login -> shell nativa (banner)", b"[shell] Velordo shell nativa" in log)
        c.check("prompt nativo pronto", b"> " in log)

        # `help` e' un built-in: prova che il REPL risponde prima di toccare
        # il percorso esterno.
        out = run_cmd(sh, "help", b"<programma>")
        c.check("help built-in", b"Comandi disponibili" in out
                and b"<programma>" in out)

        # Percorso completo: BIN_LIST -> oggetto piatto in `usr` -> EXECUTE ->
        # spawn_image. Il nome NON ha path ne' suffisso `.bin`.
        out = run_cmd(sh, "runhello hello", b"Processo terminato")
        c.check("bucket binary scoperto (usr)",
                b"[run] Esecuzione di 'runhello' dal bucket 'usr'" in out)
        c.check("runhello eseguito con argv", b"runhello: hello" in out)
        c.check("exit notificato al padre", b"[run] Processo terminato" in out)

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
        c.check("while", b"0" in out and b"1" in out and b"2" in out)

        # Error handling (unknown var)
        out = run_cmd(sh, "dump(unknown)", b"variabile non trovata")
        c.check("errore variabile", b"variabile non trovata" in out)

        # Block assignment
        out = run_cmd(sh, "let a = 1; { a = a + 1; }; dump(a)", b"2")
        c.check("assign block", b"2" in out)

        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
