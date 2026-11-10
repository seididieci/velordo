//! Connessione TCP: macchina a stati pura, testabile in host.
//! Nessun I/O, nessuna allocazione per pacchetto (i buffer sono allocati
//! alla creazione della connessione, non nel path dati).
//!
//! Stati: Closed, Listen, SynSent, SynRecv, Established, FinWait1,
//! FinWait2, CloseWait, LastAck, TimeWait.
//! Retransmission: go-back-N su RTO (1s→2s→4s, max 3 tentativi).
//! Ricezione: solo in-order; duplicati e fuori-ordine vengono ACKati ma
//! scartati. Finestra fino a WIN_SEGS segmenti.

use crate::addr::Ipv6Addr;
use crate::tcp::codec::{FLAG_ACK, FLAG_FIN, FLAG_RST, FLAG_SYN, TcpHeader, checksum_tcp, write};
use alloc::collections::VecDeque;

const MSS: usize = 1440;
const SEND_BUF: usize = 4096;
const RECV_BUF: usize = 4096;
const RTO_MIN: u64 = 100; // 1 s a 100 Hz
const RTO_MAX: u64 = 400; // 4 s
const MAX_RETRIES: u8 = 3;
const TIME_WAIT: u64 = 500; // 5 s

/// Stato della connessione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynRecv,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    TimeWait,
}

/// Segmento da trasmettere: `next_tx` scrive header+payload in `out` e
/// ritorna queste info più `len` (byte totali in `out`).
#[derive(Clone, Copy, Debug)]
pub struct TxInfo {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub len: usize,
}

/// Esito di `on_segment`.
pub struct RxOutcome<'a> {
    /// Segmento di risposta da inviare (ACK, SYN+ACK, RST, FIN+ACK...).
    /// `next_tx`-style info; il caller costruisce i byte via `tcp::write_checked`.
    pub tx: Option<TxInfo>,
    /// Dati ricevuti in ordine (slice di `payload` passato a `on_segment`).
    pub data: Option<&'a [u8]>,
    /// La connessione è appena entrata in Established (handshake completato).
    pub established: bool,
    /// La connessione è appena entrata in Closed (dopo RST o TimeWait).
    pub closed: bool,
    /// FIN ricevuto (peer ha chiuso la sua direzione).
    pub fin_recv: bool,
    /// RST ricevuto.
    pub rst: bool,
}

/// Connessione TCP (stato puro).
pub struct Conn {
    pub state: State,
    pub local_port: u16,
    pub remote_port: u16,
    pub remote_addr: Ipv6Addr,
    /// Indirizzo locale usato nei checksum (lo imposta il layer `Net`: dipende
    /// dal next-hop, link-local o global).
    pub local_addr: Ipv6Addr,

    // Sequenze.
    iss: u32, // initial send sequence
    irs: u32, // initial receive sequence
    snd_una: u32, // primo byte non ancora acknowledged
    snd_nxt: u32, // prossimo byte da inviare
    rcv_nxt: u32, // prossimo byte atteso

    // Finestre.
    snd_wnd: u16, // finestra pubblicata dal receiver (limita snd_nxt - snd_una)
    rcv_wnd: u16, // finestra che noi pubblichiamo (spazio in recv_buf)

    // Buffer di invio: byte [snd_una, snd_nxt) (in coda o in volo).
    send_buf: VecDeque<u8>,
    // Buffer di ricezione: dati ricevuti in ordine, non ancora letti.
    recv_buf: VecDeque<u8>,

    // Timer e retransmission.
    rto: u64,            // RTO corrente (tick)
    rto_deadline: u64,   // scadenza (tick)
    retries: u8,         // tentativi di retransmission in corso

    // Flag di chiusura.
    fin_queued: bool, // close() chiamato, FIN da inviare
    fin_sent: bool,   // FIN inviato, non ancora acked
    fin_seq: u32,     // numero di sequenza del FIN
    fin_recv: bool,   // FIN ricevuto dal peer
    pub closed: bool, // connessione definitivamente chiusa

    listener: bool, // true se creata da new_listener
    syn_sent: bool, // SYN iniziale già inviato (stato SynSent)
}

impl Conn {
    /// Nuova connessione attiva (client), in ascolto su `local_port`.
    pub fn new_client(local_port: u16) -> Self {
        Self {
            state: State::Closed,
            local_port,
            remote_port: 0,
            remote_addr: Ipv6Addr::UNSPECIFIED,
            local_addr: Ipv6Addr::UNSPECIFIED,
            iss: 0,
            irs: 0,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            snd_wnd: 65535,
            rcv_wnd: RECV_BUF as u16,
            send_buf: VecDeque::new(),
            recv_buf: VecDeque::new(),
            rto: RTO_MIN,
            rto_deadline: 0,
            retries: 0,
            fin_queued: false,
            fin_sent: false,
            fin_seq: 0,
            fin_recv: false,
            closed: false,
            listener: false,
            syn_sent: false,
        }
    }

    /// Nuova connessione passiva (server), in ascolto su `local_port`.
    pub fn new_listener(local_port: u16) -> Self {
        let mut c = Self::new_client(local_port);
        c.state = State::Listen;
        c.listener = true;
        c
    }

    /// Apertura attiva (client): prepara l'invio di SYN verso `dst:dst_port`.
    /// Il SYN viene effettivamente trasmesso alla prima chiamata di `next_tx`.
    pub fn open(&mut self, dst: Ipv6Addr, dst_port: u16, iss: u32, now: u64) {
        self.remote_addr = dst;
        self.remote_port = dst_port;
        self.iss = iss;
        self.irs = 0;
        self.snd_una = iss;
        self.snd_nxt = iss + 1; // SYN consuma un seq
        self.state = State::SynSent;
        self.rto = RTO_MIN;
        self.rto_deadline = now + RTO_MIN;
        self.retries = 0;
        self.fin_queued = false;
        self.fin_sent = false;
        self.fin_recv = false;
        self.closed = false;
        self.syn_sent = false;
    }

    /// Imposta l'indirizzo locale (usato nei checksum; lo calcola il layer
    /// `Net` in base al next-hop).
    pub fn set_local_addr(&mut self, a: Ipv6Addr) {
        self.local_addr = a;
    }

    /// Restituisce lo stato attuale.
    pub fn state(&self) -> State {
        self.state
    }

    /// Restituisce true se ci sono dati da leggere.
    pub fn readable(&self) -> usize {
        self.recv_buf.len()
    }

    /// Scrive `data` nel buffer di invio. Ritorna i byte accettati.
    /// **Non** avanza `snd_nxt`: il progresso lo fa `next_tx` quando i byte
    /// entrano davvero in volo (`snd_nxt` = prossimo byte da spedire,
    /// `snd_una` = più vecchio non confermato).
    pub fn write(&mut self, data: &[u8], _now: u64) -> usize {
        if self.state != State::Established && self.state != State::CloseWait {
            return 0;
        }
        let max_send = SEND_BUF.saturating_sub(self.send_buf.len());
        let n = data.len().min(max_send);
        self.send_buf.extend(data[..n].iter().copied());
        n
    }

    /// Legge fino a `out.len()` byte dal buffer di ricezione.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.recv_buf.len());
        for i in 0..n {
            out[i] = self.recv_buf.pop_front().unwrap();
        }
        self.rcv_wnd = (RECV_BUF as u16).saturating_sub(self.recv_buf.len() as u16);
        n
    }

    /// Mette in coda un FIN (chiusura ordinata della nostra direzione).
    /// Il FIN viene trasmesso alla prossima chiamata di `next_tx`.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        match self.state {
            State::Established => {
                self.state = State::FinWait1;
                self.fin_queued = true;
            }
            State::CloseWait => {
                self.state = State::LastAck;
                self.fin_queued = true;
            }
            State::SynSent | State::SynRecv => {
                // Chiusura durante l'handshake: invia RST.
                self.state = State::Closed;
                self.closed = true;
            }
            _ => {}
        }
    }

    /// Abort: invia RST e chiude immediatamente.
    pub fn abort(&mut self) {
        self.state = State::Closed;
        self.closed = true;
        self.fin_queued = false;
        self.fin_sent = false;
    }

    /// Timer periodico (chiamato dal chiamante a ogni tick). L'RTO è gestito
    /// **solo** in `next_tx` (una sola sorgente di retransmission: due
    /// contatori separati conterebbero i retry due volte). Qui resta la
    /// scadenza di TIME_WAIT.
    pub fn on_tick(&mut self, now: u64) {
        if self.closed {
            return;
        }
        if self.state == State::TimeWait && now >= self.rto_deadline {
            self.state = State::Closed;
            self.closed = true;
        }
    }

    /// Ritorna true se una retransmission è in scadenza (da gestire in `next_tx`).
    fn rto_expired(&self, now: u64) -> bool {
        !self.closed && now >= self.rto_deadline
    }

    /// Sequenza `a` è dopo `b` nello spazio circolare a 32 bit.
    fn seq_after(a: u32, b: u32) -> bool {
        a.wrapping_sub(b) < 0x8000_0000
    }

    /// Sequenza `a` è prima o uguale a `b` (usato come documentazione del
    /// confronto circolare; la logica dei test lo esercita).
    #[allow(dead_code)]
    fn seq_before_or_eq(a: u32, b: u32) -> bool {
        !Self::seq_after(a, b)
    }

    /// Finestra di invio disponibile (byte non ancora in volo).
    /// In volo = `snd_nxt - snd_una`; il limite è `snd_wnd` (cap `SEND_BUF`).
    fn snd_wnd_available(&self) -> usize {
        let in_flight = self.snd_nxt.wrapping_sub(self.snd_una) as usize;
        let wnd = (self.snd_wnd as usize).min(SEND_BUF);
        wnd.saturating_sub(in_flight)
    }

    /// Costruisce le info di un segmento di controllo (ACK, SYN+ACK, FIN+ACK,
    /// RST) senza payload. Usa un buffer locale sullo stack (sempre 20 byte).
    fn ctrl_segment_info(
        &self,
        src_port: u16,
        dst_port: u16,
        seq: u32,
        ack: u32,
        flags: u8,
    ) -> TxInfo {
        let mut buf = [0u8; 20];
        let total = write(
            &mut buf,
            src_port,
            dst_port,
            seq,
            ack,
            flags,
            self.rcv_wnd,
            0, // checksum, calcolato dopo
        );
        let c = checksum_tcp(
            &self.local_addr,
            &self.remote_addr,
            &buf[..total],
        );
        buf[16..18].copy_from_slice(&c.to_be_bytes());
        TxInfo {
            src_port,
            dst_port,
            seq,
            ack,
            flags,
            window: self.rcv_wnd,
            len: total,
        }
    }

    /// Serializza nel buffer del chiamante un segmento di controllo già
    /// descritto da `TxInfo` (header TCP + checksum). `next_tx` deve sempre
    /// lasciare i byte pronti: limitarsi a `TxInfo` emetteva segmenti vuoti.
    fn write_ctrl_into(&self, info: &TxInfo, out: &mut [u8]) {
        let len = write(
            out,
            info.src_port,
            info.dst_port,
            info.seq,
            info.ack,
            info.flags,
            info.window,
            0,
        );
        let c = checksum_tcp(&self.local_addr, &self.remote_addr, &out[..len]);
        out[16..18].copy_from_slice(&c.to_be_bytes());
    }

    /// Costruisce un segmento dati (header + payload da `send_buf`).
    /// Scrive header+payload in `out` e ritorna `TxInfo`.
    fn data_segment(
        &mut self,
        src_port: u16,
        dst_port: u16,
        seq: u32,
        ack: u32,
        out: &mut [u8],
    ) -> TxInfo {
        let window = self.rcv_wnd;
        let mut total = write(
            &mut out[..20],
            src_port,
            dst_port,
            seq,
            ack,
            FLAG_ACK,
            window,
            0,
        );
        let payload_start = seq.wrapping_sub(self.snd_una) as usize;
        let max_payload = out.len().saturating_sub(total).min(MSS);
        let available = self.send_buf.len().saturating_sub(payload_start);
        let payload_len = max_payload.min(available);
        if payload_len > 0 {
            for (i, b) in self
                .send_buf
                .range(payload_start..payload_start + payload_len)
                .enumerate()
            {
                out[total + i] = *b;
            }
            total += payload_len;
        }
        let c = checksum_tcp(
            &self.local_addr,
            &self.remote_addr,
            &out[..total],
        );
        out[16..18].copy_from_slice(&c.to_be_bytes());
        TxInfo {
            src_port,
            dst_port,
            seq,
            ack,
            flags: FLAG_ACK,
            window,
            len: total,
        }
    }

    /// Ritorna il prossimo segmento da trasmettere (SYN, dati, ritrasmissione,
    /// FIN). Scrive header+payload in `out`.
    pub fn next_tx(&mut self, out: &mut [u8], now: u64) -> Option<TxInfo> {
        if self.closed {
            return None;
        }

        // 1. Prima trasmissione del SYN (stato SynSent).
        if self.state == State::SynSent && !self.syn_sent {
            self.syn_sent = true;
            self.rto = RTO_MIN;
            self.rto_deadline = now + RTO_MIN;
            self.retries = 0;
            let info = self.ctrl_segment_info(
                self.local_port,
                self.remote_port,
                self.iss,
                0,
                FLAG_SYN,
            );
            self.write_ctrl_into(&info, out);
            return Some(info);
        }

        // 2. Ritrasmissione su RTO.
        if self.rto_expired(now) {
            self.rto = (self.rto * 2).min(RTO_MAX);
            self.rto_deadline = now + self.rto;
            self.retries = self.retries.saturating_add(1);
            if self.retries >= MAX_RETRIES {
                self.state = State::Closed;
                self.closed = true;
                return None;
            }
            match self.state {
                State::SynSent => {
                    let info = self.ctrl_segment_info(
                        self.local_port,
                        self.remote_port,
                        self.iss,
                        0,
                        FLAG_SYN,
                    );
                    self.write_ctrl_into(&info, out);
                    return Some(info);
                }
                State::SynRecv => {
                    let info = self.ctrl_segment_info(
                        self.local_port,
                        self.remote_port,
                        self.iss,
                        self.rcv_nxt,
                        FLAG_SYN | FLAG_ACK,
                    );
                    self.write_ctrl_into(&info, out);
                    return Some(info);
                }
                State::Established
                | State::FinWait1
                | State::FinWait2
                | State::CloseWait
                | State::LastAck => {
                    if self.fin_sent && !self.fin_acked() {
                        // Ritrasmetti FIN.
                        let info = self.ctrl_segment_info(
                            self.local_port,
                            self.remote_port,
                            self.fin_seq,
                            self.rcv_nxt,
                            FLAG_FIN | FLAG_ACK,
                        );
                        self.write_ctrl_into(&info, out);
                        return Some(info);
                    }
                    if !self.send_buf.is_empty() {
                        // Go-back-N: ritrasmetti da snd_una.
                        return Some(self.data_segment(
                            self.local_port,
                            self.remote_port,
                            self.snd_una,
                            self.rcv_nxt,
                            out,
                        ));
                    }
                }
                _ => {}
            }
        }

        // 3. Invio FIN (se in coda).
        if self.fin_queued && !self.fin_sent {
            self.fin_queued = false;
            self.fin_sent = true;
            self.fin_seq = self.snd_nxt;
            self.snd_nxt = self.snd_nxt.wrapping_add(1); // FIN consuma un seq
            self.rto = RTO_MIN;
            self.rto_deadline = now + RTO_MIN;
            self.retries = 0;
            let info = self.ctrl_segment_info(
                self.local_port,
                self.remote_port,
                self.fin_seq,
                self.rcv_nxt,
                FLAG_FIN | FLAG_ACK,
            );
            self.write_ctrl_into(&info, out);
            return Some(info);
        }

        // 4. Invio di nuovi dati (dentro la finestra).
        if !self.send_buf.is_empty() {
            let available = self.snd_wnd_available();
            if available > 0 {
                let in_flight = self.snd_nxt.wrapping_sub(self.snd_una) as usize;
                let unsent = self.send_buf.len().saturating_sub(in_flight);
                let to_send = unsent.min(MSS).min(available);
                if to_send > 0 {
                    let info = self.data_segment(
                        self.local_port,
                        self.remote_port,
                        self.snd_nxt,
                        self.rcv_nxt,
                        out,
                    );
                    self.snd_nxt = self.snd_nxt.wrapping_add(to_send as u32);
                    if self.retries == 0 {
                        self.rto = RTO_MIN;
                        self.rto_deadline = now + RTO_MIN;
                    }
                    return Some(info);
                }
            }
        }

        None
    }

    /// Ritorna true se il FIN è stato acknowledged (snd_una passato fin_seq).
    fn fin_acked(&self) -> bool {
        self.fin_sent && Self::seq_after(self.snd_una, self.fin_seq)
    }

    /// Elabora un segmento in arrivo. Ritorna le azioni da compiere.
    pub fn on_segment<'a>(&mut self, hdr: &TcpHeader, payload: &'a [u8], now: u64) -> RxOutcome<'a> {
        let mut out = RxOutcome {
            tx: None,
            data: None,
            established: false,
            closed: false,
            fin_recv: false,
            rst: false,
        };

        // RST: abort immediato.
        if hdr.flags & FLAG_RST != 0 {
            self.state = State::Closed;
            self.closed = true;
            out.closed = true;
            out.rst = true;
            return out;
        }

        // Aggiornamento della finestra del peer (RFC 793 §3.9): vale in tutti
        // gli stati in cui stiamo trasmettendo.
        if hdr.flags & FLAG_ACK != 0
            && matches!(
                self.state,
                State::Established
                    | State::FinWait1
                    | State::FinWait2
                    | State::CloseWait
                    | State::LastAck
            )
        {
            self.snd_wnd = hdr.window;
        }

        match self.state {
            State::Closed => {
                // Qualsiasi segmento verso una connessione chiusa → RST.
                out.tx = Some(self.ctrl_segment_info(
                    self.local_port,
                    hdr.src_port,
                    0,
                    hdr.seq.wrapping_add(1),
                    FLAG_RST | FLAG_ACK,
                ));
            }
            State::Listen => {
                if hdr.flags & FLAG_SYN != 0 {
                    // Ricevuta SYN: rispondi con SYN+ACK.
                    self.irs = hdr.seq;
                    self.rcv_nxt = hdr.seq.wrapping_add(1);
                    self.iss = self.next_iss();
                    self.snd_una = self.iss;
                    self.snd_nxt = self.iss.wrapping_add(1);
                    self.remote_port = hdr.src_port;
                    self.state = State::SynRecv;
                    self.rto = RTO_MIN;
                    self.rto_deadline = now + RTO_MIN;
                    self.retries = 0;
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.iss,
                        self.rcv_nxt,
                        FLAG_SYN | FLAG_ACK,
                ));
                }
            }
            State::SynSent => {
                if hdr.flags & FLAG_ACK != 0 && hdr.ack == self.snd_nxt {
                    // Ricevuta SYN+ACK.
                    self.irs = hdr.seq;
                    self.rcv_nxt = hdr.seq.wrapping_add(1);
                    self.snd_una = hdr.ack;
                    self.send_buf.clear();
                    self.state = State::Established;
                    self.rto = RTO_MIN;
                    self.rto_deadline = now + RTO_MIN;
                    self.retries = 0;
                    out.established = true;
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.snd_nxt,
                        self.rcv_nxt,
                        FLAG_ACK,
                ));
                } else if hdr.flags & FLAG_SYN != 0 {
                    // Simultaneous open: riceviamo SYN, rispondiamo SYN+ACK.
                    self.irs = hdr.seq;
                    self.rcv_nxt = hdr.seq.wrapping_add(1);
                    self.state = State::SynRecv;
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.snd_nxt,
                        self.rcv_nxt,
                        FLAG_SYN | FLAG_ACK,
                ));
                }
            }
            State::SynRecv => {
                if hdr.flags & FLAG_ACK != 0 && Self::seq_after(hdr.ack, self.snd_una) {
                    self.snd_una = hdr.ack;
                    self.send_buf.clear();
                    self.state = State::Established;
                    self.rto = RTO_MIN;
                    self.rto_deadline = now + RTO_MIN;
                    self.retries = 0;
                    out.established = true;
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.snd_nxt,
                        self.rcv_nxt,
                        FLAG_ACK,
                ));
                } else if hdr.flags & FLAG_SYN != 0 {
                    // Retransmission della SYN del peer: rispondi di nuovo SYN+ACK.
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.iss,
                        self.rcv_nxt,
                        FLAG_SYN | FLAG_ACK,
                ));
                }
            }
            State::Established => {
                // Elabora ACK (avanza snd_una).
                if hdr.flags & FLAG_ACK != 0 && Self::seq_after(hdr.ack, self.snd_una) {
                    let acked = hdr.ack.wrapping_sub(self.snd_una);
                    self.snd_una = hdr.ack;
                    for _ in 0..acked {
                        self.send_buf.pop_front();
                    }
                    self.rto = RTO_MIN;
                    self.rto_deadline = now + RTO_MIN;
                    self.retries = 0;
                }

                // Elabora dati (solo in ordine).
                let mut consumed = 0u32;
                if !payload.is_empty() && hdr.seq == self.rcv_nxt {
                    let data_len = payload.len().min(RECV_BUF - self.recv_buf.len());
                    if data_len > 0 {
                        out.data = Some(&payload[..data_len]);
                        self.recv_buf.extend(payload[..data_len].iter().copied());
                        self.rcv_nxt = self.rcv_nxt.wrapping_add(data_len as u32);
                        consumed = data_len as u32;
                    }
                }

                // Elabora FIN: il flag occupa il seq subito dopo i dati del
                // segmento (RFC 793 §3.3), non quello dell'ultimo byte.
                if hdr.flags & FLAG_FIN != 0
                    && !self.fin_recv
                    && hdr.seq.wrapping_add(consumed) == self.rcv_nxt
                {
                    self.fin_recv = true;
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                    self.state = State::CloseWait;
                    out.fin_recv = true;
                }

                // ACK in risposta (se abbiamo ricevuto qualcosa di significativo).
                if !payload.is_empty() || hdr.flags & FLAG_FIN != 0 || hdr.flags & FLAG_SYN != 0 {
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.snd_nxt,
                        self.rcv_nxt,
                        FLAG_ACK,
                ));
                }
            }
            State::FinWait1 => {
                if hdr.flags & FLAG_ACK != 0 && Self::seq_after(hdr.ack, self.snd_una) {
                    let acked = hdr.ack.wrapping_sub(self.snd_una);
                    self.snd_una = hdr.ack;
                    for _ in 0..acked {
                        self.send_buf.pop_front();
                    }
                    self.rto = RTO_MIN;
                    self.rto_deadline = now + RTO_MIN;
                    self.retries = 0;
                    if self.fin_recv {
                        self.state = State::TimeWait;
                        self.rto_deadline = now + TIME_WAIT;
                    } else {
                        self.state = State::FinWait2;
                    }
                }
                if hdr.flags & FLAG_FIN != 0 {
                    self.fin_recv = true;
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                    if self.state == State::FinWait1 {
                        self.state = State::TimeWait;
                        self.rto_deadline = now + TIME_WAIT;
                    } else if self.state == State::FinWait2 {
                        self.state = State::TimeWait;
                        self.rto_deadline = now + TIME_WAIT;
                    }
                    out.fin_recv = true;
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.snd_nxt,
                        self.rcv_nxt,
                        FLAG_ACK,
                ));
                }
            }
            State::FinWait2 => {
                if hdr.flags & FLAG_FIN != 0 {
                    self.fin_recv = true;
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                    self.state = State::TimeWait;
                    self.rto_deadline = now + TIME_WAIT;
                    out.fin_recv = true;
                    out.tx = Some(self.ctrl_segment_info(
                        self.local_port,
                        hdr.src_port,
                        self.snd_nxt,
                        self.rcv_nxt,
                        FLAG_ACK,
                ));
                }
            }
            State::CloseWait => {
                // In attesa che il chiamante invochi close().
                if hdr.flags & FLAG_ACK != 0 && Self::seq_after(hdr.ack, self.snd_una) {
                    let acked = hdr.ack.wrapping_sub(self.snd_una);
                    self.snd_una = hdr.ack;
                    for _ in 0..acked {
                        self.send_buf.pop_front();
                    }
                    self.rto = RTO_MIN;
                    self.rto_deadline = now + RTO_MIN;
                    self.retries = 0;
                }
            }
            State::LastAck => {
                if hdr.flags & FLAG_ACK != 0 && Self::seq_after(hdr.ack, self.snd_una) {
                    self.snd_una = hdr.ack;
                    self.send_buf.clear();
                    self.state = State::Closed;
                    self.closed = true;
                    out.closed = true;
                }
            }
            State::TimeWait => {
                // Ritrasmetti l'ultimo ACK.
                out.tx = Some(self.ctrl_segment_info(
                    self.local_port,
                    hdr.src_port,
                    self.snd_nxt,
                    self.rcv_nxt,
                    FLAG_ACK,
                ));
            }
        }

        out
    }

    /// Prossimo ISS (sequence number iniziale): contatore incrementale.
    /// Il chiamante dovrebbe passare un ISS casuale in `open()`.
    fn next_iss(&mut self) -> u32 {
        self.iss = self.iss.wrapping_add(0x1234_5678);
        self.iss
    }

    /// Ritorna true se la connessione è in ascolto (listener).
    pub fn is_listener(&self) -> bool {
        self.listener
    }

    /// Ritorna l'indirizzo e porta remota (se conosciuti).
    pub fn remote(&self) -> Option<(Ipv6Addr, u16)> {
        if self.remote_port == 0 && self.remote_addr.is_unspecified() {
            None
        } else {
            Some((self.remote_addr, self.remote_port))
        }
    }
}

impl Default for Conn {
    fn default() -> Self {
        Self::new_client(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::Ipv6Addr;

    fn make_hdr(src_port: u16, dst_port: u16, seq: u32, ack: u32, flags: u8) -> TcpHeader {
        TcpHeader {
            src_port,
            dst_port,
            seq,
            ack,
            flags,
            window: 65535,
            checksum: 0,
            len: 20,
        }
    }

    fn complete_handshake(c: &mut Conn, s: &mut Conn, now: u64) {
        // Client invia SYN.
        let mut buf = [0u8; 200];
        let syn = c.next_tx(&mut buf, now).expect("SYN");
        assert_eq!(syn.flags, FLAG_SYN);

        // Server riceve SYN → SYN+ACK.
        let hdr = make_hdr(50001, 50002, syn.seq, 0, FLAG_SYN);
        let sres = s.on_segment(&hdr, &[], now);
        let synack = sres.tx.expect("SYN+ACK");
        assert_eq!(synack.flags, FLAG_SYN | FLAG_ACK);
        assert_eq!(synack.ack, 1001);

        // Client riceve SYN+ACK → ACK.
        let hdr = make_hdr(50002, 50001, synack.seq, synack.ack, FLAG_SYN | FLAG_ACK);
        let cres = c.on_segment(&hdr, &[], now);
        assert!(c.state == State::Established);
        assert!(cres.established);
        let ack = cres.tx.expect("ACK");
        assert_eq!(ack.flags, FLAG_ACK);

        // Server riceve ACK → Established.
        let hdr = make_hdr(50001, 50002, ack.seq, ack.ack, FLAG_ACK);
        let _sres2 = s.on_segment(&hdr, &[], now);
        assert_eq!(s.state, State::Established);
    }

    #[test]
    fn client_handshake() {
        let mut c = Conn::new_client(50001);
        let mut s = Conn::new_listener(50002);

        c.open(Ipv6Addr::LOOPBACK, 50002, 1000, 0);
        assert_eq!(c.state, State::SynSent);

        complete_handshake(&mut c, &mut s, 0);
    }

    #[test]
    fn listener_syn_creates_child() {
        let mut s = Conn::new_listener(50002);
        assert!(s.is_listener());
        assert_eq!(s.state, State::Listen);

        let hdr = make_hdr(50001, 50002, 3000, 0, FLAG_SYN);
        let res = s.on_segment(&hdr, &[], 0);
        assert!(res.tx.is_some());
        let synack = res.tx.unwrap();
        assert_eq!(synack.flags, FLAG_SYN | FLAG_ACK);
        assert_eq!(synack.seq, s.iss);
        assert_eq!(synack.ack, 3001);
        assert_eq!(s.state, State::SynRecv);
        assert_eq!(s.irs, 3000);
        assert_eq!(s.rcv_nxt, 3001);
    }

    #[test]
    fn data_transfer_in_order() {
        let mut c = Conn::new_client(50001);
        let mut s = Conn::new_listener(50002);
        c.open(Ipv6Addr::LOOPBACK, 50002, 1000, 0);
        complete_handshake(&mut c, &mut s, 0);

        // Client invia dati.
        c.write(b"hello", 0);
        let mut buf = [0u8; 200];
        let info = c.next_tx(&mut buf, 0).unwrap();
        assert_eq!(info.flags, FLAG_ACK);
        assert_eq!(info.len, 20 + 5);
        assert_eq!(&buf[20..25], b"hello");

        // Server riceve dati.
        let hdr = make_hdr(50001, 50002, info.seq, info.ack, FLAG_ACK);
        let sres = s.on_segment(&hdr, &buf[20..info.len], 0);
        assert_eq!(sres.data, Some(&b"hello"[..]));
        assert_eq!(s.readable(), 5);
        assert_eq!(sres.tx.unwrap().flags, FLAG_ACK);
        assert_eq!(sres.tx.unwrap().ack, info.seq.wrapping_add(5));

        // Server legge i dati.
        let mut out = [0u8; 10];
        assert_eq!(s.read(&mut out), 5);
        assert_eq!(&out[..5], b"hello");
        assert_eq!(s.readable(), 0);
    }

    #[test]
    fn duplicate_data_not_delivered_twice() {
        let mut c = Conn::new_client(50001);
        let mut s = Conn::new_listener(50002);
        c.open(Ipv6Addr::LOOPBACK, 50002, 1000, 0);
        complete_handshake(&mut c, &mut s, 0);

        c.write(b"data", 0);
        let mut buf = [0u8; 200];
        let info = c.next_tx(&mut buf, 0).unwrap();
        // Server riceve due volte lo stesso segmento.
        let hdr = make_hdr(50001, 50002, info.seq, info.ack, FLAG_ACK);
        let sres1 = s.on_segment(&hdr, &buf[20..info.len], 0);
        assert_eq!(sres1.data, Some(&b"data"[..]));
        let sres2 = s.on_segment(&hdr, &buf[20..info.len], 0);
        assert_eq!(sres2.data, None); // duplicato ignorato
        assert_eq!(s.readable(), 4);
    }

    #[test]
    fn rto_retransmits() {
        let mut c = Conn::new_client(50001);
        c.open(Ipv6Addr::LOOPBACK, 50002, 1000, 0);

        let mut buf = [0u8; 200];
        // Prima trasmissione del SYN.
        let syn = c.next_tx(&mut buf, 0).unwrap();
        assert_eq!(syn.flags, FLAG_SYN);

        // Nessuna risposta: RTO scade e ritrasmette, fino a MAX_RETRIES.
        let mut sent = 1;
        let mut now = c.rto_deadline;
        while let Some(info) = c.next_tx(&mut buf, now) {
            assert_eq!(info.flags, FLAG_SYN);
            sent += 1;
            now = c.rto_deadline;
            assert!(sent <= MAX_RETRIES + 1, "loop di ritrasmissione senza fine");
        }
        assert_eq!(usize::from(sent), MAX_RETRIES as usize, "SYN iniziale + ritrasmissioni");
        assert!(c.closed);
        assert_eq!(c.state, State::Closed);
    }

    #[test]
    fn ack_advances_send_window() {
        let mut c = Conn::new_client(50001);
        // Simula handshake completato e connessione stabilita.
        c.state = State::Established;
        c.irs = 2000;
        c.rcv_nxt = 2001;
        c.snd_una = 1001;
        c.snd_nxt = 1001;
        c.rto_deadline = 10000; // lontano, niente RTO

        c.write(b"abcdefghij", 0);
        let mut buf = [0u8; 200];
        let info1 = c.next_tx(&mut buf, 0).unwrap();
        assert_eq!(info1.seq, 1001);
        assert_eq!(info1.len, 20 + 10);
        // snd_nxt è avanzato di 10.
        assert_eq!(c.snd_nxt, 1011);

        // Ricevi ACK che conferma metà (5 byte): restano 5 byte nel buffer.
        let hdr = make_hdr(50002, 50001, 2001, 1006, FLAG_ACK);
        let _ = c.on_segment(&hdr, &[], 0);
        assert_eq!(c.snd_una, 1006);
        assert_eq!(c.send_buf.len(), 5); // fghij rimasti

        // RTO scaduto: ritrasmette da snd_una.
        let now = c.rto_deadline;
        let info2 = c.next_tx(&mut buf, now).unwrap();
        assert_eq!(info2.seq, 1006);
        // Payload dovrebbe essere "fghij".
        assert_eq!(&buf[20..25], b"fghij");
    }

    #[test]
    fn fin_close_client() {
        let mut c = Conn::new_client(50001);
        c.state = State::Established;
        c.snd_una = 1000;
        c.snd_nxt = 1000;
        c.rcv_nxt = 2000;
        c.rto_deadline = 10000;

        let mut buf = [0u8; 200];
        c.close();
        assert_eq!(c.state, State::FinWait1);
        assert!(c.fin_queued);

        let info = c.next_tx(&mut buf, 0).unwrap();
        assert_eq!(info.flags, FLAG_FIN | FLAG_ACK);
        assert_eq!(info.seq, 1000); // FIN consuma un seq
        assert_eq!(c.fin_sent, true);
        assert_eq!(c.snd_nxt, 1001);

        // Ricevi ACK del FIN.
        let hdr = make_hdr(50002, 50001, 2000, 1001, FLAG_ACK);
        let res = c.on_segment(&hdr, &[], 0);
        assert_eq!(c.state, State::FinWait2);

        // Ricevi FIN dal server.
        let hdr = make_hdr(50002, 50001, 2000, 1001, FLAG_FIN);
        let res = c.on_segment(&hdr, &[], 0);
        assert!(res.fin_recv);
        assert_eq!(c.state, State::TimeWait);

        // TimeWait scade → Closed.
        c.on_tick(c.rto_deadline + 1);
        assert_eq!(c.state, State::Closed);
        assert!(c.closed);
    }

    #[test]
    fn rst_closes_immediately() {
        let mut c = Conn::new_client(50001);
        c.state = State::Established;

        let hdr = make_hdr(50002, 50001, 2000, 1000, FLAG_RST);
        let res = c.on_segment(&hdr, &[], 0);
        assert!(res.rst);
        assert!(c.closed);
        assert_eq!(c.state, State::Closed);
    }

    #[test]
    fn seq_wraparound_comparison() {
        // 0x10 è dopo 0xFFFFFFF0 (wraparound).
        assert!(Conn::seq_after(0x0000_0010, 0xFFFF_FFF0));
        // 0xFFFFFFF0 NON è dopo 0x0000_0000.
        assert!(!Conn::seq_after(0xFFFF_FFF0, 0x0000_0000));
        assert!(Conn::seq_before_or_eq(1000, 2000));
        assert!(!Conn::seq_before_or_eq(2000, 1000));
    }

    #[test]
    fn write_respects_buffer_limit() {
        let mut c = Conn::new_client(50001);
        c.state = State::Established;
        c.snd_una = 0;
        c.snd_nxt = 0;
        c.rto_deadline = 10000;

        let data = [0xAAu8; SEND_BUF * 2];
        let n = c.write(&data, 0);
        assert_eq!(n, SEND_BUF);
        assert_eq!(c.send_buf.len(), SEND_BUF);
    }

    #[test]
    fn segment_toward_closed_gets_rst() {
        let mut c = Conn::new_client(50001);
        // Connessione mai aperta: Closed.
        assert_eq!(c.state, State::Closed);
        let hdr = make_hdr(50002, 50001, 100, 0, FLAG_SYN);
        let res = c.on_segment(&hdr, &[], 0);
        let tx = res.tx.expect("RST verso porta chiusa");
        assert_eq!(tx.flags, FLAG_RST | FLAG_ACK);
        assert_eq!(tx.ack, 101);
    }

    #[test]
    fn out_of_order_rejected_then_recovered() {
        let mut c = Conn::new_client(50001);
        let mut s = Conn::new_listener(50002);
        c.open(Ipv6Addr::LOOPBACK, 50002, 1000, 0);
        complete_handshake(&mut c, &mut s, 0);

        // Il client invia 10 byte.
        c.write(b"0123456789", 0);
        let mut buf = [0u8; 200];
        let info = c.next_tx(&mut buf, 0).unwrap();
        assert_eq!(info.len, 30);

        // Il server riceve prima la seconda metà (fuori ordine): scartata.
        let second = &buf[20 + 5..30];
        let hdr2 = make_hdr(50001, 50002, info.seq.wrapping_add(5), info.ack, FLAG_ACK);
        let res2 = s.on_segment(&hdr2, second, 0);
        assert_eq!(res2.data, None, "fuori ordine scartato");
        assert_eq!(s.readable(), 0);
        // L'ACK ripete rcv_nxt (nessun avanzamento).
        assert_eq!(res2.tx.unwrap().ack, info.seq);

        // Ritrasmissione completa (go-back-N): ora è in ordine.
        let hdr1 = make_hdr(50001, 50002, info.seq, info.ack, FLAG_ACK);
        let res1 = s.on_segment(&hdr1, &buf[20..30], 0);
        assert_eq!(res1.data, Some(&b"0123456789"[..]));
        assert_eq!(s.readable(), 10);
    }

    #[test]
    fn window_zero_pauses_then_reopens() {
        let mut c = Conn::new_client(50001);
        c.state = State::Established;
        c.snd_una = 1000;
        c.snd_nxt = 1000;
        c.rcv_nxt = 2000;
        c.rto_deadline = u64::MAX / 2;

        // Il peer annuncia finestra 0 (ACK valido che non conferma nulla).
        let mut hdr = make_hdr(50002, 50001, 2000, 1000, FLAG_ACK);
        hdr.window = 0;
        let _ = c.on_segment(&hdr, &[], 0);
        assert_eq!(c.snd_wnd, 0);

        c.write(b"hello", 0);
        let mut buf = [0u8; 200];
        assert!(c.next_tx(&mut buf, 0).is_none(), "finestra 0: niente da inviare");

        // Il peer riapre la finestra: i dati partono.
        let hdr = make_hdr(50002, 50001, 2000, 1000, FLAG_ACK);
        let mut hdr = hdr;
        hdr.window = 1024;
        let _ = c.on_segment(&hdr, &[], 0);
        let info = c.next_tx(&mut buf, 0).expect("dati dopo riapertura");
        assert_eq!(info.len, 25);
        assert_eq!(&buf[20..25], b"hello");
    }

    #[test]
    fn arbitrary_input_never_panics() {
        // Segmenti spuri in ogni stato: l'invariante è "nessun panic".
        let states = [
            State::Closed,
            State::Listen,
            State::SynSent,
            State::SynRecv,
            State::Established,
            State::FinWait1,
            State::FinWait2,
            State::CloseWait,
            State::LastAck,
            State::TimeWait,
        ];
        let flags = [
            0u8,
            FLAG_SYN,
            FLAG_ACK,
            FLAG_FIN,
            FLAG_RST,
            FLAG_SYN | FLAG_ACK,
            FLAG_FIN | FLAG_ACK,
            FLAG_RST | FLAG_ACK,
            0xff,
        ];
        for st in states {
            for f in flags {
                for seq in [0u32, 1, 0xffff_ffff, 0x8000_0000] {
                    for ack in [0u32, 1, 0xffff_ffff] {
                        let mut c = Conn::new_client(50001);
                        c.state = st;
                        c.remote_port = 50002;
                        c.snd_una = 1000;
                        c.snd_nxt = 1000;
                        c.rcv_nxt = 2000;
                        c.rto_deadline = 1_000_000;
                        let hdr = make_hdr(50002, 50001, seq, ack, f);
                        let _ = c.on_segment(&hdr, b"xyz", 0);
                        let mut buf = [0u8; 64];
                        let _ = c.next_tx(&mut buf, 2_000_000);
                        c.on_tick(3_000_000);
                    }
                }
            }
        }
    }

    #[test]
    fn close_wait_then_fin() {        let mut c = Conn::new_client(50001);
        c.state = State::CloseWait;
        c.snd_una = 1000;
        c.snd_nxt = 1000;
        c.rcv_nxt = 2000;
        c.rto_deadline = 10000;

        let mut buf = [0u8; 200];
        c.close();
        assert_eq!(c.state, State::LastAck);

        let info = c.next_tx(&mut buf, 0).unwrap();
        assert_eq!(info.flags, FLAG_FIN | FLAG_ACK);

        // Ricevi ACK del FIN → Closed.
        let hdr = make_hdr(50002, 50001, 2000, 1001, FLAG_ACK);
        let res = c.on_segment(&hdr, &[], 0);
        assert_eq!(c.state, State::Closed);
        assert!(c.closed);
    }
}
