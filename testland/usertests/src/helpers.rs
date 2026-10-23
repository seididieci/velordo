use super::*;
use alloc::string::String;

// Tags protocollo (speculari a usertest-client / usertest-spin).
pub const T_CFG: u64 = 100;
pub const T_ACK: u64 = 101;
pub const T_REQ: u64 = 102;
pub const T_DONE: u64 = 103;
pub const T_STOP: u64 = 104;
pub const T_OPENED: u64 = 105;
pub const T_GO: u64 = 106;
pub const T_READY: u64 = 107;

// Mode client.
pub const M_ECHO: u64 = 0;
pub const M_ZERO: u64 = 1;
// Lifecycle (Fase 14).
pub const M_CHURN: u64 = 4;
pub const M_KILLME: u64 = 5;
pub const M_SRVDIE: u64 = 6;
pub const M_SYNCWAIT: u64 = 7;
pub const M_MNTDIE: u64 = 8;
pub const M_OPENDIE: u64 = 9;
pub const M_MAPHAMMER: u64 = 10;
pub const M_FLOOD: u64 = 11;
pub const M_NEST: u64 = 12;
// Fase 29: fault di protezione (helper muore con FAULT_EXIT_CODE).
pub const M_FAULT_RO: u64 = 13;
pub const M_FAULT_NONE: u64 = 14;
pub const M_FAULT_NX: u64 = 15;
pub const M_FAULT_GUARD: u64 = 16;
pub const M_FAULT_GPF: u64 = 17;
pub const M_SHMDEMO: u64 = 18;
pub const M_FAULT_CODE: u64 = 19;
// Fase 33: COW su regione condivisa (l'helper mappa COW, legge il pattern del
// parent, scrive due pagine → copie private, il parent non deve vederle).
pub const M_COWDEMO: u64 = 20;
// Fase 34: fork COW di se stesso (isolamento padre/figlio su globale,
// report sul canale di nascita, exit 0).
pub const M_FORKDEMO: u64 = 21;
// Fase 35 (t50): tentativi ostili che devono fallire (kill non-figlio,
// register servizio di sistema). w1 = pid target del kill.
pub const M_HARDEN: u64 = 23;
// Fase 36 (t51): driver sacrificale su "/dev/t51" (come MNTDIE su /dev/tdie,
// stesso binario testcli → stesso image_hash per il same-image positivo).
pub const M_REG51: u64 = 24;
// Fase 37 (t52): demo exec in-place (diventa testspin su T_GO, stesso PID).
// w1 seleziona il target: 0 = testspin senza argv (nucleo 37.0); 1 = testcli
// con argv ["ARGPROBE","hello","world"] (37.1: sonda argv post-exec).
pub const M_EXECDEMO: u64 = 25;
// Fase 45 (t57): sonda diritti GRANT+PIPE sul proprio canale (testcli con
// riga test-policy ALL) + detail bitmask esiti.
pub const M_GRANTDENY: u64 = 32;
// Fase 36 (t51): w0 magico per la sonda di squat di usertest-spin (binario
// diverso da testcli → hash diverso per il negativo). Speculare a
// SQUAT_MAGIC in usertest-spin (come i tag T_* speculari al client).
pub const SPIN_SQUAT_MAGIC: u64 = 0x5351_5541_5435_31;

// VA per i test map_physical/aliasing (zona libera tra USER_FS_BUFFER e lo
// heap: 0x4000_0020_0000..0x4000_0040_0000).
pub const VA_A: u64 = 0x0000_4000_0030_0000;
pub const VA_B: u64 = 0x0000_4000_0038_0000;
pub const SPIN_VA: u64 = 0x0000_4000_003C_0000;

pub const HELLO: &[u8] = b"Hello from Velordo ramfs!\n";

// ── mini-harness ─────────────────────────────────────────────────────

pub fn report(total: &mut u32, ok: &mut u32, name: &str, pass: bool) {
    *total += 1;
    if pass {
        *ok += 1;
        println!("[usertests] {}: PASS", name);
    } else {
        println!("[usertests] {}: FAIL", name);
    }
}


/// Spawna un helper da disco (Fase 21: `/test/*.bin` iniettati a build) e gli
/// invia la CFG (modo=w0,param=w1) sul canale di nascita (ADR-0008). Ritorna
/// (canale verso il figlio, ack.w0). Qualunque processo puo' spawnare senza
/// porte (primitiva generale); le porte restano privilegio di init.
pub fn spawn_cfg(path: &str, name: &str, prio: u8, mode: u64, param: u64) -> Option<(u64, u64)> {
    let img = civis::load_file(path)?;
    let meta = civis::SpawnMeta::new(name, prio, &[])?;
    let chan = civis::spawn_image(&img, &meta).ok()? as u64;
    let ack = civis::send(chan, T_CFG, mode, param).ok()?;
    Some((chan, ack.w0))
}

/// Legge `want` entry di una dir e dice se contiene `needle`.
pub fn dir_contains(path: &str, needle: &str) -> bool {
    let mut e = [0u8; 2048];
    let Ok(n) = civis::readdir(path, &mut e, 2048) else {
        return false;
    };
    let mut found = false;
    civis::test::each_name(&e, n, |name| {
        if name == needle {
            found = true;
        }
    });
    found
}

pub fn read_all(fd: i64, out: &mut Vec<u8>, total: usize) -> bool {
    let mut got = 0usize;
    while got < total {
        let mut chunk = [0u8; 2000];
        let n = match civis::read_fs(fd, &mut chunk, 2000) {
            Ok(n) => n,
            Err(_) => return false,
        };
        if n == 0 {
            return false;
        }
        out.extend_from_slice(&chunk[..n]);
        got += n;
    }
    got == total
}

/// Attende un T_DONE da uno dei canali in `chans`. Risponde col request-id.
/// Risponde a qualunque messaggio (anche da canali estranei, es. un DONE
/// tardivo di un test precedente) per non lasciare mittenti bloccati, ma conta
/// solo un T_DONE da un canale atteso.
/// Ritorna (ok, canale del mittente).
pub fn recv_done(chans: &[u64]) -> (bool, u64) {
    loop {
        match civis::recv() {
            Ok(m) => {
                let _ = civis::reply(T_ACK, 0, 0);
                if m.tag == T_DONE && chans.contains(&m.channel) {
                    return (m.w0 == 1, m.channel);
                }
                // Messaggio estraneo: risposto, continua ad attendere.
            }
            Err(_) => return (false, 0),
        }
    }
}

/// Attende un messaggio con `tag` dal canale `chan`, rispondendo e scartando
/// qualunque messaggio estraneo arrivi prima (residui di test precedenti).
/// Ritorna true se il messaggio atteso e' arrivato con w0==1.
pub fn recv_expect(chan: u64, tag: u64) -> bool {
    loop {
        match civis::recv() {
            Ok(m) => {
                let _ = civis::reply(T_ACK, 0, 0);
                if m.tag == tag && m.channel == chan {
                    return m.w0 == 1;
                }
                // Estraneo: risposto e scartato, continua.
            }
            Err(_) => return false,
        }
    }
}

/// Svuota i messaggi residui in coda (es. le notifiche EXIT_NOTIFY dei helper
/// dei test precedenti, che escono dopo il loro T_DONE). Da chiamare all'inizio
/// dei test che usano `recv`/`wait_reply` "stretti". Le EXIT_NOTIFY vengono
/// scartate SENZA reply: il mittente e' morto, rispondere e' concettualmente
/// sbagliato (notifica unificata, Fase 14).
pub fn drain_stray() {
    while let Some(m) = civis::recv_poll() {
        if !civis::is_exit_notify(&m) {
            let _ = civis::reply(T_ACK, 0, 0);
        }
    }
}

/// Fase 14 — attende sul canale `chan` la notifica `EXIT_NOTIFY` del kernel
/// (il figlio e' morto: w0 = exit code, w1 = pid). Gli estranei vengono
/// ignorati (niente reply: il canale di un figlio morto non ha peer vivo).
pub fn wait_exit(chan: u64) -> Option<(i64, i64)> {
    loop {
        match civis::recv() {
            Ok(m) if m.channel == chan && civis::is_exit_notify(&m) => {
                return Some((m.w0 as i64, m.w1 as i64));
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Ferma il flooder di t30 (T_STOP sync: l'helper risponde entro ~64 op) e lo
/// reaped (T_DONE + EXIT_NOTIFY). Se l'helper e' gia' morto, send fallisce e
/// resta solo il reap. Mai hang: l'helper o risponde o e' morto.
pub fn stop_flooder(fchan: u64) {
    if civis::send(fchan, T_STOP, 0, 0).is_ok() {
        let _ = recv_done(&[fchan]);
    }
    let _ = wait_exit(fchan);
}

/// Contenuto atteso del fixture `hello.txt` sulla radice ArcaFS (seedato a
/// build-time: `scripts/arca-tool.sh` -> `ns:hello.txt`).
pub const ARCA_HELLO: &[u8] = b"Hello from Velordo ramfs!\n";

/// Path del fixture sulla radice (volume root ArcaFS).
pub const ARCA_HELLO_PATH: &str = "/hello.txt";

/// Magic del superblock ArcaFS (4 byte a offset 0 del volume).
pub const ACFS_MAGIC: &[u8; 4] = b"ACFS";

/// Device che ospita il volume ArcaFS root. **Mai per lettera**: l'ordine
/// IDE puo' essere inverso (SWAP_DRIVES) e il volume si riconosce dal magic
/// ACFS nel settore 0 (whole-disk o partizione: il superblock sta a LBA0
/// del nodo). Ritorna il path `/dev/<nome>` del primo nodo che risponde.
pub fn find_arca_device() -> Option<String> {
    let mut eb = [0u8; 1024];
    let cap = eb.len();
    // NB: `readdir` restituisce il NUMERO di entry, non i byte: il bound
    // della scansione e' la capienza del buffer (voci NUL-separate).
    if civis::readdir("/dev", &mut eb, cap).is_err() {
        return None;
    }
    let mut i = 0usize;
    while i < cap && eb[i] != 0 {
        let start = i;
        while i < cap && eb[i] != 0 {
            i += 1;
        }
        let name = &eb[start..i];
        if name.len() >= 3 && &name[..2] == b"sd" {
            let path = format!("/dev/{}", core::str::from_utf8(name).unwrap_or(""));
            if node_is_arca(&path) {
                return Some(path);
            }
        }
        i += 1;
    }
    None
}

/// Il nodo ha un superblock ArcaFS (magic ACFS) nel settore 0?
fn node_is_arca(path: &str) -> bool {
    let Ok(fd) = civis::open_wait(path, 0, 1000, civis::POLL_PERIOD_TICKS) else {
        return false;
    };
    let mut buf = [0u8; 512];
    let n = civis::read_fs(fd, &mut buf, 512).unwrap_or(0);
    let _ = civis::close(fd);
    n == 512 && &buf[..4] == ACFS_MAGIC
}

/// Il device ArcaFS root e' leggibile raw (settore 0 col magic ACFS)?
/// Prova il data-plane DISK di block e il relay DEV di cardo in un colpo solo.
pub fn disk_sector0_ok() -> bool {
    match find_arca_device() {
        Some(p) => node_is_arca(&p),
        None => false,
    }
}

/// Legge tutto il file `fd` in `dst` (come testfat `read_all`).
pub fn t33_read_all(fd: i64, dst: &mut [u8]) -> usize {
    let mut got = 0usize;
    while got < dst.len() {
        let rest = dst.len() - got;
        let n = match civis::read_fs(fd, &mut dst[got..], rest) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        got += n;
    }
    got
}

/// Attende T_READY(w0, w1) sul canale `chan` (handshake NEST, Fase 22):
/// come `recv_expect` ma ritorna i payload invece di un bool. Le EXIT_NOTIFY
/// altrui si scartano senza reply (mittente morto); gli altri estranei con
/// reply, come `recv_expect`.
pub fn recv_ready(chan: u64) -> Option<(u64, u64)> {
    loop {
        match civis::recv() {
            Ok(m) if m.channel == chan && m.tag == T_READY => {
                let _ = civis::reply(T_ACK, 0, 0);
                return Some((m.w0, m.w1));
            }
            Ok(m) if civis::is_exit_notify(&m) => {}
            Ok(_) => {
                let _ = civis::reply(T_ACK, 0, 0);
            }
            Err(_) => return None,
        }
    }
}

/// Attesa throttled che `ps_info(pid)` sparisca (processo terminato +
/// reclamato). Batch di spin puri tra i get_ticks (igiene scheduler).
pub fn poll_gone(pid: u64, bound_ticks: i64) -> bool {
    let t0 = civis::get_ticks();
    loop {
        if civis::ps_info(pid as u32).is_none() {
            return true;
        }
        if civis::get_ticks() - t0 > bound_ticks {
            return false;
        }
        for _ in 0..512 {
            core::hint::spin_loop();
        }
    }
}

/// Attesa throttled della prima snapshot `ps` di `pid`: ritorna il parent
/// osservato, o None a timeout / pid mai apparso.
pub fn poll_parent(pid: u64, bound_ticks: i64) -> Option<Option<u32>> {
    let t0 = civis::get_ticks();
    loop {
        if let Some(e) = civis::ps_info(pid as u32) {
            return Some(e.parent);
        }
        if civis::get_ticks() - t0 > bound_ticks {
            return None;
        }
        for _ in 0..512 {
            core::hint::spin_loop();
        }
    }
}

/// true se il buffer readdir (voci NUL-separate) contiene `name` intero.
pub fn readdir_contains(buf: &[u8], name: &[u8]) -> bool {
    let mut i = 0usize;
    while i < buf.len() && buf[i] != 0 {
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if &buf[start..i] == name {
            return true;
        }
        i += 1;
    }
    false
}

