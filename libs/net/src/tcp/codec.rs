//! TCP: header, parse/write e checksum su pseudo-header IPv6 (RFC 793, RFC 2460 §8.1).
//!
//! Puro: nessun I/O, nessuna allocazione, nessun panic su input arbitrario.
//! Versione minimale N3: handshake a 3 vie, SYN cookie, FIN/ACK, nessuna
//! retransmission complessa (timeout fisso), finestra fissa.

use crate::addr::Ipv6Addr;
use crate::checksum;
use crate::ipv6::NEXT_TCP;

/// Header TCP: 20 byte (senza opzioni).
pub const TCP_HDR: usize = 20;

/// Flag TCP.
pub const FLAG_FIN: u8 = 0x01;
pub const FLAG_SYN: u8 = 0x02;
pub const FLAG_RST: u8 = 0x04;
pub const FLAG_ACK: u8 = 0x10;

/// Header TCP parsato.
#[derive(Clone, Copy, Debug)]
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub checksum: u16,
    /// Lunghezza del segmento (header + payload).
    pub len: u16,
}

/// Parsa l'header TCP da `pkt`. Ritorna `None` se troppo corto o len inconsistente.
pub fn parse(pkt: &[u8]) -> Option<(TcpHeader, &[u8])> {
    if pkt.len() < TCP_HDR {
        return None;
    }
    let src_port = u16::from_be_bytes([pkt[0], pkt[1]]);
    let dst_port = u16::from_be_bytes([pkt[2], pkt[3]]);
    let seq = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
    let ack = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
    let data_offset = (pkt[12] >> 4) as usize;
    let flags = pkt[13];
    let window = u16::from_be_bytes([pkt[14], pkt[15]]);
    let checksum = u16::from_be_bytes([pkt[16], pkt[17]]);
    let _urgent = u16::from_be_bytes([pkt[18], pkt[19]]);
    if data_offset < 5 {
        return None;
    }
    let hdr_len = data_offset * 4;
    if hdr_len > pkt.len() {
        return None;
    }
    Some((
        TcpHeader {
            src_port,
            dst_port,
            seq,
            ack,
            flags,
            window,
            checksum,
            len: hdr_len as u16,
        },
        &pkt[hdr_len..],
    ))
}

/// Scrive l'header TCP in `out` (checksum a zero se `checksum == 0`).
/// Ritorna i byte scritti (sempre TCP_HDR per header senza opzioni).
pub fn write(
    out: &mut [u8],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    checksum: u16,
) -> usize {
    out[0..2].copy_from_slice(&src_port.to_be_bytes());
    out[2..4].copy_from_slice(&dst_port.to_be_bytes());
    out[4..8].copy_from_slice(&seq.to_be_bytes());
    out[8..12].copy_from_slice(&ack.to_be_bytes());
    out[12] = 5 << 4; // data offset = 5 (20 byte), nessuna opzione
    out[13] = flags;
    out[14..16].copy_from_slice(&window.to_be_bytes());
    out[16..18].copy_from_slice(&checksum.to_be_bytes());
    out[18..20].copy_from_slice(&0u16.to_be_bytes()); // urgent pointer = 0
    TCP_HDR
}

/// Scrive l'header TCP con checksum calcolato sul pseudo-header IPv6.
/// `payload` è il payload TCP (senza header). Ritorna la lunghezza totale.
pub fn write_checked(
    out: &mut [u8],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    src: &Ipv6Addr,
    dst: &Ipv6Addr,
    payload: &[u8],
) -> usize {
    let hdr_len = write(out, src_port, dst_port, seq, ack, flags, window, 0);
    out[hdr_len..hdr_len + payload.len()].copy_from_slice(payload);
    let seg_len = (hdr_len + payload.len()) as u16;
    // Il checksum TCP copre pseudo-header IPv6 + intero segmento TCP
    // (header con checksum a zero + payload).
    let c = checksum_tcp(src, dst, &out[..hdr_len + payload.len()]);
    out[16..18].copy_from_slice(&c.to_be_bytes());
    hdr_len + payload.len()
}

/// Verifica il checksum TCP su un pacchetto già parsato (header + payload in `pkt`).
pub fn checksum_ok(src: &Ipv6Addr, dst: &Ipv6Addr, pkt: &[u8]) -> bool {
    if pkt.len() < TCP_HDR {
        return false;
    }
    let header = match parse(pkt) {
        Some((h, _)) => h,
        None => return false,
    };
    // Checksum obbligatorio: zero = invalido.
    if header.checksum == 0 {
        return false;
    }
    checksum::fold(
        checksum::sum16(&src.0, 0)
            + checksum::sum16(&dst.0, 0)
            + ((pkt.len() as u32 >> 16) & 0xffff)
            + (pkt.len() as u32 & 0xffff)
            + NEXT_TCP as u32
            + checksum::sum16(pkt, 0),
    ) == 0
}

/// Calcola il checksum TCP da inserire nell'header.
/// `segment` è l'intero segmento TCP (header con checksum a zero + payload).
pub fn checksum_tcp(src: &Ipv6Addr, dst: &Ipv6Addr, segment: &[u8]) -> u16 {
    let mut sum = 0u32;
    // Pseudo-header IPv6.
    sum = checksum::sum16(&src.0, sum);
    sum = checksum::sum16(&dst.0, sum);
    let len = segment.len() as u32;
    sum += (len >> 16) & 0xffff;
    sum += len & 0xffff;
    sum += NEXT_TCP as u32;
    // Segmento TCP (header con checksum a zero + payload).
    sum = checksum::sum16(segment, sum);
    checksum::fold(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn make_seg(src_port: u16, dst_port: u16, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = alloc::vec![0u8; TCP_HDR + payload.len()];
        let total = write_checked(
            &mut out,
            src_port,
            dst_port,
            1000,
            2000,
            flags,
            65535,
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            payload,
        );
        assert_eq!(total, TCP_HDR + payload.len());
        out
    }

    #[test]
    fn roundtrip_parse_write() {
        let payload = b"hello tcp";
        let seg = make_seg(12345, 7, FLAG_SYN | FLAG_ACK, payload);
        let (hdr, body) = parse(&seg).expect("parse");
        assert_eq!(hdr.src_port, 12345);
        assert_eq!(hdr.dst_port, 7);
        assert_eq!(hdr.seq, 1000);
        assert_eq!(hdr.ack, 2000);
        assert_eq!(hdr.flags, FLAG_SYN | FLAG_ACK);
        assert_ne!(hdr.checksum, 0, "checksum deve essere non-zero");
        assert_eq!(body.len(), payload.len(), "body length mismatch");
        assert!(body == &payload[..], "body content mismatch");
        assert!(checksum_ok(
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            &seg
        ), "checksum valido");
    }

    #[test]
    fn zero_checksum_invalido() {
        let payload = b"x";
        let mut seg = make_seg(12345, 7, FLAG_SYN, payload);
        seg[16..18].copy_from_slice(&0u16.to_be_bytes());
        assert!(!checksum_ok(
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            &seg
        ), "checksum zero deve essere rifiutato");
    }

    #[test]
    fn payload_alterato_fail_checksum() {
        let payload = b"original";
        let mut seg = make_seg(12345, 7, FLAG_SYN | FLAG_ACK, payload);
        seg[TCP_HDR] ^= 0xff;
        assert!(!checksum_ok(
            &Ipv6Addr::LOOPBACK,
            &Ipv6Addr::LOOPBACK,
            &seg
        ), "payload corrotto deve fallire");
    }

    #[test]
    fn too_short_rejected() {
        assert!(parse(&[0u8; 19]).is_none());
    }

    #[test]
    fn fin_flag_parsed() {
        let seg = make_seg(12345, 7, FLAG_FIN, b"");
        let (hdr, _) = parse(&seg).expect("parse");
        assert_eq!(hdr.flags, FLAG_FIN);
    }

    #[test]
    fn rst_flag_parsed() {
        let seg = make_seg(12345, 7, FLAG_RST, b"");
        let (hdr, _) = parse(&seg).expect("parse");
        assert_eq!(hdr.flags, FLAG_RST);
    }
}
