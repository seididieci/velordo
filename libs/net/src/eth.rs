//! Ethernet II: header e tipi (ADR-0069).

use crate::addr::MacAddr;

pub const ETHERTYPE_IPV6: u16 = 0x86dd;
pub const ETH_HDR: usize = 14;

#[derive(Clone, Copy, Debug)]
pub struct EthHeader {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub ethertype: u16,
}

/// Parsa l'header Ethernet; ritorna `(header, payload)`.
pub fn parse(frame: &[u8]) -> Option<(EthHeader, &[u8])> {
    if frame.len() < ETH_HDR {
        return None;
    }
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&frame[0..6]);
    src.copy_from_slice(&frame[6..12]);
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    Some((
        EthHeader {
            dst: MacAddr(dst),
            src: MacAddr(src),
            ethertype,
        },
        &frame[ETH_HDR..],
    ))
}

/// Scrive l'header Ethernet in `out`; ritorna i byte scritti.
pub fn write(out: &mut [u8], dst: MacAddr, src: MacAddr, ethertype: u16) -> usize {
    out[0..6].copy_from_slice(&dst.0);
    out[6..12].copy_from_slice(&src.0);
    out[12..14].copy_from_slice(&ethertype.to_be_bytes());
    ETH_HDR
}
