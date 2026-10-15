//! Layout argv/env sullo stack user (37.1.2, env in 43a): meccanismo neutro
//! (ADR-0025) condiviso da exec e spawn — il kernel non ispeziona mai il
//! contenuto (byte opachi, la convenzione `NAME=val` vive in `civis`/shell).
//!
//! Formato blocco: `[argc:8][envc:8][esattamente argc+envc stringhe
//! NUL-terminate concatenate, niente coda]`; bound `ARGS_MAX` + fit nello
//! stack (<= 16 KiB: vale per lo stack standard e a maggior ragione per il
//! bigstack S2.5d).
//!
//! `layout_argv` scrive a partire da `write_base` ma registra indirizzi VA
//! calcolati da `stack_top`: in exec i due coincidono (CR3 proprio attivo,
//! scrittura via VA user); nello spawn `write_base` e' la direct map del top
//! stack fisico (CR3 del chiamante attivo: scrivere VA user del figlio
//! toccherebbe il chiamante) mentre gli indirizzi registrati restano VA user
//! del figlio. Stessi offset in entrambi i casi (il layout dipende solo dalle
//! taglie): ritorna il nuovo rsp (punta ad argc, `rsp % 16 == 8`).

/// Pagina stack: il layout argv sta nei primi 16 KiB (come lo stack user
/// standard; il bigstack e' solo piu' fondo).
const STACK_BYTES: u64 = 4 * 0x1000;

/// Argomenti parsati: `arg_strs`/`env_strs` = coppie (offset, len) nel
/// payload, senza NUL (prima gli argv, poi gli env). Il blocco e' la copia
/// owned del chiamante (heap kernel): sopravvive al teardown dello spazio
/// user (exec) e alla creazione (spawn).
pub(crate) struct ParsedArgs<'a> {
    pub(crate) argc: u64,
    pub(crate) envc: u64,
    pub(crate) payload: &'a [u8],
    pub(crate) arg_strs: alloc::vec::Vec<(usize, usize)>,
    pub(crate) env_strs: alloc::vec::Vec<(usize, usize)>,
}

/// Parsifica+valida il blocco args PRIMA di toccare qualunque stato: `None` =
/// rifiuto atomico (processo intatto, mai teardown a meta').
pub(crate) fn parse_args(block: &[u8]) -> Option<ParsedArgs<'_>> {
    if block.len() < 16 || block.len() as u64 > syscall_numbers::ARGS_MAX + 16 {
        return None;
    }
    let argc = u64::from_le_bytes(block[..8].try_into().ok()?);
    let envc = u64::from_le_bytes(block[8..16].try_into().ok()?);
    if argc > 1024 || envc > 1024 {
        return None;
    }
    let payload = &block[16..];
    let mut arg_strs = alloc::vec::Vec::new();
    let mut env_strs = alloc::vec::Vec::new();
    let mut off = 0usize;
    for _ in 0..argc {
        let end = payload.get(off..)?.iter().position(|&b| b == 0)?;
        arg_strs.push((off, end));
        off += end + 1;
    }
    for _ in 0..envc {
        let end = payload.get(off..)?.iter().position(|&b| b == 0)?;
        env_strs.push((off, end));
        off += end + 1;
    }
    if off != payload.len() {
        return None;
    }
    let total = payload.len() as u64 + 8 * (argc + 1) + 8 * (envc + 1) + 8 + 16;
    if total > STACK_BYTES {
        return None;
    }
    Some(ParsedArgs { argc, envc, payload, arg_strs, env_strs })
}

/// Stende lo stack argv+env in ordine Linux sotto `stack_top` (stringhe in
/// alto, poi array `argv[]` + NULL + `envp[]` + NULL, argc in basso).
/// Scrive a partire da `write_base` (= `stack_top` in exec; direct map del
/// top fisico nello spawn); gli indirizzi VA registrati negli array sono
/// sempre relativi a `stack_top`. Infallibile per costruzione (fit
/// pre-verificato da `parse_args`).
pub(crate) unsafe fn layout_argv(
    write_base: u64,
    stack_top: u64,
    parsed: &ParsedArgs<'_>,
) -> u64 {
    // Scostamento base-scrittura − base-VA (0 in exec; phys−user nello spawn).
    let delta = write_base.wrapping_sub(stack_top);
    // Indirizzi VA delle stringhe (heap kernel, mai stack kernel: l'array
    // statico da 8 KiB rischierebbe l'overflow dei 16 KiB di stack).
    let mut arg_addrs = alloc::vec::Vec::new();
    let mut env_addrs = alloc::vec::Vec::new();
    let mut sp_str = stack_top;
    for &(off, len) in parsed.arg_strs.iter().chain(parsed.env_strs.iter()) {
        sp_str -= len as u64 + 1;
        unsafe {
            core::ptr::copy_nonoverlapping(
                parsed.payload[off..].as_ptr(),
                sp_str.wrapping_add(delta) as *mut u8,
                len + 1,
            );
        }
        if arg_addrs.len() < parsed.argc as usize {
            arg_addrs.push(sp_str);
        } else {
            env_addrs.push(sp_str);
        }
    }
    let arr = (sp_str & !15) - 8 * (parsed.argc + 1) - 8 * (parsed.envc + 1);
    for (i, &a) in arg_addrs.iter().enumerate() {
        unsafe { core::ptr::write((arr + i as u64 * 8).wrapping_add(delta) as *mut u64, a) };
    }
    unsafe {
        core::ptr::write((arr + parsed.argc * 8).wrapping_add(delta) as *mut u64, 0); // argv NULL
        let ebase = arr + (parsed.argc + 1) * 8;
        for (i, &a) in env_addrs.iter().enumerate() {
            core::ptr::write((ebase + i as u64 * 8).wrapping_add(delta) as *mut u64, a);
        }
        core::ptr::write((ebase + parsed.envc * 8).wrapping_add(delta) as *mut u64, 0); // envp NULL
        let rsp = arr - 8;
        core::ptr::write(rsp.wrapping_add(delta) as *mut u64, parsed.argc);
        rsp
    }
}
