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
//!   bit5 (32) `obj_put` nel bucket di sistema `sys` NEGATO (ADR-0077)
//!   bit6 (64) `open` con TRUNC di un binario in `bin/` NEGATO (ADR-0077)
//!   bit7 (128) `open` in scrittura di `sys:identis` NEGATO (ADR-0077)
//!
//! `T_DONE(1, 255)` atteso da `t_policy`.

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

    // ── ADR-0077 (fase 0): bucket di fiducia protetti ──
    // bit5: scrivere nel bucket di sistema deve essere negato (prima della
    // fix `DEFAULT_UNKNOWN_OPS` includeva WRITE e questo put RIUSCIVA).
    if civis::obj_put(b"sys", b"tamper", b"x").is_err() {
        esiti |= 32;
    }
    // bit6: troncare un binario di sistema via path deve essere negato.
    if civis::open("/bin/vestigia.bin", civis::OPEN_TRUNC).is_err() {
        esiti |= 64;
    }
    // bit7: il dato delle credenziali ha un owner (Identis), non noi.
    if civis::open("sys:identis", civis::OPEN_CREATE).is_err() {
        esiti |= 128;
    }
    // bit8 (ADR-0096): e **non si legge**. Prima chiunque apriva il bucket
    // delle credenziali in lettura e vedeva gli hash; ora e' riservato a
    // Identis/init (i nomi li da' `IDENTIS_LIST`).
    if civis::open("sys:identis", 0).is_err() && civis::obj_get(b"sys", b"identis").is_err() {
        esiti |= 256;
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
