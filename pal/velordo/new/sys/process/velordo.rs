//! Processi Velordo (S2.4c2): fork (`SYS_SPAWN_COPY`) + exec con argv/env,
//! attesa via `EXIT_NOTIFY` (mini-client IPC qui dentro: `recv`/demux).
//! Forma di `motor.rs` (spawn reale non-unix); storage `OsString` come
//! `unsupported.rs` (i path sono byte, non UTF-8).
//!
//! Onesta' v1 (gap documentati, mai bugie):
//! - niente ricerca PATH: `program` deve essere un file leggibile (la shell
//!   risolve per conto suo); altrimenti `NotFound`.
//! - `MakePipe` a spawn → `Err(Unsupported)` (pipe ai figli in c3).
//! - `Null` su stdout/stderr perde su console (manca il routing fd: c3);
//!   `Null` su stdin e' EOF onesto (default gia' EOF).
//! - niente concorrenza FS durante fork+exec (ring condiviso senza lock;
//!   il lock globale arriva in c3 coi drain thread).
//! - `getpid` = panic (il kernel non espone il proprio pid: come unsupported).
//! - `Drop` di `Process` = detach (il figlio gira; notify stale filtrate
//!   per canale, evict-oldest a tabella piena).

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
        // Risoluzione stdio v1 (vedi onesta' in testa): MakePipe rifiutato
        // onesto (c3); il resto non tocca fd (in c2 il figlio eredita la
        // console: il routing fd arriva in c3).
        let use_pipe = [&self.stdin, &self.stdout, &self.stderr]
            .iter()
            .any(|s| matches!(s, Some(Stdio::MakePipe) | Some(Stdio::Fd(_))));
        if use_pipe {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdio piped/fd in S2.4c3",
            ));
        }
        let _ = (default, needs_stdin);
        // Immagine dal path (niente PATH search: la shell risolve).
        let img = crate::fs::read(Path::new(&self.program))?;
        // Blocco argv+env `[argc:8][envc:8][payload NUL]` (stesse regole di
        // `libr::serialize_argv_redir_env`, senza magic redirect in c2).
        let args = build_argv(&self.args, &self.env)?;
        let cwd = self.cwd.clone();
        // Fork: padre riceve (pid, chan), figlio 0 (come `fork()` POSIX).
        let (rax, rdi, _, _, _) = unsafe { syscall4_out(SYS_SPAWN_COPY, 0, 0, 0, 0) };
        if rax < 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "spawn_copy fallita"));
        }
        if rax == 0 {
            // Figlio: FS avvelenato (ring del padre non riusabili) — solo
            // syscall raw fino all'exec (che riparte da BSS pristine).
            vfs::poison_child_fstate();
            if let Some(dir) = cwd.as_ref() {
                let b = dir.as_encoded_bytes();
                let r = unsafe { syscall4(SYS_CHDIR, b.as_ptr().addr() as u64, b.len() as u64, 0, 0) };
                if r < 0 {
                    child_fail(b"chdir nel figlio fallita\n");
                }
            }
            let r = unsafe {
                syscall4(SYS_EXEC, img.as_ptr().addr() as u64, img.len() as u64, args.as_ptr().addr() as u64, args.len() as u64)
            };
            let _ = r;
            child_fail(b"exec nel figlio fallita\n");
        }
        Ok((
            Process { pid: rax as u32, chan: rdi },
            StdioPipes { stdin: None, stdout: None, stderr: None },
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

pub fn output(_cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    // Capture in c3 (serve il routing fd + drain concorrente).
    Err(io::Error::new(io::ErrorKind::Unsupported, "Command::output in S2.4c3"))
}

pub fn read_output(
    _out: ChildPipe,
    _stdout: &mut Vec<u8>,
    _err: ChildPipe,
    _stderr: &mut Vec<u8>,
) -> io::Result<()> {
    // Drain concorrente in c3 (col lock FS globale).
    Err(io::Error::new(io::ErrorKind::Unsupported, "read_output in S2.4c3"))
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
