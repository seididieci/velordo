//! Checksum Internet (one's complement) e pseudo-header ICMPv6.

use crate::addr::Ipv6Addr;

/// Somma a 16 bit (big-endian) di `data`, accumulata in `sum`.
pub fn sum16(data: &[u8], mut sum: u32) -> u32 {
    let mut i = 0usize;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    sum
}

/// Ripiega i carry e complementa a uno.
pub fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Checksum ICMPv6 (RFC 4443) con pseudo-header IPv6.
///
/// `payload` deve avere il campo checksum a zero.
pub fn icmpv6(src: &Ipv6Addr, dst: &Ipv6Addr, payload: &[u8]) -> u16 {
    let mut sum = 0u32;
    sum = sum16(&src.0, sum);
    sum = sum16(&dst.0, sum);
    let len = payload.len() as u32;
    sum += (len >> 16) & 0xffff;
    sum += len & 0xffff;
    sum += 58; // next header ICMPv6
    sum = sum16(payload, sum);
    fold(sum)
}
