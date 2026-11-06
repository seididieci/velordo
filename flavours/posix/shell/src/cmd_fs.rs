use super::*;

// ── Commands ────────────────────────────────────────────────────────

pub(crate) fn cmd_ls(args: &[&str]) -> i64 {
    // `ls [-l] [path]`: senza flag elenca i nomi; con -l una riga per entry
    // "tipo size nome[ (ro)]" (stretch Fase 19.2: metadati via civis::stat,
    // 1 round trip per entry — ok per directory piccole; niente owner/mtime,
    // `Stat` non li ha). Una write per riga (convenzione shell: i pezzi
    // restano contigui nel log seriale).
    let (long, raw) = match args.get(1) {
        Some(&"-l") => (true, args.get(2).copied().unwrap_or(".")),
        _ => (false, args.get(1).copied().unwrap_or(".")),
    };
    let path = cwd::resolve(raw);
    let mut buf = vec![0u8; 4096];
    if civis::readdir(&path, &mut buf, 4096).is_err() {
        term::term_err("ls: error\n");
        return 1;
    }
    // Formato "name\0name\0...\0\0"
    let mut i = 0;
    let mut wrote = false;
    while i < buf.len() {
        if buf[i] == 0 { break; }
        let start = i;
        while i < buf.len() && buf[i] != 0 { i += 1; }
        if let Ok(name) = core::str::from_utf8(&buf[start..i]) {
            if long {
                // Path assoluto dell'entry per stat (attento a "/" root).
                let mut full = path.clone();
                if !full.ends_with('/') {
                    full.push('/');
                }
                full.push_str(name);
                let mut line = String::new();
                let mut st = civis::Stat { size: 0, kind: 0, readonly: false, mtime: 0 };
                if civis::stat(&full, &mut st).is_ok() {
                    line.push(if st.is_dir() {
                        'd'
                    } else if st.is_device() {
                        'v'
                    } else {
                        '-'
                    });
                    line.push(' ');
                    cmd_info::push_u64(&mut line, st.size);
                    line.push(' ');
                    line.push_str(name);
                    if st.readonly {
                        line.push_str(" (ro)");
                    }
                } else {
                    // Race (entry rimossa tra readdir e stat): mai abortire.
                    line.push_str("? ");
                    line.push_str(name);
                }
                term::term_print(&line);
                term::term_print("\n");
            } else {
                term::term_print(name);
                term::term_print("  ");
            }
            wrote = true;
        }
        i += 1; // skip null
    }
    if wrote && !long {
        term::term_print("\n");
    }
    0
}

pub(crate) fn cmd_cat(args: &[&str]) -> i64 {
    if args.len() < 2 {
        // Senza file: stdin redirectato (`<`, Fase 40.4d). Non redirectato =
        // `missing file` (mai tastiera: il REPL la possiede).
        if libr::stdin_fd() < 0 {
            term::term_err("cat: missing file\n");
            return 1;
        }
        let data = term::term_read_stdin();
        if let Ok(s) = core::str::from_utf8(&data) {
            term::term_print(s);
        }
        term::term_print("\n");
        return 0;
    }
    let path = cwd::resolve(args[1]);
    let Ok(fd) = civis::open(&path, 0) else {
        term::term_err("cat: cannot open ");
        term::term_err(args[1]);
        term::term_err("\n");
        return 1;
    };
    let mut buf = vec![0u8; 4000];
    loop {
        match civis::read_fs(fd, &mut buf, 4000) {
            Ok(0) => break, // EOF
            Ok(n) => {
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    term::term_print(s);
                }
            }
            Err(_) => break,
        }
    }
    let _ = civis::close(fd);
    term::term_print("\n");
    0
}

pub(crate) fn cmd_touch(args: &[&str]) -> i64 {
    if args.len() < 2 {
        term::term_err("touch: missing file\n");
        return 1;
    }
    let path = cwd::resolve(args[1]);
    let Ok(fd) = civis::open(&path, civis::OPEN_CREATE) else {
        term::term_err("touch: failed\n");
        return 1;
    };
    let _ = civis::close(fd);
    0
}

pub(crate) fn cmd_mkdir(args: &[&str]) -> i64 {
    if args.len() < 2 {
        term::term_err("mkdir: missing directory\n");
        return 1;
    }
    let path = cwd::resolve(args[1]);
    if civis::mkdir(&path).is_err() {
        term::term_err("mkdir: failed\n");
        return 1;
    }
    0
}

pub(crate) fn cmd_mount(args: &[&str]) -> i64 {
    if args.len() < 3 {
        term::term_err("mount: usage: mount <source> <target>\n");
        return 1;
    }
    // La sorgente NON si risolve: puo' essere `UUID=`/`LABEL=` o un device.
    let target = cwd::resolve(args[2]);
    if civis::mount(args[1], &target).is_err() {
        term::term_err("mount: failed\n");
        return 1;
    }
    0
}

pub(crate) fn cmd_umount(args: &[&str]) -> i64 {
    if args.len() < 2 {
        term::term_err("umount: usage: umount <target>\n");
        return 1;
    }
    let target = cwd::resolve(args[1]);
    if civis::umount(&target).is_err() {
        term::term_err("umount: failed (busy or not mounted?)\n");
        return 1;
    }
    0
}

/// Copia file client-side (Fase 18.2): read a chunk + write. Usata da `cp`
/// e `mv`. Niente nuove op FS: mkdir/rmdir/rm su /fat restano rifiutati
/// (niente unlink, fuori scope), la write funziona (Fase 20) e la copia
/// fallisce pulita senza toccare la sorgente.
fn copy_file(src: &str, dst: &str) -> bool {
    let from = cwd::resolve(src);
    let to = cwd::resolve(dst);
    let Ok(fd_in) = civis::open(&from, 0) else {
        term::term_err("cp: cannot open ");
        term::term_err(src);
        term::term_err("\n");
        return false;
    };
    let Ok(fd_out) = civis::open(&to, 0x200 /* OPEN_CREATE */) else {
        term::term_err("cp: cannot create ");
        term::term_err(dst);
        term::term_err("\n");
        let _ = civis::close(fd_in);
        return false;
    };
    let mut buf = vec![0u8; 4000];
    let mut ok = true;
    loop {
        // Come `cat`: EOF (Ok(0)) o errore chiudono il loop.
        let n = match civis::read_fs(fd_in, &mut buf, 4000) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        if civis::write_fs(fd_out, &buf[..n], n) != Ok(n) {
            ok = false;
            break;
        }
    }
    let _ = civis::close(fd_in);
    let _ = civis::close(fd_out);
    if !ok {
        term::term_err("cp: I/O error\n");
    }
    ok
}
pub(crate) fn cmd_cp(args: &[&str]) -> i64 {
    if args.len() < 3 {
        term::term_err("cp: usage: cp <src> <dst>\n");
        return 1;
    }
    if copy_file(args[1], args[2]) { 0 } else { 1 }
}

pub(crate) fn cmd_mv(args: &[&str]) -> i64 {
    if args.len() < 3 {
        term::term_err("mv: usage: mv <src> <dst>\n");
        return 1;
    }
    // mv = cp + rm client-side, zero nuove op (Fase 18.2): la sorgente si
    // rimuove SOLO a copia riuscita.
    if !copy_file(args[1], args[2]) {
        return 1;
    }
    let src = cwd::resolve(args[1]);
    if civis::remove(&src).is_err() {
        term::term_err("mv: copied but cannot remove source\n");
        return 1;
    }
    0
}

pub(crate) fn cmd_rm(args: &[&str]) -> i64 {
    if args.len() < 2 {
        term::term_err("rm: missing file\n");
        return 1;
    }
    let path = cwd::resolve(args[1]);
    if civis::remove(&path).is_err() {
        term::term_err("rm: cannot remove ");
        term::term_err(args[1]);
        term::term_err("\n");
        return 1;
    }
    0
}

pub(crate) fn cmd_rmdir(args: &[&str]) -> i64 {
    if args.len() < 2 {
        term::term_err("rmdir: missing directory\n");
        return 1;
    }
    // Stessa op del server (dir vuote): il server rifiuta le non vuote.
    let path = cwd::resolve(args[1]);
    if civis::remove(&path).is_err() {
        term::term_err("rmdir: failed (not empty or missing?)\n");
        return 1;
    }
    0
}

/// chmod MODE PATH (S1.1): projection server-side, mai enforcement fino ad
/// A4. MODE ottale (es. 755); accetta su esistente, errore su mancante.
pub(crate) fn cmd_chmod(args: &[&str]) -> i64 {
    if args.len() < 3 {
        term::term_err("chmod: usage: chmod <mode-octal> <path>\n");
        return 1;
    }
    let mut mode: u32 = 0;
    for b in args[1].as_bytes() {
        if !b.is_ascii_digit() || *b > b'7' {
            term::term_err("chmod: mode non ottale\n");
            return 1;
        }
        mode = mode * 8 + (*b - b'0') as u32;
        if mode > 0o7777 {
            term::term_err("chmod: mode oltre 12 bit\n");
            return 1;
        }
    }
    let path = cwd::resolve(args[2]);
    if civis::chmod(&path, mode).is_err() {
        term::term_err("chmod: failed: ");
        term::term_err(args[2]);
        term::term_err("\n");
        return 1;
    }
    0
}
