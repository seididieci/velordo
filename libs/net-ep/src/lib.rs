//! net-ep — formato degli slot dei ring endpoint (N7.0, ADR-0074).
//!
//! **Unica definizione** del layout che client e servizi si scambiano nei
//! ring SPSC per-endpoint (ADR-0069). Prima di N7.0 lo stesso layout era
//! ricopiato in otto client e in ogni servizio, con invarianti divergenti
//! (chi scartava in silenzio, chi non controllava, chi ritentava).
//!
//! Layout di uno slot (little-endian), `A` = lunghezza dell'indirizzo:
//!
//! ```text
//!   off        campo            note
//!   0          len:u16          totale dello slot, payload compreso
//!   2          addr:[A]         IPv6 (16) oppure IPv4 (4)
//!   2+A        port:u16         porta remota: destinazione in TX, sorgente in RX
//!   4+A        local:u16        porta locale (0 in TX: la assegna il servizio)
//!   6+A        event:u8         tipo di evento (0 = dati)
//!   7+A        payload:[]       fino a `slot_size` byte
//! ```
//!
//! Il crate e' **puro**: nessun `unsafe`, nessuna syscall, nessun accesso
//! volatile. Le funzioni lavorano su una finestra di byte che il chiamante
//! ha gia' validato (lunghezza compresa); i puntatori del ring, gli accessi
//! volatile e la `fence` restano nei wrapper (servizio e `civis::net`).
//!
//! Due layout in uso, stesso codice: `Layout::IPV6` (23 byte di metadati,
//! servizio `net`) e `Layout::IPV4` (11 byte, `siit` e lato-client di `dns`).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

/// Lunghezza di un indirizzo IPv6 in byte.
pub const ADDR_IPV6: usize = 16;
/// Lunghezza di un indirizzo IPv4 in byte.
pub const ADDR_IPV4: usize = 4;

/// Layout di uno slot: la sola variabile e' la lunghezza dell'indirizzo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    addr_len: usize,
}

impl Layout {
    /// Servizio `net` (IPv6, 23 byte di metadati).
    pub const IPV6: Self = Self { addr_len: ADDR_IPV6 };
    /// `siit` e lato-client di `dns` (IPv4, 11 byte di metadati).
    pub const IPV4: Self = Self { addr_len: ADDR_IPV4 };

    /// Layout con indirizzo di `addr_len` byte.
    pub const fn with_addr_len(addr_len: usize) -> Self {
        Self { addr_len }
    }

    pub const fn addr_len(self) -> usize {
        self.addr_len
    }

    /// Lunghezza dei metadati (`len` + indirizzo + due porte + evento).
    pub const fn meta(self) -> usize {
        7 + self.addr_len
    }

    pub const fn addr_off(self) -> usize {
        2
    }

    /// Porta remota: destinazione in TX, sorgente in RX.
    pub const fn port_off(self) -> usize {
        2 + self.addr_len
    }

    /// Porta locale (0 in TX).
    pub const fn local_off(self) -> usize {
        4 + self.addr_len
    }

    pub const fn event_off(self) -> usize {
        6 + self.addr_len
    }

    pub const fn payload_off(self) -> usize {
        7 + self.addr_len
    }

    /// Totale dello slot per un payload di `payload_len` byte.
    pub const fn total(self, payload_len: usize) -> usize {
        self.meta() + payload_len
    }

    /// Lunghezza del payload se `total` e' ammissibile per questo layout.
    pub const fn payload_len(self, total: usize) -> Option<usize> {
        if total < self.meta() {
            None
        } else {
            Some(total - self.meta())
        }
    }
}

/// Valida un `len` letto dal ring: `Ok(payload_len)` se sta nello slot.
///
/// Un `len` fuori range e' corruzione (o un client che scrive male): il
/// chiamante consuma lo slot e lo salta, mai panic.
pub fn check(layout: Layout, total: usize, slot_size: usize) -> Option<usize> {
    if total > slot_size {
        return None;
    }
    layout.payload_len(total)
}

/// Scrive uno slot completo (compreso `len`) in `buf`; ritorna il totale.
///
/// `buf` deve essere la finestra esatta dello slot (almeno `total` byte).
/// `addr` deve essere lunga `layout.addr_len()`. Nessun byte oltre `total`
/// viene toccato.
pub fn write(
    buf: &mut [u8],
    layout: Layout,
    addr: &[u8],
    port: u16,
    local: u16,
    event: u8,
    payload: &[u8],
) -> Option<usize> {
    if addr.len() != layout.addr_len() {
        return None;
    }
    let total = layout.total(payload.len());
    if buf.len() < total {
        return None;
    }
    buf[0..2].copy_from_slice(&(total as u16).to_le_bytes());
    buf[layout.addr_off()..layout.addr_off() + layout.addr_len()].copy_from_slice(addr);
    buf[layout.port_off()..layout.port_off() + 2].copy_from_slice(&port.to_le_bytes());
    buf[layout.local_off()..layout.local_off() + 2].copy_from_slice(&local.to_le_bytes());
    buf[layout.event_off()] = event;
    buf[layout.payload_off()..total].copy_from_slice(payload);
    Some(total)
}

/// Uno slot letto dal ring, in prestito dalla finestra del chiamante.
#[derive(Clone, Copy, Debug)]
pub struct Slot<'a> {
    /// Indirizzo remoto (`addr_len` byte).
    pub addr: &'a [u8],
    /// Porta remota: sorgente dello slot ricevuto.
    pub port: u16,
    /// Porta locale (0 negli slot scritti dal client).
    pub local: u16,
    pub event: u8,
    pub payload: &'a [u8],
}

/// Legge uno slot da una finestra gia' validata con [`check`].
pub fn read(buf: &[u8], layout: Layout, total: usize) -> Option<Slot<'_>> {
    let payload_len = layout.payload_len(total)?;
    if buf.len() < total {
        return None;
    }
    let a = layout.addr_off();
    let alen = layout.addr_len();
    let p = layout.port_off();
    let l = layout.local_off();
    Some(Slot {
        addr: &buf[a..a + alen],
        port: u16::from_le_bytes([buf[p], buf[p + 1]]),
        local: u16::from_le_bytes([buf[l], buf[l + 1]]),
        event: buf[layout.event_off()],
        payload: &buf[layout.payload_off()..layout.payload_off() + payload_len],
    })
}

// ── Cursore di ring ────────────────────────────────────────────────

/// Indice (assoluto, non modulo) del prossimo slot libero, o `None` se il
/// ring e' pieno. `head`/`tail` sono contatori monotoni che wrappano.
pub fn push_index(head: u32, tail: u32, slots: u32) -> Option<u32> {
    if slots == 0 || tail.wrapping_sub(head) >= slots {
        None
    } else {
        Some(tail)
    }
}

/// Avanza un contatore del ring (head o tail).
pub const fn next(v: u32) -> u32 {
    v.wrapping_add(1)
}

// ── Test host (fuori dal gate: `./scripts/run-unit-tests.sh`) ───────

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: usize = 64;

    #[test]
    fn roundtrip_ipv6() {
        let l = Layout::IPV6;
        let addr = [0x20u8, 1, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let payload = b"ciao";
        let mut buf = [0xAAu8; SLOT];
        let total = write(&mut buf, l, &addr, 8, 0, 3, payload).unwrap();
        assert_eq!(total, 23 + payload.len());
        assert_eq!(u16::from_le_bytes([buf[0], buf[1]]) as usize, total);
        let s = read(&buf, l, total).unwrap();
        assert_eq!(s.addr, &addr);
        assert_eq!(s.port, 8);
        assert_eq!(s.local, 0);
        assert_eq!(s.event, 3);
        assert_eq!(s.payload, payload);
        // Nessun byte oltre il totale.
        assert_eq!(buf[total], 0xAA);
    }

    #[test]
    fn roundtrip_ipv4() {
        let l = Layout::IPV4;
        assert_eq!(l.meta(), 11);
        let addr = [93u8, 184, 216, 34];
        let mut buf = [0u8; SLOT];
        let total = write(&mut buf, l, &addr, 53, 40000, 0, b"abc").unwrap();
        assert_eq!(total, 14);
        let s = read(&buf, l, total).unwrap();
        assert_eq!(s.addr, &addr);
        assert_eq!(s.port, 53);
        assert_eq!(s.local, 40000);
        assert_eq!(s.event, 0);
        assert_eq!(s.payload, b"abc");
    }

    #[test]
    fn write_rejects_undersized_buffer_and_wrong_addr() {
        let l = Layout::IPV6;
        let mut small = [0u8; 8];
        assert!(write(&mut small, l, &[0u8; 16], 1, 0, 0, &[]).is_none());
        let mut buf = [0u8; SLOT];
        assert!(write(&mut buf, l, &[0u8; 4], 1, 0, 0, &[]).is_none());
    }

    #[test]
    fn check_bounds() {
        let l = Layout::IPV6;
        assert_eq!(check(l, 22, SLOT), None); // sotto i metadati
        assert_eq!(check(l, 23, SLOT), Some(0));
        assert_eq!(check(l, SLOT, SLOT), Some(SLOT - 23));
        assert_eq!(check(l, SLOT + 1, SLOT), None); // oltre lo slot
    }

    #[test]
    fn ring_full_and_wrap() {
        const S: u32 = 16;
        // Vuoto: si scrive.
        assert_eq!(push_index(0, 0, S), Some(0));
        // 15 in volo: ancora posto (slot 15).
        assert_eq!(push_index(0, 15, S), Some(15));
        // 16 in volo: pieno.
        assert_eq!(push_index(0, 16, S), None);
        // Il chiamante consuma: si torna a scrivere.
        assert_eq!(push_index(3, 16, S), Some(16));
        // Wrap dei contatori.
        assert_eq!(push_index(u32::MAX - 1, u32::MAX, S), Some(u32::MAX));
        assert_eq!(next(u32::MAX), 0);
    }

    #[test]
    fn slot_index_is_modulo_slots() {
        // L'indice assoluto si riduce a slot del ring: 18 % 16 = 2.
        let abs = push_index(16, 18, 16).unwrap();
        assert_eq!(abs % 16, 2);
    }
}
