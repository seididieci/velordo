use super::*;
use crate::*;

// ── FS async 1-in-volo (Fase 13) ──────────────────────────────────
//
// La variante async del percorso FS: scrive il request frame, poi `send_async`
// (non bloccante) e ritorna il req_id da raccogliere con `fs_collect`. Il
// formato frame NON ha lunghezza payload esplicita → al massimo 1 operazione
// in volo per processo (`FS_PENDING`): gli altri wrapper FS sincroni e la
// prossima async si rifiutano finche' non si raccoglie. `read_async` supporta
// un singolo round trip (count <= RING_MAX_PAYLOAD), coerente con read_fs che
// spezza le richieste piu' grandi.

/// `read_async(fd, count)`: come `read_fs` (un solo chunk) ma non blocca: scrive
/// il frame `R_READ` nel request ring, notifica cardo con `send_async` e
/// ritorna il `req_id` (>= 1) da passare a `fs_collect`/`fs_collect_msg`
/// (Fase 39: errore nativo invece di -1).
pub fn read_async(fd: i64, count: usize) -> Result<i64, Error> {
    let want = count.min(ring::RING_MAX_PAYLOAD);
    fs_op_async(FS_NOTIFY, R_READ, fd as u64, want as u64, &[])
}

/// `write_async(fd, data)`: come `write_fs` (un solo chunk <= RING_MAX_PAYLOAD)
/// ma non blocca: scrive il frame `R_WRITE` e notifica con `send_async`
/// (Fase 39: `Result` invece di req_id/-1). Il payload
/// resta nel request ring per i device remoti (consumato dal driver, come nel
/// percorso sincrono) — la collect legge il result frame come `write_fs`.
pub fn write_async(fd: i64, data: &[u8]) -> Result<i64, Error> {
    fs_op_async(FS_NOTIFY, R_WRITE, fd as u64, data.len() as u64, data)
}

/// `open_async(path, flags)`: come `open` ma non blocca (Fase 39: `Result`).
/// Da raccogliere con `fs_collect_msg(..., is_read=false)`: fd o errore.
pub fn open_async(path: &str, flags: u32) -> Result<i64, Error> {
    fs_op_async(FS_NOTIFY, R_OPEN, path.len() as u64, flags as u64, path.as_bytes())
}

/// `fs_register_async(prefix)`: come `fs_register` ma non blocca: scrive il
/// frame `R_REGISTER` e notifica con `send_async` (Fase 39: `Result`).
/// Da raccogliere con `fs_collect_msg(..., is_read=false)`: `Ok` = registrato.
/// NOTA: a differenza delle altre op FS, la registrazione viaggia sul tag IPC
/// `FS_REGISTER` (non `FS_NOTIFY`): cardo la serve in un handler dedicato.
pub fn fs_register_async(prefix: &[u8]) -> Result<i64, Error> {
    fs_op_async(FS_REGISTER, R_REGISTER, prefix.len() as u64, 0, prefix)
}

/// `fs_buf_reg_async()`: (re)invia gli indirizzi dei ring (handshake
/// `FS_BUF_REG`) con `send_async`, senza frame e senza bloccare. Serve ai
/// driver-server dopo un restart di cardo o un cambio canale: la tabella
/// `rings` di cardo e' indicizzata per canale, quindi sotto un NUOVO canale
/// serve un NUOVO handshake (altrimenti ogni op prende `ERR_NOHANDSHAKE`).
/// Ritorna il `req_id` o -1 (ring mai allocati / op in volo / send fallita).
/// Collect: messaggio con req_id matchato e w0==0 (nessun frame nel ring:
/// NON usare `fs_collect_msg`). Chiama `fs_init()` prima (alloca i ring alla
/// prima volta; le chiamate dopo sono no-op che riusano le pagine).
/// Fase 39: `Result` invece di req_id/-1.
pub fn fs_buf_reg_async() -> Result<i64, Error> {
    session::fs_gate()?;
    let (req_phys, resp_phys) = session::ring_phys();
    if req_phys == 0 || resp_phys == 0 {
        return Err(Error::NotReady);
    }
    let c = session::fs_chan();
    if c < 0 {
        return Err(Error::NotReady);
    }
    match ipc::send_async(c as u64, FS_BUF_REG, req_phys, resp_phys) {
        Ok(req) => {
            session::guard_take(req);
            Ok(req)
        }
        Err(_) => {
            session::chan_set(-1);
            Err(Error::RingFull)
        }
    }
}

/// Op FS async a basso livello (Fase 15, driver-server): scrive un frame
/// (frame_tag,w0,w1,payload) nel request ring e notifica cardo con
/// `send_async` sul tag IPC `ipc_tag` (`FS_NOTIFY` per le op, `FS_REGISTER`
/// per la registrazione driver).
/// Ritorna il `req_id` (>= 1); errori nativi invece di -1 (Fase 39: `Pending`
/// se un'op e' in volo, `Invalid` se il payload eccede, `RingFull` se il ring
/// e' pieno o la send fallisce con rollback del frame). Mai bloccante.
/// `read_async`/`write_async` sono wrapper tipizzati; i driver usano questa
/// direttamente per tag senza wrapper (es. `R_REGISTER`).
pub fn fs_op_async(ipc_tag: u64, frame_tag: u32, w0: u64, w1: u64, payload: &[u8]) -> Result<i64, Error> {
    session::fs_gate()?;
    if payload.len() > ring::RING_MAX_PAYLOAD {
        return Err(Error::Invalid);
    }
    if !ring::req_ring_write(frame_tag, w0, w1, payload) {
        return Err(Error::RingFull);
    }
    let c = session::fs_chan();
    if c < 0 {
        ring::req_ring_rollback(20 + payload.len());
        return Err(Error::NotReady);
    }
    match ipc::send_async(c as u64, ipc_tag, 0, 0) {
        Ok(req) => {
            session::guard_take(req);
            Ok(req)
        }
        Err(_) => {
            // Notifica non consegnata (coda piena = backpressure, o canale
            // morto: peer riavviato e cache FS_CHAN stale): invalida la cache
            // cosi' il prossimo tentativo ri-risolve per nome. Come `fs_send`
            // fa sul path sincrono (Fase 14, init-restart).
            session::chan_set(-1);
            // Togli il frame dal request ring.
            ring::req_ring_rollback(20 + payload.len());
            Err(Error::RingFull)
        }
    }
}

/// `fs_collect(req, dst, cap)`: raccoglie la risposta alla `read_async` che ha
/// ritornato `req`. Attende (bloccante, FIFO) la reply async con quel req_id,
/// poi legge il response frame (payload) in `dst`. Ritorna i byte letti;
/// errori nativi invece di -1 (Fase 39).
/// Resetta il guard 1-in-volo (anche su errore).
/// NOTA (Fase 14): l'attesa filtra per canale (`wait_reply_chan` sul canale FS
/// cachato, stabile per vita del processo): le EXIT_NOTIFY *stale* di altri
/// peer morti vengono saltate, solo la morte del server FS da' errore. Niente
/// retry automatico qui (il retry-once vive in `fs_send`, Fase 14.12; il
/// restart in init).
pub fn fs_collect(req: i64, dst: &mut [u8], cap: usize) -> Result<usize, Error> {
    // Invariante: collect segue una read_async riuscita, che ha risolto e
    // cachato FS_CHAN (>= 0) prima di registrare FS_PENDING.
    let fchan = session::chan_get().max(0) as u64;
    let msg = match ipc::wait_reply_chan(req, fchan) {
        Ok(m) => m,
        Err(ipc::WaitReplyError::ServerDied { pid, code }) => {
            // Server morto mentre attendevamo: niente retry automatico qui
            // (scelta voluta: il retry-once vive in `fs_send`); il chiamante
            // vede l'errore. Azzera i
            // ring: il frame async e' orfano (mai consumato o senza reply) e
            // disallineerebbe le op successive; la prossima op riscrive.
            println!("[civis] fs_collect: server pid {} morto (code {}), req {} perso", pid, code, req);
            unsafe {
                session::ring_reset(ring::REQ_RING_VA);
                session::ring_reset(ring::RESP_RING_VA);
            }
            session::guard_clear();
            return Err(Error::ServerDied);
        }
        Err(e) => {
            session::guard_clear();
            return Err(Error::from(e));
        }
    };
    session::guard_clear();
    fs_collect_msg(&msg, dst, cap, true).map(|n| n as usize)
}

/// `fs_collect_msg(m, dst, cap, is_read)`: raccoglie SENZA BLOCCARE la risposta
/// a una `read_async`/`write_async`/`fs_op_async` il cui messaggio e' GIA'
/// stato ricevuto con `recv_poll` (il chiamante verifica `m.req_id == req` e
/// `m.req_id > 0`). Legge il response frame come `fs_collect` (read, con
/// payload in `dst`) o come `write_fs` (write/result-only: consume + result),
/// resetta il guard 1-in-volo e ritorna byte/result; errori nativi (Fase 39).
/// Per un driver-server (tty) che non puo' mai bloccarsi: serve le relay DEV
/// nel mentre invece di attendere in `wait_reply` (ciclo cardo<->driver).
pub fn fs_collect_msg(m: &IpcMsg, dst: &mut [u8], cap: usize, is_read: bool) -> Result<i64, Error> {
    session::guard_clear();
    if is_read {
        if m.w0 == ring::ERR {
            if ring::resp_ring_read().is_some() {
                ring::resp_ring_consume(16);
            }
            return Err(Error::Failed);
        }
        match ring::resp_ring_read() {
            Some((result, _w1, payload_len)) => {
                // Stessa disciplina dei wrapper sync: consuma sempre il frame
                // (anche a diniego), poi interpreta — mai disallineamenti.
                let checked = session::fs_reply_check(result);
                let avail = checked
                    .map(|v| (v as usize).min(payload_len).min(cap))
                    .unwrap_or(0);
                if avail > 0 {
                    ring::resp_ring_read_payload(&mut dst[..avail], avail);
                } else {
                    ring::resp_ring_consume(16);
                }
                checked.map(|_| avail as i64)
            }
            None => session::fs_reply_check(m.w0).map(|v| v as i64),
        }
    } else {
        match ring::resp_ring_read() {
            Some((result, _, _)) => {
                ring::resp_ring_consume(16);
                session::fs_reply_check(result).map(|v| v as i64)
            }
        None => Err(Error::NotReady),
        }
    }
}

/// Scarta un'op async in volo (Fase 15, driver-server): azzera il guard e i
/// ring (come il path ServerDied di `fs_collect`). Da chiamare quando il
/// server muore (EXIT_NOTIFY) prima di riaprire i peer: frame orfani
/// disallineerebbero le op successive; la prossima op riscrive da zero.
pub fn fs_abort_pending() {
    session::guard_clear();
    unsafe {
        session::ring_reset(ring::REQ_RING_VA);
        session::ring_reset(ring::RESP_RING_VA);
    }
}
