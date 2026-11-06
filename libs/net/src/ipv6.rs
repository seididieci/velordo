//! IPv6: header fisso e tipi di next header (ADR-0069).
//!
//! Niente extension header nella prima versione: hop-by-hop, routing,
//! fragment e destination options vengono **droppati** (ADR-0068 I5).

use crate::addr::Ipv6Addr;

pub const IPV6_HDR: usize = 40;
pub const NEXT_ICMPV6: u8 = 58;
pub const NEXT_NONE: u8 = 59;
pub const HOP_LIMIT: u8 = 64;

#[derive(Clone, Copy, Debug)]
pub struct Ipv6Header {
    pub payload_len: u16,
    pub next_header: u8,
    pub hop_limit: u8,
    pub src: Ipv6Addr,
    pub dst: Ipv6Addr,
}

/// Parsa l'header IPv6; `None` se malformato o con extension header.
pub fn parse(pkt: &[u8]) -> Option<(Ipv6Header, &[u8])> {
    if pkt.len() < IPV6_HDR {
        return None;
    }
    if pkt[0] >> 4 != 6 {
        return None;
    }
    let payload_len = u16::from_be_bytes([pkt[4], pkt[5]]);
    let next_header = pkt[6];
    let hop_limit = pkt[7];
    // Extension header: fuori scope N1 (drop, mai parsing ricorsivo).
    if matches!(next_header, 0 | 43 | 44 | 50 | 51 | 60) {
        return None;
    }
    let mut src = [0u8; 16];
    let mut dst = [0u8; 16];
    src.copy_from_slice(&pkt[8..24]);
    dst.copy_from_slice(&pkt[24..40]);
    let end = IPV6_HDR.checked_add(payload_len as usize)?;
    if end > pkt.len() {
        return None;
    }
    Some((
        Ipv6Header {
            payload_len,
            next_header,
            hop_limit,
            src: Ipv6Addr(src),
            dst: Ipv6Addr(dst),
        },
        &pkt[IPV6_HDR..end],
    ))
}

/// Scrive l'header IPv6 in `out`; ritorna i byte scritti.
pub fn write(
    out: &mut [u8],
    next: u8,
    hop: u8,
    src: Ipv6Addr,
    dst: Ipv6Addr,
    payload_len: u16,
) -> usize {
    out[0] = 0x60;
    out[1] = 0;
    out[2] = 0;
    out[3] = 0;
    out[4..6].copy_from_slice(&payload_len.to_be_bytes());
    out[6] = next;
    out[7] = hop;
    out[8..24].copy_from_slice(&src.0);
    out[24..40].copy_from_slice(&dst.0);
    IPV6_HDR
}
