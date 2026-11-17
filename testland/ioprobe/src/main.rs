//! ioprobe — sonda delle porte I/O dal manifest (ADR-0079, fase 4).
//!
//! Il processo viene firmato `system` e la sua riga in `sys:manifest`
//! dichiara il range `80-80`. Cardo registra le porte nella trust table del
//! kernel, che le scrive nel TSS del processo: nessun `SpawnMeta.io_ranges`,
//! nessun rebuild del kernel.
//!
//! Protocollo (come gli helper della suite): il parent manda `T_CFG` con il
//! modo in `w0`; la sonda risponde `T_ACK` ed esegue.
//!
//!   modo 0 — legge la porta `0x80` (concessa dal manifest) e riporta
//!            `T_DONE(1, valore)` sul canale di nascita, poi esce 0.
//!   modo 1 — legge la porta `0x2F8` (NON concessa: il parent la dichiara in
//!            `SpawnMeta`, che il kernel ignora) → `#GP` → il kernel termina
//!            il processo con `FAULT_EXIT_CODE`. Se invece il processo
//!            sopravvive, riporta `T_DONE(0, 0)` (la dichiarazione del
//!            chiamante avrebbe funzionato: FAIL del test).
//!
//! La porta `0x80` e' il registro di diagnostica POST: una lettura non ha
//! effetti collaterali.

#![no_std]
#![no_main]

use civis::println;

const T_CFG: u64 = 100;
const T_ACK: u64 = 101;
const T_DONE: u64 = 103;

/// Porta concessa dal manifest (`sys:manifest`, riga di questo binario).
const PORT_OK: u16 = 0x80;
/// Porta mai concessa: se la si legge, il TSS deve fare `#GP`.
const PORT_DENY: u16 = 0x2F8;

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    let mode = match civis::recv() {
        Ok(m) if m.tag == T_CFG => {
            let _ = civis::reply(T_ACK, 0, 0);
            m.w0
        }
        _ => {
            println!("[ioprobe] nessun T_CFG");
            civis::exit(1)
        }
    };

    if mode == 0 {
        let v = unsafe { civis::pio::inb(PORT_OK) };
        // Report al parent (canale 0 = nascita).
        // Report asincrono (fire-and-forget): il parent non deve rispondere.
        let _ = civis::send_async(0, T_DONE, 1, v as u64);
        civis::exit(0)
    } else {
        // Il parent dichiara PORT_DENY in SpawnMeta: il kernel la ignora, il
        // TSS non la consente → #GP (mai ritorno).
        let v = unsafe { civis::pio::inb(PORT_DENY) };
        println!("[ioprobe] porta negata letta: {}", v);
        let _ = civis::send_async(0, T_DONE, 0, 0);
        civis::exit(0)
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[ioprobe] panic");
    civis::exit(1)
}
