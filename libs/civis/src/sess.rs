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
    send, Service, SESS_ATTACH, SESS_ATTACH_DEFAULT, SESS_AUTH, SESS_AUTH_SET, SESS_BIND,
    SESS_CLOSE, SESS_CREATE, SESS_DUMP, SESS_ECHO, SESS_FOCUS, SESS_INFO, SESS_INPUT, SESS_READ,
    SESS_RESIZE, SESS_STATE, SESS_STATS, SESS_TEST_INPUT, SESS_WHOAMI, SESS_WRITE,
};
use sess_proto::{AUTH_NAME_MAX, IN_OFF, MAILBOX_BYTES, MAX_CHUNK, OUT_OFF};

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
    /// La geometria e' cambiata dall'ultima `info()` (il server la consuma).
    pub resized: bool,
}

/// L'**utente autenticato** di una sessione (S3.3c). Non e' un segreto e non
/// viaggia: e' un fatto che il server conserva e risolve per attestazione.
#[derive(Clone, Copy)]
pub struct Auth {
    name: [u8; AUTH_NAME_MAX],
    len: usize,
}

impl Auth {
    /// Nome utente (vuoto = sessione anonima).
    pub fn as_bytes(&self) -> &[u8] {
        &self.name[..self.len]
    }

    /// Nome utente come `str` (il server valida UTF-8).
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    /// `true` se c'e' un'identita'.
    pub fn is_authenticated(&self) -> bool {
        self.len != 0
    }
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
            resized: (r.w1 & 4) != 0,
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

    /// L'**utente autenticato** della sessione (S3.3c): si chiede al server
    /// sulla sessione **attaccata** al canale (l'id non si passa, come
    /// `resize`/`echo`). Solo immagini >= `System`: l'identita' non e' un
    /// valore che un programma qualsiasi puo' leggere da un'altra sessione.
    pub fn auth(&self) -> Result<Auth, SessError> {
        auth_request(self.va, 0)
    }

    /// Scrive l'**utente autenticato** sulla sessione attaccata (solo immagini
    /// >= `System`: il login sulla 0, il DE quando propaga a una tile nuova).
    /// `user` vuoto = deautentica (fine sessione). La morte di chi l'ha
    /// stabilita porta via l'identita'.
    pub fn set_auth(&self, user: &[u8]) -> Result<(), SessError> {
        if user.len() > AUTH_NAME_MAX {
            return Err(SessError::Denied);
        }
        unsafe {
            ptr::copy_nonoverlapping(user.as_ptr(), (self.va + OUT_OFF) as *mut u8, user.len());
        }
        let r = send(service_chan()?, SESS_AUTH_SET, user.len() as u64, 0)
            .map_err(|_| SessError::Ipc)?;
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
        // Se era la sessione in cache (`current`), la cache e' stantia: senza
        // questo, chi chiude la propria sessione continuerebbe a parlarle (e
        // ogni operazione tornerebbe `Denied`) finche' un errore di write non
        // la butta — l'ha trovato t75, che dopo la `close` di t74 si ritrovava
        // il Session di una sessione chiusa.
        if unsafe { (*core::ptr::addr_of!(CURRENT)).as_ref() }.map(|c| c.id) == Some(self.id) {
            forget_current();
        }
        Ok(())
    }

    /// **Inietta tasti** (D8e): accoda `bytes` nella coda di **input** della
    /// sessione (non e' testo: e' quello che fa la tastiera) e sveglia il
    /// lettore dichiarato con l'hint.
    pub fn input(&self, bytes: &[u8]) -> Result<(), SessError> {
        if bytes.len() > MAX_CHUNK {
            return Err(SessError::Denied);
        }
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), (self.va + OUT_OFF) as *mut u8, bytes.len());
        }
        let r = send(service_chan()?, SESS_INPUT, bytes.len() as u64, 0)
            .map_err(|_| SessError::Ipc)?;
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

/// Un'applicazione a **schermo intero** (ADR-0098): la tela alternativa, il
/// cursore posizionabile, le cancellazioni e la misura.
///
/// Non e' un secondo modello: sono le sequenze ANSI che un terminale vero
/// capisce (`ESC[?1049h`, `ESC[2J`, `ESC[H`…) scritte sulla **sessione del
/// processo** ([`current`]). La sessione ha gia' tutto (scrollback, danno,
/// mirror seriale); l'app prende il controllo finche' non chiama [`Screen::close`],
/// e allora il testo di prima torna com'era.
pub struct Screen {
    cols: u16,
    rows: u16,
}

impl Screen {
    /// Entra nella tela alternativa: lettore dichiarato, eco spento, cursore
    /// nascosto, schermo pulito. Da qui in poi l'app disegna con [`Screen::at`].
    pub fn open() -> Result<Screen, SessError> {
        let s = current().ok_or(SessError::NotAttached)?;
        let _ = s.echo(false);
        s.write_screen(b"\x1b[?1049h\x1b[?25l\x1b[2J").map(|_| ())?;
        let info = s.info()?;
        Ok(Screen { cols: info.cols, rows: info.rows })
    }

    /// La misura della tela (celle).
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Rilegge la misura e ritorna `true` se e' cambiata (consuma il flag nel
    /// server): l'app riadatta il disegno invece di aspettare un tasto.
    pub fn poll_resize(&mut self) -> bool {
        let Some(s) = current() else {
            return false;
        };
        match s.info() {
            Ok(i) => {
                let changed = i.cols != self.cols || i.rows != self.rows || i.resized;
                self.cols = i.cols;
                self.rows = i.rows;
                changed
            }
            Err(_) => false,
        }
    }

    /// Porta il cursore in `(col, row)` (1-based, come il CUP): il prossimo
    /// byte si scrive li'.
    pub fn at(&self, col: u16, row: u16) -> Result<(), SessError> {
        let mut buf = [0u8; 24];
        let n = format_cup(&mut buf, col, row);
        self.write(&buf[..n])
    }

    /// Cancella lo schermo (il cursore non si muove).
    pub fn clear(&self) -> Result<(), SessError> {
        self.write(b"\x1b[2J")
    }

    /// Mostra/nasconde il cursore.
    pub fn cursor(&self, visible: bool) -> Result<(), SessError> {
        self.write(if visible { b"\x1b[?25h" } else { b"\x1b[?25l" })
    }

    /// Scrive byte nella tela (testo o sequenze).
    pub fn write(&self, bytes: &[u8]) -> Result<(), SessError> {
        let s = current().ok_or(SessError::NotAttached)?;
        s.write_screen(bytes).map(|_| ())
    }

    /// Lascia la tela: cursore e schermata principale tornano come prima.
    pub fn close(self) -> Result<(), SessError> {
        let s = current().ok_or(SessError::NotAttached)?;
        s.write_screen(b"\x1b[?25h\x1b[?1049l").map(|_| ())
    }
}

/// `ESC[<row>;<col>H` in un buffer fisso (`col`/`row` 1-based).
fn format_cup(buf: &mut [u8; 24], col: u16, row: u16) -> usize {
    let mut n = 0usize;
    buf[n] = 0x1B;
    buf[n + 1] = b'[';
    n += 2;
    n += write_u16(&mut buf[n..], row.max(1));
    buf[n] = b';';
    n += 1;
    n += write_u16(&mut buf[n..], col.max(1));
    buf[n] = b'H';
    n + 1
}

/// Scrive un `u16` in decimale, ritorna i byte scritti (niente `format!`:
/// questo sta nel percorso di disegno).
fn write_u16(dst: &mut [u8], mut v: u16) -> usize {
    let mut tmp = [0u8; 5];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in 0..n {
        dst[i] = tmp[n - 1 - i];
    }
    n
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

/// Un **iniettore** si attacca a una sessione **nominata** (>= `ThirdParty`) e
/// ne riceve la mailbox: e' cosi' che il DE manda i tasti (D8e). Non lo
/// dichiara **lettore** (quello resta [`Session::read`]).
pub fn attach_to_session(id: u32) -> Result<Session, SessError> {
    let r = send(service_chan()?, SESS_ATTACH, id as u64, 0).map_err(|_| SessError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(SessError::Denied);
    }
    Session::attach_to(id, r.w1 as u32)
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

// ── La sessione del processo (D8c2/D8d) ──────────────────────────────

/// Sessione corrente del processo, risolta **una volta** e riusata: la
/// risoluzione costa due IPC (attestazione + eventuale default), le
/// operazioni una sola. Una cache stantia (la `console` e' ripartita: la
/// mailbox e' morta e l'attachment non esiste piu') si butta al primo errore
/// con [`forget_current`].
static mut CURRENT: Option<Session> = None;

/// Canale del servizio `console` al momento della risoluzione: serve a
/// riconoscere la sua morte in [`crate::input::wait_until`] (dopo un restart
/// il canale e' un altro, e la cache va rifatta).
static mut CURRENT_CHAN: u64 = 0;

/// La **sessione del processo**: prima l'attestazione (`whoami`: in D8e il DE
/// lega il figlio alla sua tile), poi l'unico ingresso nominato
/// (`attach_default`: lo schermo). `None` = nessuna sessione raggiungibile.
///
/// E' la cache condivisa da stdout ([`out`]) e input ([`crate::input`]): un
/// processo parla con **una** sessione, sempre la stessa.
pub fn current() -> Option<&'static Session> {
    unsafe {
        if let Some(s) = &*core::ptr::addr_of!(CURRENT) {
            return Some(s);
        }
    }
    let chan = service_chan().ok()?;
    let s = whoami().or_else(|_| attach_default()).ok()?;
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT_CHAN) = chan;
        *core::ptr::addr_of_mut!(CURRENT) = Some(s);
        (*core::ptr::addr_of!(CURRENT)).as_ref()
    }
}

/// Butta la cache (la `console` e' morta o ha rifiutato): la prossima
/// operazione risolve di nuovo. Idempotente.
pub fn forget_current() {
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = None;
        *core::ptr::addr_of_mut!(CURRENT_CHAN) = 0;
    }
}

/// Il canale su cui parla la sessione in cache (0 = nessuna).
pub(crate) fn current_chan() -> u64 {
    unsafe { *core::ptr::addr_of!(CURRENT_CHAN) }
}

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
    let Some(s) = current() else {
        return false;
    };
    if s.write_with(bytes, screen_only).is_ok() {
        return true;
    }
    // Cache stantia (console ripartita): una risoluzione, un tentativo.
    forget_current();
    let Some(s) = current() else {
        return false;
    };
    s.write_with(bytes, screen_only).is_ok()
}

/// L'**utente autenticato della sessione del processo** (S3.3c). Solo immagini
/// >= `System`: l'identita' non si legge da un'altra sessione.
pub fn auth() -> Result<Auth, SessError> {
    current().ok_or(SessError::NotAttached)?.auth()
}

/// Scrive l'**utente autenticato** sulla **sessione del processo** (il login,
/// sulla 0). Il DE che propaga a una tile nuova usa invece
/// [`Session::set_auth`] sulla sessione a cui si e' appena attaccato.
pub fn set_auth(user: &[u8]) -> Result<(), SessError> {
    current().ok_or(SessError::NotAttached)?.set_auth(user)
}

/// L'utente autenticato della sessione del processo `pid` (S3.3c): il kernel
/// da' il pid, il server risale i padri (attestazione) e dice la sessione.
/// Serve ai servizi `System` che devono attribuire una richiesta (Identis).
/// La mailbox e' quella del canale del chiamante (`current`, creata al primo
/// uso); una cache stantia si butta e si riprova una volta.
pub fn auth_of(pid: i64) -> Result<Auth, SessError> {
    if let Some(s) = current() {
        if let Ok(a) = auth_request(s.va, pid) {
            return Ok(a);
        }
        forget_current();
    }
    let s = current().ok_or(SessError::NotAttached)?;
    auth_request(s.va, pid)
}

/// `SESS_AUTH {pid}`: leggermente diversa dalle altre, perche' la risposta
/// **non** e' un errore quando la sessione e' anonima (`w0 = 0`, `w1 = len`).
fn auth_request(va: usize, pid: i64) -> Result<Auth, SessError> {
    let r = send(service_chan()?, SESS_AUTH, pid as u64, 0).map_err(|_| SessError::Ipc)?;
    if r.w0 == !0u64 {
        return Err(SessError::Denied);
    }
    let mut name = [0u8; AUTH_NAME_MAX];
    if r.w0 == 0 {
        return Ok(Auth { name, len: 0 });
    }
    let len = (r.w1 as usize).min(AUTH_NAME_MAX);
    unsafe {
        ptr::copy_nonoverlapping((va + IN_OFF) as *const u8, name.as_mut_ptr(), len);
    }
    Ok(Auth { name, len })
}

/// Risolve il canale del servizio `console`.
pub(crate) fn service_chan() -> Result<u64, SessError> {
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
