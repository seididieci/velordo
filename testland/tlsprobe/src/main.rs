//! tlsprobe — prova di handshake TLS 1.3 **reale** (N6.2, ADR-0072).
//!
//! Fuori dal gate: si esegue solo con `net=1` **e** `tls=1` in cmdline, e
//! solo se `scripts/arca-tool.sh` ha seedato `sys:tls.conf` + `sys:ca.der`
//! (li genera `scripts/run-tls-tests.sh`). Senza config il probe si salta
//! (stessa convenzione dei probe di rete N2–N6).
//!
//! Cosa prova:
//! 1. handshake TLS 1.3 completo contro `openssl s_server` sull'host
//!    (raggiunto al gateway di passt, che mappa il loopback dell'host);
//! 2. **verifica del certificato**: catena firmata da una CA di test e
//!    hostname (`sni`) — non `NoVerify`;
//! 3. record applicativo in chiaro dopo l'handshake (GET HTTP/1.0): il
//!    traffico e' davvero cifrato/decifrato, non solo il ClientHello.
//!
//! Il trasporto e' l'endpoint TCP del servizio `net` (ring SPSC), come in
//! N6.3 fara' `dns`: qui c'e' solo l'adattatore `Transport`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use civis::{println, NET_ADDR, NET_ADDR_ROUTER};
use tls_net::TcpStream;

static FUTEX: u32 = 0;

civis::entry!(real_main);

fn real_main(_sp: u64) -> ! {
    // ── Config: senza `sys:tls.conf` il probe si salta ─────────────────
    let (sni, port) = match civis::obj_get(b"sys", b"tls.conf") {
        Ok(data) => match parse_conf(&data) {
            Some(v) => v,
            None => {
                println!("[tlsprobe] SKIP: sys:tls.conf invalido");
                finish(true);
            }
        },
        Err(_) => {
            println!("[tlsprobe] SKIP: nessun sys:tls.conf (test fuori gate)");
            finish(true);
        }
    };
    let ca = match civis::obj_get(b"sys", b"ca.der") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            // Config presente ma CA assente: fail loud, mai handshake senza
            // trust anchor.
            println!("[tlsprobe] FAIL: sys:ca.der mancante (trust anchor)");
            finish(false);
        }
    };

    let net_chan = match wait_service(civis::Service::Net) {
        Some(c) => c,
        None => {
            println!("[tlsprobe] FAIL: servizio net non trovato");
            finish(false);
        }
    };
    let dst = match router_addr(net_chan) {
        Some(a) => a,
        None => {
            println!("[tlsprobe] SKIP: nessun router (nessun RA)");
            finish(true);
        }
    };
    println!("[tlsprobe] handshake verso {}:{} sni={}", fmt_addr(&dst), port, sni);

    // ── Endpoint TCP CONNECT (porta effimera) ──────────────────────────
    // Trasporto condiviso con `dns` (N6.3): `libs/tls-net`.
    let mut transport = match TcpStream::connect(net_chan, dst, port) {
        Some(t) => t,
        None => {
            println!("[tlsprobe] FAIL: endpoint TCP non aperto");
            finish(false);
        }
    };
    println!("[tlsprobe] endpoint TCP id={}", transport.ep_id());

    // Buffer record nel Box (heap civis): 16 KiB l'uno, mai sullo stack.
    let mut rbuf = alloc::vec![0u8; tls::RECORD_READ_BUF].into_boxed_slice();
    let mut wbuf = alloc::vec![0u8; tls::RECORD_WRITE_BUF].into_boxed_slice();
    let cfg = tls::Config {
        server_name: &sni,
        ca_der: &ca,
        alpn: &[&b"dot"[..]],
    };

    // `&mut`: il socket resta nostro anche se l'handshake fallisce (lo
    // chiudiamo esplicitamente, senza lasciare la connessione appesa).
    let mut sess = match tls::connect::<&mut TcpStream, tls_net::SystemClock>(
        &mut transport,
        &cfg,
        &mut rbuf[..],
        &mut wbuf[..],
    ) {
        Ok(s) => s,
        Err(e) => {
            println!("[tlsprobe] FAIL: handshake: {:?}", e);
            transport.abort();
            transport.shutdown();
            finish(false);
        }
    };
    println!("[tlsprobe] handshake: PASS (TLS 1.3, catena + hostname verificati)");

    // ── Record applicativo: GET e risposta del server ──────────────────
    let req = b"GET / HTTP/1.0\r\n\r\n";
    if let Err(e) = sess.write_all(req) {
        println!("[tlsprobe] FAIL: write applicativo: {:?}", e);
        transport.abort();
        transport.shutdown();
        finish(false);
    }
    let mut resp = [0u8; 512];
    let n = match sess.read(&mut resp) {
        Ok(n) => n,
        Err(e) => {
            println!("[tlsprobe] FAIL: read applicativo: {:?}", e);
            transport.abort();
            transport.shutdown();
            finish(false);
        }
    };
    let http = resp[..n].starts_with(b"HTTP/");
    if http {
        println!("[tlsprobe] appdata: PASS ({} byte, HTTP)", n);
    } else {
        println!("[tlsprobe] appdata: FAIL ({} byte, non HTTP)", n);
    }

    // Chiusura ordinata (close_notify) + endpoint TCP.
    let _ = sess.close();
    transport.shutdown();
    finish(http);
}

// ── Config e utility ─────────────────────────────────────────────────

/// `sys:tls.conf`: righe `chiave = valore`. Chiavi usate: `sni`, `port`.
fn parse_conf(data: &[u8]) -> Option<(String, u16)> {
    let txt = core::str::from_utf8(data).ok()?;
    let mut sni = String::new();
    let mut port = 0u16;
    for line in txt.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=')?;
        match k.trim() {
            "sni" => sni = String::from(v.trim()),
            "port" => port = v.trim().parse().ok()?,
            _ => {}
        }
    }
    if sni.is_empty() || port == 0 {
        return None;
    }
    Some((sni, port))
}

fn wait_service(svc: civis::Service) -> Option<u64> {
    for _ in 0..200 {
        if let Ok(c) = civis::service_lookup(svc) {
            if c > 0 {
                return Some(c as u64);
            }
        }
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
    None
}

/// Indirizzo del default router (`NET_ADDR_ROUTER`): con passt e' il gateway,
/// che mappa il loopback dell'host (stessa strada del responder DNS N6.0).
fn router_addr(net_chan: u64) -> Option<[u8; 16]> {
    let r = civis::send(net_chan, NET_ADDR, NET_ADDR_ROUTER, 0).ok()?;
    if r.w0 == 0 && r.w1 == 0 {
        return None;
    }
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&r.w0.to_be_bytes());
    a[8..].copy_from_slice(&r.w1.to_be_bytes());
    Some(a)
}

/// IPv6 in forma compatta.
fn fmt_addr(a: &[u8; 16]) -> String {
    let mut s = String::new();
    let mut i = 0;
    while i < 16 {
        let g = ((a[i] as u16) << 8) | a[i + 1] as u16;
        if i > 0 {
            s.push(':');
        }
        if g == 0 {
            s.push('0');
        } else {
            let mut buf = [0u8; 4];
            let mut n = 0;
            let mut v = g;
            while v > 0 {
                buf[n] = b"0123456789abcdef"[(v & 0xf) as usize];
                v >>= 4;
                n += 1;
            }
            while n > 0 {
                n -= 1;
                s.push(buf[n] as char);
            }
        }
        i += 2;
    }
    s
}

fn finish(ok: bool) -> ! {
    if ok {
        println!("[tlsprobe] risultato: PASS");
    } else {
        println!("[tlsprobe] risultato: FAIL");
    }
    let _ = civis::send(
        civis::CHANNEL_PARENT,
        civis::TEST_DONE,
        if ok { 1 } else { 0 },
        0,
    );
    civis::exit(if ok { 0 } else { 1 })
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Posizione e messaggio: un panic muto costa un boot per capirlo.
    let _ = civis::print_string(b"[tlsprobe] panic: ");
    match info.location() {
        Some(l) => {
            println!("{}:{}", l.file(), l.line());
        }
        None => {
            println!("(nessuna posizione)");
        }
    }
    println!("[tlsprobe] motivo: {}", info.message());
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
