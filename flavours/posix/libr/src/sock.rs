//! Socket POSIX sopra `civis::net` (N7.1, ADR-0076).
//!
//! **Opzione (b)**: i fd `FD_BASE .. FD_BASE+NSOCK` sono riservati alle socket,
//! con una tabella statica in `libr`; i fd sotto `FD_BASE` restano ai file
//! (stdio e redirect, `libr::stdio`). Deviazione dichiarata in ADR-0076: la
//! tabella fd unificata (file + socket, con `read`/`write` che dispatchano per
//! range) e' il follow-up.
//!
//! Il meccanismo `civis::net` (N7.0, ADR-0074) e' riusato invariato: zero
//! syscall nuove, kernel intatto. Il servizio `net` resta l'unico che tocca il
//! device.
//!
//! Convenzione d'errore: come il resto di `libr`, le funzioni non impostano un
//! `errno` globale (non esiste nel meccanismo). Ritornano **`-errno`**
//! (`-EINVAL`, `-ENOTCONN`, ...): il chiamante confronta `< 0`, e il numero
//! assoluto e' l'errno POSIX standard. L'unica traduzione nativo→errno resta
//! [`crate::posix::to_errno`].

use crate::posix::{to_errno, EAFNOSUPPORT, EBADF, EINVAL, ENOTCONN, ETIMEDOUT};
use civis::net::{Endpoint, Event, Flow, Proto, Recv, SendError};
use civis::{self, Error, Service};

/// Primo fd delle socket: sotto questo numero i fd sono dei file.
pub const FD_BASE: i64 = 64;
/// Socket contemporanee per processo.
pub const NSOCK: usize = 8;

/// `AF_INET6` (Linux/x86-64). `AF_INET` (2) e' rifiutato con `EAFNOSUPPORT`:
/// il percorso IPv4 (via `siit`) e' un follow-up.
const AF_INET6: i64 = 10;
/// `SOCK_DGRAM` / `SOCK_STREAM`.
const SOCK_DGRAM: i64 = 2;
const SOCK_STREAM: i64 = 1;

/// `sockaddr_in6` (RFC 3493): `family:u16, port:u16, flowinfo:u32,
/// addr:[u8;16], scope_id:u32` = 28 byte.
const SOCKADDR_IN6_LEN: usize = 28;
/// Offset dell'indirizzo dentro `sockaddr_in6`.
const SOCKADDR_IN6_ADDR_OFF: usize = 8;

/// Deadline di default di `recv`/`recvfrom` quando il socket e' bloccante:
/// 5 s (500 tick da 10 ms). Oltre, `-ETIMEDOUT`.
const RECV_DEADLINE_TICKS: u64 = 500;

/// Parola di futex per le attese idle (ADR-0067): mai spin puro al prompt.
/// Come in `netecho`: si passa solo l'indirizzo, non si legge mai.
static FUTEX: u32 = 0;

/// Nativo → `-errno`.
fn err(e: Error) -> i64 {
    -to_errno(e)
}

/// Trasporto di un socket.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Udp,
    Tcp,
}

struct Sock {
    kind: Kind,
    ep: Endpoint,
    /// Destinazione memorizzata da `connect` (UDP "connesso" e TCP).
    dst: [u8; 16],
    dst_port: u16,
    connected: bool,
}

/// Tabella statica: `None` = slot libero.
static mut TABLE: [Option<Sock>; NSOCK] = [const { None }; NSOCK];

fn slot_of(fd: i64) -> Option<usize> {
    if fd < FD_BASE || fd >= FD_BASE + NSOCK as i64 {
        None
    } else {
        Some((fd - FD_BASE) as usize)
    }
}

fn fd_of(slot: usize) -> i64 {
    FD_BASE + slot as i64
}

/// Accesso alla entry `slot` (il chiamante ha gia' validato l'indice).
///
/// # Safety
/// `slot < NSOCK`.
unsafe fn entry_mut(slot: usize) -> Option<&'static mut Sock> {
    unsafe { (*core::ptr::addr_of_mut!(TABLE[slot])).as_mut() }
}

/// Canale verso il servizio `net`, con retry idle (il servizio puo' partire
/// dopo il programma che lo cerca).
fn net_chan() -> Result<u64, Error> {
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(Service::Net) {
            if c > 0 {
                return Ok(c as u64);
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    Err(Error::NotReady)
}

/// Legge un `sockaddr_in6` dal chiamante: `(indirizzo, porta)`.
///
/// # Safety
/// `addr` deve puntare ad almeno `len` byte leggibili.
unsafe fn read_sockaddr(addr: *const u8, len: usize) -> Result<([u8; 16], u16), i64> {
    if addr.is_null() || len < SOCKADDR_IN6_LEN {
        return Err(-EINVAL);
    }
    let family = unsafe { u16::from_le_bytes([*addr, *addr.add(1)]) };
    if family != AF_INET6 as u16 {
        return Err(-EAFNOSUPPORT);
    }
    // La porta e' in network byte order anche nel sockaddr.
    let port = unsafe { u16::from_be_bytes([*addr.add(2), *addr.add(3)]) };
    let mut ip = [0u8; 16];
    unsafe {
        core::ptr::copy_nonoverlapping(
            addr.add(SOCKADDR_IN6_ADDR_OFF),
            ip.as_mut_ptr(),
            16,
        )
    };
    Ok((ip, port))
}

/// Scrive `sockaddr_in6` nel buffer del chiamante (per `recvfrom`).
///
/// # Safety
/// `addr`/`len_ptr` devono essere nulli insieme, o puntare a memoria
/// scrivibile (`*len_ptr` = capacita' del buffer).
unsafe fn write_sockaddr(
    addr: *mut u8,
    len_ptr: *mut usize,
    ip: [u8; 16],
    port: u16,
) {
    if addr.is_null() || len_ptr.is_null() {
        return;
    }
    let cap = unsafe { *len_ptr };
    if cap < SOCKADDR_IN6_LEN {
        return;
    }
    unsafe {
        core::ptr::write_bytes(addr, 0, SOCKADDR_IN6_LEN);
        let fam = (AF_INET6 as u16).to_le_bytes();
        *addr = fam[0];
        *addr.add(1) = fam[1];
        let p = port.to_be_bytes();
        *addr.add(2) = p[0];
        *addr.add(3) = p[1];
        core::ptr::copy_nonoverlapping(ip.as_ptr(), addr.add(SOCKADDR_IN6_ADDR_OFF), 16);
        *len_ptr = SOCKADDR_IN6_LEN;
    }
}

/// `socket(domain, type, protocol)` → fd (>= `FD_BASE`) o `-errno`.
pub fn socket(domain: i64, type_: i64, protocol: i64) -> i64 {
    if domain != AF_INET6 {
        return -EAFNOSUPPORT;
    }
    // `protocol` 0 = default del tipo; il resto non e' supportato.
    if protocol != 0 {
        return -EINVAL;
    }
    let kind = match type_ {
        SOCK_DGRAM => Kind::Udp,
        SOCK_STREAM => Kind::Tcp,
        _ => return -EINVAL,
    };

    let mut slot = None;
    for i in 0..NSOCK {
        if unsafe { (*core::ptr::addr_of!(TABLE[i])).is_none() } {
            slot = Some(i);
            break;
        }
    }
    let Some(slot) = slot else {
        return err(Error::NoMemory);
    };

    let chan = match net_chan() {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let (proto, flow) = match kind {
        Kind::Udp => (Proto::Udp, Flow::Ephemeral),
        Kind::Tcp => (Proto::Tcp, Flow::Connect),
    };
    let ep = match Endpoint::open(chan, proto, flow) {
        Ok(e) => e,
        Err(e) => return err(e),
    };

    unsafe {
        *core::ptr::addr_of_mut!(TABLE[slot]) = Some(Sock {
            kind,
            ep,
            dst: [0u8; 16],
            dst_port: 0,
            connected: false,
        });
    }
    fd_of(slot)
}

/// `connect(fd, addr, addrlen)` → `0` o `-errno`.
///
/// Su UDP memorizza la destinazione (il primo `send` la usa); su TCP la
/// connessione la apre il **core** al primo invio (N6.1c), quindi qui si
/// registra solo la destinazione. Nessuna attesa: `send`/`recv` dicono se il
/// peer risponde.
///
/// # Safety
/// `addr` deve puntare a `addrlen` byte leggibili.
pub unsafe fn connect(fd: i64, addr: *const u8, addrlen: usize) -> i64 {
    let Some(slot) = slot_of(fd) else {
        return -EBADF;
    };
    let (ip, port) = match unsafe { read_sockaddr(addr, addrlen) } {
        Ok(v) => v,
        Err(e) => return e,
    };
    let e = unsafe { entry_mut(slot) };
    let Some(s) = e else {
        return -EBADF;
    };
    s.dst = ip;
    s.dst_port = port;
    s.connected = true;
    0
}

/// `send(fd, buf, len, flags)` → byte inviati o `-errno`.
///
/// # Safety
/// `buf` deve puntare a `len` byte leggibili.
pub unsafe fn send(fd: i64, buf: *const u8, len: usize, flags: i64) -> i64 {
    unsafe { sendto(fd, buf, len, flags, core::ptr::null(), 0) }
}

/// `sendto(fd, buf, len, flags, addr, addrlen)` → byte inviati o `-errno`.
///
/// `addr` nullo = usa la destinazione di `connect` (altrimenti `-ENOTCONN`).
/// Backpressure del ring (`Full`) = `-EAGAIN`: il chiamante ritenta.
///
/// # Safety
/// `buf` deve puntare a `len` byte leggibili; `addr`, se non nullo, ad
/// `addrlen` byte.
pub unsafe fn sendto(
    fd: i64,
    buf: *const u8,
    len: usize,
    _flags: i64,
    addr: *const u8,
    addrlen: usize,
) -> i64 {
    let Some(slot) = slot_of(fd) else {
        return -EBADF;
    };
    if buf.is_null() && len != 0 {
        return -EINVAL;
    }
    let (dst, port) = if !addr.is_null() {
        match unsafe { read_sockaddr(addr, addrlen) } {
            Ok(v) => v,
            Err(e) => return e,
        }
    } else {
        let e = unsafe { entry_mut(slot) };
        let Some(s) = e else {
            return -EBADF;
        };
        if !s.connected {
            return -ENOTCONN;
        }
        (s.dst, s.dst_port)
    };
    let e = unsafe { entry_mut(slot) };
    let Some(s) = e else {
        return -EBADF;
    };
    let payload = unsafe { core::slice::from_raw_parts(buf, len) };
    match s.ep.send(dst, port, Event::Data, payload) {
        Ok(()) => len as i64,
        Err(SendError::Full) => -crate::posix::EAGAIN,
        Err(SendError::TooBig) => -crate::posix::EFBIG,
        Err(SendError::Dead) => err(Error::ServerDied),
    }
}

/// `recv(fd, buf, len, flags)` → byte ricevuti o `-errno`.
///
/// # Safety
/// `buf` deve puntare a `len` byte scrivibili.
pub unsafe fn recv(fd: i64, buf: *mut u8, len: usize, flags: i64) -> i64 {
    unsafe {
        recvfrom(
            fd,
            buf,
            len,
            flags,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    }
}

/// `recvfrom(fd, buf, len, flags, addr, addrlen)` → byte ricevuti o `-errno`.
///
/// Attesa bloccante a deadline sull'**evento** del servizio `net`
/// (`Endpoint::wait`, E2/ADR-0084): niente spin ne' risveglio a tick.
/// `-ETIMEDOUT` oltre `RECV_DEADLINE_TICKS`.
///
/// # Safety
/// `buf` deve puntare a `len` byte scrivibili; `addr`/`addrlen` o entrambi
/// nulli o validi.
pub unsafe fn recvfrom(
    fd: i64,
    buf: *mut u8,
    len: usize,
    _flags: i64,
    addr: *mut u8,
    addrlen: *mut usize,
) -> i64 {
    let Some(slot) = slot_of(fd) else {
        return -EBADF;
    };
    if buf.is_null() && len != 0 {
        return -EINVAL;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };

    let deadline = civis::get_ticks() + RECV_DEADLINE_TICKS as i64;
    let mut got: Option<Recv> = None;
    while civis::get_ticks() < deadline {
        let e = unsafe { entry_mut(slot) };
        let Some(s) = e else {
            return -EBADF;
        };
        if let Some(r) = s.ep.recv(out) {
            // TCP: gli eventi di controllo non sono dati. `SynAck` arriva
            // prima dell'echo (handshake del core, N6.1c) e `Accept`/`Other`
            // non riguardano un client: si scartano e si continua ad
            // aspettare. `Fin` = EOF, `Rst` = connessione resettata.
            if s.kind == Kind::Tcp {
                match r.event {
                    Event::Data => {}
                    Event::Fin => return 0,
                    Event::Rst => return -crate::posix::ECONNRESET,
                    _ => continue,
                }
            }
            got = Some(r);
            break;
        }
        // E2 (ADR-0084): dorme sull'hint del servizio, non a tick. Il ring
        // e' stato ricontrollato sopra, quindi un hint perso non perde dati.
        match s.ep.wait(deadline as u64) {
            civis::net::Wait::Dead => return -crate::posix::ECONNRESET,
            _ => {}
        }
    }
    let Some(r) = got else {
        return -ETIMEDOUT;
    };
    unsafe { write_sockaddr(addr, addrlen, r.addr, r.port) };
    r.len as i64
}

/// `close(fd)` → `0` o `-errno`. Idempotente sul fd gia' chiuso (`-EBADF`).
pub fn close(fd: i64) -> i64 {
    let Some(slot) = slot_of(fd) else {
        return -EBADF;
    };
    let taken = unsafe { (*core::ptr::addr_of_mut!(TABLE[slot])).take() };
    match taken {
        Some(mut s) => {
            s.ep.close();
            0
        }
        None => -EBADF,
    }
}
