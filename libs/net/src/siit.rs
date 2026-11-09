//! SIIT/NAT64: traduttore IPv6↔IPv4 puro.
//!
//! Operazioni su slice, nessuna allocazione, nessun panic su input arbitrario.
//! Il servizio `userland/siit` gestisce lo stato (tabella NAPT, shm, ring);
//! questo modulo fornisce solo la matematica di traduzione e checksum.

use crate::addr::Ipv6Addr;
use crate::checksum;

// ── Prefisso SIIT well-known (RFC 6052): 64:ff9b::/96 ───────────────

/// Prefisso SIIT: i primi 6 byte sono 64:ff9b::, gli ultimi 4 sono l'IPv4.
pub const SIIT_PREFIX: [u8; 6] = [0x64, 0xff, 0x9b, 0x00, 0x00, 0x00];

/// Porta del traduttore per ricevere risposte IPv4 (mappate in IPv6 SIIT).
pub const SIIT_TRANSLATOR_PORT: u16 = 30000;

/// Verifica se `addr` è nel prefisso SIIT (64:ff9b::/96).
pub fn is_siit_address(addr: &Ipv6Addr) -> bool {
    addr.0[..6] == SIIT_PREFIX
}

/// Estrae l'indirizzo IPv4 dagli ultimi 32 bit di un indirizzo IPv6 SIIT.
pub fn ipv4_from_siit(addr: &Ipv6Addr) -> Option<[u8; 4]> {
    if !is_siit_address(addr) {
        return None;
    }
    Some([addr.0[12], addr.0[13], addr.0[14], addr.0[15]])
}

/// Costruisce un indirizzo IPv6 SIIT da un indirizzo IPv4.
pub fn siit_from_ipv4(v4: &[u8; 4]) -> Ipv6Addr {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&SIIT_PREFIX);
    b[12..16].copy_from_slice(v4);
    Ipv6Addr(b)
}

/// ── Header IPv4 (20 byte, nessuna opzione) ──────────────────────────

/// Header IPv4 parsato.
#[derive(Clone, Copy, Debug)]
pub struct Ipv4Header {
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub protocol: u8,
    pub total_len: u16,
    pub id: u16,
    pub ttl: u8,
}

/// Parsa l'header IPv4 da `pkt`. Ritorna `None` se troppo corto,
/// versione != 4, o ihl < 5.
pub fn parse_ipv4(pkt: &[u8]) -> Option<(Ipv4Header, &[u8])> {
    if pkt.len() < 20 {
        return None;
    }
    if pkt[0] >> 4 != 4 {
        return None;
    }
    let ihl = (pkt[0] & 0x0f) as usize;
    if ihl < 5 {
        return None;
    }
    let hdr_len = ihl * 4;
    if hdr_len > pkt.len() {
        return None;
    }
    Some((
        Ipv4Header {
            src: [pkt[12], pkt[13], pkt[14], pkt[15]],
            dst: [pkt[16], pkt[17], pkt[18], pkt[19]],
            protocol: pkt[9],
            total_len: u16::from_be_bytes([pkt[2], pkt[3]]),
            id: u16::from_be_bytes([pkt[4], pkt[5]]),
            ttl: pkt[8],
        },
        &pkt[hdr_len..],
    ))
}

/// Scrive l'header IPv4 in `out` (20 byte, checksum a zero).
/// Ritorna i byte scritti (sempre 20).
pub fn write_ipv4(
    out: &mut [u8],
    src: &[u8; 4],
    dst: &[u8; 4],
    protocol: u8,
    total_len: u16,
    ttl: u8,
) -> usize {
    out[0] = 0x45; // version 4, ihl 5
    out[1] = 0;
    out[2..4].copy_from_slice(&total_len.to_be_bytes());
    out[4..6].copy_from_slice(&0u16.to_be_bytes()); // id
    out[6..8].copy_from_slice(&0u16.to_be_bytes()); // flags+fragment
    out[8] = ttl;
    out[9] = protocol;
    out[10..12].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    out[12..16].copy_from_slice(src);
    out[16..20].copy_from_slice(dst);
    20
}

/// Calcola il checksum dell'header IPv4 (campo checksum a zero).
pub fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < header.len() {
        sum += u16::from_be_bytes([header[i], header[i + 1]]) as u32;
        i += 2;
    }
    if i < header.len() {
        sum += (header[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// ── UDP su IPv4 ────────────────────────────────────────────────────

/// Scrive header UDP + payload con checksum su pseudo-header IPv4.
/// `out` deve contenere spazio per 8 + payload.len() byte.
/// Ritorna la lunghezza totale (8 + payload.len()).
pub fn write_udp_ipv4(
    out: &mut [u8],
    src_port: u16,
    dst_port: u16,
    src: &[u8; 4],
    dst: &[u8; 4],
    payload: &[u8],
) -> usize {
    let len = 8 + payload.len();
    out[0..2].copy_from_slice(&src_port.to_be_bytes());
    out[2..4].copy_from_slice(&dst_port.to_be_bytes());
    out[4..6].copy_from_slice(&(len as u16).to_be_bytes());
    out[6..8].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    out[8..8 + payload.len()].copy_from_slice(payload);
    let c = udp_ipv4_checksum(src, dst, len as u16, &out[8..8 + payload.len()]);
    out[6..8].copy_from_slice(&c.to_be_bytes());
    8 + payload.len()
}

/// Calcola il checksum UDP su pseudo-header IPv4.
fn udp_ipv4_checksum(src: &[u8; 4], dst: &[u8; 4], len: u16, payload: &[u8]) -> u16 {
    let mut sum = 0u32;
    sum = checksum::sum16(src, sum);
    sum = checksum::sum16(dst, sum);
    sum += (len as u32 >> 16) & 0xffff;
    sum += len as u32 & 0xffff;
    sum += 17; // next header UDP
    sum = checksum::sum16(payload, sum);
    let mut s = sum;
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

/// ── TCP su IPv4 ────────────────────────────────────────────────────

/// Flag TCP.
pub const FLAG_FIN: u8 = 0x01;
pub const FLAG_SYN: u8 = 0x02;
pub const FLAG_RST: u8 = 0x04;
pub const FLAG_ACK: u8 = 0x10;

/// Scrive header TCP (20 byte, nessuna opzione) + payload con checksum
/// su pseudo-header IPv4. Ritorna la lunghezza totale (20 + payload.len()).
pub fn write_tcp_ipv4(
    out: &mut [u8],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    src: &[u8; 4],
    dst: &[u8; 4],
    payload: &[u8],
) -> usize {
    let hdr_len = 20;
    out[0..2].copy_from_slice(&src_port.to_be_bytes());
    out[2..4].copy_from_slice(&dst_port.to_be_bytes());
    out[4..8].copy_from_slice(&seq.to_be_bytes());
    out[8..12].copy_from_slice(&ack.to_be_bytes());
    out[12] = 5 << 4; // data offset = 5
    out[13] = flags;
    out[14..16].copy_from_slice(&window.to_be_bytes());
    out[16..18].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    out[18..20].copy_from_slice(&0u16.to_be_bytes()); // urgent pointer
    out[20..20 + payload.len()].copy_from_slice(payload);
    let seg_len = hdr_len + payload.len();
    let c = tcp_ipv4_checksum(src, dst, src_port, dst_port, seg_len as u16, &out[hdr_len..]);
    out[16..18].copy_from_slice(&c.to_be_bytes());
    hdr_len + payload.len()
}

/// Calcola il checksum TCP su pseudo-header IPv4.
fn tcp_ipv4_checksum(
    src: &[u8; 4],
    dst: &[u8; 4],
    src_port: u16,
    dst_port: u16,
    len: u16,
    segment: &[u8],
) -> u16 {
    let mut sum = 0u32;
    sum = checksum::sum16(src, sum);
    sum = checksum::sum16(dst, sum);
    sum += (len as u32 >> 16) & 0xffff;
    sum += len as u32 & 0xffff;
    sum += 6; // next header TCP
    sum += src_port as u32;
    sum += dst_port as u32;
    sum += len as u32;
    sum += 0; // checksum field
    sum = checksum::sum16(segment, sum);
    let mut s = sum;
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn siit_address_detection() {
        let siit = Ipv6Addr::from_bytes([
            0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 0, 0, 1,
        ]);
        assert!(is_siit_address(&siit));
        assert_eq!(ipv4_from_siit(&siit).unwrap(), [10, 0, 0, 1]);

        let not_siit = Ipv6Addr::from_bytes([
            0x2a, 0x01, 0x0e, 0x11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]);
        assert!(!is_siit_address(&not_siit));
    }

    #[test]
    fn siit_from_ipv4_roundtrip() {
        let v4 = [192, 168, 1, 1];
        let siit = siit_from_ipv4(&v4);
        assert!(is_siit_address(&siit));
        assert_eq!(ipv4_from_siit(&siit).unwrap(), v4);
    }

    #[test]
    fn parse_ipv4_header() {
        let mut pkt = [0u8; 40];
        pkt[0] = 0x45;
        pkt[2..4].copy_from_slice(&60u16.to_be_bytes());
        pkt[8] = 64;
        pkt[9] = 17; // UDP
        pkt[12..16].copy_from_slice(&[10, 0, 0, 1]);
        pkt[16..20].copy_from_slice(&[10, 0, 0, 2]);
        let c = ipv4_checksum(&pkt[..20]);
        pkt[10..12].copy_from_slice(&c.to_be_bytes());
        let (hdr, _payload) = parse_ipv4(&pkt).unwrap();
        assert_eq!(hdr.src, [10, 0, 0, 1]);
        assert_eq!(hdr.dst, [10, 0, 0, 2]);
        assert_eq!(hdr.protocol, 17);
        assert_eq!(hdr.ttl, 64);
    }

    #[test]
    fn udp_ipv4_checksum_roundtrip() {
        let mut out = [0u8; 8 + 6];
        let len = write_udp_ipv4(
            &mut out,
            12345,
            53,
            &[10, 0, 0, 1],
            &[10, 0, 0, 2],
            b"hello",
        );
        assert_eq!(len, 14);
        let c = udp_ipv4_checksum(&[10, 0, 0, 1], &[10, 0, 0, 2], len as u16, &out[8..]);
        assert_eq!(c, u16::from_be_bytes([out[6], out[7]]));
        assert_ne!(c, 0);
    }

    #[test]
    fn tcp_ipv4_checksum_roundtrip() {
        let mut out = [0u8; 20 + 4];
        let len = write_tcp_ipv4(
            &mut out,
            12345,
            80,
            1000,
            2000,
            FLAG_SYN,
            65535,
            &[10, 0, 0, 1],
            &[10, 0, 0, 2],
            b"abcd",
        );
        assert_eq!(len, 24);
        let c = tcp_ipv4_checksum(
            &[10, 0, 0, 1],
            &[10, 0, 0, 2],
            12345,
            80,
            len as u16,
            &out[20..],
        );
        assert_eq!(c, u16::from_be_bytes([out[16], out[17]]));
        assert_ne!(c, 0);
    }
}
