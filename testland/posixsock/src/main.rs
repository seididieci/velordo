//! posixsock — probe delle socket POSIX sopra `civis::net` (N7.1, ADR-0076).
//!
//! Eseguito da init solo con `net=1`, dopo `dnsprobe`. Apre le socket con le
//! **sole** API POSIX della personalità (`libr::socket/connect/sendto/recvfrom/
//! send/recv/close`), fa echo UDP verso `netecho:7` e TCP verso `netecho:8`,
//! poi segnala `TEST_DONE` a init.
//!
//! Prova che il percorso completo funziona: personalità POSIX -> `libr::sock`
//! (tabella fd 64..72) -> `civis::net` (N7.0) -> servizio `net` -> core TCP/
//! UDP. Nessuna syscall nuova, kernel intatto.
//!
//! Il binario e' seedato anche in `usr/bin/posixsock.bin`: la shell nativa puo'
//! lanciarlo a mano (`posixsock udp ::1 7 ciao`).

#![no_std]
#![no_main]

use civis::println;
use civis::{self};
use libr::{close, connect, recv, recvfrom, send, sendto, socket};
use net::Ipv6Addr;

const AF_INET6: i64 = 10;
const SOCK_STREAM: i64 = 1;
const SOCK_DGRAM: i64 = 2;
/// `sockaddr_in6`: family(2) + port(2) + flowinfo(4) + addr(16) + scope(4).
const SA6_LEN: usize = 28;

/// Porte di `netecho` (RFC 862 per UDP, 8 per TCP).
const ECHO_UDP_PORT: u16 = 7;
const ECHO_TCP_PORT: u16 = 8;

const PAYLOAD: &[u8] = b"velordo-posixsock";

/// Quale prova eseguire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Udp,
    Tcp,
    Both,
}

libr::entry!(real_main);

fn real_main(sp: u64) -> ! {
    // `posixsock [udp|tcp] <host> <porta> <msg>` da shell; senza argomenti
    // (spawn di init) esegue la prova completa contro ::1.
    //
    // `Args` mutua i byte dello stack iniziale: si copia cio' che serve in
    // valori propri (nessun borrow che sopravvive al blocco).
    let mut which = Which::Both;
    let mut host_buf = [0u8; 64];
    let mut host_len = 0usize;
    let mut port: Option<u16> = None;
    let mut msg_buf = [0u8; 128];
    let mut msg_len = PAYLOAD.len();
    msg_buf[..msg_len].copy_from_slice(PAYLOAD);

    if let Some(args) = civis::args_from_stack(sp) {
        let p1 = args.get(1).and_then(|a| core::str::from_utf8(a).ok());
        which = match p1 {
            Some("udp") => Which::Udp,
            Some("tcp") => Which::Tcp,
            _ => Which::Both,
        };
        if which != Which::Both {
            let p2 = args.get(2).and_then(|a| core::str::from_utf8(a).ok());
            let p3 = args.get(3).and_then(|a| core::str::from_utf8(a).ok());
            let p4 = args.get(4).and_then(|a| core::str::from_utf8(a).ok());
            let (h, p, m) = match (p2, p3, p4) {
                (Some(h), Some(p), Some(m)) => (Some(h), Some(p), Some(m)),
                (Some(p), Some(m), None) => (None, Some(p), Some(m)),
                _ => (None, None, None),
            };
            if let Some(h) = h {
                let n = h.len().min(host_buf.len());
                host_buf[..n].copy_from_slice(&h.as_bytes()[..n]);
                host_len = n;
            }
            if let Some(p) = p {
                port = p.parse().ok();
            }
            if let Some(m) = m {
                let n = m.len().min(msg_buf.len());
                msg_buf[..n].copy_from_slice(&m.as_bytes()[..n]);
                msg_len = n;
            }
        }
    }

    let host = if host_len == 0 {
        "::1"
    } else {
        core::str::from_utf8(&host_buf[..host_len]).unwrap_or("::1")
    };
    let msg = &msg_buf[..msg_len];

    let ip = match Ipv6Addr::parse(host) {
        Some(a) => a,
        None => fail("indirizzo IPv6 invalido"),
    };

    let ok = match which {
        Which::Udp => {
            let r = udp(ip, port.unwrap_or(ECHO_UDP_PORT), msg);
            report("udp", r);
            r
        }
        Which::Tcp => {
            let r = tcp(ip, port.unwrap_or(ECHO_TCP_PORT), msg);
            report("tcp", r);
            r
        }
        // Spawn di init: entrambe le prove, in sequenza.
        Which::Both => {
            let a = udp(ip, ECHO_UDP_PORT, msg);
            report("udp", a);
            let b = tcp(ip, ECHO_TCP_PORT, msg);
            report("tcp", b);
            a && b
        }
    };

    finish(ok)
}

fn report(proto: &str, ok: bool) {
    if ok {
        println!("[posixsock] {} PASS", proto);
    } else {
        println!("[posixsock] {} FAIL", proto);
    }
}

/// `sockaddr_in6` in network byte order (la porta la converte `libr`).
fn sockaddr_in6(ip: Ipv6Addr, port: u16) -> [u8; SA6_LEN] {
    let mut sa = [0u8; SA6_LEN];
    sa[0..2].copy_from_slice(&(AF_INET6 as u16).to_le_bytes());
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[8..24].copy_from_slice(&ip.to_bytes());
    sa
}

/// UDP: `sendto` verso l'echo, `recvfrom`, confronto del payload.
fn udp(ip: Ipv6Addr, port: u16, msg: &[u8]) -> bool {
    let sa = sockaddr_in6(ip, port);
    let fd = socket(AF_INET6, SOCK_DGRAM, 0);
    if fd < 0 {
        println!("[posixsock] socket(udp): errno {}", -fd);
        return false;
    }
    let sent = unsafe { sendto(fd, msg.as_ptr(), msg.len(), 0, sa.as_ptr(), SA6_LEN) };
    if sent != msg.len() as i64 {
        println!("[posixsock] sendto: {}", sent);
        let _ = close(fd);
        return false;
    }
    let mut buf = [0u8; 256];
    let mut from = [0u8; SA6_LEN];
    let mut from_len = SA6_LEN;
    let got = unsafe {
        recvfrom(
            fd,
            buf.as_mut_ptr(),
            buf.len(),
            0,
            from.as_mut_ptr(),
            &mut from_len,
        )
    };
    let _ = close(fd);
    if got < 0 {
        println!("[posixsock] recvfrom: errno {}", -got);
        return false;
    }
    let n = got as usize;
    if n != msg.len() || &buf[..n] != msg {
        println!("[posixsock] udp: echo non corrispondente ({} byte)", n);
        return false;
    }
    true
}

/// TCP: `connect` + `send`, `recv`, confronto del payload.
fn tcp(ip: Ipv6Addr, port: u16, msg: &[u8]) -> bool {
    let sa = sockaddr_in6(ip, port);
    let fd = socket(AF_INET6, SOCK_STREAM, 0);
    if fd < 0 {
        println!("[posixsock] socket(tcp): errno {}", -fd);
        return false;
    }
    let rc = unsafe { connect(fd, sa.as_ptr(), SA6_LEN) };
    if rc < 0 {
        println!("[posixsock] connect: errno {}", -rc);
        let _ = close(fd);
        return false;
    }
    let sent = unsafe { send(fd, msg.as_ptr(), msg.len(), 0) };
    if sent != msg.len() as i64 {
        println!("[posixsock] send: {}", sent);
        let _ = close(fd);
        return false;
    }
    let mut buf = [0u8; 256];
    let got = unsafe { recv(fd, buf.as_mut_ptr(), buf.len(), 0) };
    let _ = close(fd);
    if got < 0 {
        println!("[posixsock] recv: errno {}", -got);
        return false;
    }
    let n = got as usize;
    if n != msg.len() || &buf[..n] != msg {
        println!("[posixsock] tcp: echo non corrispondente ({} byte)", n);
        return false;
    }
    true
}

/// Esito + `TEST_DONE` a init (contratto dei probe, come `netudp`).
fn finish(ok: bool) -> ! {
    if ok {
        println!("[posixsock] risultato: PASS");
    } else {
        println!("[posixsock] risultato: FAIL");
    }
    let _ = civis::send(
        civis::CHANNEL_PARENT,
        civis::TEST_DONE,
        if ok { 1 } else { 0 },
        0,
    );
    civis::exit(if ok { 0 } else { 1 })
}

fn fail(why: &str) -> ! {
    println!("[posixsock] {}", why);
    finish(false)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[posixsock] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
