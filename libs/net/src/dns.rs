//! DNS: header, parse/write di pacchetti DNS (RFC 1035).
//!
//! Puro: nessun I/O, nessuna allocazione per pacchetto, nessun panic su input
//! arbitrario. Supporta query A (IPv4) e AAAA (IPv6), e parsing di risposte
//! con record A e AAAA.

extern crate alloc;

use alloc::vec::Vec;

/// Tipi di record DNS.
pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;

/// Classe Internet.
pub const CLASS_IN: u16 = 1;

/// Flag DNS.
pub const FLAG_QR: u16 = 0x8000; // Response
pub const FLAG_RD: u16 = 0x0100; // Recursion Desired
pub const FLAG_RA: u16 = 0x0080; // Recursion Available
pub const FLAG_TC: u16 = 0x0200; // Truncated
pub const FLAG_RCODE_MASK: u16 = 0x000F;

/// Header DNS: 12 byte.
pub const DNS_HEADER_LEN: usize = 12;

/// Lunghezza massima di un nome di dominio.
pub const DNS_MAX_NAME_LEN: usize = 255;

/// Lunghezza massima di un pacchetto DNS UDP.
pub const DNS_MAX_PACKET_LEN: usize = 512;

/// Header DNS parsato.
#[derive(Clone, Copy, Debug)]
pub struct DNS_HEADER {
    pub id: u16,
    pub flags: u16,
    pub qdcount: u16,
    pub ancount: u16,
    pub nscount: u16,
    pub arcount: u16,
}

/// Scrive l'header DNS in `out`. Ritorna i byte scritti.
pub fn write_header(
    out: &mut [u8],
    id: u16,
    flags: u16,
    qdcount: u16,
    ancount: u16,
    nscount: u16,
    arcount: u16,
) -> usize {
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[4..6].copy_from_slice(&qdcount.to_be_bytes());
    out[6..8].copy_from_slice(&ancount.to_be_bytes());
    out[8..10].copy_from_slice(&nscount.to_be_bytes());
    out[10..12].copy_from_slice(&arcount.to_be_bytes());
    DNS_HEADER_LEN
}

/// Parsa l'header DNS da `pkt`. Ritorna `None` se troppo corto.
pub fn parse_header(pkt: &[u8]) -> Option<DNS_HEADER> {
    if pkt.len() < DNS_HEADER_LEN {
        return None;
    }
    Some(DNS_HEADER {
        id: u16::from_be_bytes([pkt[0], pkt[1]]),
        flags: u16::from_be_bytes([pkt[2], pkt[3]]),
        qdcount: u16::from_be_bytes([pkt[4], pkt[5]]),
        ancount: u16::from_be_bytes([pkt[6], pkt[7]]),
        nscount: u16::from_be_bytes([pkt[8], pkt[9]]),
        arcount: u16::from_be_bytes([pkt[10], pkt[11]]),
    })
}

/// Codifica un nome di dominio nel formato DNS (etichette con lunghezza-prefisso).
/// Ritorna il numero di byte scritti.
pub fn encode_name(out: &mut [u8], name: &str) -> Option<usize> {
    let mut off = 0usize;
    for label in name.split('.') {
        if label.is_empty() {
            return None;
        }
        if off + 1 + label.len() > out.len() {
            return None;
        }
        out[off] = label.len() as u8;
        off += 1;
        out[off..off + label.len()].copy_from_slice(label.as_bytes());
        off += label.len();
    }
    if off + 1 > out.len() {
        return None;
    }
    out[off] = 0;
    off += 1;
    Some(off)
}

/// Decodifica un nome di dominio dal formato DNS.
/// Ritorna (nome come Vec<u8>, nuovo offset).
pub fn decode_name(pkt: &[u8], mut off: usize) -> Option<(Vec<u8>, usize)> {
    let mut labels = Vec::new();
    let mut jumped = false;
    let mut max_loops = 0usize;

    loop {
        if max_loops > DNS_MAX_NAME_LEN {
            return None;
        }
        max_loops += 1;

        if off >= pkt.len() {
            return None;
        }
        let len = pkt[off] as usize;
        if len == 0 {
            off += 1;
            break;
        }
        if len & 0xC0 != 0 {
            if off + 1 >= pkt.len() {
                return None;
            }
            jumped = true;
            off = ((len & 0x3F) << 8) | (pkt[off + 1] as usize);
            continue;
        }
        if off + 1 + len > pkt.len() {
            return None;
        }
        labels.push(&pkt[off + 1..off + 1 + len]);
        off += 1 + len;
    }

    // Concatena i label con punti.
    let mut result = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            result.push(b'.');
        }
        result.extend_from_slice(label);
    }
    Some((result, off))
}

/// Codifica una domanda DNS completa (nome + tipo + classe).
pub fn encode_question(
    out: &mut [u8],
    name: &str,
    qtype: u16,
    qclass: u16,
) -> Option<usize> {
    let name_len = encode_name(out, name)?;
    if out.len() < name_len + 4 {
        return None;
    }
    out[name_len..name_len + 2].copy_from_slice(&qtype.to_be_bytes());
    out[name_len + 2..name_len + 4].copy_from_slice(&qclass.to_be_bytes());
    Some(name_len + 4)
}

/// Decodifica una domanda DNS dal pacchetto.
/// Ritorna (nome come Vec<u8>, tipo, classe, nuovo offset).
pub fn decode_question(pkt: &[u8], off: usize) -> Option<(Vec<u8>, u16, u16, usize)> {
    let mut off = off;
    let mut name_bytes = Vec::new();
    loop {
        if off >= pkt.len() {
            return None;
        }
        let len = pkt[off] as usize;
        if len == 0 {
            off += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // puntatori non supportati in questo parsing semplice
        }
        if off + 1 + len > pkt.len() {
            return None;
        }
        name_bytes.extend_from_slice(&pkt[off + 1..off + 1 + len]);
        name_bytes.push(b'.');
        off += 1 + len;
    }
    if name_bytes.is_empty() {
        return None;
    }
    name_bytes.pop(); // rimuovi l'ultimo punto
    if off + 4 > pkt.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([pkt[off], pkt[off + 1]]);
    let qclass = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]);
    off += 4;
    Some((name_bytes, qtype, qclass, off))
}

/// Costruisce una query DNS completa in `out`.
pub fn build_query(
    out: &mut [u8],
    id: u16,
    name: &str,
    qtype: u16,
) -> Option<usize> {
    let mut off = write_header(out, id, FLAG_RD, 1, 0, 0, 0);
    off += encode_question(&mut out[off..], name, qtype, CLASS_IN)?;
    Some(off)
}

/// Decodifica un record A da `pkt` partendo da `off`.
pub fn decode_rr_a(pkt: &[u8], mut off: usize) -> Option<([u8; 4], usize)> {
    // Salta il nome (assume già parsito o semplice).
    // Per semplicità, salta il nome leggendo etichette.
    loop {
        if off >= pkt.len() {
            return None;
        }
        let len = pkt[off] as usize;
        if len == 0 {
            off += 1;
            break;
        }
        if len & 0xC0 != 0 {
            off += 2;
            break;
        }
        off += 1 + len;
    }
    if off + 10 > pkt.len() {
        return None;
    }
    let rrtype = u16::from_be_bytes([pkt[off], pkt[off + 1]]);
    let rrclass = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]);
    let _ttl = u32::from_be_bytes([pkt[off + 4], pkt[off + 5], pkt[off + 6], pkt[off + 7]]);
    let rdlength = u16::from_be_bytes([pkt[off + 8], pkt[off + 9]]);
    off += 10;
    if rrtype != TYPE_A || rrclass != CLASS_IN || rdlength != 4 {
        if off + rdlength as usize > pkt.len() {
            return None;
        }
        off += rdlength as usize;
        return None;
    }
    let mut rdata = [0u8; 4];
    rdata.copy_from_slice(&pkt[off..off + 4]);
    off += 4;
    Some((rdata, off))
}

/// Decodifica un record AAAA da `pkt` partendo da `off`.
pub fn decode_rr_aaaa(pkt: &[u8], mut off: usize) -> Option<([u8; 16], usize)> {
    loop {
        if off >= pkt.len() {
            return None;
        }
        let len = pkt[off] as usize;
        if len == 0 {
            off += 1;
            break;
        }
        if len & 0xC0 != 0 {
            off += 2;
            break;
        }
        off += 1 + len;
    }
    if off + 10 > pkt.len() {
        return None;
    }
    let rrtype = u16::from_be_bytes([pkt[off], pkt[off + 1]]);
    let rrclass = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]);
    let _ttl = u32::from_be_bytes([pkt[off + 4], pkt[off + 5], pkt[off + 6], pkt[off + 7]]);
    let rdlength = u16::from_be_bytes([pkt[off + 8], pkt[off + 9]]);
    off += 10;
    if rrtype != TYPE_AAAA || rrclass != CLASS_IN || rdlength != 16 {
        if off + rdlength as usize > pkt.len() {
            return None;
        }
        off += rdlength as usize;
        return None;
    }
    let mut rdata = [0u8; 16];
    rdata.copy_from_slice(&pkt[off..off + 16]);
    off += 16;
    Some((rdata, off))
}

/// Decodifica tutti i record di una risposta DNS.
pub fn parse_response(pkt: &[u8]) -> Option<(Vec<[u8; 4]>, Vec<[u8; 16]>)> {
    let hdr = parse_header(pkt)?;
    if hdr.ancount == 0 {
        return Some((Vec::new(), Vec::new()));
    }
    let mut off = DNS_HEADER_LEN;
    for _ in 0..hdr.qdcount {
        let (_q, _, _, new_off) = decode_question(pkt, off)?;
        off = new_off;
    }
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    for _ in 0..hdr.ancount {
        if let Some((rr, new_off)) = decode_rr_a(pkt, off) {
            off = new_off;
            ipv4.push(rr);
        } else if let Some((rr, new_off)) = decode_rr_aaaa(pkt, off) {
            off = new_off;
            ipv6.push(rr);
        } else {
            break;
        }
    }
    Some((ipv4, ipv6))
}

// ── Framing DNS over TCP (RFC 7858 §3.3, che eredita RFC 1035 §4.2.2) ──
//
// Su TCP (e quindi su DoT) ogni messaggio e' preceduto dalla sua lunghezza
// in due byte big-endian. Senza il prefisso il receiver non sa dove finisce
// un messaggio; con il prefisso sbagliato legge il messaggio successivo.

/// Lunghezza del prefisso di framing su TCP.
pub const TCP_LEN_PREFIX: usize = 2;

/// Scrive `[len:2][payload]` in `out` e ritorna la lunghezza totale.
///
/// `None` se il payload non e' rappresentabile (0 = nessun messaggio, o
/// > 65535 = fuori dal campo a 16 bit) o se `out` non basta. Fail-loud: mai
/// un frame troncato che il receiver leggerebbe come messaggio successivo.
pub fn frame_tcp(out: &mut [u8], payload: &[u8]) -> Option<usize> {
    if payload.is_empty() || payload.len() > u16::MAX as usize {
        return None;
    }
    let total = TCP_LEN_PREFIX + payload.len();
    if out.len() < total {
        return None;
    }
    out[..2].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    out[2..total].copy_from_slice(payload);
    Some(total)
}

/// Legge la lunghezza annunciata dal prefisso di un frame TCP.
///
/// `None` se il prefisso non c'e' ancora (meno di 2 byte: il chiamante deve
/// leggerne altri) o se annuncia `0` (frame vuoto: protocollo violato).
pub fn tcp_prefix_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < TCP_LEN_PREFIX {
        return None;
    }
    let n = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if n == 0 { None } else { Some(n) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_name() {
        let mut out = [0u8; 256];
        let len = encode_name(&mut out, "www.example.com").unwrap();
        assert_eq!(len, 17);
    }

    #[test]
    fn build_and_parse_query() {
        let mut out = [0u8; 512];
        let len = build_query(&mut out, 0x1234, "example.com", TYPE_A).unwrap();
        let hdr = parse_header(&out).unwrap();
        assert_eq!(hdr.id, 0x1234);
        assert_eq!(hdr.flags, FLAG_RD);
        assert_eq!(hdr.qdcount, 1);
        let (qname, qtype, qclass, _) = decode_question(&out, DNS_HEADER_LEN).unwrap();
        assert_eq!(core::str::from_utf8(&qname).unwrap(), "example.com");
        assert_eq!(qtype, TYPE_A);
        assert_eq!(qclass, CLASS_IN);
        assert_eq!(len, 13 + 4 + DNS_HEADER_LEN);
    }

    #[test]
    fn parse_response_a() {
        let mut pkt = [0u8; 512];
        let mut off = write_header(&mut pkt, 0x1234, FLAG_QR | FLAG_RD, 1, 1, 0, 0);
        off += encode_question(&mut pkt[off..], "example.com", TYPE_A, CLASS_IN).unwrap();
        off += encode_name(&mut pkt[off..], "example.com").unwrap();
        pkt[off..off + 2].copy_from_slice(&TYPE_A.to_be_bytes());
        pkt[off + 2..off + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
        pkt[off + 4..off + 8].copy_from_slice(&300u32.to_be_bytes());
        pkt[off + 8..off + 10].copy_from_slice(&4u16.to_be_bytes());
        off += 10;
        pkt[off..off + 4].copy_from_slice(&[192, 168, 1, 1]);
        off += 4;

        let (ipv4, ipv6) = parse_response(&pkt[..off]).unwrap();
        assert_eq!(ipv4.len(), 1);
        assert_eq!(ipv4[0], [192, 168, 1, 1]);
        assert_eq!(ipv6.len(), 0);
    }

    #[test]
    fn tcp_frame_roundtrip() {
        let q = b"\x12\x34\x01\x00";
        let mut frame = [0u8; 16];
        let n = frame_tcp(&mut frame, q).expect("frame");
        assert_eq!(n, 2 + q.len());
        assert_eq!(&frame[..2], &[0x00, 0x04]);
        assert_eq!(&frame[2..n], q);
        assert_eq!(tcp_prefix_len(&frame[..n]), Some(q.len()));
    }

    #[test]
    fn tcp_frame_rejects_empty_and_oversize() {
        let mut frame = [0u8; 16];
        assert!(frame_tcp(&mut frame, &[]).is_none(), "payload vuoto");
        // Buffer troppo piccolo: fail-loud, mai frame troncato.
        assert!(frame_tcp(&mut frame[..3], b"abcdef").is_none());
        // Prefisso parziale o nullo.
        assert!(tcp_prefix_len(&[]).is_none());
        assert!(tcp_prefix_len(&[0x00]).is_none());
        assert!(tcp_prefix_len(&[0x00, 0x00]).is_none(), "lunghezza 0");
        assert_eq!(tcp_prefix_len(&[0x02, 0x00]), Some(512));
    }

    #[test]
    fn parse_response_aaaa() {
        let mut pkt = [0u8; 512];
        let mut off = write_header(&mut pkt, 0x1234, FLAG_QR | FLAG_RD, 1, 1, 0, 0);
        off += encode_question(&mut pkt[off..], "example.com", TYPE_AAAA, CLASS_IN).unwrap();
        off += encode_name(&mut pkt[off..], "example.com").unwrap();
        pkt[off..off + 2].copy_from_slice(&TYPE_AAAA.to_be_bytes());
        pkt[off + 2..off + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
        pkt[off + 4..off + 8].copy_from_slice(&300u32.to_be_bytes());
        pkt[off + 8..off + 10].copy_from_slice(&16u16.to_be_bytes());
        off += 10;
        let v6: [u8; 16] = [
            0x2a, 0x01, 0x0e, 0x11, 0x24, 0x30, 0x7a, 0xf0,
            0, 0, 0, 0, 0, 0, 0, 1,
        ];
        pkt[off..off + 16].copy_from_slice(&v6);
        off += 16;

        let (ipv4, ipv6) = parse_response(&pkt[..off]).unwrap();
        assert_eq!(ipv4.len(), 0);
        assert_eq!(ipv6.len(), 1);
        assert_eq!(ipv6[0], v6);
    }
}
