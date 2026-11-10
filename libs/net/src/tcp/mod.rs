//! TCP: header, parse/write/checksum (codec) + connection state machine (conn).
//! Puro: nessun I/O, nessuna syscall, nessuna allocazione per pacchetto.

pub mod codec;
pub mod conn;

// Ri-export della superficie pubblica esistente (compatibilità con N3).
pub use codec::{
    checksum_ok, parse, write, write_checked, TCP_HDR,
    FLAG_FIN, FLAG_SYN, FLAG_RST, FLAG_ACK, TcpHeader,
};
pub use conn::{Conn, RxOutcome, State, TxInfo};
