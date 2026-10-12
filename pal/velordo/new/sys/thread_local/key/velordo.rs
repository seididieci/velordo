//! TLS a chiavi Velordo (S2.3): modello alla `xous`, adattato — in
//! ring3 non si legge alcun registro thread (niente FSGSBASE user), quindi
//! la tabella del thread si trova via RSP (sempre leggibile): registro
//! globale di slot {base,size,table} + tabella statica per il main (stack
//! di range ignoto, unico per processo). Cap: 32 thread con TLS e 64 chiavi
//! (oltre = rtabort loud). I dtor girano in `destroy_tls`, chiamato dal
//! trampolino all'uscita (il `guard::enable` e' no-op come hermit/xous).

use core::arch::asm;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::alloc::System;
use crate::mem::ManuallyDrop;

pub type Key = usize;
pub type Dtor = unsafe extern "C" fn(*mut u8);

/// Chiavi 1..=MAX_KEYS (0 riservata come sentinella, come xous).
pub const MAX_KEYS: usize = 64;
/// Voci tabella (indice 0 inutilizzato).
const TABLE_LEN: usize = MAX_KEYS + 1;
const MAX_SLOTS: usize = 32;

static NEXT_KEY: AtomicUsize = AtomicUsize::new(1);
static DTORS: AtomicPtr<Node> = AtomicPtr::new(ptr::null_mut());

struct Slot {
    base: AtomicUsize,
    size: AtomicUsize,
    table: AtomicPtr<u8>,
}

impl Slot {
    const fn empty() -> Slot {
        Slot {
            base: AtomicUsize::new(0),
            size: AtomicUsize::new(0),
            table: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

const EMPTY_SLOT: Slot = Slot::empty();
static SLOTS: [Slot; MAX_SLOTS] = [
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT,
    EMPTY_SLOT
];

/// Tabella del main thread (statica: mai liberata; i dtor del main non
/// girano — all'exit del processo muore tutto, parita' uefi).
static MAIN_TLS: [AtomicUsize; TABLE_LEN] = [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)];

struct Node {
    dtor: Dtor,
    key: Key,
    next: *mut Node,
}

#[inline]
fn rsp_now() -> usize {
    let x: usize;
    unsafe {
        asm!("mov {}, rsp", out(reg) x, options(nomem, nostack, preserves_flags));
    }
    x
}

/// Tabella del thread che possiede `r` nel suo stack, o MAIN_TLS.
fn table_for(r: usize) -> *mut u8 {
    for s in SLOTS.iter() {
        let base = s.base.load(Ordering::Acquire);
        if base != 0 {
            let size = s.size.load(Ordering::Acquire);
            if r.wrapping_sub(base) < size {
                return s.table.load(Ordering::Acquire);
            }
        }
    }
    MAIN_TLS.as_ptr() as *mut u8
}

/// Registra {stack, tabella} (da `Thread::new`): vera se c'e' uno slot.
/// Ordine: size/table PRIMA del CAS che pubblica base (i lettori vedono
/// base!=0 solo dopo, con Acquire: tutto gia' scritto).
pub fn register_slot(base: usize, size: usize, table: *mut u8) -> bool {
    for s in SLOTS.iter() {
        if s.base.load(Ordering::Relaxed) == 0 {
            s.size.store(size, Ordering::Relaxed);
            s.table.store(table, Ordering::Relaxed);
            if s
                .base
                .compare_exchange(0, base, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
            // Perso la gara: lo slot non e' nostro, riprova altrove.
        }
    }
    false
}

/// Libera lo slot di `base` (fallimento CREATE: il thread non parte).
pub fn release_base(base: usize) {
    for s in SLOTS.iter() {
        if s.base.load(Ordering::Acquire) == base {
            s.base.store(0, Ordering::Release);
            return;
        }
    }
}

/// Libera lo slot del thread corrente (dopo i dtor, mai piu' tocchi).
pub fn release_own_slot() {
    let r = rsp_now();
    for s in SLOTS.iter() {
        let base = s.base.load(Ordering::Acquire);
        if base != 0 {
            let size = s.size.load(Ordering::Acquire);
            if r.wrapping_sub(base) < size {
                s.base.store(0, Ordering::Release);
                return;
            }
        }
    }
}

#[inline]
pub fn create(dtor: Option<Dtor>) -> Key {
    let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    if key > MAX_KEYS {
        rtabort!("TLS Velordo: oltre 64 chiavi");
    }
    if let Some(f) = dtor {
        unsafe { register_dtor(key, f) };
    }
    key
}

#[inline]
pub unsafe fn set(key: Key, value: *mut u8) {
    rtassert!(key != 0 && key <= MAX_KEYS);
    let t = table_for(rsp_now()) as *mut *mut u8;
    unsafe { t.add(key).write(value) };
}

#[inline]
pub unsafe fn get(key: Key) -> *mut u8 {
    rtassert!(key != 0 && key <= MAX_KEYS);
    let t = table_for(rsp_now()) as *mut *mut u8;
    unsafe { t.add(key).read() }
}

#[inline]
pub unsafe fn destroy(_key: Key) {
    // Leak come xous: i set di chiavi sono staticamente noti e piccoli.
}

unsafe fn register_dtor(key: Key, dtor: Dtor) {
    // System (non il global allocator): niente interferenze con TLS usata
    // dall'allocatore stesso.
    let mut node =
        ManuallyDrop::new(unsafe { Box::new_in(Node { key, dtor, next: ptr::null_mut() }, System) });

    let mut head = DTORS.load(Ordering::Acquire);
    loop {
        node.next = head;
        match DTORS.compare_exchange(head, &mut **node, Ordering::Release, Ordering::Acquire) {
            Ok(_) => return,
            Err(cur) => head = cur,
        }
    }
}

/// Gira i dtor del thread corrente + cleanup runtime (dal trampolino
/// all'uscita; include `thread_cleanup` come lo `run_dtors` di xous).
/// Poi libera lo slot (la tabella la libera il chiamante via TCB).
pub(crate) unsafe fn destroy_tls() {
    let mut any_run = true;
    // Come xous: fino a 5 round (i dtor possono creare nuove TLS).
    for _ in 0..5 {
        if !any_run {
            break;
        }
        any_run = false;
        let mut cur = DTORS.load(Ordering::Acquire);
        while !cur.is_null() {
            let ptr = unsafe { get((*cur).key) };
            if !ptr.is_null() {
                unsafe { set((*cur).key, ptr::null_mut()) };
                unsafe { ((*cur).dtor)(ptr) };
                any_run = true;
            }
            cur = unsafe { (*cur).next };
        }
    }

    crate::rt::thread_cleanup();
    release_own_slot();
}
