//! Client delle **sessioni di terminale** del DE (D8c1, ADR-0089).
//!
//! Una sessione (`littera`) vive nel servizio `console`: modello a celle,
//! scrollback, parser ANSI, input ed eco. Questo modulo da' al resto del
//! sistema la capability per parlarle: la **mailbox** e' una pagina condivisa
//! (shm) creata dal server, mappata in RW dal client, con due meta'
//! (`sess-proto`): OUT client→sessione (feed), IN sessione→client (input e
//! dump). Il testo passa solo da qui: `/dev/console` non esiste piu' (D8c2).
//!
//! La mailbox e' **dell'attachment** (un canale, un buffer): due clienti della
//! stessa sessione non si sovrascrivono i byte, come il ring FS per processo.
//! L'unico sink di stdout dei programmi e' [`out`] (sessione attestata, poi
//! default, poi seriale): il mirror seriale lo fa il server, non il client.
//!
//! Il meccanismo e' **neutro**: nessun verbo POSIX e nessuna nozione di tile
//! o di DE. La sessione di un processo si trova per **attestazione del pid**
//! ([`whoami`]): il kernel dice chi sta chiamando, il server risolve; nessun
//! handle passa dallo spawn, nessuna variabile d'ambiente.

use core::ptr;

use crate::sys::{munmap, shm_map};
use crate::{
    send, Service, SESS_ATTACH_DEFAULT, SESS_BIND, SESS_CLOSE, SESS_CREATE, SESS_DUMP,
    SESS_ECHO, SESS_FOCUS, SESS_INFO, SESS_READ, SESS_RESIZE, SESS_STATE, SESS_STATS,
    SESS_TEST_INPUT, SESS_WHOAMI, SESS_WRITE,
};
use sess_proto::{IN_OFF, MAILBOX_BYTES, MAX_CHUNK, OUT_OFF};

/// Esito di un'operazione sulla sessione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessError {
    /// Il servizio `console` non e' raggiungibile.
    ServiceDead,
    /// Il servizio ha rifiutato la richiesta (sessione inesistente, nessun
    /// attachment, quota raggiunta, policy).
    Denied,
    /// Il processo non ha (piu') una sessione attestata.
    NotAttached,
    /// Errore IPC (canale rotto).
    Ipc,
}

/// Cosa sa la sessione di se stessa.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// Colonne del modello.
    pub cols: u16,
    /// Righe del modello.
    pub rows: u16,
    /// Eco dell'input accesa.
    pub echo: bool,
    /// La sessione e' quella disegnata (in focus).
    pub focus: bool,
}

/// La capability verso una sessione: id stabile + mailbox mappata in RW.
///
/// Il `Drop` smappa la mailbox (a pagina): chi non chiama [`Session::close`]
/// non lascia comunque una mappatura appesa.
pub struct Session {
    /// Id della sessione (`0` = lo schermo).
    pub id: u32,
    /// Id della shm della mailbox (per diagnostica; server-owned).
    pub shm: u32,
    va: usize,
}

impl Session {
    /// Mappa la mailbox consegnata dal server e costruisce la capability.
    fn attach_to(id: u32, shm: u32) -> Result<Session, SessError> {
        let va = shm_map(shm, 0, crate::PROT_READ | crate::PROT_WRITE)
            .map_err(|_| SessError::Ipc)?;
        Ok(Session { id, shm, va })
    }

    /// Feed: scrive `bytes` nel terminale (a blocchi di [`MAX_CHUNK`]).
    ///
    /// La chiamata e' **sincrona**: il server consuma la meta' OUT prima di
    /// rispondere, quindi non serve un cursore di coda.
    pub fn write(&self, bytes: &[u8]) -> Result<usize, SessError> {
        self.write_with(bytes, false)
    }

    /// Feed **solo schermo** (D8c2): non entra nel mirror seriale del server
    /// (`SESS_WRITE.w1 = 1`). E' per l'eco dei tasti, i ridisegni e i test:
    /// il log seriale resta il transcript dell'output, non dei tasti.
    pub fn write_screen(&self, bytes: &[u8]) -> Result<usize, SessError> {
        self.write_with(bytes, true)
    }

    fn write_with(&self, bytes: &[u8], screen_only: bool) -> Result<usize, SessError> {
        let mut done = 0usize;
        while done < bytes.len() {
            let n = (bytes.len() - done).min(MAX_CHUNK);
            unsafe {
                ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(done),
                    (self.va + OUT_OFF) as *mut u8,
                    n,
                );
            }
            let r = send(service_chan()?, SESS_WRITE, n as u64, screen_only as u64)
                .map_err(|_| SessError::Ipc)?;
            if r.w0 == !0u64 {
                return Err(SessError::Denied);
            }
            done += n;
        }
        Ok(done)
    }

    /// Legge fino a `out.len()` byte di input in attesa.
    pub fn read(&self, out: &mut [u8]) -> Result<usize, SessError> {
        let want = out.len().min(MAX_CHUNK);
        let r = send(service_chan()?, SESS_READ, want as u64, 0).map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        let n = (r.w0 as usize).min(want);
        unsafe {
            ptr::copy_nonoverlapping((self.va + IN_OFF) as *const u8, out.as_mut_ptr(), n);
        }
        Ok(n)
    }

    /// Copia la coda del testo visibile: `(scritti, totale)`.
    pub fn dump(&self, max: usize, out: &mut [u8]) -> Result<(usize, usize), SessError> {
        let want = max.min(out.len()).min(MAX_CHUNK);
        let r = send(service_chan()?, SESS_DUMP, self.id as u64, want as u64)
            .map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        let n = (r.w0 as usize).min(want);
        unsafe {
            ptr::copy_nonoverlapping((self.va + IN_OFF) as *const u8, out.as_mut_ptr(), n);
        }
        Ok((n, r.w1 as usize))
    }

    /// Geometria, eco e focus della sessione.
    pub fn info(&self) -> Result<Info, SessError> {
        let r = send(service_chan()?, SESS_INFO, self.id as u64, 0).map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        let g = r.w0 as u32;
        Ok(Info {
            cols: (g >> 16) as u16,
            rows: (g & 0xFFFF) as u16,
            echo: (r.w1 & 1) != 0,
            focus: (r.w1 & 2) != 0,
        })
    }

    /// Celle non vuote + CRC del testo delle ultime `limit` celle.
    pub fn state(&self, limit: usize) -> Result<(usize, u32), SessError> {
        let r = send(service_chan()?, SESS_STATE, self.id as u64, limit as u64)
            .map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        Ok((r.w0 as usize, r.w1 as u32))
    }

    /// Ridimensiona il modello (reflow: il testo non si perde, D4).
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), SessError> {
        // ABI: `SESS_RESIZE {cols<<16|rows}` — la sessione e' quella attaccata
        // (l'id non viaggia: e' implicito nell'attachment, come l'eco).
        let packed = ((cols as u64) << 16) | rows as u64;
        let r = send(service_chan()?, SESS_RESIZE, packed, 0).map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        Ok(())
    }

    /// Accende/spegne l'eco dell'input (flag di sessione).
    pub fn echo(&self, on: bool) -> Result<(), SessError> {
        let r = send(service_chan()?, SESS_ECHO, on as u64, 0).map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        Ok(())
    }

    /// Chiude la sessione (mai la 0: il server nega). Il `Drop` smappa la
    /// mailbox.
    pub fn close(self) -> Result<(), SessError> {
        let r = send(service_chan()?, SESS_CLOSE, self.id as u64, 0).map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        Ok(())
    }

    /// **Solo test**: inietta input nella sessione (come la tastiera, D8d).
    pub fn test_input(&self, bytes: &[u8]) -> Result<(), SessError> {
        if bytes.len() > MAX_CHUNK {
            return Err(SessError::Denied);
        }
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), (self.va + OUT_OFF) as *mut u8, bytes.len());
        }
        let r = send(service_chan()?, SESS_TEST_INPUT, bytes.len() as u64, 0)
            .map_err(|_| SessError::Ipc)?;
        if r.w0 == !0u64 {
            return Err(SessError::Denied);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = munmap(self.va, MAILBOX_BYTES);
    }
}

/// Crea una sessione **legata** al processo chiamante (owner = pid): da li'
/// [`whoami`] la trova.
pub fn create() -> Result<Session, SessError> {
    create_flags(0)
}

/// Crea una sessione **non legata**: il chiamante deve essere >= ThirdParty e
/// legarla a un figlio con [`bind`] (e' cio' che fara' il DE).
pub fn create_unbound() -> Result<Session, SessError> {
    create_flags(1)
}

fn create_flags(flags: u64) -> Result<Session, SessError> {
    let r = send(service_chan()?, SESS_CREATE, flags, 0).map_err(|_| SessError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(SessError::Denied);
    }
    Session::attach_to(r.w0 as u32, r.w1 as u32)
}

/// La sessione **del processo chiamante** (attestata dal kernel via pid).
pub fn whoami() -> Result<Session, SessError> {
    let r = send(service_chan()?, SESS_WHOAMI, 0, 0).map_err(|_| SessError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(SessError::NotAttached);
    }
    Session::attach_to(r.w0 as u32, r.w1 as u32)
}

/// L'unico ingresso **nominato**: la sessione 0 (lo schermo).
pub fn attach_default() -> Result<Session, SessError> {
    let r = send(service_chan()?, SESS_ATTACH_DEFAULT, 0, 0).map_err(|_| SessError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(SessError::Denied);
    }
    Session::attach_to(r.w0 as u32, r.w1 as u32)
}

/// Lega `pid` alla sessione `id` (solo >= ThirdParty). Senza argomenti di
/// spawn: il DE binda il figlio appena nato.
pub fn bind(id: u32, pid: i64) -> Result<(), SessError> {
    let r = send(service_chan()?, SESS_BIND, id as u64, pid as u64).map_err(|_| SessError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(SessError::Denied);
    }
    Ok(())
}

/// La sessione disegnata (D8c1: solo la 0).
pub fn focus(id: u32) -> Result<(), SessError> {
    let r = send(service_chan()?, SESS_FOCUS, id as u64, 0).map_err(|_| SessError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(SessError::Denied);
    }
    Ok(())
}

/// `(sessioni vive, sessione in focus)`.
pub fn stats() -> Result<(usize, usize), SessError> {
    let r = send(service_chan()?, SESS_STATS, 0, 0).map_err(|_| SessError::Ipc)?;
    Ok((r.w0 as usize, r.w1 as usize))
}

// ── Il sink di default (D8c2) ────────────────────────────────────────

/// Sessione corrente del processo, risolta **una volta** e riusata: la
/// risoluzione costa due IPC (attestazione + eventuale default), il feed una
/// sola. Una cache stantia (la `console` e' ripartita: la mailbox e' morta e
/// l'attachment non esiste piu') si butta al primo errore.
static mut DEFAULT: Option<Session> = None;

/// Scrive `bytes` sulla **sessione del processo** (attestata dal kernel) o, se
/// non ne ha una, sulla sessione di default (lo schermo). Ritorna `true` se il
/// testo e' andato sulla sessione; `false` = nessuna sessione raggiungibile e
/// il chiamante deve usare il seriale: **l'output non si perde mai**.
///
/// E' il sink di stdout dei programmi (login, shell, comandi, personalita'
/// POSIX): il mirror seriale lo fa il server, non il client.
pub fn out(bytes: &[u8]) -> bool {
    out_impl(bytes, false)
}

/// Come [`out`], ma **solo schermo**: non entra nel mirror seriale del server.
/// Per l'eco dei tasti, i ridisegni e i test.
pub fn out_screen(bytes: &[u8]) -> bool {
    out_impl(bytes, true)
}

fn out_impl(bytes: &[u8], screen_only: bool) -> bool {
    if bytes.is_empty() {
        return true;
    }
    // 1. la cache: se la sessione c'e' ancora, un solo IPC.
    unsafe {
        if let Some(s) = &*core::ptr::addr_of!(DEFAULT) {
            if s.write_with(bytes, screen_only).is_ok() {
                return true;
            }
            *core::ptr::addr_of_mut!(DEFAULT) = None;
        }
    }
    // 2. risoluzione: prima l'attestazione (in D8e il DE lega il figlio alla
    //    sua tile), poi l'unico ingresso nominato (lo schermo).
    let s = match whoami().or_else(|_| attach_default()) {
        Ok(s) => s,
        Err(_) => return false,
    };
    if s.write_with(bytes, screen_only).is_err() {
        return false;
    }
    unsafe { *core::ptr::addr_of_mut!(DEFAULT) = Some(s) };
    true
}

/// Risolve il canale del servizio `console`.
fn service_chan() -> Result<u64, SessError> {
    crate::service_lookup(Service::Console)
        .map(|c| c as u64)
        .map_err(|_| SessError::ServiceDead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errori_distinti() {
        assert_ne!(SessError::ServiceDead, SessError::Denied);
        assert_ne!(SessError::Denied, SessError::NotAttached);
        assert_ne!(SessError::NotAttached, SessError::Ipc);
        let _ = format!("{:?}", SessError::Ipc);
    }

    #[test]
    fn info_decodifica() {
        // 80x25, eco accesa, focus spento.
        let g = (80u64 << 16) | 25;
        let info = Info {
            cols: (g >> 16) as u16,
            rows: (g & 0xFFFF) as u16,
            echo: true,
            focus: false,
        };
        assert_eq!((info.cols, info.rows), (80, 25));
        assert!(info.echo && !info.focus);
    }
}
