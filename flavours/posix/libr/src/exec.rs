//! Convenzioni di lancio POSIX (ADR-0041, Fase 58.3): serializzazione
//! argv/env e `exec` per path.
//!
//! Il meccanismo sta in `civis` (`exec_image`/`exec_image_args` = `SYS_EXEC`,
//! nudo); qui vive la personalita': il blocco argv in stile Linux, la spec
//! redirect contrabbandata come ultimo argv (Fase 40.4c, semantica in
//! `crate::stdio`) e `exec`/`exec_env` che leggono il file dal FS prima di
//! sostituire l'immagine.

use civis::*;

/// Serializza gli argv nel blocco `[argc:8][envc:8][payload NUL-separated]`
/// per `exec_image_args`/`SYS_EXEC` (Fase 37.1/37.2, env in 43a): `None` se
/// troppi (> 1024) o oltre `ARGS_MAX`. Usato da `exec` e da chi carica prima
/// del fork (la shell: il figlio post-fork ha l'FS avvelenato e non puo' piu'
/// allocare comodo — il parent prepara tutto, il figlio solo esegue).
pub fn serialize_argv(argv: &[&str]) -> Option<alloc::vec::Vec<u8>> {
    serialize_argv_redir_env(argv, &[], &[])
}

/// Come `serialize_argv` ma con spec redirect contrabbandata come ultimo argv
/// (Fase 40.4c/d): una voce per slot attivo (max 3). Formato `REDIR_MAGIC +
/// vfd:noncehex` (grant) o `vfd:@slot` (alias `2>&1`, zero grant) separati da
/// `;`, hex minuscolo senza NUL (il kernel rifiuta code extra e spezza al NUL).
/// Lo startup (`stdio_restore` via `libr::entry!`) la nasconde ad
/// `args_from_stack`: il programma non la vede. `None` a spec invalida o oltre
/// `ARGS_MAX`.
pub fn serialize_argv_redir(
    argv: &[&str],
    spec: &[crate::stdio::RedirEntry],
) -> Option<alloc::vec::Vec<u8>> {
    serialize_argv_redir_env(argv, &[], spec)
}

/// Come `serialize_argv_redir` con in piu' l'environment (Fase 43a): `env` =
/// coppie (nome, valore) serializzate `NAME=val` dopo gli argv (e dopo il
/// magic redirect, che resta l'ULTIMO argv: ordine argv/magic/env, letto
/// cosi' dal kernel). La convenzione `NAME=val` vive qui (il kernel vede byte
/// opachi): voci con nome vuoto o con `=`/NUL nel nome o NUL nel valore sono
/// saltate (mai fail: l'env e' best-effort, gli argv no). `None` se argv/env
/// troppi (> 1024 l'uno) o oltre `ARGS_MAX`.
pub fn serialize_argv_redir_env(
    argv: &[&str],
    env: &[(&str, &str)],
    spec: &[crate::stdio::RedirEntry],
) -> Option<alloc::vec::Vec<u8>> {
    use crate::stdio::RedirEntry;
    if spec.len() > crate::stdio::REDIR_MAX_ENTRIES
        || argv.len() + if spec.is_empty() { 0 } else { 1 } > 1024
        || env.len() > 1024
    {
        return None;
    }
    for e in spec {
        let (v, extra_ok) = match *e {
            RedirEntry::Grant { vfd, .. } => (vfd, true),
            RedirEntry::Alias { vfd, target } => (vfd, target <= 2),
        };
        if v > 2 || !extra_ok {
            return None;
        }
    }
    let mut buf = alloc::vec::Vec::new();
    let argc = argv.len() + if spec.is_empty() { 0 } else { 1 };
    // Conta solo le voci env valide (stessa regola della scrittura sotto).
    let mut envc = 0usize;
    for (k, v) in env {
        if !k.is_empty()
            && !k.as_bytes().iter().any(|&b| b == b'=' || b == 0)
            && !v.as_bytes().iter().any(|&b| b == 0)
        {
            envc += 1;
        }
    }
    buf.extend_from_slice(&(argc as u64).to_le_bytes());
    buf.extend_from_slice(&(envc as u64).to_le_bytes());
    for a in argv {
        buf.extend_from_slice(a.as_bytes());
        buf.push(0);
    }
    // La spec redirect e' l'ULTIMO argv (magic): va subito dopo gli argv,
    // prima degli env (il kernel legge argc stringhe poi envc — l'ordine
    // argv/magic/env e' il contratto).
    if !spec.is_empty() {
        buf.extend_from_slice(civis::args::REDIR_MAGIC);
        for (i, e) in spec.iter().enumerate() {
            if i > 0 {
                buf.push(b';');
            }
            match *e {
                RedirEntry::Grant { vfd, nonce } => {
                    buf.push(b'0' + vfd);
                    buf.push(b':');
                    for shift in (0..16).rev() {
                        let nib = ((nonce >> (shift * 4)) & 0xf) as u8;
                        buf.push(if nib < 10 { b'0' + nib } else { b'a' + nib - 10 });
                    }
                }
                RedirEntry::Alias { vfd, target } => {
                    buf.push(b'0' + vfd);
                    buf.push(b':');
                    buf.push(b'@');
                    buf.push(b'0' + target);
                }
            }
        }
        buf.push(0);
    }
    for (k, v) in env {
        if !k.is_empty()
            && !k.as_bytes().iter().any(|&b| b == b'=' || b == 0)
            && !v.as_bytes().iter().any(|&b| b == 0)
        {
            buf.extend_from_slice(k.as_bytes());
            buf.push(b'=');
            buf.extend_from_slice(v.as_bytes());
            buf.push(0);
        }
    }
    if buf.len() as u64 > ARGS_MAX + 16 {
        return None;
    }
    Some(buf)
}

/// `exec(path, argv)`: lancia il programma `path` nell'immagine corrente
/// (Fase 37.1): legge il file via FS, serializza gli argv e chiama
/// `exec_image_args`. Il kernel non tocca mai il FS (ADR-0005). NON ritorna
/// mai in caso di successo; errori nativi (Fase 39: `NotFound` se il file non
/// si carica — dettaglio in Fase 40 —, `TooBig` se gli argv eccedono il bound,
/// `Invalid` a rifiuto del kernel; processo intatto).
#[inline]
pub fn exec(path: &str, argv: &[&str]) -> Result<(), Error> {
    exec_env(path, argv, &[])
}

/// Verifica di firma per un `exec` per path (ADR-0079 fase 3): chiede a cardo
/// il verdetto su `path` (namespace `ns`: il VFS detiene la verita' sui path)
/// e registra l'identita' nella trust table del kernel. `Ok(())` = eseguibile
/// (la classe la decide il kernel al load, ricalcolando l'id); `Denied` =
/// firma assente in un path di sistema o sidecar invalido.
///
/// Da chiamare nel processo che **legge** i byte e prima dell'`exec`
/// (la shell fa fork+exec: il figlio ha l'FS avvelenato, verifica il parent).
pub fn verify_exec(path: &str) -> Result<(), Error> {
    let ns_key = path.trim_start_matches('/');
    match civis::fs::rights::rights_execute_verdict(b"ns", ns_key.as_bytes()) {
        Ok((true, _class)) => Ok(()),
        Ok((false, _)) => Err(Error::Denied),
        // Non e' nel namespace (es. file su FAT): prosegui, sara' Untrusted.
        Err(Error::NotFound) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `exec_env(path, argv, env)`: come `exec` con in piu' l'environment (43a).
/// `env` = coppie (nome, valore); stesse regole di `serialize_argv_redir_env`.
///
/// ADR-0079 fase 3: prima di sostituire l'immagine chiede a cardo il verdetto
/// di firma per il path (`verify_exec`). Il kernel, al `SYS_EXEC`, ricalcola
/// l'`ImageId` sui byte caricati e fa lookup: se i byte sono cambiati tra la
/// verifica e l'exec la classe cade su `Untrusted` (TOCTOU chiuso).
#[inline]
pub fn exec_env(path: &str, argv: &[&str], env: &[(&str, &str)]) -> Result<(), Error> {
    let img = match load_file(path) {
        Some(b) if !b.is_empty() => b,
        _ => return Err(Error::NotFound),
    };
    let buf = match serialize_argv_redir_env(argv, env, &[]) {
        Some(b) => b,
        None => return Err(Error::TooBig),
    };
    verify_exec(path)?;
    exec_image_args(&img, &buf)
}
