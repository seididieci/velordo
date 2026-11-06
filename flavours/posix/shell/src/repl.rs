use super::*;

// ── Sorgente righe (tastiera vs script) ─────────────────────────────
// Il riempimento heredoc e il driver degli script pescano dalla stessa
// sorgente: un heredoc in script consuma le righe dopo la sua (come bash).

pub(crate) trait LineSrc {
    fn next_line(&mut self, prompt: &str) -> Option<String>;
}

pub(crate) struct TtySrc;

impl LineSrc for TtySrc {
    fn next_line(&mut self, prompt: &str) -> Option<String> {
        // Il prompt secondario `> ` e' dell'heredoc (corpo letterale, mai in
        // history — 43b); il prompt comando registra.
        Some(term::read_line_rec(prompt, prompt != "> "))
    }
}

/// Script in memoria: righe logiche e corpi heredoc condividono il cursore.
pub(crate) struct ScriptSrc {
    lines: Vec<String>,
    pos: usize,
}

impl ScriptSrc {
    pub(crate) fn new(text: &str) -> Self {
        let mut lines = Vec::new();
        for l in text.split('\n') {
            // Tolleranza CRLF (file iniettati da host via mtools).
            let mut s = String::from(l);
            if s.ends_with('\r') {
                s.pop();
            }
            lines.push(s);
        }
        Self { lines, pos: 0 }
    }
}

impl LineSrc for ScriptSrc {
    fn next_line(&mut self, _prompt: &str) -> Option<String> {
        if self.pos < self.lines.len() {
            let l = self.lines[self.pos].clone();
            self.pos += 1;
            Some(l)
        } else {
            None
        }
    }
}

/// Esito di una riga logica: `Continue` aggiorna `$?`, `Exit` termina lo
/// script col code (mai la shell: `exit` in script = fine script).
/// `None` = riga vuota (niente da eseguire, `$?` invariato).
pub(crate) enum LineOutcome {
    Continue(i64),
    Exit(i64),
}

// ── Entry point ─────────────────────────────────────────────────────

libr::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    let _ = civis::print_string(b"[shell] starting\n");

    // Apri il terminale (tastiera + output VGA via console server).
    if !term::term_init() {
        let _ = civis::print_string(b"[shell] cannot open /dev/input/keyboard\n");
        civis::exit(1);
    }

    // Banner
    term::term_print("Velordo shell v0.1\n");
    term::term_print("Type 'help' for commands\n");
    term::term_print("\n");
    cwd::cwd_set(String::from("/"));
    let mut status: i64 = 0; // `$?`: exit code dell'ultimo comando eseguito

    // REPL
    loop {
        // Prompt dinamico con cwd (Fase 18.1-bis): "/" → "$ ", senno'
        // "<cwd>$ ". La cwd non passa mai da tty::emit: nessun impatto sul
        // floor del backspace (conta solo i digitati).
        let cwd = cwd::cwd_get();
        let prompt;
        if cwd == "/" {
            prompt = String::from("$ ");
        } else {
            prompt = cwd + "$ ";
        }
        // T4/bulk: ogni comando ha un budget di mirror seriale (un output
        // enorme non deve passare tutto sulla UART). Il reset e' prima del
        // prompt: cosi' anche il prompt del comando successivo e' mirrorato.
        term::reset_mirror_budget();
        let line = term::read_line(&prompt);
        // In REPL `Exit` non accade mai (script_mode=false: `exit` diverge
        // via civis::exit); l'arm resta per totalita'.
        match run_one_line(&line, status, &mut TtySrc, false) {
            None => {}
            Some(LineOutcome::Continue(s)) => status = s,
            Some(LineOutcome::Exit(s)) => status = s,
        }
    }
}

/// Esegue una riga logica: parse con `$?` + riempimento heredoc dalla
/// sorgente + esecuzione con short-circuit. `status` = `$?` in ingresso.
pub(crate) fn run_one_line(
    line: &str,
    status: i64,
    src: &mut dyn LineSrc,
    script_mode: bool,
) -> Option<LineOutcome> {
    // Parser Fase 41+42: quote/escape/commenti, ; && || & | <<, redirect
    // 40.4, $VAR ${VAR} $? $$, ~, glob. Comandi gia' espansi con connettori.
    let mut seq = match parser::parse_line(line, status) {
        Ok(s) => s,
        Err(e) => {
            match e {
                parser::ParseError::MissingTarget(op) => {
                    term::term_err("redirect: missing target after ");
                    term::term_err(op);
                }
                parser::ParseError::BadSubst => {
                    term::term_err("sostituzione errata");
                }
            }
            term::term_err("\n");
            return Some(LineOutcome::Continue(2));
        }
    };
    // Heredoc (Fase 42): per ogni `<<DELIM` senza corpo, leggi righe fino
    // alla riga col solo delimitatore (corpo sempre letterale: niente
    // parsing/espansione dentro). Come bash, si legge tutto PRIMA di
    // eseguire (anche per gli stadi dopo la pipe). In script le righe
    // vengono dal file, non dalla tastiera.
    if !fill_heredocs(&mut seq, src, script_mode) {
        return Some(LineOutcome::Continue(1));
    }
    exec_seq(&seq, status, script_mode)
}

/// Riempie i corpi heredoc mancanti dalla sorgente. False = EOF prima del
/// delimitatore (solo script: la tastiera non da' mai EOF).
fn fill_heredocs(seq: &mut parser::Seq, src: &mut dyn LineSrc, script: bool) -> bool {
    for cmd in seq.cmds.iter_mut() {
        for r in cmd.redirs.iter_mut() {
            if r.heredoc && r.heredoc_body.is_none() {
                let delim = r.target.clone();
                let mut body = String::new();
                loop {
                    match src.next_line("> ") {
                        Some(l) if l == delim => break,
                        Some(l) => {
                            body.push_str(&l);
                            body.push('\n');
                        }
                        None => {
                            if script {
                                term::term_err("source: unexpected EOF in heredoc\n");
                            }
                            return false;
                        }
                    }
                }
                r.heredoc_body = Some(body);
            }
        }
    }
    true
}

/// Esecuzione di una Seq: gruppi di stadi legati da Pipe girano concorrenti
/// (una pipeline); il resto resta sequenziale con short-circuit.
/// None = Seq vuota (`$?` invariato).
fn exec_seq(seq: &parser::Seq, status: i64, script_mode: bool) -> Option<LineOutcome> {
    if seq.cmds.is_empty() {
        return None;
    }
    let mut st = status;
    let mut i = 0;
    while i < seq.cmds.len() {
        let mut j = i;
        while j + 1 < seq.cmds.len()
            && matches!(seq.cons.get(j), Some(parser::Conn::Pipe))
        {
            j += 1;
        }
        if j > i {
            // Pipeline: short-circuit sul connettore prima del gruppo.
            if i > 0 {
                let go = match seq.cons.get(i - 1) {
                    Some(parser::Conn::And) => st == 0,
                    Some(parser::Conn::Or) => st != 0,
                    _ => true,
                };
                if !go {
                    i = j + 1;
                    continue;
                }
            }
            st = cmd_run::cmd_pipeline(&seq.cmds[i..=j]);
            i = j + 1;
            continue;
        }
        // Comando singolo: short-circuit come prima.
        if i > 0 {
            let go = match seq.cons.get(i - 1) {
                Some(parser::Conn::And) => st == 0,
                Some(parser::Conn::Or) => st != 0,
                _ => true,
            };
            if !go {
                i += 1;
                continue;
            }
        }
        let cmd = &seq.cmds[i];
        // Singolo con heredoc ma senza pipe: stessa via pipeline a 1
        // stadio (il percorso exec_single non sa i corpi heredoc).
        if cmd.redirs.iter().any(|r| r.heredoc) {
            st = cmd_run::cmd_pipeline(&seq.cmds[i..=i]);
        } else {
            match exec_single(cmd, st, script_mode) {
                LineOutcome::Continue(c) => st = c,
                LineOutcome::Exit(c) => return Some(LineOutcome::Exit(c)),
            }
        }
        i += 1;
    }
    Some(LineOutcome::Continue(st))
}

/// Esegue un comando parsato (redirect + dispatch + restore). `status` = `$?`
/// in ingresso (serve a `source`: la prima riga dello script espande il `$?`
/// esterno). In script_mode `exit` non uccide la shell ma termina lo script.
/// I prefissi `VAR=v` (43a): senza comando = set persistenti; con builtin =
/// save/set/restore; con esterni = blocco envp (mai nell'ambiente shell).
fn exec_single(cmd: &parser::Command, status: i64, script_mode: bool) -> LineOutcome {
    let args: Vec<&str> = cmd.argv.iter().map(|s| s.as_str()).collect();
    if args.is_empty() {
        // Solo env e/o redirect (`A=1`, `> /f`, `A=1 > /f`): prima gli effetti
        // collaterali dei redirect (bash), poi i set persistenti. A open
        // fallita: niente set.
        if !cmd.redirs.is_empty() {
            match redirect::open_all(&cmd.redirs) {
                Ok(fds) => redirect::close_all(fds),
                Err((t, e)) => {
                    redirect::report_open_error(&t, e);
                    return LineOutcome::Continue(1);
                }
            }
        }
        for (name, val) in cmd.env.iter() {
            parser::vars_set(name, val);
        }
        return LineOutcome::Continue(0);
    }
    // In script `exit [code]` termina lo script (mai la shell).
    if script_mode && args[0] == "exit" {
        let code = match args.get(1) {
            None => 0,
            Some(s) => match cmd_info::parse_i64(s) {
                Some(n) => n,
                None => {
                    term::term_err("exit: bad code\n");
                    1
                }
            },
        };
        return LineOutcome::Exit(code);
    }
    // Apre TUTTI i target in ordine; a fallimento riporta su stderr e
    // salta il comando (mai nel file).
    // `run` e gli esterni (bare word via PATH) non passano di qui:
    // aprono+grant da se' in `cmd_run`.
    let external = args[0] != "run" && !is_builtin(args[0]);
    let mut fds = [-1i64; 3];
    if !cmd.redirs.is_empty() && !external && args[0] != "run" {
        match redirect::open_all(&cmd.redirs) {
            Ok(f) => fds = f,
            Err((t, e)) => {
                redirect::report_open_error(&t, e);
                return LineOutcome::Continue(1);
            }
        }
        libr::set_stdio(fds);
    }
    let code = match args[0] {
        "run" => cmd_run::cmd_run(&args, &cmd.redirs, cmd.bg, &cmd.env),
        _ if external => {
            // Bare word (43a): ricerca PATH + run implicito. Con `/` e'
            // path diretto (come bash), senza e' cercato in PATH.
            match cmd_run::resolve_prog(args[0]) {
                Some(path) => {
                    let mut v: Vec<&str> = Vec::new();
                    v.push("run");
                    v.push(path.as_str());
                    for a in args.iter().skip(1) {
                        v.push(a);
                    }
                    cmd_run::cmd_run(&v, &cmd.redirs, cmd.bg, &cmd.env)
                }
                None => {
                    term::term_err("unknown command: ");
                    term::term_err(args[0]);
                    term::term_err("\n");
                    127
                }
            }
        }
        _ => with_env(&cmd.env, | | {
            if args[0] == "source" {
                cmd_source::cmd_source(&args, status)
            } else {
                dispatch_builtin(&args)
            }
        }),
    };
    // Restore: output/errori giá instradati (hook B1 / term_err); le write
    // restano best-effort (mirror seriale gia' emesso).
    if fds != [-1i64; 3] {
        libr::clear_stdio();
        redirect::close_all(fds);
    }
    LineOutcome::Continue(code)
}

/// Applica `env` alle VARS, esegue `f`, ripristina (builtin mono-comando,
/// 43a). Ultimo vince tra duplicati (bash `A=1 A=2 cmd` → A=2).
fn with_env(env: &[(String, String)], f: impl FnOnce() -> i64) -> i64 {
    let mut saved: Vec<(String, Option<String>)> = Vec::new();
    for (k, _) in env.iter() {
        if !saved.iter().any(|(sk, _)| sk == k) {
            saved.push((k.clone(), parser::vars_get(k)));
        }
    }
    for (k, v) in env.iter() {
        parser::vars_set(k, v);
    }
    let code = f();
    for (k, old) in saved.iter() {
        match old {
            Some(v) => parser::vars_set(k, v),
            None => parser::vars_unset(k),
        }
    }
    code
}

/// Nomi gestiti da `dispatch_builtin` (builtin prima di PATH/bare-word).
pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "ls" | "cat"
            | "touch"
            | "mkdir"
            | "mount"
            | "umount"
            | "echo"
            | "clear"
            | "wc"
            | "hexdump"
            | "kill"
            | "cd"
            | "pwd"
            | "cp"
            | "mv"
            | "rm"
            | "rmdir"
            | "chmod"
            | "ps"
            | "export"
            | "source"
            | "jobs"
            | "wait"
            | "fg"
            | "bg"
            | "exit"
            | "help"
    )
}

/// Dispatch dei builtin (Fase 42: condiviso tra esecuzione in-processo e
/// stadi builtin delle pipeline, che girano in figli fork con stdio proprio).
/// `run` resta fuori (fork+exec dedicata in `cmd_run`); ignoto = 127.
/// `source` in pipeline gira nel figlio (effetti scoped, `$?` iniziale 0).
pub(crate) fn dispatch_builtin(args: &[&str]) -> i64 {
    match args[0] {
        "ls" => cmd_fs::cmd_ls(args),
        "cat" => cmd_fs::cmd_cat(args),
        "touch" => cmd_fs::cmd_touch(args),
        "mkdir" => cmd_fs::cmd_mkdir(args),
        "mount" => cmd_fs::cmd_mount(args),
        "umount" => cmd_fs::cmd_umount(args),
        "echo" => cmd_info::cmd_echo(args),
        "clear" => cmd_info::cmd_clear(),
        "wc" => cmd_info::cmd_wc(args),
        "hexdump" => cmd_info::cmd_hexdump(args),
        "kill" => cmd_info::cmd_kill(args),
        "cd" => cwd::cmd_cd(args),
        "pwd" => cwd::cmd_pwd(),
        "cp" => cmd_fs::cmd_cp(args),
        "mv" => cmd_fs::cmd_mv(args),
        "rm" => cmd_fs::cmd_rm(args),
        "rmdir" => cmd_fs::cmd_rmdir(args),
        "chmod" => cmd_fs::cmd_chmod(args),
        "ps" => cmd_info::cmd_ps(),
        "export" => cmd_info::cmd_export(args),
        "source" => cmd_source::cmd_source(args, 0),
        "jobs" => cmd_run::cmd_jobs(),
        "wait" => cmd_run::cmd_wait(args),
        "fg" => cmd_run::cmd_fg(args),
        "bg" => cmd_run::cmd_bg(args),
        "exit" => match args.get(1) {
            None => civis::exit(0),
            Some(s) => match cmd_info::parse_i64(s) {
                Some(n) => civis::exit(n),
                None => {
                    term::term_err("exit: bad code\n");
                    1
                }
            },
        },
        "help" => cmd_info::cmd_help(),
        _ => {
            term::term_err("unknown command: ");
            term::term_err(args[0]);
            term::term_err("\n");
            127
        }
    }
}
