use core::sync::atomic::{AtomicI64, Ordering};
use civis::*;
use civis::args::{redir_spec_arg, REDIR_MAGIC};

// ── Tabelle stdio vfd (Fase 40.2, P1) ─────────────────────────────────
// Redirect `>`/`>>`/`<`/`2>`/`2>&1`: quando attive, lo stdout dei programmi
// (`println!`/`print_str!`, via `print::flush`) va sul file invece che sul
// seriale, e stdin si legge dal file invece che dalla tastiera.
//
// Separazione dei sink (decisione 40.2): `print_string`/`write_stdout`/
// `sys::write(1,…)` restano SEMPRE seriale (sink debug: mirror shell, log
// init/server). Solo il percorso line-buffer e' instradabile — e' quello che
// usano i programmi reali. Con tabelle inattive (default) lo stdout va sul
// **terminale**: la sessione del processo (`civis::sess::out`, D8c2 — il
// mirror seriale lo fa il server); i server nativi (senza hook `persona`)
// restano invariati sul seriale.
//
// Slot 2 (`2>`/`2>&1`) e' memorizzato ma non hookato in 40.2: niente in
// userland scrive fd 2 oggi; l'aggancio dei builtin viene con la shell 40.4
// (i builtin usano `term_print`, non `println!`).

/// fd reali di stdin/stdout/stderr quando il redirect e' attivo (-1 = seriale).
static STDIO: [AtomicI64; 3] =
    [AtomicI64::new(-1), AtomicI64::new(-1), AtomicI64::new(-1)];

/// Attiva il redirect: `fds` = fd reali di [stdin, stdout, stderr] (-1 per
/// slot = quello slot resta seriale). Chiamata dalla shell (builtin: temporaneo
/// con restore; figli `run`: prima dell'exec, ereditata via fork-COW).
pub fn set_stdio(fds: [i64; 3]) {
    STDIO[0].store(fds[0], Ordering::Relaxed);
    STDIO[1].store(fds[1], Ordering::Relaxed);
    STDIO[2].store(fds[2], Ordering::Relaxed);
}

/// Disattiva il redirect: tutto torna seriale (cleanup builtin/figli).
pub fn clear_stdio() {
    set_stdio([-1, -1, -1]);
}

/// fd reale dello stdout redirectato (-1 = seriale/terminale).
/// Usato dalla shell (hook B1 in `term_write_bytes`, Fase 40.4b): i builtin
/// scrivono su terminale via `TERM_FD`, non via `println!`, quindi il routing
/// di `route_out` non li copre — la shell instrada esplicitamente qui.
pub fn stdout_fd() -> i64 {
    STDIO[1].load(Ordering::Relaxed)
}

/// fd reale dello stdin redirectato (-1 = tastiera). I builtin che leggono
/// stdin (`cat` senza file, Fase 40.4d) distinguono "non redirectato" (errore
/// `missing file`) da "file vuoto" (EOF subito) senza tentare letture.
pub fn stdin_fd() -> i64 {
    STDIO[0].load(Ordering::Relaxed)
}

/// fd reale dello stderr redirectato (-1 = terminale). Hook dei messaggi
/// d'errore dei builtin (`term_err`, Fase 40.4d): mai fallback sullo stdout
/// (gli errori non devono inquinare il file redirectato).
pub fn stderr_fd() -> i64 {
    STDIO[2].load(Ordering::Relaxed)
}

/// Max voci nella spec (un vfd per slot: stdin/stdout/stderr).
pub(crate) const REDIR_MAX_ENTRIES: usize = 3;

/// Voce di spec redirect: grant da riscuotere o alias di un altro slot
/// (stesso fd, zero claim). Il parent emette in ordine di slot (0,1,2) con
/// gli alias per ultimi: l'alias richiede lo slot target gia' assegnato.
#[derive(Clone, Copy)]
pub enum RedirEntry {
    /// Riscuoti `nonce` come fd dello slot `vfd`.
    Grant {
        vfd: u8,
        nonce: u64,
    },
    /// Lo slot `vfd` condivide l'fd dello slot `target` (gia' assegnato).
    Alias {
        vfd: u8,
        target: u8,
    },
}

/// Parsa le voci dopo `REDIR_MAGIC` in `out`. `None` a qualunque deviazione
/// (vfd/target fuori 0..2, hex malformato, troppe voci, coda). Strict: una
/// spec malformata si ignora intera (fail-open loud), mai a meta'.
pub(crate) fn parse_redir_entries(arg: &[u8], out: &mut [RedirEntry; 3]) -> Option<usize> {
    let body = arg.strip_prefix(REDIR_MAGIC)?;
    if body.is_empty() {
        return None;
    }
    let mut n = 0usize;
    let mut i = 0usize;
    while i < body.len() {
        if n >= REDIR_MAX_ENTRIES {
            return None;
        }
        // vfd: un char '0'..'2', poi ':'.
        if i + 1 >= body.len() {
            return None;
        }
        let vfd = body[i];
        if !(b'0'..=b'2').contains(&vfd) || body[i + 1] != b':' {
            return None;
        }
        i += 2;
        // Alias `2:@1` oppure nonce hex (1..16 nibble minuscoli).
        if i < body.len() && body[i] == b'@' {
            i += 1;
            if i >= body.len() || !(b'0'..=b'2').contains(&body[i]) {
                return None;
            }
            out[n] = RedirEntry::Alias { vfd: vfd - b'0', target: body[i] - b'0' };
            n += 1;
            i += 1;
        } else {
            let start = i;
            while i < body.len() && body[i] != b';' {
                if !body[i].is_ascii_hexdigit() || body[i].is_ascii_uppercase() {
                    return None;
                }
                i += 1;
            }
            let digits = i - start;
            if digits == 0 || digits > 16 {
                return None;
            }
            let mut nonce: u64 = 0;
            for &d in &body[start..i] {
                let v = match d {
                    b'0'..=b'9' => (d - b'0') as u64,
                    b'a'..=b'f' => (d - b'a' + 10) as u64,
                    _ => return None,
                };
                nonce = (nonce << 4) | v;
            }
            out[n] = RedirEntry::Grant { vfd: vfd - b'0', nonce };
            n += 1;
        }
        // Separatore `;` obbligatorio tra voci, vietato in coda.
        if i < body.len() {
            if body[i] != b';' {
                return None;
            }
            i += 1;
            if i >= body.len() {
                return None;
            }
        }
    }
    if n == 0 { None } else { Some(n) }
}

/// Ripristina i redirect all'avvio da una spec contrabbandata nell'ultimo argv
/// (Fase 40.4c). Chiamata dal wrapper `entry!` prima di `real_main`: senza
/// magic nessuno effetto (costo = una scansione dello stack); con magic fa
/// claim dei grant + `set_stdio`. A claim fallita chiude i gia' riscossi e
/// prosegue SENZA redirect (fail-open loud su seriale: l'output resta visibile,
/// mai perso in silenzio). Mai fatale: lo startup non deve morire per un
/// redirect (il programma gira, solo non redirectato).
/// CRT-internal (pub perche' l'espansione `entry!` vive nei crate utenti).
pub fn stdio_restore(sp: u64) {
    let Some(arg) = redir_spec_arg(sp) else {
        return;
    };
    let mut entries = [RedirEntry::Grant { vfd: 0, nonce: 0 }; 3];
    let Some(n) = parse_redir_entries(arg, &mut entries) else {
        let _ = civis::print_string(b"[libr] stdio_restore: spec malformata, ignoro\n");
        return;
    };
    let mut fds = [-1i64; 3];
    let abort = |fds: &[i64; 3], msg: &[u8]| {
        for &f in fds {
            if f >= 0 {
                let _ = civis::close(f);
            }
        }
        let _ = civis::print_string(msg);
    };
    for e in &entries[..n] {
        match *e {
            RedirEntry::Grant { vfd, nonce } => match civis::dup_claim(nonce) {
                Ok(fd) => fds[vfd as usize] = fd,
                Err(_) => {
                    abort(&fds, b"[libr] stdio_restore: claim fallita, senza redirect\n");
                    return;
                }
            },
            RedirEntry::Alias { vfd, target } => {
                let t = fds[target as usize];
                if t < 0 {
                    abort(&fds, b"[libr] stdio_restore: alias su slot vuoto, senza redirect\n");
                    return;
                }
                fds[vfd as usize] = t;
            }
        }
    }
    set_stdio(fds);
}

/// True se almeno uno slot e' redirectato.
pub fn stdio_active() -> bool {
    STDIO[0].load(Ordering::Relaxed) >= 0
        || STDIO[1].load(Ordering::Relaxed) >= 0
        || STDIO[2].load(Ordering::Relaxed) >= 0
}

/// Instrada `bytes` (stdout). Due casi:
/// - **redirect attivo** (`STDIO[1] >= 0`): tutto sul file, o prefisso sul file
///   + coda sul seriale a write parziale (mai duplicazione);
/// - **default** (`STDIO[1] < 0`): la **sessione di terminale** del processo
///   (`civis::sess::out`, D8c2): l'output appare sulla console *e* resta nel
///   log seriale — ma il mirror lo fa il **server**, non questo client.
/// Ritorna true se "gestito" (mirror seriale incluso: il chiamante
/// `print::flush` non riscrive). False = nessuna sessione e nessun redirect: il
/// chiamante riversa tutto sul seriale (l'output non si perde mai).
pub(crate) fn route_out(bytes: &[u8]) -> bool {
    let fd = STDIO[1].load(Ordering::Relaxed);
    if fd >= 0 {
        return match write_fs(fd, bytes, bytes.len()) {
            Ok(n) if n == bytes.len() => true,
            Ok(n) => {
                // Parziale (errore dopo progressi): la coda va sul seriale.
                let _ = sys::write(1, bytes.as_ptr().wrapping_add(n), bytes.len() - n);
                true
            }
            Err(_) => false,
        };
    }
    civis::sess::out(bytes)
}

/// Legge un byte da stdin redirectato. None = non attivo, vuoto o errore.
/// NESSUNA semantica retry/EOF qui: a 0 byte il chiamante decide — la shell
/// sa se stdin e' un device (retry con spin, come la tastiera) o un file
/// (None = EOF). Condivide il guard 1-in-volo degli altri wrapper (via
/// `read_fs`: a op in volo → `Pending` → None, mai disallineamento ring).
pub fn stdin_byte() -> Option<u8> {
    let fd = STDIO[0].load(Ordering::Relaxed);
    if fd < 0 {
        return None;
    }
    let mut b = [0u8; 1];
    match read_fs(fd, &mut b, 1) {
        Ok(1) => Some(b[0]),
        _ => None,
    }
}
