//! foreign — attore IGNOTO della suite t57 (Fase 45): prova il default
//! restrittivo fail-closed della policy su identita' (hash fuori manifest).
//!
//! Il suo binario NON compare in `SERVICE_POLICY` ne' in `TEST_POLICY`: cardo
//! lo classifica con `DEFAULT_UNKNOWN_OPS` (0x19F = ALL senza MOUNT/UMOUNT/
//! GRANT/PIPE). Questo processo verifica che il tetto sia applicato davvero:
//!
//!   bit0 (1)  rights_get ritorna il tetto fail-closed 0x19F (non ALL)
//!   bit1 (2)  mount NEGATO
//!   bit2 (4)  dup_grant NEGATO
//!   bit3 (8)  pipe NEGATA
//!   bit4 (16) open+read LECITI dopo i rifiuti (anti-wedge)
//!
//! `T_DONE(1, 31)` atteso da `t_policy`.

#![no_std]
#![no_main]

use civis::println;

const T_DONE: u64 = 103;

/// Tetto ops per hash ignoto (identico a `cardo::policy::DEFAULT_UNKNOWN_OPS`).
const DEFAULT_UNKNOWN_OPS: u32 = 0x19F;

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    let mut esiti: u64 = 0;

    // bit0: il GET deve riportare il tetto fail-closed, non ALL.
    let mut sb = [0u8; 32];
    if civis::rights_get(&mut sb) == Ok(DEFAULT_UNKNOWN_OPS) {
        esiti |= 1;
    }

    // bit1: mount negato (fail-closed).
    if civis::mount("UUID=4F4C4556", "/mnt/foreign").is_err() {
        esiti |= 2;
    }

    // Serve un fd per provare il diniego di GRANT e una read lecita.
    if let Ok(fd) = civis::open("/hello.txt", 0) {
        // bit2: grant negato.
        if civis::dup_grant(fd).is_err() {
            esiti |= 4;
        }
        // bit4: read lecita (op consentita dal tetto, dopo i dinieghi).
        let mut buf = [0u8; 16];
        if let Ok(n) = civis::read_fs(fd, &mut buf, 16) {
            if n > 0 {
                esiti |= 16;
            }
        }
        let _ = civis::close(fd);
    }

    // bit3: pipe negata.
    if civis::pipe().is_err() {
        esiti |= 8;
    }

    let _ = civis::send(civis::CHANNEL_PARENT, T_DONE, 1, esiti);
    println!("[foreign] esiti={:#b}", esiti);
    civis::exit(0);
}

#[panic_handler]
fn panic_handler(_info: &core::panic::PanicInfo) -> ! {
    println!("[foreign] panic");
    civis::exit(1)
}
