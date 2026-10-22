//! DEBUG FACILITY intenzionale (ADR-0005 §3): resta nel kernel, la console
//! vera vive nel console server userspace.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::{Lazy, Mutex};
use uart_16550::SerialPort;

pub static SERIAL1: Lazy<Mutex<SerialPort>> = Lazy::new(|| {
    let mut serial_port = unsafe { SerialPort::new(0x3F8) };
    serial_port.init();
    Mutex::new(serial_port)
});

/// Vero se il prossimo carattere da scrivere inizia una nuova riga.
/// Persiste tra chiamate `_print` (le righe parziali senza `\n` non devono
/// ricevere un nuovo timestamp). Accesso solo con interrupt disabilitati.
static AT_LINE_START: AtomicBool = AtomicBool::new(true);

/// Wrapper stile dmesg: inserisce `[<secondi>.<centesimi>] ` all'inizio di
/// ogni riga non vuota (sorgente: `pit::ticks()` a 100 Hz). Le righe vuote
/// restano pulite. Copre kernel E userland: l'output user passa da
/// `sys_write` -> `serial_println!` -> qui.
struct DmesgPort<'a> {
    port: &'a mut SerialPort,
}

impl fmt::Write for DmesgPort<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut at_start = AT_LINE_START.load(Ordering::Relaxed);
        for ch in s.chars() {
            if at_start && ch != '\n' {
                let t = crate::pit::ticks();
                write!(self.port, "[{}.{:02}] ", t / 100, t % 100)?;
                at_start = false;
            }
            let mut cbuf = [0u8; 4];
            self.port.write_str(ch.encode_utf8(&mut cbuf))?;
            if ch == '\n' {
                at_start = true;
            }
        }
        AT_LINE_START.store(at_start, Ordering::Relaxed);
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: ::core::fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;

    interrupts::without_interrupts(|| {
        let mut guard = SERIAL1.lock();
        let mut w = DmesgPort { port: &mut guard };
        w.write_fmt(args).expect("Printing to serial failed");
    });
}

/// Scrive byte raw sulla seriale con la stessa disciplina dmesg di `_print
/// (timestamp a inizio riga, tracking `\n` via `AT_LINE_START`).
///
/// Usata da `sys_write` (output userspace, syscall 0/1): a differenza di
/// `_print` non interpreta UTF-8 — i byte passano tali e quali (audit fedele:
/// byte in = byte sul filo) e non alloca mai, per qualunque lunghezza.
/// Lo stato inizio-riga e' globale e persiste tra chiamate, quindi il
/// chunking del chiamante (e `\n` a cavallo tra chunk) e' trasparente.
///
/// **Byte fedeli**: si usa `send_raw`, non `send`. Il driver `uart_16550`
/// con `send()` *espande* il backspace (0x08/0x7F) nella sequenza
/// "BS SP BS" (comportamento da terminale): su questo percorso il byte
/// utente deve arrivare sul filo identico a come e' stato scritto
/// (l'audit "byte in = byte sul filo" e' la proprieta' che rende il
/// mirror seriale confrontabile con il terminale). Con `send()` la
/// sequenza di cancellazione "\x08 \x08" (3 byte) arrivava sul filo come
/// 7 byte: il mirror seriale non combaciava con lo schermo.
#[doc(hidden)]
pub fn _write_bytes(data: &[u8]) {
    use x86_64::instructions::interrupts;

    interrupts::without_interrupts(|| {
        let mut guard = SERIAL1.lock();
        let mut at_start = AT_LINE_START.load(Ordering::Relaxed);
        for &b in data {
            if at_start && b != b'\n' {
                let t = crate::pit::ticks();
                // Timestamp senza `fmt::Write` (niente formattazione qui):
                // costruzione manuale su stack, zero alloc.
                let mut ts = [0u8; TS_LEN];
                let n = format_ts(&mut ts, t / 100, t % 100);
                for &tb in &ts[..n] {
                    guard.send_raw(tb);
                }
                at_start = false;
            }
            guard.send_raw(b);
            if b == b'\n' {
                at_start = true;
            }
        }
        AT_LINE_START.store(at_start, Ordering::Relaxed);
    });
}

/// Lunghezza massima di `[<sec>.<cc:02>] `: `[` + 20 cifre (u64 max) +
/// `.` + 2 cifre + `]` + ` ` = 26. Il buffer basta sempre, per qualunque tick.
const TS_LEN: usize = 26;

/// Scrive `[<sec>.<cc:02>] ` in `buf`, ritorna la lunghezza (sempre ≤ TS_LEN).
/// Timestamp manuale senza `core::fmt`: questo percorso non deve allocare.
fn format_ts(buf: &mut [u8; TS_LEN], sec: u64, cc: u64) -> usize {
    let mut n = 0;
    buf[n] = b'[';
    n += 1;
    n += format_u64(&mut buf[n..], sec);
    buf[n] = b'.';
    n += 1;
    buf[n] = b'0' + (cc / 10) as u8;
    n += 1;
    buf[n] = b'0' + (cc % 10) as u8;
    n += 1;
    buf[n] = b']';
    n += 1;
    buf[n] = b' ';
    n + 1
}

/// Scrive un u64 decimale in `buf`, ritorna le cifre scritte.
/// Il chiamante garantisce spazio sufficiente (u64 max = 20 cifre).
fn format_u64(buf: &mut [u8], mut v: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    if v == 0 {
        tmp[n] = b'0';
        n = 1;
    }
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for (i, &b) in tmp[..n].iter().rev().enumerate() {
        buf[i] = b;
    }
    n
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => ($crate::serial::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}
