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
use alloc::vec::Vec;

use civis::println;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use civis::{
    NET_ADDR, NET_ADDR_ROUTER, NET_EP_CLOSE, NET_EP_FLAG_CONNECT, NET_EP_OPEN, NET_EP_RX_RING_OFF,
    NET_EP_RING_HDR, NET_EP_RING_SLOTS, NET_EP_SLOT_SIZE, NET_EP_TX, NET_EP_TX_RING_OFF,
    NET_PROTO_TCP,
};
use tls::{IoError, Transport};

static FUTEX: u32 = 0;

const EV_DATA: u8 = 0;
const EV_SYNACK: u8 = 2;
const EV_FIN: u8 = 3;
const EV_RST: u8 = 4;

/// Metadata slot: `[len:2][addr:16][dport:2][sport:2][evento:1][payload]`.
const SLOT_META: usize = 23;

/// Chunk di scrittura: <= `SEND_BUF` del core TCP (4096) meno il margine,
/// cosi' un chunk entra sempre nel buffer di invio. Il servizio non riporta
/// quanti byte ha accettato: un parziale non e' distinguibile (vedi sotto).
const WRITE_CHUNK: usize = 1024;

/// Attesa massima per un record in arrivo (10 s a 100 Hz).
const READ_TICKS: u64 = 1000;
/// Attesa massima per far posto nel buffer di invio TCP.
const WRITE_TICKS: u64 = 300;

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
    let r = match civis::send(
        net_chan,
        NET_EP_OPEN,
        (NET_PROTO_TCP as u64) << 16,
        NET_EP_FLAG_CONNECT as u64,
    ) {
        Ok(r) => r,
        Err(_) => {
            println!("[tlsprobe] FAIL: open (IPC)");
            finish(false);
        }
    };
    if r.w0 == !0u64 {
        println!("[tlsprobe] FAIL: open rifiutato");
        finish(false);
    }
    let ep_id = r.w0 as u16;
    let ring_id = (r.w1 & 0xffff_ffff) as u32;
    let local_port = (r.w1 >> 32) as u16;
    println!("[tlsprobe] endpoint TCP id={} porta={}", ep_id, local_port);

    let ring_base = match civis::shm_map(ring_id, 0, civis::PROT_READ | civis::PROT_WRITE) {
        Ok(b) => b,
        Err(_) => {
            println!("[tlsprobe] FAIL: shm map");
            finish(false);
        }
    } as *mut u8;
    unsafe {
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_RX_RING_OFF + 4) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF) as *mut u32, 0);
        write_volatile(ring_base.add(NET_EP_TX_RING_OFF + 4) as *mut u32, 0);
    }

    let transport = NetTcp {
        net_chan,
        ep_id,
        ring: ring_base,
        dst,
        port,
        tx_seq: 0,
        rx_head: 0,
        pending: Vec::new(),
        ppos: 0,
        eof: false,
    };

    // Buffer record nel Box (heap civis): 16 KiB l'uno, mai sullo stack.
    let mut rbuf = alloc::vec![0u8; tls::RECORD_READ_BUF].into_boxed_slice();
    let mut wbuf = alloc::vec![0u8; tls::RECORD_WRITE_BUF].into_boxed_slice();
    let cfg = tls::Config {
        server_name: &sni,
        ca_der: &ca,
        alpn: &[&b"dot"[..]],
    };

    let mut sess = match tls::connect::<NetTcp, tls::SystemClock>(
        transport,
        &cfg,
        &mut rbuf[..],
        &mut wbuf[..],
    ) {
        Ok(s) => s,
        Err(e) => {
            println!("[tlsprobe] FAIL: handshake: {:?}", e);
            let _ = civis::send(net_chan, NET_EP_CLOSE, ep_id as u64, 0);
            finish(false);
        }
    };
    println!("[tlsprobe] handshake: PASS (TLS 1.3, catena + hostname verificati)");

    // ── Record applicativo: GET e risposta del server ──────────────────
    let req = b"GET / HTTP/1.0\r\n\r\n";
    if let Err(e) = sess.write_all(req) {
        println!("[tlsprobe] FAIL: write applicativo: {:?}", e);
        let _ = civis::send(net_chan, NET_EP_CLOSE, ep_id as u64, 0);
        finish(false);
    }
    let mut resp = [0u8; 512];
    let n = match sess.read(&mut resp) {
        Ok(n) => n,
        Err(e) => {
            println!("[tlsprobe] FAIL: read applicativo: {:?}", e);
            let _ = civis::send(net_chan, NET_EP_CLOSE, ep_id as u64, 0);
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
    let _ = civis::send(net_chan, NET_EP_CLOSE, ep_id as u64, 0);
    finish(http);
}

// ── Trasporto: endpoint TCP di `net` come stream ──────────────────────

struct NetTcp {
    net_chan: u64,
    ep_id: u16,
    ring: *mut u8,
    dst: [u8; 16],
    port: u16,
    /// Indice di slot TX (modulo `NET_EP_RING_SLOTS`).
    tx_seq: u32,
    rx_head: u32,
    /// Payload di uno slot RX piu' grande della richiesta: il resto va qui
    /// (semantica di stream, mai byte persi).
    pending: Vec<u8>,
    ppos: usize,
    eof: bool,
}

impl NetTcp {
    fn take(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.pending.len() - self.ppos);
        buf[..n].copy_from_slice(&self.pending[self.ppos..self.ppos + n]);
        self.ppos += n;
        n
    }

    fn wait_tick() {
        let _ = civis::futex_wait(
            &FUTEX as *const u32 as usize,
            0,
            (civis::get_ticks() + 1) as u64,
        );
    }
}

impl Transport for NetTcp {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        if self.ppos < self.pending.len() {
            return Ok(self.take(buf));
        }
        if self.eof {
            return Err(IoError::Closed);
        }
        let deadline = civis::get_ticks() as u64 + READ_TICKS;
        loop {
            let tail = unsafe {
                read_volatile(self.ring.add(NET_EP_RX_RING_OFF + 4) as *const u32)
            };
            if self.rx_head != tail {
                let slot = (self.rx_head % NET_EP_RING_SLOTS as u32) as usize;
                let p = unsafe {
                    self.ring
                        .add(NET_EP_RX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE)
                };
                let total = unsafe { read_volatile(p as *const u16) } as usize;
                let event = if total >= SLOT_META {
                    unsafe { read_volatile(p.add(22)) }
                } else {
                    0xff
                };
                let len = total.saturating_sub(SLOT_META);
                self.pending.clear();
                self.ppos = 0;
                if event == EV_DATA && len > 0 {
                    self.pending.extend_from_slice(unsafe {
                        core::slice::from_raw_parts(p.add(SLOT_META), len)
                    });
                }
                unsafe {
                    write_volatile(
                        self.ring.add(NET_EP_RX_RING_OFF) as *mut u32,
                        self.rx_head.wrapping_add(1),
                    )
                };
                self.rx_head = self.rx_head.wrapping_add(1);
                match event {
                    EV_DATA if len > 0 => return Ok(self.take(buf)),
                    EV_SYNACK => continue, // handshake: il core lo ha gia' chiuso
                    EV_RST | EV_FIN => {
                        self.eof = true;
                        return Err(IoError::Closed);
                    }
                    _ => continue,
                }
            }
            if civis::get_ticks() as u64 >= deadline {
                return Err(IoError::Failed);
            }
            Self::wait_tick();
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let chunk = buf.len().min(WRITE_CHUNK);
        let deadline = civis::get_ticks() as u64 + WRITE_TICKS;
        loop {
            let slot = (self.tx_seq % NET_EP_RING_SLOTS as u32) as usize;
            write_slot(self.ring, slot, &self.dst, self.port, EV_DATA, &buf[..chunk]);
            match civis::send(self.net_chan, NET_EP_TX, self.ep_id as u64, slot as u64) {
                Ok(r) if r.w0 == 0 => {
                    self.tx_seq = self.tx_seq.wrapping_add(1);
                    return Ok(chunk);
                }
                _ => {
                    // Buffer di invio TCP pieno (o handshake non ancora
                    // avviabile): si riprova lo stesso slot dopo un tick. Il
                    // servizio non riporta l'accettazione parziale; con
                    // chunk <= WRITE_CHUNK e buffer quasi vuoto non accade,
                    // e se accadesse il record TLS sarebbe invalido (fallisce
                    // loud, non silenzioso).
                    if civis::get_ticks() as u64 >= deadline {
                        return Err(IoError::Failed);
                    }
                    Self::wait_tick();
                }
            }
        }
    }
}

/// Scrive lo slot TX `slot` (payload applicativo).
fn write_slot(ring: *mut u8, slot: usize, dst: &[u8; 16], port: u16, event: u8, payload: &[u8]) {
    let total = SLOT_META + payload.len();
    let p = unsafe { ring.add(NET_EP_TX_RING_OFF + NET_EP_RING_HDR + slot * NET_EP_SLOT_SIZE) };
    unsafe {
        write_volatile(p as *mut u16, total as u16);
        core::ptr::copy_nonoverlapping(dst.as_ptr(), p.add(2), 16);
        write_volatile(p.add(18) as *mut u16, port);
        write_volatile(p.add(20) as *mut u16, 0);
        write_volatile(p.add(22) as *mut u8, event);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(SLOT_META), payload.len());
    }
    fence(Ordering::Release);
    unsafe {
        write_volatile(
            ring.add(NET_EP_TX_RING_OFF + 4) as *mut u32,
            (slot + 1) as u32,
        )
    };
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
