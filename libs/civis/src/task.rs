//! Executor async minimale sopra l'IPC asincrona (ADR-0019, 26.1).
//!
//! Sintassi `async/await` (solo `core::future`, niente dipendenze) sopra le
//! syscall 33/34 invariate. Disegno a **router centrale**: i task non chiamano
//! mai `recv` direttamente — `block_on`/`run` sono gli unici a leggere dal
//! canale e instradano ogni messaggio (risposte → task proprietario per
//! `req_id`, `EXIT_NOTIFY` → waiter secondo filtro canale). Questo risolve
//! `UnexpectedMsg` per costruzione nel multi-task: un task vede solo la SUA
//! risposta (o la morte del SUO server), mai i messaggi altrui.
//!
//! Dominio: traffico reply-only + `EXIT_NOTIFY` (client async, registrazioni
//! driver). Richieste server in arrivo a un client in attesa NON sono gestite
//! (scartate, come `wait_reply` oggi le consuma e fallisce): il run
//! server-side con handler e' un passo successivo.
//!
//! Vincoli ereditati dalla Fase 13 (non rilassati): no mix sync/async in volo
//! per processo; routing FIFO per `req_id`; FS 1-in-volo (`FS_PENDING`).
//! Kernel invariato; reply implicita invariata (`reply()` come oggi).
//!
//! Costi: zero heap per-op (task su stack, pin con `new_unchecked` contenuto;
//! il `Waker` e' no-op su statiche). I server sono single-thread: niente lock,
//! niente code condivise oltre gli slot dei task.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::{is_exit_notify, recv, recv_poll, IpcMsg, WaitReplyError};

// ── Waker no-op ─────────────────────────────────────────────────────
// L'executor ripolla SEMPRE dopo ogni cambio di stato (deposito da `recv`),
// quindi nessun task dipende mai dalla wake per avanzare: il waker puo'
// essere no-op. Sound: funzioni pure su puntatore null, mai dereferenziato.

unsafe fn waker_clone(_: *const ()) -> RawWaker {
    noop_raw_waker()
}
unsafe fn waker_wake(_: *const ()) {}
unsafe fn waker_wake_by_ref(_: *const ()) {}
unsafe fn waker_drop(_: *const ()) {}

const WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

fn noop_raw_waker() -> RawWaker {
    RawWaker::new(core::ptr::null(), &WAKER_VTABLE)
}

fn noop_waker() -> Waker {
    // SAFETY: vtable no-op su puntatore null, mai dereferenziato.
    unsafe { Waker::from_raw(noop_raw_waker()) }
}

// ── Tratto di instradamento ─────────────────────────────────────────
// L'executor (`block_on`/`run`) e' generico su `R: Receivable`: non sa COSA
// attende il task, ma sa CONSEGNARGLI i messaggi (il task decide se tenerli).
// Questo separa routing (executor) da correlazione (future).

/// Task instradabile: accetta i messaggi pertinenti nel proprio slot.
pub trait Receivable {
    /// true se questo messaggio e' per noi (risposta attesa o EXIT_NOTIFY
    /// pertinente): il router lo deposita, altrimenti lo ignora.
    fn accepts(&self, m: &IpcMsg) -> bool;
    /// Deposita un messaggio accettato (slot drenato a ogni poll).
    fn deposit(&mut self, m: IpcMsg);
}

// ── Future di base ──────────────────────────────────────────────────

/// Attende la risposta async a una richiesta (correlazione per `req_id`,
/// come `wait_reply`, ma come `Future` componibile).
///
/// Il messaggio arriva nello `slot` SOLO dal router (`block_on`/`run`), mai
/// per lettura diretta: `poll` consuma e interpreta, non tocca il canale.
/// Filtro morte: `new` = qualunque `EXIT_NOTIFY` e' morte del server
/// (semantica `wait_reply`); `on_chan` = solo quelle sul canale `c`, le altre
/// sono stale e il router non le deposita (semantica `wait_reply_chan`, per FS).
pub struct WaitReply {
    req: i64,
    chan_filter: Option<u64>,
    slot: Option<IpcMsg>,
}

impl WaitReply {
    /// Come `wait_reply(req)`: qualunque EXIT_NOTIFY = server morto.
    pub fn new(req: i64) -> Self {
        Self { req, chan_filter: None, slot: None }
    }

    /// Come `wait_reply_chan(req, chan)`: EXIT_NOTIFY su altri canali ignorata
    /// dal router (mai depositata qui).
    pub fn on_chan(req: i64, chan: u64) -> Self {
        Self { req, chan_filter: Some(chan), slot: None }
    }
}

impl Receivable for WaitReply {
    fn accepts(&self, m: &IpcMsg) -> bool {
        m.req_id == self.req
            || (is_exit_notify(m) && self.chan_filter.map_or(true, |c| m.channel == c))
    }

    fn deposit(&mut self, m: IpcMsg) {
        // Slot drenato a ogni poll: all'arrivo e' sempre vuoto. Un duplicato
        // (impossibile per protocollo: 1 reply per req) tiene il primo.
        if self.slot.is_none() {
            self.slot = Some(m);
        }
    }
}

impl Future for WaitReply {
    type Output = Result<IpcMsg, WaitReplyError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.slot.take() {
            Some(m) if m.req_id == this.req => Poll::Ready(Ok(m)),
            Some(m) if is_exit_notify(&m) => Poll::Ready(Err(WaitReplyError::ServerDied {
                pid: m.w1,
                code: m.w0 as i64,
            })),
            // Impossibile via router (deposita solo accettati): difetta a
            // Pending rimettendo a posto (mai perdita, mai wedge).
            Some(m) => {
                this.slot = Some(m);
                Poll::Pending
            }
            None => Poll::Pending,
        }
    }
}

/// Prossimo messaggio in coda senza bloccare (come `recv_poll`), come
/// `Future`: `Ready(Some(m))` se presente, `Pending` se coda vuota.
/// Per loop proprietari del canale (futuro run server-side): NON usarla sotto
/// `block_on`/`run` (il router possiede il `recv` li').
pub struct RecvMsg;

impl Future for RecvMsg {
    type Output = Option<IpcMsg>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match recv_poll() {
            Some(m) => Poll::Ready(Some(m)),
            None => Poll::Pending,
        }
    }
}

// ── Client FS reale (ADR-0019, 26.3) ─────────────────────────────
// Prova che la sintassi scala al protocollo FS sopra `read_async`/
// `fs_collect_msg` INVARIATI (stesso guard `FS_PENDING`, stesso formato
// frame, stesso chan-filter `wait_reply_chan`).

/// Lettura FS async come `Future`: `read_async` (un chunk ≤ RING_MAX_PAYLOAD)
/// + attesa della reply via router + lettura del response frame.
///
/// L'invio avviene a COSTRUZIONE (`new`, come `read_async`: syscall non
/// bloccante, niente da attendere), mai al primo poll: cosi' il `req_id` e'
/// noto al router fin da subito. Il poll non blocca mai (il collect dal ring
/// e' puro consumo, come `fs_collect_msg`).
///
/// Composizione invece di duplicazione: il routing e' delegato a un
/// `WaitReply::on_chan` interno (stessa semantica `fs_collect`: solo la morte
/// del server FS sul canale cachato conta, le stale no).
pub struct FsRead<'a> {
    inner: WaitReply,
    dst: &'a mut [u8],
    cap: usize,
}

impl<'a> FsRead<'a> {
    /// Come `read_async(fd, cap)` ma ritorna il future invece del req_id.
    /// `Err` nei casi di `read_async` (Fase 39: errore nativo). Il buffer `dst`
    /// e' riempito al completamento.
    pub fn new(fd: i64, dst: &'a mut [u8], cap: usize) -> Result<Self, crate::Error> {
        let req = crate::read_async(fd, cap)?;
        // Invariante di `fs_collect`: la read_async riuscita ha risolto e
        // cachato FS_CHAN prima di registrare FS_PENDING.
        let fchan =
            crate::fs::session::chan_get().max(0) as u64;
        Ok(Self { inner: WaitReply::on_chan(req, fchan), dst, cap })
    }
}

impl Receivable for FsRead<'_> {
    fn accepts(&self, m: &IpcMsg) -> bool {
        self.inner.accepts(m)
    }

    fn deposit(&mut self, m: IpcMsg) {
        self.inner.deposit(m);
    }
}

impl Future for FsRead<'_> {
    /// Byte letti, o -1 (stessi casi di `fs_collect`: errore IO, server morto
    /// con reset ring + guard come `fs_collect`, mai wedge).
    type Output = i64;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // `WaitReply: Unpin` (solo scalari/Option): pin diretto, niente unsafe.
        match Pin::new(&mut this.inner).poll(cx) {
            Poll::Ready(Ok(m)) => {
                Poll::Ready(crate::fs_collect_msg(&m, &mut *this.dst, this.cap, true).unwrap_or(-1))
            }
            Poll::Ready(Err(e)) => {
                // Come `fs_collect` sul path errore: frame orfano, reset ring
                // + guard, -1 al chiamante (niente retry qui, mai wedge). Il
                // log pid/code solo per morte reale; gli altri Err sono
                // irraggiungibili via router (deposita solo accettati).
                if let crate::WaitReplyError::ServerDied { pid, code } = e {
                    crate::println!(
                        "[civis] fs_read_async: server pid {} morto (code {})",
                        pid,
                        code
                    );
                }
                crate::fs::session::ring_reset(crate::fs::ring::REQ_RING_VA);
                crate::fs::session::ring_reset(crate::fs::ring::RESP_RING_VA);
                crate::fs::session::guard_clear();
                Poll::Ready(-1)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ── Executor ────────────────────────────────────────────────────────
// Unico punto di `recv` per i task gestiti: ogni messaggio letto viene
// instradato PRIMA del poll successivo (nessun task vede mai messaggi altrui).

/// Pinna una future sullo stack per poll ludici senza heap.
/// SAFETY del chiamante: non muovere il valore dopo il pin (qui: array/locali
/// che restano fermi fino a `Ready`, mai `mem::swap`).
unsafe fn pin_stack<T>(v: &mut T) -> Pin<&mut T> {
    unsafe { Pin::new_unchecked(v) }
}

// ── Composizione (ADR-0019, 26.4) ────────────────────────────────
// Con router-esterno solo i combinatori TRASPARENTI compongono: il router deve
// VEDERE i waiter foglia per instradarli, e un blocco `async` e' opaco (i suoi
// waiter interni sono inaccessibili → non instradabile). `Join` espone i figli
// e delega: `Join` di `Join` si annidano a piacere. Gli `async fn` opachi
// arrivano con la fase server-run (executor che guida anche gli handler).

/// Composizione parallela di due task: completa quando ENTRAMBI sono Ready.
/// `Join` di `Join` per N task annidati. Routing delegato ai figli (ognuno
/// riceve solo i propri messaggi); `Future` completa con la tupla.
pub struct Join<A: Future, B: Future> {
    a: A,
    b: B,
    done_a: bool,
    done_b: bool,
    out_a: Option<A::Output>,
    out_b: Option<B::Output>,
}

/// Costruisce una composizione parallela (come `futures::join!` su 2).
pub fn join<A: Future, B: Future>(a: A, b: B) -> Join<A, B> {
    Join { a, b, done_a: false, done_b: false, out_a: None, out_b: None }
}

impl<A: Receivable + Future, B: Receivable + Future> Receivable for Join<A, B> {
    fn accepts(&self, m: &IpcMsg) -> bool {
        (!self.done_a && self.a.accepts(m)) || (!self.done_b && self.b.accepts(m))
    }

    fn deposit(&mut self, m: IpcMsg) {
        // A TUTTI gli accettanti non-finiti (una reply ha un solo
        // proprietario; un EXIT pertinente sveglia ogni waiter che lo accetta).
        if !self.done_a && self.a.accepts(&m) {
            self.a.deposit(m);
        }
        if !self.done_b && self.b.accepts(&m) {
            self.b.deposit(m);
        }
    }
}

impl<A: Receivable + Future, B: Receivable + Future> Future for Join<A, B> {
    type Output = (A::Output, B::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: come `pin_stack` — i campi `a`/`b` non sono mai mossi (solo
        // poll in place); `out_a`/`out_b` non sono pinnati (take lecito).
        let this = unsafe { self.get_unchecked_mut() };
        if !this.done_a {
            // SAFETY: come `pin_stack` (campi mai mossi dopo il pin).
            if let Poll::Ready(v) = unsafe { pin_stack(&mut this.a) }.poll(cx) {
                this.out_a = Some(v);
                this.done_a = true;
            }
        }
        if !this.done_b {
            if let Poll::Ready(v) = unsafe { pin_stack(&mut this.b) }.poll(cx) {
                this.out_b = Some(v);
                this.done_b = true;
            }
        }
        if this.done_a && this.done_b {
            let a = this.out_a.take().expect("task::Join: done senza output (a)");
            let b = this.out_b.take().expect("task::Join: done senza output (b)");
            Poll::Ready((a, b))
        } else {
            Poll::Pending
        }
    }
}

/// Esegue un singolo task fino a `Ready`. Tra un poll e l'altro resta
/// bloccato in `recv` (UNA attesa in volo: come `wait_reply`, ma sopra
/// qualunque `Receivable`, non solo reply).
///
/// Dominio reply-only + EXIT (vedi modulo): il resto e' scartato.
pub fn block_on<R: Receivable + Future>(fut: R) -> R::Output {
    let mut fut = fut;
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = unsafe { pin_stack(&mut fut) }.poll(&mut cx) {
            return v;
        }
        // Pending: solo un messaggio puo' sbloccarci. Lo instradiamo se nostro,
        // altrimenti (traffico fuori dominio) lo scartiamo e ripolliamo.
        if let Ok(m) = recv() {
            if fut.accepts(&m) {
                fut.deposit(m);
            }
        }
        // `recv` fallita: niente da instradare, ripolla (il task resta
        // Pending; in pratica non accade — vedi `wait_reply`).
    }
}

/// Esegue N task concorrenti fino a `Ready` di tutti (const-generic, stack,
/// zero heap). Ogni `recv` bloccante instrada a TUTTI gli accettanti
/// (una reply ha un solo proprietario; un EXIT_NOTIFY pertinente sveglia
/// ogni waiter che lo accetta).
///
/// Dominio reply-only + EXIT (vedi modulo): il resto e' scartato.
pub fn run<const N: usize, R: Receivable + Future>(tasks: [R; N]) -> [R::Output; N] {
    let mut tasks = tasks;
    let mut done = [false; N];
    let mut outs: [Option<R::Output>; N] = core::array::from_fn(|_| None);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut remaining = N;
    while remaining > 0 {
        for i in 0..N {
            if done[i] {
                continue;
            }
            if let Poll::Ready(v) = unsafe { pin_stack(&mut tasks[i]) }.poll(&mut cx) {
                outs[i] = Some(v);
                done[i] = true;
                remaining -= 1;
            }
        }
        if remaining == 0 {
            break;
        }
        // Tutti Pending: solo un messaggio puo' sbloccarci.
        if let Ok(m) = recv() {
            for i in 0..N {
                if !done[i] && tasks[i].accepts(&m) {
                    tasks[i].deposit(m);
                }
            }
        }
    }
    outs.map(|o| o.expect("task::run: task marcato done senza output"))
}
