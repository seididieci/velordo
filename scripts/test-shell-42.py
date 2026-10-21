#!/usr/bin/env python3
"""Test shell PIPE (Fase 42): a | b, N stadi, redirect+pipe, waitpid status, stadi run, bg rifiutata, heredoc, EOF, streaming oltre capacita'."
Avvia il proprio QEMU (seriale + monitor dedicati); i comandi viaggiano in
script via `source` (heredoc inclusi: corpi nel file, come da tastiera).
Verifica sul log seriale. Autonomo: prepara le immagini (salvo --no-prep),
boota, testa, pulisce le sue fixture. Vedi scripts/shell_harness.py.
"""
import sys, os, re
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from shell_harness import Shell, Checker, prep_images, parse_shell_args, has_line

SH = "/test/sh"


def main():
    args = parse_shell_args("/tmp/velordo-42-mon.sock", "/tmp/velordo-42-serial.log")
    if not args.no_prep:
        prep_images()
    sh = Shell(mon=args.mon, serial=args.serial, kernel=args.kernel, arca_img=args.arca, arca_format=args.arca_format)
    c = Checker()
    try:
        sh.boot()
        # Pipe base builtin|builtin + 3 stadi.
        out = sh.run_source(SH + "/p42a.txt")
        found = b"hello42" in out
        c.check("42 pipe base (echo | cat)", found)
        # cat aggiunge sempre un \n finale: hello.txt da 26B/1 riga diventa
        # 27B/2 righe nella pipe (coerente col redirect su file).
        found = b"2 4 27 -" in out
        c.check("42 cat | wc (2 4 27)", found)
        found = b"b" in out and b"[exit" not in out
        c.check("42 pipe 3 stadi", found)

        # Pipe + redirect combinati (esplicito vince sul link).
        out = sh.run_source(SH + "/p42b.txt")
        found = b"comb42" in out
        c.check("42 pipe + > file", found)

        # Errore nello stadio sinistro: messaggio sul terminale, destro a EOF.
        out = sh.run_source(SH + "/p42c.txt")
        found = b"cannot open missing404" in out
        c.check("42 errore stadio sx", found)

        # waitpid con status: status del gruppo = ultimo stadio (bash).
        # Riga intera "] N" (non ultima-riga: burst kernel ai bordi).
        out = sh.run_source(SH + "/p42d.txt")
        found = has_line(out, b"0")
        c.check("42 status = ultimo stadio (0)", found)
        out = sh.run_source(SH + "/p42e.txt")
        found = b"[exit 1]" in out
        c.check("42 status ultimo fallito ([exit 1])", found)
        found = has_line(out, b"1")
        c.check("42 $? dopo pipe fallita (=1)", found)

        # Stadio run (fork+exec con grant, non builtin).
        out = sh.run_source(SH + "/p42f.txt")
        found = b"runhello: hello" in out
        c.check("42 stadio run | cat", found)
        found = b"[exit 1]" in out
        c.check("42 run fallito in pipe ([exit 1])", found)

        # & su pipeline multi-stadio: job multi-pid rimandato oltre la 44a
        # (rifiuto chiaro, mai hang).
        out = sh.run_source(SH + "/p42g.txt")
        found = b"non supportata" in out
        c.check("42 bg pipeline rifiutata", found)
        # Pipe trailing ignorata come gli altri connettori (mai errore).
        found = b"tp42" in out
        c.check("42 pipe trailing ignorata", found)

        # Heredoc da file: corpo letterale nello stdin di cat.
        out = sh.run_source(SH + "/p42h.txt")
        found = b"riga uno" in out and b"riga due" in out
        c.check("42 heredoc base", found)
        # Heredoc + pipe: corpo + \n finale di cat ("aa bb\ncc\n" -> 3 3 10,
        # come `cat hello.txt | wc`: cat aggiunge sempre un \n in coda).
        found = b"3 3 10 -" in out
        c.check("42 heredoc | wc (3 3 10)", found)
        # Heredoc + redirect file.
        found = b"salvata" in out
        c.check("42 heredoc > file", found)

        # EOF vero: file vuoto in pipe (mai hang su Empty). cat stampa comunque
        # il suo \n finale: la pipe vede "\n" = 1 0 1 (l'EOF e' propagato).
        out = sh.run_source(SH + "/p42i.txt")
        found = b"1 0 1 -" in out
        c.check("42 pipe EOF (1 0 1)", found)

        # Streaming oltre la capacita' pipe (8192): raddoppi via >> poi pipe.
        # p42j: 25 comandi + ~10 KiB di copie/pipe (lento per natura):
        # sleep esplicito, altrimenti lo slice si chiude a meta'.
        out = sh.run_source(SH + "/p42j.txt", sleep=4.0)
        m = re.search(rb"(\d+) (\d+) (\d+) -", out)
        # Atteso >> cap (seed 19B con \n di echo+cat, 9 raddoppi + \n di cat
        # a ogni giro: 10239B): conta linee/parole/byte.
        found = m is not None and int(m.group(3)) > 8192
        c.check("42 pipe oltre capacita' (>8192B)", found)
        m2 = re.search(rb"(\d+) (\d+) (\d+) /big42", out)
        # La pipe vede file + \n finale di cat: linee+1, parole uguali, byte+1.
        found = (m is not None and m2 is not None
                 and int(m.group(1)) == int(m2.group(1)) + 1
                 and m.group(2) == m2.group(2)
                 and int(m.group(3)) == int(m2.group(3)) + 1)
        c.check("42 pipe == file + newline cat", found)
        return 0 if c.ok else 1
    except RuntimeError as e:
        print("FAIL: %s" % e)
        return 1
    finally:
        sh.terminate()


if __name__ == "__main__":
    sys.exit(main())
