// Fase 37 (exec in-place): sys_exec.
use super::spawn::SPAWN_IMAGE_MAX;

/// exec_image(img_ptr, img_len, args_ptr, args_len): sostituisce l'immagine
/// del CHIAMANTE con l'ELF in sua memoria (Fase 37; argv in 37.1.2). Il kernel
/// non tocca mai il FS (ADR-0005): caricare da path e' compito di `civis::exec`
/// (load_file + questa syscall). `args_len == 0` = nessun argv (argc=0, come
/// `exec_image`); altrimenti blocco `[argc:8][envc:8][payload]` entro
/// `ARGS_MAX` (env = byte opachi, mai ispezionati: kernel neutro).
/// Stesso bound immagine di `spawn_image` (256 KiB); validazione ELF + args
/// prima di toccare qualunque stato (fallita = -1, processo intatto).
/// Successo = nessun ritorno (salto all'entry nuova); il valore 0 non e' mai
/// osservato dal chiamante.
pub(super) fn sys_exec(img_ptr: u64, img_len: usize, args_ptr: u64, args_len: usize) -> i64 {
    if img_len == 0 || img_len > SPAWN_IMAGE_MAX {
        return -1;
    }
    if args_len as u64 > syscall_numbers::ARGS_MAX + 8 {
        return -1;
    }
    if !crate::arc::vmm_user::is_user_range(img_ptr, img_len)
        || (args_len > 0 && !crate::arc::vmm_user::is_user_range(args_ptr, args_len))
    {
        crate::serial_println!("[syscall] exec: fuori dallo spazio user");
        return -1;
    }
    // S2.5c (exec streaming): l'IMMAGINE resta slice user (niente staging in
    // heap: `exec_current` la legge prima del teardown, come `spawn_image`).
    // Solo gli ARGS si copiano (<= 8 KiB: il blocco sopravvive perche'
    // `layout_argv` gira DOPO, a spazio nuovo attivo).
    let src = unsafe { core::slice::from_raw_parts(img_ptr as *const u8, img_len) };
    let args = if args_len == 0 {
        None
    } else {
        let asrc =
            unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len) };
        Some(asrc.to_vec())
    };
    match crate::ordo::sched::exec_current(src, args.as_deref()) {
        Ok(()) => 0,
        Err(()) => -1,
    }
}
