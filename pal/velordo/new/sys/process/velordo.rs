//! Processi Velordo (S2.4c2): fork (`SYS_SPAWN_COPY`) + exec con argv/env,
//! attesa via `EXIT_NOTIFY` (mini-client IPC qui dentro: `recv`/demux).
//! Forma di `motor.rs` (spawn reale non-unix); storage `OsString` come
//! `unsupported.rs` (i path sono byte, non UTF-8).
//!
//! Onesta' (gap documentati, mai bugie):
//! - niente ricerca PATH: `program` deve essere un file leggibile (la shell
//!   risolve per conto suo); altrimenti `NotFound`.
//! - `Null` su stdout/stderr perde su console (manca /dev/null: il routing
//!   fd copre solo fd reali); `Null` su stdin e' EOF onesto.
//! - concorrenza FS: i wrapper pipe serializzano (drain `output()` sicuri);
//!   le op File restano single-thread-assumed (come da S2.2).
//! - `getpid` = panic (il kernel non espone il proprio pid: come unsupported).
//! - `Drop` di `Process` = detach (il figlio gira; notify stale filtrate
//!   per canale, evict-oldest a tabella piena).
//! - grant non reclamati a exec fallita restano server-side (bounded, come
//!   la shell che cancella solo a morte osservata).
//! - `wait`/`try_wait` SOLO dal leader (S2.4c3b): le code IPC sono per-TID e
//!   la notify atterra sul leader; da un thread spawnato appende per sempre.
//!   `output()` drena in thread ma aspetta sul main per questo.

use crate::ffi::{OsStr, OsString};
use crate::num::NonZeroI32;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::{fmt, io};

pub use crate::ffi::OsString as EnvKey;
use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
use crate::sys::fs::velordo as vfs;
use crate::sys::pal::velordo::numbers::{
    ARGS_MAX, EXIT_NOTIFY, SYS_CHDIR, SYS_EXEC, SYS_EXIT, SYS_KILL, SYS_RECV, SYS_RECV_NONBLOCK,
    SYS_REPLY, SYS_SPAWN_COPY,
};
use crate::sys::pal::{syscall4, syscall4_out};
use crate::sys::pal::write_console;

/// Messaggio IPC (stesso layout di `civis`, Fase 13/ADR-0008).
struct IpcMsg {
    channel: u64,
    req_id: i64,
    tag: u64,
    w0: u64,
    w1: u64,
}

fn recv_msg(blocking: bool) -> Option<IpcMsg> {
    let num = if blocking { SYS_RECV } else { SYS_RECV_NONBLOCK };
    let (rax, rdi, rsi, rdx, r10) = unsafe { syscall4_out(num, 0, 0, 0, 0) };
    if rax < 0 {
        return None;
    }
    let signed = rdi as i64;
    if signed < 0 {
        Some(IpcMsg { channel: 0, req_id: -signed, tag: rsi, w0: rdx, w1: r10 })
    } else {
        Some(IpcMsg { channel: rdi, req_id: 0, tag: rsi, w0: rdx, w1: r10 })
    }
}

fn reply_current() {
    // Reply difensiva a richieste estranee (come la shell): solo richieste
    // normali (mai notify/async, mai senza contesto).
    unsafe {
        syscall4(SYS_REPLY, 0, 0, 0, 0);
    }
}

/// Tabella exit-code per notify arrivate "per altri" (demux come `note_exit`
/// della shell): 4 slot, evict-oldest. Chiave = canale (u64::MAX = libero).
struct ExitSlot {
    chan: AtomicU64,
    code: AtomicU64,
}
impl ExitSlot {
    const fn empty() -> ExitSlot {
        ExitSlot { chan: AtomicU64::new(u64::MAX), code: AtomicU64::new(0) }
    }
}
static EXIT_TABLE: [ExitSlot; 4] =
    [ExitSlot::empty(), ExitSlot::empty(), ExitSlot::empty(), ExitSlot::empty()];
static EXIT_NEXT: AtomicU64 = AtomicU64::new(0);

fn note_exit(chan: u64, code: i64) {
    for s in EXIT_TABLE.iter() {
        if s.chan.load(Ordering::Relaxed) == u64::MAX {
            s.code.store(code as u64, Ordering::Relaxed);
            s.chan.store(chan, Ordering::Relaxed);
            return;
        }
    }
    let i = (EXIT_NEXT.fetch_add(1, Ordering::Relaxed) % 4) as usize;
    EXIT_TABLE[i].code.store(code as u64, Ordering::Relaxed);
    EXIT_TABLE[i].chan.store(chan, Ordering::Relaxed);
}

fn take_exit(chan: u64) -> Option<i64> {
    for s in EXIT_TABLE.iter() {
        if s.chan.load(Ordering::Relaxed) == chan {
            s.chan.store(u64::MAX, Ordering::Relaxed);
            return Some(s.code.load(Ordering::Relaxed) as i64);
        }
    }
    None
}

/// Attende la morte sul canale `chan` (bloccante): demux delle notify altrui
/// in tabella (per `try_wait` e figli concorrenti).
fn wait_exit(chan: u64) -> io::Result<i64> {
    if let Some(c) = take_exit(chan) {
        return Ok(c);
    }
    loop {
        match recv_msg(true) {
            Some(m) if m.req_id == 0 && m.tag == EXIT_NOTIFY => {
                if m.channel == chan {
                    return Ok(m.w0 as i64);
                }
                note_exit(m.channel, m.w0 as i64);
            }
            Some(m) if m.req_id == 0 && m.tag != EXIT_NOTIFY => reply_current(),
            Some(_) => {}
            None => {
                return Err(io::Error::new(io::ErrorKind::Other, "recv fallita in wait"));
            }
        }
    }
}

/// Drena le notify pendenti senza bloccare (per `try_wait`).
fn poll_exits() {
    let mut n = 0;
    while n < 32 {
        match recv_msg(false) {
            Some(m) if m.req_id == 0 && m.tag == EXIT_NOTIFY => {
                note_exit(m.channel, m.w0 as i64);
            }
            Some(m) if m.req_id == 0 && m.tag != EXIT_NOTIFY => reply_current(),
            _ => break,
        }
        n += 1;
    }
}

////////////////////////////////////////////////////////////////////////////////
// Command
////////////////////////////////////////////////////////////////////////////////

#[derive(Debug)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    /// fd cardo gia' aperto (c3: grant ai figli; c2: solo riconversione
    /// `ChildPipe`→`Stdio`, mai creato qui).
    Fd(i64),
}

impl From<ChildPipe> for Stdio {
    fn from(pipe: ChildPipe) -> Stdio {
        Stdio::Fd(pipe.fd())
    }
}

impl From<crate::sys::fs::velordo::File> for Stdio {
    fn from(file: crate::sys::fs::velordo::File) -> Stdio {
        Stdio::Fd(file.into_inner())
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_inherit: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_inherit: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,
    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_owned())
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &OsStr) {
        self.cwd = Some(dir.to_owned());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        CommandArgs { iter: self.args.iter() }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }

    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(|p| Path::new(p))
    }

    pub fn spawn(&mut self, default: Stdio, needs_stdin: bool) -> io::Result<(Process, StdioPipes)> {
        // Risoluzione stdio (S2.4c3): MakePipe crea pipe (estremita' parent
        // qui, grant al figlio); Fd passa l'fd aperto; il resto non tocca fd.
        // `default` copre i None (come Unix); stdin non richiesto = Null.
        // stdin non richiesto = Null (default Unix); il resto come chiesto.
        // stdin: parent scrive, figlio legge (fd0). stdout/stderr: parent
        // legge, figlio scrive (fd1/fd2). Fd: grant diretto.
        let cfg_stdin = if needs_stdin { self.stdin.as_ref().unwrap_or(&default) } else { self.stdin.as_ref().unwrap_or(&Stdio::Null) };
        // (nonce, slot) per il figlio + fd figli da chiudere nel padre.
        let mut grants: Vec<(u64, usize)> = Vec::new();
        let mut child_fds: Vec<i64> = Vec::new();
        let mut tmp_in: Option<ChildPipe> = None;
        let mut tmp_out: Option<ChildPipe> = None;
        let mut tmp_err: Option<ChildPipe> = None;
        match cfg_stdin {
            Stdio::MakePipe => {
                let (a, b) = crate::sys::pipe::pipe()?;
                let nonce = vfs::pipe_grant(a.fd())?;
                child_fds.push(a.fd());
                grants.push((nonce, 0));
                tmp_in = Some(b);
            }
            Stdio::Fd(fd) => {
                let nonce = vfs::pipe_grant(*fd)?;
                grants.push((nonce, 0));
            }
            _ => {}
        }
        let cfg_out = self.stdout.as_ref().unwrap_or(&default);
        match cfg_out {
            Stdio::MakePipe => {
                let (a, b) = crate::sys::pipe::pipe()?;
                let nonce = vfs::pipe_grant(b.fd())?;
                child_fds.push(b.fd());
                grants.push((nonce, 1));
                tmp_out = Some(a);
            }
            Stdio::Fd(fd) => {
                let nonce = vfs::pipe_grant(*fd)?;
                grants.push((nonce, 1));
            }
            _ => {}
        }
        let cfg_err = self.stderr.as_ref().unwrap_or(&default);
        match cfg_err {
            Stdio::MakePipe => {
                let (a, b) = crate::sys::pipe::pipe()?;
                let nonce = vfs::pipe_grant(b.fd())?;
                child_fds.push(b.fd());
                grants.push((nonce, 2));
                tmp_err = Some(a);
            }
            Stdio::Fd(fd) => {
                let nonce = vfs::pipe_grant(*fd)?;
                grants.push((nonce, 2));
            }
            _ => {}
        }
        // Immagine dal path (niente PATH search: la shell risolve).
        // NOTA (S2.4c3b): l'argv+env NON si serializza qui — il figlio
        // aggiunge `VELORDO_FD*` all'env DOPO il fork, quindi il blocco va
        // costruito NEL figlio (l'env serializzato pre-fork le perderebbe).
        let img = crate::fs::read(Path::new(&self.program))?;
        let progn = self.args.clone();
        let envsrc = self.env.clone();
        let cwd = self.cwd.clone();
        // Fork: padre riceve (pid, chan), figlio 0 (come `fork()` POSIX).
        // Niente lock tenuto qui (i wrapper rilasciano subito; il figlio
        // non blocca mai pre-exec): niente deadlock fork+lock.
        let (rax, rdi, _, _, _) = unsafe { syscall4_out(SYS_SPAWN_COPY, 0, 0, 0, 0) };
        if rax < 0 {
            // Cleanup: chiudi fd figli + cancella i grant (best-effort).
            for fd in child_fds {
                vfs::pipe_close(fd);
            }
            for (nonce, _) in grants.iter() {
                vfs::pipe_cancel(*nonce);
            }
            return Err(io::Error::new(io::ErrorKind::Other, "spawn_copy fallita"));
        }
        if rax == 0 {
            // Figlio: FS avvelenato poi re-inizializzato (serve per i claim:
            // come i figli builtin della shell, Fase 42). Solo syscall raw +
            // FS reinit fino all'exec (che riparte da BSS pristine).
            vfs::poison_child_fstate();
            if !vfs::child_reinit_fstate() {
                child_fail(b"fs reinit nel figlio fallita\n");
            }
            if let Some(dir) = cwd.as_ref() {
                let b = dir.as_encoded_bytes();
                let r = unsafe { syscall4(SYS_CHDIR, b.as_ptr().addr() as u64, b.len() as u64, 0, 0) };
                if r < 0 {
                    child_fail(b"chdir nel figlio fallita\n");
                }
            }
            // Claim grant → fd. NIENTE setenv/HashMap qui (la growth su
            // mappa COW-ereditata si pianta): gli fd viaggiano accodati al
            // blocco argv+env (l'exec azzera la BSS ma l'env sopravvive e
            // `_start` rilegge i VELORDO_FD* e arma gli slot).
            let mut claimed: Vec<(usize, i64)> = Vec::new();
            for (nonce, slot) in grants.iter() {
                match vfs::claim_grant(*nonce) {
                    Ok(fd) => claimed.push((*slot, fd)),
                    Err(_) => child_fail(b"claim grant nel figlio fallito\n"),
                }
            }
            let mut args = match build_argv(&progn, &envsrc) {
                Ok(b) => b,
                Err(_) => child_fail(b"argv troppo lunghi nel figlio\n"),
            };
            // envc sta a [8..16] LE: si accodano le voci e si aggiorna.
            let mut envc = u64::from_le_bytes(args[8..16].try_into().unwrap_or([0; 8]));
            for (slot, fd) in claimed.iter() {
                let name: &[u8] = match slot {
                    0 => b"VELORDO_FD0=",
                    1 => b"VELORDO_FD1=",
                    2 => b"VELORDO_FD2=",
                    _ => child_fail(b"slot stdio invalido\n"),
                };
                // fd decimale a mano (niente format!: solo il Vec proprio).
                let mut digits = [0u8; 20];
                let mut n = *fd as u64;
                let mut len = 0usize;
                if n == 0 {
                    digits[0] = b'0';
                    len = 1;
                } else {
                    let mut rev = [0u8; 20];
                    while n > 0 {
                        rev[len] = b'0' + (n % 10) as u8;
                        n /= 10;
                        len += 1;
                    }
                    let mut i = 0;
                    while i < len {
                        digits[i] = rev[len - 1 - i];
                        i += 1;
                    }
                }
                if args.len() + name.len() + len + 1 > ARGS_MAX as usize {
                    child_fail(b"env fd oltre ARGS_MAX\n");
                }
                args.extend_from_slice(name);
                args.extend_from_slice(&digits[..len]);
                args.push(0);
                envc += 1;
            }
            args[8..16].copy_from_slice(&envc.to_le_bytes());
            let r = unsafe {
                syscall4(SYS_EXEC, img.as_ptr().addr() as u64, img.len() as u64, args.as_ptr().addr() as u64, args.len() as u64)
            };
            let _ = r;
            child_fail(b"exec nel figlio fallita\n");
        }
        // Padre: chiude le estremita' figlio (senza, niente EOF).
        for fd in child_fds {
            vfs::pipe_close(fd);
        }
        Ok((
            Process { pid: rax as u32, chan: rdi },
            StdioPipes { stdin: tmp_in, stdout: tmp_out, stderr: tmp_err },
        ))
    }
}

/// Costruisce il blocco exec (regole `libr`, senza redirect): argv[0] =
/// programma; env ereditato salvo clear + overlay esplicito; voci env con
/// nome vuoto o `=`/NUL nel nome o NUL nel valore saltate (best-effort).
fn build_argv(args: &[OsString], env: &CommandEnv) -> io::Result<alloc::vec::Vec<u8>> {
    use crate::os::velordo::ffi::OsStrExt as _;
    if args.len() > 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "troppi argv"));
    }
    // Env: `capture()` fa gia' inherit-salvo-clear + overlay + remove.
    // (sola lettura della mappa: niente growth, sicuro nel figlio).
    let merged = env.capture();
    let mut envc = 0usize;
    let mut env_len = 0usize;
    for (k, v) in merged.iter() {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        if kb.is_empty() || kb.contains(&b'=') || kb.contains(&0) || vb.contains(&0) {
            continue;
        }
        envc += 1;
        env_len += kb.len() + 1 + vb.len() + 1;
    }
    if envc > 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "troppe env"));
    }
    let mut total = 16usize;
    for a in args {
        total += a.as_bytes().len() + 1;
    }
    total += env_len;
    if total > ARGS_MAX as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "argv+env oltre ARGS_MAX"));
    }
    let mut buf = alloc::vec::Vec::with_capacity(total);
    buf.extend_from_slice(&(args.len() as u64).to_le_bytes());
    buf.extend_from_slice(&(envc as u64).to_le_bytes());
    for a in args {
        buf.extend_from_slice(a.as_bytes());
        buf.push(0);
    }
    for (k, v) in merged.iter() {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        if kb.is_empty() || kb.contains(&b'=') || kb.contains(&0) || vb.contains(&0) {
            continue;
        }
        buf.extend_from_slice(kb);
        buf.push(b'=');
        buf.extend_from_slice(vb);
        buf.push(0);
    }
    Ok(buf)
}

/// Fallimento pre-exec nel figlio: seriale diretta (niente FS) + exit.
/// Non ritorna mai (l'exec riuscita non ritorna affatto).
fn child_fail(msg: &[u8]) -> ! {
    let _ = write_console(msg);
    unsafe { syscall4(SYS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop()
    }
}

pub fn output(cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    // Come motor: pipe su stdout/stderr, stdin scartato (EOF al figlio),
    // drain concorrente + wait in scope.
    let (mut process, mut pipes) = cmd.spawn(Stdio::MakePipe, false)?;
    drop(pipes.stdin.take());
    // Drain in thread, WAIT SUL MAIN (leader): le code IPC sono per-TID e
    // la EXIT_NOTIFY atterra sulla coda del leader (canale di nascita) — un
    // waiter non-leader non la vedrebbe mai (diagnosi S2.4c3b: figlio
    // morto+reaped con waiter muto). Stesso vincolo per `Child::wait`.
    let out = pipes.stdout.take();
    let err = pipes.stderr.take();
    let (status, (stdout, r_out), (stderr, r_err)) = crate::thread::scope(|scope| {
        let h_out = scope.spawn(move || {
            let mut buf = Vec::new();
            let r = match out {
                Some(o) => o.read_to_end(&mut buf).map(|_| ()),
                None => Ok(()),
            };
            (buf, r)
        });
        let h_err = scope.spawn(move || {
            let mut buf = Vec::new();
            let r = match err {
                Some(e) => e.read_to_end(&mut buf).map(|_| ()),
                None => Ok(()),
            };
            (buf, r)
        });
        let status = process.wait();
        let o = h_out.join().expect("drain stdout fallito");
        let e = h_err.join().expect("drain stderr fallito");
        (status, o, e)
    });
    r_out?;
    r_err?;
    Ok((status?, stdout, stderr))
}

pub fn read_output(
    out: ChildPipe,
    stdout: &mut Vec<u8>,
    err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    // Un thread per estremita' (drain concorrente: niente deadlock su pipe
    // piena). Le read sono serializzate dal lock pipe globale.
    crate::thread::scope(|scope| {
        let t = scope.spawn(|| err.read_to_end(stderr));
        let r = out.read_to_end(stdout);
        let r2 = t.join().expect("drain stderr fallito");
        r.map(|_| ()).and(r2.map(|_| ()))
    })
}

impl fmt::Debug for Command {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ExitStatus(i32);

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        if self.0 == 0 { Ok(()) } else { Err(ExitStatusError(*self)) }
    }

    pub fn code(&self) -> Option<i32> {
        Some(self.0)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit code: {}", self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitStatusError(ExitStatus);

impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        self.0
    }
}

impl ExitStatusError {
    pub fn code(self) -> Option<NonZeroI32> {
        NonZeroI32::new(self.0.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(i32);

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(&self) -> i32 {
        self.0
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code as i32)
    }
}

pub struct Process {
    pid: u32,
    chan: u64,
}

impl Drop for Process {
    fn drop(&mut self) {
        // Detach: il figlio continua; la sua notify resta in tabella
        // (filtrata per canale, evict-oldest). Mai kill al drop (come Unix).
    }
}

impl Process {
    pub fn id(&self) -> u32 {
        // PID pool < 32 (Fase 14): nessun troncamento reale a u32.
        self.pid
    }

    pub fn kill(&mut self) -> io::Result<()> {
        let r = unsafe { syscall4(SYS_KILL, self.pid as u64, 0, 0, 0) };
        if r < 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "kill rifiutata"));
        }
        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        wait_exit(self.chan).map(|c| ExitStatus(c as i32))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        poll_exits();
        Ok(take_exit(self.chan).map(|c| ExitStatus(c as i32)))
    }
}

pub type ChildPipe = crate::sys::pipe::Pipe;

pub fn getpid() -> u32 {
    panic!("no pids on this platform")
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<&'a OsStr> {
        self.iter.next().map(|os| &**os)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}

impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}
