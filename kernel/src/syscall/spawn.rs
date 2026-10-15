// Split from syscall.rs (byte-identical move; see facade).
use super::entry::current_id;
use super::service::finish_spawn;
/// spawn(name_ptr, name_len): crea un nuovo processo dal binario embedded
/// chiamato `name`. Crea il canale di nascita tra il chiamante (parent) e il
/// figlio (ADR-0008): il figlio lo eredita come canale 0, e il chiamante riceve
/// qui il channel id. Ritorna il channel id, o -1 se il nome non e' noto / la
/// creazione fallisce.
pub(super) fn sys_spawn(name_ptr: u64, name_len: usize) -> i64 {
    if name_len == 0 || name_len > 64 {
        return -1;
    }
    // Validazione: il nome deve stare nello spazio user mappato (U=1).
    if !crate::arc::vmm_user::is_user_range(name_ptr, name_len) {
        crate::serial_println!("[syscall] spawn: nome fuori dallo spazio user");
        return -1;
    }
    let bytes =
        unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len) };
    let name = match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    // S-T (T5): i figli dei thread appartengono al gruppo (parent = leader:
    // niente orfani con parent-tid riusabile, la cascata resta sensata).
    let parent = crate::ordo::sched::group_leader(current_id() as usize);
    match crate::user_binary::spawn_named(name, Some(parent), None) {
        Some(pid) => finish_spawn(parent, pid, name),
        None => {
            crate::serial_println!("[syscall] spawn: binario sconosciuto '{}'", name);
            -1
        }
    }
}

/// Layout di `SpawnMeta` (Fase 21, 40 B, `repr(C)` anche in civis; S2.5e: 56 B
/// con `args_ptr`/`args_len`): nome NUL-padded (non vuoto), priorita', porte
/// I/O + flags (Fase 22: solo DETACH). `args_*` = blocco packed
/// `[argc:8][envc:8][stringhe NUL]` (stesso formato di exec, 37.1.2; zero =
/// nessun argv → argc=0). Il kernel valida tutto (init e' trusted ma il
/// formato deve essere fail-loud, mai UB).
#[repr(C)]
struct SpawnMeta {
    name: [u8; 16],
    prio: u8,
    io_count: u8,
    flags: u8,
    _pad: [u8; 5],
    io_ranges: [(u16, u16); 4],
    args_ptr: u64,
    args_len: u64,
}

/// Immagine massima spawabile (Fase 39: single source in `syscall-numbers`,
/// condivisa con `civis` che pre-valida; S2.5d: 128 MiB per lo staticone —
/// lo spawn streama, il bound protegge il pool frame e i binari grandi
/// vogliono `RUN_MEM` adeguata). Stesso bound per `exec` (Fase 37).
pub(crate) use syscall_numbers::SPAWN_IMAGE_MAX;

/// spawn_image(img_ptr, img_len, meta_ptr, meta_len): come `spawn` ma il
/// binario e' letto dalla memoria del chiamante (servizi da disco, Fase 21).
/// E' la primitiva generale di creazione (come fork+exec): le PORTE I/O sono
/// un privilegio root — solo pid 1 (init) puo' chiederle, gli altri devono
/// avere `io_count == 0` (stesse capacita' dello spawn per-nome di oggi, dove
/// chiunque poteva spawnare anche `utspin_high`: la prio resta 1..31 per tutti,
/// mai 0/idle). Flag `SPAWN_FLAG_DETACH` (Fase 22): il figlio non partecipa
/// alla cascata di morte del parent (ri-parentato a init). Ritorna il channel
/// di nascita o -1.
pub(super) fn sys_spawn_image(img_ptr: u64, img_len: usize, meta_ptr: u64, meta_len: usize) -> i64 {
    if img_len == 0 || img_len > SPAWN_IMAGE_MAX {
        return -1;
    }
    if meta_len != core::mem::size_of::<SpawnMeta>() {
        return -1;
    }
    if !crate::arc::vmm_user::is_user_range(img_ptr, img_len)
        || !crate::arc::vmm_user::is_user_range(meta_ptr, meta_len)
    {
        crate::serial_println!("[syscall] spawn_image: fuori dallo spazio user");
        return -1;
    }
    // Copia meta sullo stack kernel (read_unaligned: il chiamante puo' non
    // allinearla; validazione su copia stabile: niente TOCTOU).
    let meta: SpawnMeta = unsafe { core::ptr::read_unaligned(meta_ptr as *const SpawnMeta) };
    if meta.name[0] == 0 || meta.io_count as usize > meta.io_ranges.len() {
        return -1;
    }
    if meta.name.iter().any(|&b| b != 0 && (b < 0x20 || b > 0x7e)) {
        return -1; // nome stampabile (ps/log), niente control byte
    }
    // Flags (Fase 22 + S2.5d): DETACH e BIGSTACK ammessi; i bit riservati
    // devono essere 0 (forward-compat: nuovi flag futuri restano rifiutati,
    // mai ignorati).
    if meta.flags
        & !(syscall_numbers::SPAWN_FLAG_DETACH | syscall_numbers::SPAWN_FLAG_BIGSTACK)
        != 0
    {
        return -1;
    }
    let detached = meta.flags & syscall_numbers::SPAWN_FLAG_DETACH != 0;
    let stack_frames = if meta.flags & syscall_numbers::SPAWN_FLAG_BIGSTACK != 0 {
        syscall_numbers::BIGSTACK_FRAMES as u32
    } else {
        crate::arc::vmm_user::USER_STACK_FRAMES as u32
    };
    // Argv packed (S2.5e, stesso formato di exec): copiato in heap kernel
    // (stabile: niente TOCTOU) e validato PRIMA di creare nulla. Zero = argc=0.
    let args_block: Option<alloc::vec::Vec<u8>> = if meta.args_len == 0 {
        if meta.args_ptr != 0 {
            return -1;
        }
        None
    } else {
        if meta.args_ptr == 0
            || meta.args_len > syscall_numbers::ARGS_MAX + 16
            || !crate::arc::vmm_user::is_user_range(meta.args_ptr, meta.args_len as usize)
        {
            crate::serial_println!("[syscall] spawn_image: args fuori range");
            return -1;
        }
        let src =
            unsafe { core::slice::from_raw_parts(meta.args_ptr as *const u8, meta.args_len as usize) };
        let mut staged = alloc::vec::Vec::with_capacity(meta.args_len as usize);
        staged.extend_from_slice(src);
        Some(staged)
    };
    let parsed = match &args_block {
        Some(b) => match crate::arc::vmm_user::parse_args(b) {
            Some(p) => Some(p),
            None => {
                crate::serial_println!("[syscall] spawn_image: args malformati");
                return -1;
            }
        },
        None => None,
    };
    // Porte I/O: solo init (pid 1). Gli altri processi girano senza porte
    // (solo init/disk/fs sono embedded, Fase 21: gli altri partono da disco
    // con le porte del manifest): chiederle = -1.
    let is_init = current_id() == 1;
    if !is_init && meta.io_count != 0 {
        return -1;
    }
    let prio = match meta.prio {
        1..=31 => crate::ordo::sched::Priority(meta.prio),
        _ => return -1, // mai 0 (idle) ne' oltre 31
    };
    for (s, e) in meta.io_ranges[..meta.io_count as usize].iter() {
        if s > e {
            return -1;
        }
    }
    // S-T (T5): parent = leader del gruppo (vedi sys_spawn).
    let parent = crate::ordo::sched::group_leader(current_id() as usize);
    let name_len = meta.name.iter().position(|&b| b == 0).unwrap_or(16);
    match crate::user_binary::spawn_image(
        &meta.name[..name_len],
        prio,
        img_ptr as *const u8,
        img_len,
        Some(parent),
        None,
        &meta.io_ranges[..meta.io_count as usize],
        detached,
        stack_frames,
        parsed.as_ref(),
    ) {
        Some(pid) => {
            let disp = core::str::from_utf8(&meta.name[..name_len]).unwrap_or("?");
            finish_spawn(parent, pid, disp)
        }
        None => {
            crate::serial_println!("[syscall] spawn_image: creazione fallita");
            -1
        }
    }
}

/// spawn_copy (Fase 34, ADR-0024): duplica il chiamante in COW. Nessun
/// argomento. Ritorna al padre `(pid_figlio, canale_nascita)` — pid in `rax`,
/// canale in `rdi` via multi-registro (pattern `ring_alloc`/`ps_info`); al
/// figlio `(0, canale)` (il figlio usa il canale 0 = `CHANNEL_PARENT`). -1 se
/// non c'e' un PID libero, il pool canali e' esaurito o l'OOM colpisce il
/// walk/stack/TSS.
pub(super) fn sys_spawn_copy() -> i64 {
    match crate::ordo::sched::spawn_copy_current() {
        Some((pid, chan)) => super::dispatch::apply_ipc(crate::ordo::sched::IpcResult {
            rax: pid as i64,
            rdi: chan as u64,
            rsi: 0,
            rdx: 0,
            r10: 0,
        }),
        None => -1,
    }
}
