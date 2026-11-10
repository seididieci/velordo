//! Indirizzi IPv6 e MAC (ADR-0069).

use core::fmt;

/// Indirizzo IPv6 (16 byte, network order).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Ipv6Addr(pub [u8; 16]);

impl Ipv6Addr {
    pub const UNSPECIFIED: Self = Self([0; 16]);
    /// `ff02::1` — tutti i nodi del link.
    pub const ALL_NODES: Self = Self([
        0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
    ]);
    /// `ff02::2` — tutti i router del link.
    pub const ALL_ROUTERS: Self = Self([
        0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
    ]);
    /// `::1` — loopback.
    pub const LOOPBACK: Self = Self([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

    pub const fn from_bytes(b: [u8; 16]) -> Self {
        Self(b)
    }

    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    pub fn is_unspecified(&self) -> bool {
        self.0 == [0; 16]
    }

    pub fn is_multicast(&self) -> bool {
        self.0[0] == 0xff
    }

    pub fn is_link_local(&self) -> bool {
        self.0[0] == 0xfe && (self.0[1] & 0xc0) == 0x80
    }

    pub fn is_unicast(&self) -> bool {
        !self.is_multicast() && !self.is_unspecified()
    }

    /// `::1` — loopback.
    pub fn is_loopback(&self) -> bool {
        self.0 == Self::LOOPBACK.0
    }

    /// Link-local `fe80::` + IID.
    pub fn from_link_local_iid(iid: u64) -> Self {
        let mut b = [0u8; 16];
        b[0] = 0xfe;
        b[1] = 0x80;
        b[8..16].copy_from_slice(&iid.to_be_bytes());
        Self(b)
    }

    /// Prefisso /64 + IID.
    pub fn from_prefix_iid(prefix: &[u8; 8], iid: u64) -> Self {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(prefix);
        b[8..16].copy_from_slice(&iid.to_be_bytes());
        Self(b)
    }

    /// IID (ultimi 64 bit).
    pub fn iid(&self) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.0[8..16]);
        u64::from_be_bytes(b)
    }

    /// Multicast solicited-node `ff02::1:ffXX:XXXX` (ultimi 24 bit).
    pub fn solicited_node_multicast(&self) -> Self {
        let mut b = [0u8; 16];
        b[0] = 0xff;
        b[1] = 0x02;
        b[11] = 0x01;
        b[12] = 0xff;
        b[13] = self.0[13];
        b[14] = self.0[14];
        b[15] = self.0[15];
        Self(b)
    }

    /// Parsa un indirizzo IPv6 testuale (RFC 4291 §2.2): gruppi esadecimali
    /// separati da `:`, con al piu' una compressione `::`. Forme rifiutate:
    /// IPv4-mapped (`::ffff:1.2.3.4`), scope id (`%eth0`), gruppi vuoti o
    /// oltre 4 cifre. Puro: nessun panic su input arbitrario.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let mut groups = [0u16; 8];
        let mut n = 0usize; // gruppi letti
        let mut gap: Option<usize> = None; // posizione di `::`
        let mut i = 0usize;
        if bytes[0] == b':' {
            if bytes.len() < 2 || bytes[1] != b':' {
                return None;
            }
            gap = Some(0);
            i = 2;
        }
        while i < bytes.len() {
            if n >= 8 {
                return None;
            }
            let start = i;
            let mut v: u16 = 0;
            while i < bytes.len() {
                let d = match bytes[i] {
                    b'0'..=b'9' => bytes[i] - b'0',
                    b'a'..=b'f' => bytes[i] - b'a' + 10,
                    b'A'..=b'F' => bytes[i] - b'A' + 10,
                    _ => break,
                };
                v = v.checked_mul(16)?.checked_add(d as u16)?;
                i += 1;
            }
            if i == start || i - start > 4 {
                return None;
            }
            groups[n] = v;
            n += 1;
            if i == bytes.len() {
                break;
            }
            if bytes[i] != b':' {
                return None;
            }
            i += 1;
            if i < bytes.len() && bytes[i] == b':' {
                if gap.is_some() {
                    return None;
                }
                gap = Some(n);
                i += 1;
            } else if i == bytes.len() {
                return None; // `:` singolo finale
            }
        }
        let mut full = [0u16; 8];
        match gap {
            Some(g) => {
                let tail = n - g;
                if g + tail > 7 {
                    return None; // `::` deve comprimere almeno un gruppo
                }
                for k in 0..g {
                    full[k] = groups[k];
                }
                for k in 0..tail {
                    full[8 - tail + k] = groups[g + k];
                }
            }
            None => {
                if n != 8 {
                    return None;
                }
                for k in 0..8 {
                    full[k] = groups[k];
                }
            }
        }
        let mut out = [0u8; 16];
        for k in 0..8 {
            let g = full[k].to_be_bytes();
            out[2 * k] = g[0];
            out[2 * k + 1] = g[1];
        }
        Some(Self(out))
    }
}

impl fmt::Debug for Ipv6Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Ipv6Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut g = [0u16; 8];
        for i in 0..8 {
            g[i] = u16::from_be_bytes([self.0[2 * i], self.0[2 * i + 1]]);
        }
        // Run di zeri piu' lungo (>= 2 gruppi) per la compressione `::`.
        let mut best_start = 0usize;
        let mut best_len = 0usize;
        let mut i = 0usize;
        while i < 8 {
            if g[i] == 0 {
                let start = i;
                while i < 8 && g[i] == 0 {
                    i += 1;
                }
                let len = i - start;
                if len > best_len {
                    best_len = len;
                    best_start = start;
                }
            } else {
                i += 1;
            }
        }
        if best_len < 2 {
            best_len = 0;
        }
        let mut first = true;
        let mut i = 0usize;
        while i < 8 {
            if best_len > 0 && i == best_start {
                if first {
                    write!(f, "::")?;
                } else {
                    write!(f, ":")?;
                }
                first = false;
                i += best_len;
                continue;
            }
            if !first {
                write!(f, ":")?;
            }
            write!(f, "{:x}", g[i])?;
            first = false;
            i += 1;
        }
        Ok(())
    }
}

/// Indirizzo MAC (6 byte).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const BROADCAST: Self = Self([0xff; 6]);

    pub fn to_u64(self) -> u64 {
        let mut v = 0u64;
        for b in self.0 {
            v = (v << 8) | b as u64;
        }
        v
    }

    pub fn from_u64(v: u64) -> Self {
        let mut b = [0u8; 6];
        for i in 0..6 {
            b[i] = ((v >> (8 * (5 - i))) & 0xff) as u8;
        }
        Self(b)
    }

    pub fn is_multicast(&self) -> bool {
        self.0[0] & 1 != 0
    }

    pub fn is_broadcast(&self) -> bool {
        self.0 == [0xff; 6]
    }

    /// MAC multicast Ethernet per un indirizzo IPv6 multicast
    /// (`33:33` + ultimi 32 bit).
    pub fn multicast_for(addr: &Ipv6Addr) -> Self {
        Self([
            0x33,
            0x33,
            addr.0[12],
            addr.0[13],
            addr.0[14],
            addr.0[15],
        ])
    }
}

impl fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<[u8; 16]> {
        Ipv6Addr::parse(s).map(|a| a.0)
    }

    #[test]
    fn parse_full_and_compressed() {
        assert_eq!(
            p("2001:db8:1:2:3:4:5:6"),
            Some([0x20, 0x01, 0x0d, 0xb8, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6])
        );
        assert_eq!(p("::"), Some([0; 16]));
        assert_eq!(p("::1"), Some(Ipv6Addr::LOOPBACK.0));
        assert_eq!(
            p("fe80::1"),
            Some([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        );
        assert_eq!(
            p("2001:db8::"),
            Some([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        );
        // Compressione in mezzo: `64:ff9b::7` (prefisso SIIT).
        assert_eq!(
            p("64:ff9b::7"),
            Some([0, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7])
        );
        // Maiuscole accettate.
        assert_eq!(p("FE80::ABCD"), p("fe80::abcd"));
    }

    #[test]
    fn parse_rejects_malformed() {
        for s in [
            "",
            ":",
            ":::",
            "1:2:3:4:5:6:7",         // 7 gruppi senza `::`
            "1:2:3:4:5:6:7:8:9",     // 9 gruppi
            "1:2:3:4:5:6:7:8::",     // `::` non comprime nulla
            "1::2::3",               // due compressioni
            "12345::1",              // gruppo oltre 4 cifre
            "fe80::1%eth0",          // scope id non supportato
            "::ffff:1.2.3.4",        // forma IPv4-mapped non supportata
            "1:2:3:4:5:6:7:",        // `:` finale
            "1:2:3:4:5:6:7:8:",      // `:` finale dopo 8 gruppi
            "g::1",                  // cifra non esadecimale
            " ::1",                  // spazio iniziale
        ] {
            assert_eq!(p(s), None, "doveva fallire: {s:?}");
        }
    }
}
