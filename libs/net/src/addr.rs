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
