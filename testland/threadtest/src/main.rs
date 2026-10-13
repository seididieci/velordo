//! Suite thread 1:1 (S-T, ADR-0046): create/exit, stack proprio, memoria
//! condivisa, TLS. Il leader resta single-thread tranne nei test (i thread
//! fanno join via polling su atomics condivisi finche' il futex arriva in
//! T3 — spin puri IF=1 col timer che previene la fame, mai busy su syscall).
//!
//! Riga di gate `[threadtest] PASS n/n` (T6 la estende: futex, stress 32).

#![no_std]
#![no_main]

use civis::{self, println};
use core::sync::atomic::{AtomicU64, Ordering};

/// Conta pass/total e stampa la riga di gate.
struct Checks {
    pass: u32,
    total: u32,
}
impl Checks {
    fn ok(&mut self, name: &str, cond: bool) {
        self.total += 1;
        if cond {
            self.pass += 1;
            println!("[threadtest] {}: PASS", name);
        } else {
            println!("[threadtest] {}: FAIL", name);
        }
    }
}

/// Attende `*flag == want` fino a `ticks` (spin puri: niente syscall nel
/// loop, il timer previene la fame). `false` a timeout.
fn poll_flag(flag: &AtomicU64, want: u64, ticks: i64) -> bool {
    let t0 = civis::get_ticks();
    loop {
        if flag.load(Ordering::SeqCst) == want {
            return true;
        }
        if civis::get_ticks() - t0 > ticks {
            return false;
        }
        for _ in 0..512 {
            core::hint::spin_loop();
        }
    }
}

/// Alloca 16 KiB di stack via mmap, ritorna il TOP (allineato a pagina).
fn alloc_stack() -> Option<usize> {
    civis::mmap(0, 16 * 1024).ok().map(|b| b + 16 * 1024)
}

static T1_FLAG: AtomicU64 = AtomicU64::new(0);
static T1_COUNT: AtomicU64 = AtomicU64::new(0);

extern "C" fn t1_main() {
    T1_COUNT.fetch_add(1, Ordering::SeqCst);
    T1_FLAG.store(1, Ordering::SeqCst);
    civis::thread_exit(0);
}

static T2_DEPTH: AtomicU64 = AtomicU64::new(0);

extern "C" fn t2_main() {
    // 4 KiB di stack proprio: tocca ogni pagina (faulta loud se lo stack
    // del thread non e' mappato/usabile).
    let mut buf = [0u8; 4096];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    let mut sum = 0u64;
    for b in buf.iter() {
        sum += *b as u64;
    }
    T2_DEPTH.store(sum, Ordering::SeqCst);
    civis::thread_exit(0);
}

extern "C" fn t3_main() {
    // TLS via create (fs = indirizzo sentinella user): il kernel programma
    // l'MSR senza faultare (isolamento vero verificato in T2 con rdfsbase).
    T1_FLAG.store(3, Ordering::SeqCst);
    civis::thread_exit(0);
}

/// Legge FS_BASE da ring 3 (richiede FSGSBASE: qemu64/KVM lo espongono
/// sempre, il kernel logga `[cpu  ] FSGSBASE on` a boot come testimone; senza
/// va in #UD = morte loud diagnosticabile dal seriale, mai risultato muto).
fn rdfsbase() -> u64 {
    let v: u64;
    unsafe {
        core::arch::asm!("rdfsbase {}", out(reg) v, options(nostack, preserves_flags));
    }
    v
}

static T5_A: AtomicU64 = AtomicU64::new(0);
static T5_B: AtomicU64 = AtomicU64::new(0);

extern "C" fn t5a_main() {
    // 10 tick (~5 quanti a testa: switch reali tra i due thread): la base
    // letta deve restare la MIA sempre (se il kernel perdesse FS allo
    // switch vedremmo l'altra o 0).
    let t0 = civis::get_ticks();
    let mut ok = true;
    while civis::get_ticks() - t0 < 10 {
        if rdfsbase() != 0x4000_0000_2000 {
            ok = false;
            break;
        }
    }
    T5_A.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
    civis::thread_exit(0);
}

extern "C" fn t5b_main() {
    let t0 = civis::get_ticks();
    let mut ok = true;
    while civis::get_ticks() - t0 < 10 {
        if rdfsbase() != 0x4000_0000_3000 {
            ok = false;
            break;
        }
    }
    T5_B.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
    civis::thread_exit(0);
}

civis::entry!(real_main);
fn real_main(_sp: u64) -> ! {
    println!("[threadtest] starting, pid={}", civis::getpid());
    let mut c = Checks { pass: 0, total: 0 };

    // 1. create + run + exit: il thread gira davvero (contatore condiviso).
    let v1 = match alloc_stack() {
        Some(top) => match civis::thread_create(t1_main as *const () as usize, top, 0) {
            Ok(_) => poll_flag(&T1_FLAG, 1, 200) && T1_COUNT.load(Ordering::SeqCst) == 1,
            Err(_) => false,
        },
        None => false,
    };
    c.ok("create + run + exit", v1);

    // 2. stack proprio da 16 KiB usabile.
    T2_DEPTH.store(0, Ordering::SeqCst);
    let v2 = match alloc_stack() {
        Some(top) => match civis::thread_create(t2_main as *const () as usize, top, 0) {
            Ok(_) => {
                let expect: u64 = (0..4096u64).map(|i| i & 0xff).sum();
                poll_flag(&T2_DEPTH, expect, 200)
            }
            Err(_) => false,
        },
        None => false,
    };
    c.ok("stack proprio 16 KiB", v2);

    // 3. create con fs (TLS): rifiuto loud oltre user-range, Ok dentro.
    let v3a = civis::thread_create(t3_main as *const () as usize, alloc_stack().unwrap_or(0), u64::MAX as usize).is_err();
    let v3 = v3a
        && match alloc_stack() {
            Some(top) => {
                // fs = indirizzo user valido (pagina zero del binary: bounds
                // ok, mai dereferenziato dal kernel).
                let fs = 0x4000_0000_1000usize;
                match civis::thread_create(t3_main as *const () as usize, top, fs) {
                    Ok(_) => poll_flag(&T1_FLAG, 3, 200),
                    Err(_) => false,
                }
            }
            None => false,
        };
    c.ok("create con fs + bound", v3);

    // 4. thread_set_fs: Ok su base valida, Err oltre user-range.
    let v4 = civis::thread_set_fs(0x4000_0000_1000).is_ok()
        && civis::thread_set_fs(u64::MAX as usize).is_err();
    c.ok("set_fs ok/err", v4);
    let _ = civis::thread_set_fs(0);

    // 5. isolamento TLS (T2): due thread con fs diverse leggono SEMPRE la
    // propria base su 64 giri di switch (richiede FSGSBASE a boot).
    let v5 = match (alloc_stack(), alloc_stack()) {
        (Some(ta), Some(tb)) => {
            let oka = civis::thread_create(t5a_main as *const () as usize, ta, 0x4000_0000_2000).is_ok();
            let okb = civis::thread_create(t5b_main as *const () as usize, tb, 0x4000_0000_3000).is_ok();
            oka && okb && poll_flag(&T5_A, 1, 400) && poll_flag(&T5_B, 1, 400)
        }
        _ => false,
    };
    c.ok("tls isolamento fs", v5);

    // 6. join via futex (T3): il thread lavora, poi sveglia; il leader
    // dorme davvero (WAIT, non spin) finche' flag != 0.
    use core::sync::atomic::AtomicU32;
    static J_FLAG: AtomicU32 = AtomicU32::new(0);
    static J_WORK: AtomicU64 = AtomicU64::new(0);
    extern "C" fn t6_main() {
        let mut acc = 0u64;
        for i in 0..1000u64 {
            acc += i;
        }
        J_WORK.store(acc, Ordering::SeqCst);
        J_FLAG.store(1, Ordering::SeqCst);
        civis::futex_wake(J_FLAG.as_ptr() as usize, u32::MAX);
        civis::thread_exit(0);
    }
    let v6 = match alloc_stack() {
        Some(top) => match civis::thread_create(t6_main as *const () as usize, top, 0) {
            Ok(_) => {
                // Join: dormi finche' il flag e' 0 (deadline generosa).
                let addr = J_FLAG.as_ptr() as usize;
                let t0 = civis::get_ticks();
                let mut ok = false;
                while J_FLAG.load(Ordering::SeqCst) == 0 {
                    if civis::get_ticks() - t0 > 400 {
                        break;
                    }
                    let dl = (civis::get_ticks() as u64) + 50;
                    let _ = civis::futex_wait(addr, 0, dl);
                }
                ok = J_FLAG.load(Ordering::SeqCst) == 1
                    && J_WORK.load(Ordering::SeqCst) == (0..1000u64).sum::<u64>();
                ok
            }
            Err(_) => false,
        },
        None => false,
    };
    c.ok("futex join lavoratore", v6);

    // 7. timeout: valore fermo + deadline breve → Ok(false) dopo ~deadline.
    // Spinner-companion: garantisce un altro runnable (senza, da soli il
    // WAIT torna spuria subito per disegno — nessuno potra' mai svegliarci).
    static T7W: AtomicU32 = AtomicU32::new(0);
    static T7GO: AtomicU64 = AtomicU64::new(0);
    static T7OUT: AtomicU64 = AtomicU64::new(0);
    extern "C" fn t7spin_main() {
        while T7GO.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }
        T7OUT.store(1, Ordering::SeqCst);
        civis::thread_exit(0);
    }
    let v7 = match alloc_stack() {
        Some(top) => {
            if civis::thread_create(t7spin_main as *const () as usize, top, 0).is_err() {
                false
            } else {
                let addr = T7W.as_ptr() as usize;
                let t0 = civis::get_ticks();
                let r = civis::futex_wait(addr, 0, (t0 as u64) + 20);
                let dt = civis::get_ticks() - t0;
                T7GO.store(1, Ordering::SeqCst);
                let ok = r == Ok(false) && dt >= 15 && poll_flag(&T7OUT, 1, 200);
                ok
            }
        }
        None => false,
    };
    c.ok("futex timeout ~deadline", v7);

    // 8. wake contato: due waiter, wake(1) ne sveglia uno solo.
    static W8: AtomicU32 = AtomicU32::new(0);
    static W8A: AtomicU64 = AtomicU64::new(0);
    static W8B: AtomicU64 = AtomicU64::new(0);
    extern "C" fn t8a_main() {
        let addr = W8.as_ptr() as usize;
        let r = civis::futex_wait(addr, 0, 0);
        W8A.store(if r == Ok(true) { 1 } else { 2 }, Ordering::SeqCst);
        civis::thread_exit(0);
    }
    extern "C" fn t8b_main() {
        let addr = W8.as_ptr() as usize;
        let r = civis::futex_wait(addr, 0, 0);
        W8B.store(if r == Ok(true) { 1 } else { 2 }, Ordering::SeqCst);
        civis::thread_exit(0);
    }
    let v8 = match (alloc_stack(), alloc_stack()) {
        (Some(ta), Some(tb)) => {
            let oka = civis::thread_create(t8a_main as *const () as usize, ta, 0).is_ok();
            let okb = civis::thread_create(t8b_main as *const () as usize, tb, 0).is_ok();
            // Attendi che entrambi dormano (stato Blocked osservabile? no:
            // poll finche' wake(0) li "vedrebbe" — piu' semplice: breve
            // attesa + wake(1) + verifica che esattamente uno proceda).
            civis::spin_ticks(30);
            let addr = W8.as_ptr() as usize;
            let w1 = civis::futex_wake(addr, 1);
            civis::spin_ticks(30);
            let one = (W8A.load(Ordering::SeqCst) == 1) != (W8B.load(Ordering::SeqCst) == 1);
            let wall = civis::futex_wake(addr, u32::MAX);
            civis::spin_ticks(30);
            let both = W8A.load(Ordering::SeqCst) == 1 && W8B.load(Ordering::SeqCst) == 1;
            oka && okb && w1 == 1 && one && wall == 1 && both
        }
        _ => false,
    };
    c.ok("futex wake contato 1+all", v8);

    // 9. canale condiviso (T5): un thread fa IPC FS sulla sessione del
    // leader (stesso canale cardo: senza T5 il kernel rifiuta l'endpoint).
    static T9_RC: AtomicU64 = AtomicU64::new(9);
    extern "C" fn t9_main() {
        let mut st = civis::Stat { size: 99, kind: 0, readonly: false, mtime: 0 };
        let ok = civis::stat("/tTH.txt", &mut st).is_ok() && st.size == 5;
        T9_RC.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
        civis::thread_exit(0);
    }
    let v9 = {
        // Fixture del leader (ramfs condivisa).
        let mut ok = false;
        if let Ok(fd) = civis::open("/tTH.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC) {
            if civis::write_fs(fd, b"12345", 5) == Ok(5) {
                ok = true;
            }
            let _ = civis::close(fd);
        }
        if ok {
            if let Some(top) = alloc_stack() {
                if civis::thread_create(t9_main as *const () as usize, top, 0).is_ok() {
                    ok = poll_flag(&T9_RC, 1, 400);
                } else {
                    ok = false;
                }
            } else {
                ok = false;
            }
        }
        let _ = civis::remove("/tTH.txt");
        ok
    };
    c.ok("ipc canale condiviso", v9);

    // 10. fd condivisi (T5): fd aperto dal leader, letto dal thread (stesso
    // offset: i thread condividono la file description come POSIX).
    static T10_RC: AtomicU64 = AtomicU64::new(9);
    static T10_FD: AtomicU64 = AtomicU64::new(0);
    extern "C" fn t10_main() {
        let fd = T10_FD.load(Ordering::SeqCst) as i64;
        let mut buf = [0u8; 5];
        let ok = civis::read_fs(fd, &mut buf, 5) == Ok(5) && &buf == b"abcde";
        T10_RC.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
        civis::thread_exit(0);
    }
    let v10 = {
        let mut ok = false;
        if let Ok(fd) = civis::open("/tTH2.txt", civis::OPEN_CREATE | civis::OPEN_TRUNC) {
            if civis::write_fs(fd, b"abcde", 5) == Ok(5)
                && civis::lseek(fd, 0, civis::SEEK_START) == Ok(0)
            {
                T10_FD.store(fd as u64, Ordering::SeqCst);
                if let Some(top) = alloc_stack() {
                    if civis::thread_create(t10_main as *const () as usize, top, 0).is_ok() {
                        ok = poll_flag(&T10_RC, 1, 400);
                    }
                }
            }
            let _ = civis::close(fd);
        }
        let _ = civis::remove("/tTH2.txt");
        ok
    };
    c.ok("fd condivisi stesso offset", v10);

    // 11. stress 32 thread (T6: il numero di rustc-hello): ognuno somma la
    // propria parte, join via futex su contatore; somma totale esatta.
    static S_N: AtomicU64 = AtomicU64::new(0);
    static S_SUM: AtomicU64 = AtomicU64::new(0);
    extern "C" fn ts_main() {
        // Il mio indice = ticket atomico (chi primo arriva, primo serve).
        let me = S_N.fetch_add(1, Ordering::SeqCst);
        let mut acc = 0u64;
        for i in 0..200u64 {
            acc += me * 1000 + i;
        }
        S_SUM.fetch_add(acc, Ordering::SeqCst);
        // Ultimo chiude: wake il leader (che dorme su S_SUM != atteso? no:
        // dorme sul contatore N con expected = n-1... piu' semplice: ogni
        // thread fa wake, il leader ricontrolla in loop).
        civis::futex_wake(S_N.as_ptr() as usize, u32::MAX);
        civis::thread_exit(0);
    }
    let v11 = {
        let mut ok = true;
        for _ in 0..32 {
            match alloc_stack() {
                Some(top) => {
                    if civis::thread_create(ts_main as *const () as usize, top, 0).is_err() {
                        ok = false;
                        break;
                    }
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            // Join: dormi finche' S_N < 32 (wake a ogni uscita).
            let addr = S_N.as_ptr() as usize;
            let t0 = civis::get_ticks();
            while S_N.load(Ordering::SeqCst) < 32 {
                if civis::get_ticks() - t0 > 800 {
                    break;
                }
                let cur = S_N.load(Ordering::SeqCst);
                let dl = (civis::get_ticks() as u64) + 50;
                let _ = civis::futex_wait(addr, cur as u32, dl);
            }
            let mut expect = 0u64;
            for me in 0..32u64 {
                for i in 0..200u64 {
                    expect += me * 1000 + i;
                }
            }
            ok = S_N.load(Ordering::SeqCst) == 32 && S_SUM.load(Ordering::SeqCst) == expect;
        }
        ok
    };
    c.ok("stress 32 thread somma", v11);

    // 12. FPU eager (S2.5b): due thread tengono pattern XMM diversi per
    // ~10 tick (switch reali in mezzo); se lo switch perdesse XMM/MXCSR
    // vedremmo l'altro pattern o zeri. Richiede SSE (qemu64/host ok;
    // senza = #UD loud, mai muto).
    static X_A: AtomicU64 = AtomicU64::new(0);
    static X_B: AtomicU64 = AtomicU64::new(0);
    // Finestra load+spin+store TUTTA in asm (S2.5b): il compilatore non puo'
    // spillare XMM0 in memoria (falso negativo) perche' non vede dentro.
    // Lo spin (~30ms > quanto 20ms) forza switch in mezzo: se il kernel
    // perdesse XMM si legge spazzatura (o #UD senza OSFXSR, mai muto).
    // `movdqa` pretende operandi a 16 (altrimenti #GP nel TEST, non nel
    // kernel: osservato). Pattern/buffer in STATICHE (allineamento garantito
    // dal linker, mai dallo stack: i locali hanno tradito una volta).
    #[repr(align(16))]
    struct A16([u32; 4]);
    static PAT_A: A16 = A16([0x0A0A0A0A; 4]);
    static PAT_B: A16 = A16([0x0B0B0B0B; 4]);
    static mut BACK_A: A16 = A16([0; 4]);
    static mut BACK_B: A16 = A16([0; 4]);
    macro_rules! xmm_window {
        ($pat:expr, $back:expr, $rounds:expr) => {{
            unsafe {
                core::arch::asm!(
                    "movdqa xmm0, [{p}]",
                    "mov ecx, {n:e}",
                    "2:",
                    "pause",
                    "dec ecx",
                    "jnz 2b",
                    "movdqa [{o}], xmm0",
                    p = in(reg) $pat.0.as_ptr(),
                    o = in(reg) core::ptr::addr_of_mut!($back.0) as usize,
                    n = in(reg) $rounds as u32,
                    out("ecx") _,
                    out("xmm0") _,
                    options(nostack),
                );
            }
            $back.0 == $pat.0
        }};
    }
    unsafe extern "C" fn txa_main() {
        let mut ok = true;
        let t0 = civis::get_ticks();
        while civis::get_ticks() - t0 < 10 {
            if !xmm_window!(PAT_A, BACK_A, 3_000_000) {
                ok = false;
                break;
            }
        }
        X_A.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
        civis::thread_exit(0);
    }
    unsafe extern "C" fn txb_main() {
        let mut ok = true;
        let t0 = civis::get_ticks();
        while civis::get_ticks() - t0 < 10 {
            if !xmm_window!(PAT_B, BACK_B, 3_000_000) {
                ok = false;
                break;
            }
        }
        X_B.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
        civis::thread_exit(0);
    }
    let v12 = {
        let mut ok = true;
        match (alloc_stack(), alloc_stack()) {
            (Some(a), Some(b)) => {
                if civis::thread_create(txa_main as *const () as usize, a, 0).is_err() {
                    ok = false;
                }
                if civis::thread_create(txb_main as *const () as usize, b, 0).is_err() {
                    ok = false;
                }
            }
            _ => ok = false,
        }
        ok = ok
            && poll_flag(&X_A, 1, 400)
            && poll_flag(&X_B, 1, 400)
            && X_A.load(Ordering::SeqCst) == 1
            && X_B.load(Ordering::SeqCst) == 1;
        ok
    };
    c.ok("xmm isolamento switch", v12);

    println!("[threadtest] PASS {}/{}", c.pass, c.total);
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    println!("[threadtest] panic");
    civis::exit(1)
}
