use super::*;

/// Messaggio ricevuto da `recv`/`recv_poll` (ADR-0008, Fase 13): porta il
/// canale sorgente (0 = canale di nascita / parent) per le richieste, oppure
/// il request-id (negativo) per una risposta async.
#[derive(Clone, Copy, Debug)]
pub struct IpcMsg {
    /// Per le richieste: il canale sorgente. Per le risposte async resta 0
    /// (il canale e' gia' noto al client) e vale `req_id`.
    pub channel: u64,
    /// Request-id (Fase 13): `> 0` se `recv` ha ricevuto una risposta async a
    /// `-req_id`; `0` se e' una richiesta normale (il kernel non lo espone ai
    /// server che rispondono per reply implicita).
    pub req_id: i64,
    pub tag: u64,
    pub w0: u64,
    pub w1: u64,
}

/// Risposta ricevuta da `send` (ADR-0008).
#[derive(Clone, Copy, Debug)]
pub struct IpcReply {
    pub tag: u64,
    pub w0: u64,
    pub w1: u64,
}

/// `send(channel, tag, w0, w1)`: invia il messaggio sul canale (0 = canale di
/// nascita verso il parent) e resta bloccato finche' il peer non risponde con
/// `reply`. Fallisce solo a canale morto/peer morto (`ServerDied`, Fase 39).
#[inline]
pub fn send(channel: u64, tag: u64, w0: u64, w1: u64) -> Result<IpcReply, Error> {
    let (rax, _rdi, rsi, rdx, r10) =
        unsafe { syscall4_out(SYS_SEND, channel, tag, w0, w1) };
    if rax < 0 {
        return Err(Error::ServerDied);
    }
    Ok(IpcReply { tag: rsi, w0: rdx, w1: r10 })
}

/// Fase 13 — `send_async(channel, tag, w0, w1)`: come `send` ma NON blocca il
/// mittente: ritorna subito il `req_id` (>= 1) della richiesta, o `Err` se la
/// coda del peer e' piena (backpressure) / canale morto. La risposta del peer
/// va raccolta con `wait_reply(req_id)` o con `recv`/`recv_poll` (un messaggio
/// con `req_id == req_id` atteso).
///
/// Vincolo del primo passo: non mescolare `send` sincrone e richieste async
/// in volo per lo stesso processo; raccogliere le risposte in ordine (FIFO).
/// Fallimento = `RingFull` (Fase 39: coda piena e canale morto indistinguibili).
#[inline]
pub fn send_async(channel: u64, tag: u64, w0: u64, w1: u64) -> Result<i64, Error> {
    let rax = unsafe { syscall4(SYS_SEND_ASYNC, channel, tag, w0, w1) };
    if rax < 0 {
        return Err(Error::RingFull);
    }
    Ok(rax)
}

/// Fase 14 — errore di `wait_reply(req_id)`: perche' la reply attesa non e'
/// arrivata. `ServerDied` porta il pid e l'exit code del processo morto
/// (notifica unificata `EXIT_NOTIFY`): il chiamante sa che il server e' morto
/// e puo' gestirlo (re-lookup, retry, uscita). NOTA: significa "UN peer e'
/// morto", non necessariamente il server atteso — una notifica stale di un
/// server precedente puo' arrivare dopo un re-lookup; confrontare `pid` se
/// serve precisione (niente retry automatico qui: vedi `fs_send`, Fase 14.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReplyError {
    /// Il processo che avrebbe dovuto rispondere e' morto (pid, exit code).
    ServerDied { pid: u64, code: i64 },
    /// Arrivato un altro messaggio fuori ordine (richiesta inattesa).
    UnexpectedMsg,
    /// Errore di `recv`.
    RecvFailed,
}

/// Fase 13 — `wait_reply(req_id)`: resta bloccato finche' non arriva la
/// risposta async alla richiesta `req_id` (un messaggio ricevuto con
/// `req_id == req_id`), poi la restituisce. Presuppone l'ordine FIFO: se
/// arriva qualcos'altro (fuori ordine o una richiesta) restituisce `Err`.
/// Se arriva una notifica `EXIT_NOTIFY` (Fase 14, notifica unificata) il
/// server e' morto: ritorna `Err(ServerDied)` subito invece di attendere
/// per sempre una reply che non arrivera' mai.
#[inline]
pub fn wait_reply(req_id: i64) -> Result<IpcMsg, WaitReplyError> {
    loop {
        match recv() {
            Ok(m) => {
                if m.req_id == req_id {
                    return Ok(m);
                }
                if sys::is_exit_notify(&m) {
                    return Err(WaitReplyError::ServerDied {
                        pid: m.w1,
                        code: m.w0 as i64,
                    });
                }
                // Fuori ordine / richiesta: non gestito nel primo passo.
                return Err(WaitReplyError::UnexpectedMsg);
            }
            Err(_) => return Err(WaitReplyError::RecvFailed),
        }
    }
}

/// Variante di `wait_reply` che filtra per canale (Fase 14): le notifiche
/// `EXIT_NOTIFY` arrivate su un canale DIVERSO da `chan` sono stale (tardive,
/// di altri peer morti — il parent le riceve tutte) e vengono saltate; solo
/// la notifica sul canale atteso diventa `Err(ServerDied)`. Usato da
/// `fs_collect`, dove il canale FS e' stabile (cachato in `FS_CHAN`) ma il
/// pid del server non e' noto al client.
#[inline]
pub fn wait_reply_chan(req_id: i64, chan: u64) -> Result<IpcMsg, WaitReplyError> {
    loop {
        match recv() {
            Ok(m) => {
                if m.req_id == req_id {
                    return Ok(m);
                }
                if sys::is_exit_notify(&m) {
                    if m.channel != chan {
                        continue; // stale: morte di un altro peer
                    }
                    return Err(WaitReplyError::ServerDied {
                        pid: m.w1,
                        code: m.w0 as i64,
                    });
                }
                return Err(WaitReplyError::UnexpectedMsg);
            }
            Err(_) => return Err(WaitReplyError::RecvFailed),
        }
    }
}

/// `recv()`: resta bloccato finche' non arriva un messaggio, poi lo restituisce.
/// Per una richiesta porta `channel` (canale sorgente); per una risposta async
/// (Fase 13) `req_id` = id della richiesta a cui risponde (e `channel` = 0).
/// Fallimento = `ServerDied` (Fase 39; in pratica non fallisce mai).
#[inline]
pub fn recv() -> Result<IpcMsg, Error> {
    let (rax, rdi, rsi, rdx, r10) = unsafe { syscall4_out(SYS_RECV, 0, 0, 0, 0) };
    if rax < 0 {
        return Err(Error::ServerDied);
    }
    Ok(decode_ipc_msg(rdi, rsi, rdx, r10))
}

/// Fase 13 — `recv_poll()`: come `recv` ma se la coda e' vuota ritorna `None`
/// subito (non blocca). Il campo `req_id` di `Some(msg)` e' `> 0` per una
/// risposta async, `0` per una richiesta.
#[inline]
pub fn recv_poll() -> Option<IpcMsg> {
    let (rax, rdi, rsi, rdx, r10) = unsafe { syscall4_out(SYS_RECV_NONBLOCK, 0, 0, 0, 0) };
    if rax < 0 {
        return None;
    }
    Some(decode_ipc_msg(rdi, rsi, rdx, r10))
}

/// Decodifica i registri di ritorno di `recv`/`recv_poll`: se `rdi` (signed) e'
/// negativo e' una risposta async e il request-id della richiesta originale e'
/// `-(rdi as i64)`; altrimenti `rdi` e' il canale sorgente di una richiesta.
#[inline]
fn decode_ipc_msg(rdi: u64, rsi: u64, rdx: u64, r10: u64) -> IpcMsg {
    let signed = rdi as i64;
    if signed < 0 {
        // Risposta async: il kernel espone il req_id negativo in rdi.
        IpcMsg { channel: 0, req_id: -signed, tag: rsi, w0: rdx, w1: r10 }
    } else {
        // Richiesta normale: rdi = canale sorgente.
        IpcMsg { channel: rdi, req_id: 0, tag: rsi, w0: rdx, w1: r10 }
    }
}

/// `reply(tag, w0, w1)`: risponde al mittente del messaggio che stiamo
/// elaborando (ADR-0008). Fallimento = `ServerDied` (Fase 39: il peer e' morto
/// mentre lo servivamo; mai reply senza `recv` prima).
#[inline]
pub fn reply(tag: u64, w0: u64, w1: u64) -> Result<(), Error> {
    let rax = unsafe { syscall4(SYS_REPLY, tag, w0, w1, 0) };
    if rax < 0 {
        return Err(Error::ServerDied);
    }
    Ok(())
}

/// Canale di nascita: il figlio lo usa come destinazione per parlare col parent
/// (ADR-0008). `spawn` ritorna il channel id (lato parent) verso il figlio.
pub const CHANNEL_PARENT: u64 = syscall_numbers::CHANNEL_PARENT;

/// S3.2b — Richiede a cardo la lista dei bucket di tipo "bin" (eseguibili).
/// Cardo risponde scrivendo i nomi separati da `\n` nel response ring del
/// client (protocollo BIN_LIST, tag 0x41: nessun payload, w0=w1=0).
/// Ritorna il vettore dei bucket (byte); `NotFound` se cardo non e' disponibile.
pub fn bin_list() -> Result<alloc::vec::Vec<alloc::vec::Vec<u8>>, Error> {
    // Risolve il canale verso cardo.
    let chan = match service_lookup(Service::Cardo) {
        Ok(c) => c as u64,
        Err(_) => return Err(Error::NotFound),
    };

    // Invia richiesta BIN_LIST: tag 0x41, w0=w1=0 (nessun payload).
    let _ = ipc::send(chan, BIN_LIST, 0, 0);

    // Legge la risposta dal response ring del client.
    // Formato: [count:8][w1:8][payload: bucket names separated by '\n']
    use crate::fs::ring::{resp_ring_read, resp_ring_consume};
    let mut buckets = alloc::vec::Vec::new();

    loop {
        match resp_ring_read() {
            Some((size, _w1, payload_len)) => {
                // Consuma l'intestazione (16 byte) per liberare il ring.
                resp_ring_consume(16);
                // Leggo il payload: e' una stringa con nomi separati da '\n'.
                let mut buf = alloc::vec::Vec::new();
                if payload_len > 0 {
                    use crate::fs::ring::resp_ring_read_payload;
                    let to_read = payload_len.min(crate::fs::ring::RING_MAX_PAYLOAD as usize);
                    resp_ring_read_payload(&mut buf, to_read);
                }
                // Suddividi per '\n' e aggiungi solo i nomi non vuoti.
                let s = core::str::from_utf8(&buf).unwrap_or("");
                for part in s.split('\n') {
                    if !part.is_empty() {
                        buckets.push(part.as_bytes().to_vec());
                    }
                }
                // Se count (size) e' 0 o la lista e' vuota, esci.
                if size == 0 || buckets.is_empty() {
                    break;
                }
            }
            None => break, // Niente nella coda: tutto ok, lista vuota o non ancora arrivata.
        }
    }

    Ok(buckets)
}
