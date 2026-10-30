use super::*;

pub(crate) fn cmd_help() -> i64 {
    term::term_print("Commands: ls [-l] [path], cat <file>, touch <file>, mkdir <dir>, mount <src> <tgt>, umount <tgt>, echo [args], clear, wc <file>, hexdump <file>, kill <pid|service>, cd [dir], pwd, cp <src> <dst>, mv <src> <dst>, rm <file>, rmdir <dir>, ps, export [NAME=val], source <file>, run <prog> [args...] [&], jobs, wait [pid], fg [%N|pid], bg [%N|pid], exit [code], help\n");
    term::term_print("Job control (Fase 44a/44b): Ctrl-Z sospende il fg (`run` singolo), `fg`/`bg` riprendono, `jobs` mostra run/stopped/done; Ctrl-C = cancel cooperativo + kill(130) se non cooperante\n");
    term::term_print("Env (Fase 43a): VAR=v (persistente), VAR=v cmd (solo comando, anche run/stadi), tutto l'env ai figli + PWD; PATH (default /usr/bin:/bin), bare word = run implicito, #! script eseguibili\n");
    term::term_print("Redirect (Fase 40.4, bash-like): > >> < 2> 2>> 2>&1 — ultimo vince per slot; cat/wc/hexdump senza file leggono stdin\n");
    term::term_print("Parser (Fase 41): '...' \"...\" \\ # ; && || & $VAR ${VAR} $? $$ ~ glob * ?\n");
    term::term_print("Pipe (Fase 42): a | b | ... (stadi concorrenti, status = ultimo), heredoc <<EOF (corpo letterale); & su pipeline in Fase 44\n");
    0
}

/// `export [NAME=val ...]` (Fase 41): set persistente o lista delle variabili.
pub(crate) fn cmd_export(args: &[&str]) -> i64 {
    if args.len() < 2 {
        for (k, v) in parser::vars_list() {
            let mut s = String::from("export ");
            s.push_str(&k);
            s.push('=');
            s.push_str(&v);
            term::term_print(&s);
            term::term_print("\n");
        }
        return 0;
    }
    for a in &args[1..] {
        match a.split_once('=') {
            Some((name, val)) if parser::valid_name(name) => parser::vars_set(name, val),
            _ => {
                term::term_err("export: bad name=value: ");
                term::term_err(a);
                term::term_err("\n");
                return 1;
            }
        }
    }
    0
}

/// Accoda `s` paddata a `width` con spazi (colonne `ps`, niente format!).
fn push_padded(out: &mut String, s: &str, width: usize) {
    out.push_str(s);
    let mut n = s.len();
    while n < width {
        out.push(' ');
        n += 1;
    }
}

/// `ps` tabellare stile Linux (Fase 19.1): PID NAME PRIO STATE TIME PARENT.
/// STATE = run (se stesso) / ready / recv / reply / blocked / stopped (44a,
/// sospeso via `SYS_SUSPEND`); TIME = tick consumati (10 ms); PARENT = pid
/// del padre ("-" per init/idle).
pub(crate) fn cmd_ps() -> i64 {
    let me = civis::getpid() as u32;
    let mut out = String::from("PID  NAME           PRIO STATE TIME PARENT\n");
    for pid in 0..civis::PS_SCAN_MAX {
        let Some(e) = civis::ps_info(pid) else { continue; };
        let mut cell = String::new();
        push_u64(&mut cell, pid as u64);
        push_padded(&mut out, &cell, 5);
        push_padded(&mut out, e.name_str(), 15);
        cell.clear();
        push_u64(&mut cell, e.prio as u64);
        push_padded(&mut out, &cell, 5);
        let state = if pid == me {
            "run"
        } else if e.stopped() {
            "stopped"
        } else if e.state == 0 {
            "ready"
        } else if e.ipc == 1 {
            "recv"
        } else if e.ipc == 2 {
            "reply"
        } else {
            "blocked"
        };
        push_padded(&mut out, state, 6);
        cell.clear();
        push_u64(&mut cell, e.ticks);
        out.push_str(&cell);
        out.push(' ');
        match e.parent {
            Some(p) => {
                cell.clear();
                push_u64(&mut cell, p as u64);
                out.push_str(&cell);
            }
            None => out.push('-'),
        }
        out.push('\n');
    }
    term::term_print(&out);
    0
}

// ── Utility Fase 18.1 ───────────────────────────────────────────────

pub(crate) fn cmd_echo(args: &[&str]) -> i64 {
    // Una sola write per riga: ogni term_print e' un IPC + una riga di
    // seriale col timestamp — i pezzi non sarebbero mai contigui nel log.
    let mut s = String::new();
    for (i, a) in args.iter().skip(1).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(a);
    }
    term::term_print(&s);
    term::term_print("\n");
    0
}

pub(crate) fn cmd_clear() -> i64 {
    // Form feed: la console pulisce tutto e torna home (Fase 18.1).
    term::term_write_bytes(b"\x0c");
    0
}
/// Accoda un u64 in decimale (niente `format!`: no_std minimale).
pub(crate) fn push_u64(s: &mut String, mut v: u64) {
    if v == 0 {
        s.push('0');
        return;
    }
    let mut digs = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        digs[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for i in (0..n).rev() {
        s.push(digs[i] as char);
    }
}

pub(crate) fn cmd_wc(args: &[&str]) -> i64 {
    // Senza file: stdin redirectato (`<`, come `cat`); conta i byte stdin e
    // stampa con nome `-` (convenzione). Non redirectato = `missing file`.
    if args.len() < 2 {
        if libr::stdin_fd() < 0 {
            term::term_err("wc: missing file\n");
            return 1;
        }
        let data = term::term_read_stdin();
        let (mut lines, mut words, mut bytes) = (0u64, 0u64, 0u64);
        let mut in_word = false;
        for &b in &data {
            bytes += 1;
            if b == b'\n' {
                lines += 1;
            }
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                in_word = false;
            } else if !in_word {
                in_word = true;
                words += 1;
            }
        }
        let mut s = String::new();
        push_u64(&mut s, lines);
        s.push(' ');
        push_u64(&mut s, words);
        s.push(' ');
        push_u64(&mut s, bytes);
        s.push_str(" -");
        term::term_print(&s);
        term::term_print("\n");
        return 0;
    }
    let path = cwd::resolve(args[1]);
    let Ok(fd) = civis::open(&path, 0) else {
        term::term_err("wc: cannot open ");
        term::term_err(args[1]);
        term::term_err("\n");
        return 1;
    };
    let mut buf = vec![0u8; 4096];
    let (mut lines, mut words, mut bytes) = (0u64, 0u64, 0u64);
    let mut in_word = false;
    loop {
        let n = match civis::read_fs(fd, &mut buf, 4096) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            bytes += 1;
            if b == b'\n' {
                lines += 1;
            }
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                in_word = false;
            } else if !in_word {
                in_word = true;
                words += 1;
            }
        }
    }
    let _ = civis::close(fd);
    let mut s = String::new();
    push_u64(&mut s, lines);
    s.push(' ');
    push_u64(&mut s, words);
    s.push(' ');
    push_u64(&mut s, bytes);
    s.push(' ');
    s.push_str(args[1]);
    term::term_print(&s);
    term::term_print("\n");
    0
}

fn push_hex_byte(s: &mut String, b: u8) {
    s.push(civis::hex::encode_nibble(b >> 4) as char);
    s.push(civis::hex::encode_nibble(b & 0xF) as char);
}

pub(crate) fn cmd_hexdump(args: &[&str]) -> i64 {
    // Senza file: stdin redirectato (`<`, come `cat`/`wc`).
    if args.len() < 2 {
        if libr::stdin_fd() < 0 {
            term::term_err("hexdump: missing file\n");
            return 1;
        }
        let data = term::term_read_stdin();
        let mut off = 0usize;
        for chunk in data.chunks(16) {
            let mut s = String::new();
            for shift in (0..8).rev() {
                s.push(civis::hex::encode_nibble(((off >> (shift * 4)) & 0xF) as u8) as char);
            }
            s.push_str(": ");
            for &b in chunk {
                push_hex_byte(&mut s, b);
                s.push(' ');
            }
            term::term_print(&s);
            term::term_print("\n");
            off += chunk.len();
        }
        return 0;
    }
    let path = cwd::resolve(args[1]);
    let Ok(fd) = civis::open(&path, 0) else {
        term::term_err("hexdump: cannot open ");
        term::term_err(args[1]);
        term::term_err("\n");
        return 1;
    };
    let mut buf = vec![0u8; 16];
    let mut off = 0usize;
    loop {
        let n = match civis::read_fs(fd, &mut buf, 16) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        // Una sola write per riga (vedi cmd_echo: timestamp per write).
        let mut s = String::new();
        for shift in (0..8).rev() {
            s.push(civis::hex::encode_nibble(((off >> (shift * 4)) & 0xF) as u8) as char);
        }
        s.push_str(": ");
        for i in 0..n {
            push_hex_byte(&mut s, buf[i]);
            s.push(' ');
        }
        term::term_print(&s);
        term::term_print("\n");
        off += n;
    }
    let _ = civis::close(fd);
    0
}

/// Parsa un intero decimale (usato anche da `wait` in cmd_run).
pub(crate) fn parse_i64(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let (neg, digs) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, &bytes[..]),
    };
    if digs.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    for &b in digs {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some(if neg { -v } else { v })
}
fn service_by_name(name: &str) -> Option<civis::Service> {
    match name {
        "gpu" => Some(civis::Service::Gpu),
        "cardo" => Some(civis::Service::Cardo),
        "vela" => Some(civis::Service::Vela),
        "init" => Some(civis::Service::Init),
        "kbd" => Some(civis::Service::Kbd),
        "porta" => Some(civis::Service::Porta),
        "block" => Some(civis::Service::Block),
        "posix" => Some(civis::Service::Posix),
        "time" => Some(civis::Service::Time),
        "vestigia" => Some(civis::Service::Vestigia),
        _ => None,
    }
}
pub(crate) fn cmd_kill(args: &[&str]) -> i64 {
    if args.len() < 2 {
        term::term_err("kill: usage: kill <pid|service>\n");
        return 1;
    }
    let pid = match parse_i64(args[1]) {
        Some(p) => p,
        // init e' sempre pid 1 (il kernel spawna solo lui) ma non registra
        // il servizio: niente lookup, diretto.
        None if args[1] == "init" => 1,
        None => match service_by_name(args[1]) {
            Some(svc) => match civis::service_pid(svc) {
                Ok(p) => p,
                Err(_) => {
                    term::term_err("kill: service not running\n");
                    return 1;
                }
            },
            None => {
                term::term_err("kill: unknown pid/service\n");
                return 1;
            }
        },
    };
    if civis::kill(pid, 1).is_err() {
        term::term_err("kill: failed (parent/init only, or init/self/unknown?)\n");
        return 1;
    }
    0
}
