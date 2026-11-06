//! NDP (RFC 4861): RS/RA/NS/NA e opzioni. Solo le opzioni necessarie a
//! SLAAC e alla risoluzione del router; redirect e opzioni sconosciute
//! vengono ignorati (ADR-0068 I8).

use crate::addr::{Ipv6Addr, MacAddr};
use crate::checksum;

pub const OPT_SOURCE_LL: u8 = 1;
pub const OPT_TARGET_LL: u8 = 2;
pub const OPT_PREFIX_INFO: u8 = 3;
pub const OPT_MTU: u8 = 5;

/// Informazioni estratte da un Router Advertisement.
#[derive(Clone, Copy, Debug, Default)]
pub struct RaInfo {
    pub cur_hop_limit: u8,
    pub flags: u8,
    pub router_lifetime: u16,
    pub reachable: u32,
    pub retrans: u32,
    pub src_ll: Option<MacAddr>,
    /// `(primi 8 byte del prefisso, prefix length)`.
    pub prefix: Option<([u8; 8], u8)>,
    pub mtu: Option<u32>,
}

fn opt_mac(opt: &[u8]) -> Option<MacAddr> {
    if opt.len() < 8 {
        return None;
    }
    let mut m = [0u8; 6];
    m.copy_from_slice(&opt[2..8]);
    Some(MacAddr(m))
}

/// Itera le opzioni NDP: `(tipo, dati)` con `dati` dopo type/len.
fn for_each_opt(mut body: &[u8], mut f: impl FnMut(u8, &[u8])) {
    while body.len() >= 2 {
        let typ = body[0];
        let units = body[1] as usize;
        if units == 0 {
            break;
        }
        let len = units * 8;
        if len > body.len() {
            break;
        }
        f(typ, &body[2..len]);
        body = &body[len..];
    }
}

/// Parsa il corpo di un RA (dopo type/code/checksum).
pub fn parse_ra(body: &[u8]) -> Option<RaInfo> {
    if body.len() < 12 {
        return None;
    }
    let mut ra = RaInfo {
        cur_hop_limit: body[0],
        flags: body[1],
        router_lifetime: u16::from_be_bytes([body[2], body[3]]),
        reachable: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
        retrans: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
        ..RaInfo::default()
    };
    for_each_opt(&body[12..], |typ, data| match typ {
        OPT_SOURCE_LL => {
            if let Some(m) = opt_mac(data) {
                ra.src_ll = Some(m);
            }
        }
        OPT_PREFIX_INFO => {
            if data.len() >= 30 {
                let prefix_len = data[0];
                let mut p = [0u8; 8];
                p.copy_from_slice(&data[14..22]);
                ra.prefix = Some((p, prefix_len));
            }
        }
        OPT_MTU => {
            if data.len() >= 6 {
                ra.mtu = Some(u32::from_be_bytes([
                    data[2], data[3], data[4], data[5],
                ]));
            }
        }
        _ => {}
    });
    Some(ra)
}

/// Parsa un NS: `(target, source link-layer se presente)`.
pub fn parse_ns(body: &[u8]) -> Option<(Ipv6Addr, Option<MacAddr>)> {
    if body.len() < 20 {
        return None;
    }
    let mut t = [0u8; 16];
    t.copy_from_slice(&body[4..20]);
    let mut ll = None;
    for_each_opt(&body[20..], |typ, data| {
        if typ == OPT_SOURCE_LL {
            if let Some(m) = opt_mac(data) {
                ll = Some(m);
            }
        }
    });
    Some((Ipv6Addr(t), ll))
}

/// Parsa un NA: `(target, flags, target link-layer se presente)`.
pub fn parse_na(body: &[u8]) -> Option<(Ipv6Addr, u8, Option<MacAddr>)> {
    if body.len() < 20 {
        return None;
    }
    let flags = body[0];
    let mut t = [0u8; 16];
    t.copy_from_slice(&body[4..20]);
    let mut ll = None;
    for_each_opt(&body[20..], |typ, data| {
        if typ == OPT_TARGET_LL {
            if let Some(m) = opt_mac(data) {
                ll = Some(m);
            }
        }
    });
    Some((Ipv6Addr(t), flags, ll))
}

fn finish_checksum(out: &mut [u8], len: usize, src: &Ipv6Addr, dst: &Ipv6Addr) {
    out[2] = 0;
    out[3] = 0;
    let c = checksum::icmpv6(src, dst, &out[..len]);
    out[2..4].copy_from_slice(&c.to_be_bytes());
}

/// Router Solicitation con source link-layer address.
pub fn write_rs(out: &mut [u8], src_ll: MacAddr, src: &Ipv6Addr, dst: &Ipv6Addr) -> usize {
    let len = 8 + 8;
    out[0] = 133;
    out[1] = 0;
    out[2] = 0;
    out[3] = 0;
    out[4..8].copy_from_slice(&[0, 0, 0, 0]);
    out[8] = OPT_SOURCE_LL;
    out[9] = 1;
    out[10..16].copy_from_slice(&src_ll.0);
    finish_checksum(out, len, src, dst);
    len
}

/// Neighbor Solicitation per `target`.
pub fn write_ns(
    out: &mut [u8],
    target: Ipv6Addr,
    src_ll: MacAddr,
    src: &Ipv6Addr,
    dst: &Ipv6Addr,
) -> usize {
    let len = 8 + 16 + 8;
    out[0] = 135;
    out[1] = 0;
    out[2] = 0;
    out[3] = 0;
    out[4..8].copy_from_slice(&[0, 0, 0, 0]);
    out[8..24].copy_from_slice(&target.0);
    out[24] = OPT_SOURCE_LL;
    out[25] = 1;
    out[26..32].copy_from_slice(&src_ll.0);
    finish_checksum(out, len, src, dst);
    len
}

/// Neighbor Advertisement (flags: 0x40 solicited, 0x20 override).
pub fn write_na(
    out: &mut [u8],
    target: Ipv6Addr,
    flags: u8,
    tgt_ll: MacAddr,
    src: &Ipv6Addr,
    dst: &Ipv6Addr,
) -> usize {
    let len = 8 + 16 + 8;
    out[0] = 136;
    out[1] = 0;
    out[2] = 0;
    out[3] = 0;
    out[4] = flags;
    out[5] = 0;
    out[6] = 0;
    out[7] = 0;
    out[8..24].copy_from_slice(&target.0);
    out[24] = OPT_TARGET_LL;
    out[25] = 1;
    out[26..32].copy_from_slice(&tgt_ll.0);
    finish_checksum(out, len, src, dst);
    len
}
