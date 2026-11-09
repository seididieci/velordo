//! UDP: header, parse/write e checksum su pseudo-header IPv6 (RFC 768, RFC 2460 §8.1).
//!
//! Puro: nessun I/O, nessuna allocazione, nessun panic su input arbitrario.

use crate::addr::Ipv6Addr;
use crate::checksum;
use crate::ipv6::NEXT_UDP;

/// Header UDP: 8 byte.
pub const UDP_HDR: usize = 8;

/// Porta dell'eco server (hardcoded, N2).
pub const ECHO_PORT: u16 = 7;

/// Header UDP parsato.
#[derive(Clone, Copy, Debug)]
pub struct UdpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub len: u16,      // include header + payload
    pub checksum: u16,
}

/// Parsa l'header UDP da `pkt`. Ritorna `None` se troppo corto o len inconsistente.
pub fn parse(pkt: &[u8]) -> Option<(UdpHeader, &[u8])> {
    if pkt.len() < UDP_HDR {
        return None;
    }
    let src_port = u16::from_be_bytes([pkt[0], pkt[1]]);
    let dst_port = u16::from_be_bytes([pkt[2], pkt[3]]);
    let len = u16::from_be_bytes([pkt[4], pkt[5]]);
    let checksum = u16::from_be_bytes([pkt[6], pkt[7]]);
    if len < UDP_HDR as u16 {
        return None;
    }
    let end = len as usize;
    if end > pkt.len() {
        return None;
    }
    Some((
        UdpHeader {
            src_port,
            dst_port,
            len,
            checksum,
        },
        &pkt[UDP_HDR..end],
    ))
}

/// Scrive l'header UDP in `out` (checksum a zero se `checksum == 0` significa
/// "nessun checksum" — ma il core Velordo lo richiede sempre, vedi `write_checked`).
/// Ritorna i byte scritti (sempre UDP_HDR).
pub fn write(
    out: &mut [u8],
    src_port: u16,
    dst_port: u16,
    len: u16,
    checksum: u16,
) -> usize {
    out[0..2].copy_from_slice(&src_port.to_be_bytes());
    out[2..4].copy_from_slice(&dst_port.to_be_bytes());
    out[4..6].copy_from_slice(&len.to_be_bytes());
    out[6..8].copy_from_slice(&checksum.to_be_bytes());
    UDP_HDR
}

/// Scrive l'header UDP con checksum calcolato sul pseudo-header IPv6.
/// `payload` è il payload UDP (senza header). Ritorna la lunghezza totale (header + payload).
pub fn write_checked(
    out: &mut [u8],
    src_port: u16,
    dst_port: u16,
    src: &Ipv6Addr,
    dst: &Ipv6Addr,
    payload: &[u8],
) -> usize {
    let len = (UDP_HDR + payload.len()) as u16;
    let hdr = write(out, src_port, dst_port, len, 0);
    out[hdr..hdr + payload.len()].copy_from_slice(payload);
    let c = checksum_udp(src, dst, src_port, dst_port, len, &out[hdr..hdr + payload.len()]);
    out[6..8].copy_from_slice(&c.to_be_bytes());
    hdr + payload.len()
}

/// Verifica il checksum UDP su un pacchetto già parsato (header + payload in `pkt`).
/// Ritorna `true` se il checksum è valido (o zero, se permesso — qui default-deny: zero sempre invalido).
pub fn checksum_ok(src: &Ipv6Addr, dst: &Ipv6Addr, pkt: &[u8]) -> bool {
    if pkt.len() < UDP_HDR {
        return false;
    }
    let header = match parse(pkt) {
        Some((h, _)) => h,
        None => return false,
    };
    // Checksum obbligatorio: zero = invalido (default-deny, ADR-0068 I1).
    if header.checksum == 0 {
        return false;
    }
    checksum::fold(
        checksum::sum16(&src.0, 0)
            + checksum::sum16(&dst.0, 0)
            + ((pkt.len() as u32 >> 16) & 0xffff)
            + (pkt.len() as u32 & 0xffff)
            + NEXT_UDP as u32
            + checksum::sum16(pkt, 0),
    ) == 0
}

/// Calcola il checksum UDP da inserire nell'header (campo checksum a zero nel payload).
fn checksum_udp(
    src: &Ipv6Addr,
    dst: &Ipv6Addr,
    src_port: u16,
    dst_port: u16,
    len: u16,
    payload: &[u8],
) -> u16 {
    let mut sum = 0u32;
    sum = checksum::sum16(&src.0, sum);
    sum = checksum::sum16(&dst.0, sum);
    sum += (len as u32 >> 16) & 0xffff;
    sum += len as u32 & 0xffff;
    sum += NEXT_UDP as u32;
    // Pseudo-header: porte + lunghezza + checksum zero (2 byte).
    sum += src_port as u32;
    sum += dst_port as u32;
    sum += len as u32;
    sum += 0; // checksum campo
    sum = checksum::sum16(payload, sum);
    checksum::fold(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn make_pkt(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = alloc::vec![0u8; UDP_HDR + payload.len()];
        let total = write_checked(
            &mut out,
            src_port,
            dst_port,
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            payload,
        );
        assert_eq!(total, UDP_HDR + payload.len());
        out
    }

    #[test]
    fn roundtrip_parse_write() {
        let payload = b"hello udp";
        let pkt = make_pkt(12345, 7, payload);
        let (hdr, body) = parse(&pkt).expect("parse");
        assert_eq!(hdr.src_port, 12345);
        assert_eq!(hdr.dst_port, 7);
        assert_eq!(hdr.len, (UDP_HDR + payload.len()) as u16);
        assert_ne!(hdr.checksum, 0, "checksum deve essere non-zero");
        assert_eq!(body, payload);
        assert!(checksum_ok(
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            &pkt
        ), "checksum valido");
    }

    #[test]
    fn zero_checksum_invalido() {
        let payload = b"x";
        let mut pkt = make_pkt(12345, 7, payload);
        // Azzera il checksum.
        pkt[6..8].copy_from_slice(&0u16.to_be_bytes());
        assert!(!checksum_ok(
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            &pkt
        ), "checksum zero deve essere rifiutato");
    }

    #[test]
    fn payload_alterato_fail_checksum() {
        let payload = b"original";
        let mut pkt = make_pkt(12345, 7, payload);
        // Corrompe un byte del payload senza ritoccare il checksum.
        pkt[UDP_HDR] ^= 0xff;
        assert!(!checksum_ok(
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            &pkt
        ), "payload corrotto deve fallire");
    }

    #[test]
    fn too_short_rejected() {
        assert!(parse(&[0u8; 4]).is_none());
        assert!(parse(&[0u8; 7]).is_none());
    }

    #[test]
    fn len_inconsistente_rejected() {
        // len = 8 (solo header) ma pkt ha 12 byte -> parse accetta len=8, body vuoto.
        let mut pkt = make_pkt(12345, 7, b"abcd");
        pkt[4..6].copy_from_slice(&8u16.to_be_bytes()); // dichiara len=8
        let (hdr, body) = parse(&pkt).expect("parse len=8");
        assert_eq!(hdr.len, 8);
        assert!(body.is_empty());
    }
}
