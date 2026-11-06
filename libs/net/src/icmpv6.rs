//! ICMPv6: echo, errori e checksum (RFC 4443).

use crate::addr::Ipv6Addr;
use crate::checksum;

pub const TYPE_ECHO_REQUEST: u8 = 128;
pub const TYPE_ECHO_REPLY: u8 = 129;
pub const TYPE_RS: u8 = 133;
pub const TYPE_RA: u8 = 134;
pub const TYPE_NS: u8 = 135;
pub const TYPE_NA: u8 = 136;
pub const TYPE_REDIRECT: u8 = 137;

#[derive(Clone, Copy, Debug)]
pub struct Icmpv6<'a> {
    pub typ: u8,
    pub code: u8,
    /// Corpo dopo i primi 4 byte (type/code/checksum).
    pub body: &'a [u8],
}

pub fn parse(pkt: &[u8]) -> Option<Icmpv6<'_>> {
    if pkt.len() < 4 {
        return None;
    }
    Some(Icmpv6 {
        typ: pkt[0],
        code: pkt[1],
        body: &pkt[4..],
    })
}

/// Verifica il checksum ICMPv6 (pseudo-header + messaggio).
pub fn checksum_ok(src: &Ipv6Addr, dst: &Ipv6Addr, pkt: &[u8]) -> bool {
    if pkt.len() < 4 {
        return false;
    }
    let mut sum = 0u32;
    sum = checksum::sum16(&src.0, sum);
    sum = checksum::sum16(&dst.0, sum);
    let len = pkt.len() as u32;
    sum += (len >> 16) & 0xffff;
    sum += len & 0xffff;
    sum += 58;
    sum = checksum::sum16(pkt, sum);
    checksum::fold(sum) == 0
}

/// Scrive un echo request/reply; ritorna la lunghezza.
pub fn write_echo(
    out: &mut [u8],
    typ: u8,
    id: u16,
    seq: u16,
    data: &[u8],
    src: &Ipv6Addr,
    dst: &Ipv6Addr,
) -> usize {
    let len = 8 + data.len();
    out[0] = typ;
    out[1] = 0;
    out[2] = 0;
    out[3] = 0;
    out[4..6].copy_from_slice(&id.to_be_bytes());
    out[6..8].copy_from_slice(&seq.to_be_bytes());
    out[8..8 + data.len()].copy_from_slice(data);
    let c = checksum::icmpv6(src, dst, &out[..len]);
    out[2..4].copy_from_slice(&c.to_be_bytes());
    len
}
