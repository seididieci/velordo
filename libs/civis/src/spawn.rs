use super::*;

/// `spawn(name)`: chiede al kernel di creare un nuovo processo dal binario
/// embedded chiamato `name`. Il kernel crea il canale di nascita tra il
/// chiamante (parent) e il figlio: il figlio lo usa come canale 0 (parent), il
/// chiamante riceve qui il channel id per parlare col figlio. Ritorna il
/// channel id; `NoMemory` se il kernel non ha PID/canali (Fase 39).
#[inline]
pub fn spawn(name: &[u8]) -> Result<i64, Error> {
    let pid = unsafe { syscall4(SYS_SPAWN, name.as_ptr() as u64, name.len() as u64, 0, 0) };
    if pid < 0 {
        return Err(Error::NoMemory);
    }
    Ok(pid)
}

/// Metadati di `spawn_image` (Fase 21, servizi da disco; S2.5e: 56 B con
/// argv): layout `repr(C)`, identico allo `SpawnMeta` kernel (validato per
/// size). Nome NUL-padded
/// (non vuoto, stampabile); `prio` 1..31; fino a 4 range I/O (start <= end);
/// `flags` (Fase 22: `SPAWN_FLAG_DETACH`; S2.5d: `SPAWN_FLAG_BIGSTACK` via
/// `.bigstack()`; resto riservato = 0).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct SpawnMeta {
    pub name: [u8; 16],
    pub prio: u8,
    pub io_count: u8,
    pub flags: u8,
    pub _pad: [u8; 5],
    pub io_ranges: [(u16, u16); 4],
    /// S2.5e: blocco packed `[argc:8][envc:8][stringhe NUL]` (come exec);
    /// zero = nessun argv → argc=0.
    pub args_ptr: u64,
    pub args_len: u64,
}

impl SpawnMeta {
    /// Costruisce i metadati da nome/priorita'/porte (tronca il nome a 16,
    /// NUL-padded; piu' di 4 range → i primi 4? No: troppi → None, fail-loud).
    /// Flags a 0 (attached: cascata di morte normale).
    pub fn new(name: &str, prio: u8, io: &[(u16, u16)]) -> Option<Self> {
        if name.is_empty() || io.len() > 4 {
            return None;
        }
        let mut m = SpawnMeta {
            name: [0u8; 16],
            prio,
            io_count: io.len() as u8,
            flags: 0,
            _pad: [0u8; 5],
            io_ranges: [(0, 0); 4],
            args_ptr: 0,
            args_len: 0,
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(16);
        m.name[..n].copy_from_slice(&bytes[..n]);
        m.io_ranges[..io.len()].copy_from_slice(io);
        Some(m)
    }

    /// Marca il figlio come detached (Fase 22): alla morte del parent viene
    /// ri-parentato a init invece di terminare in cascata. Scelta dello
    /// spawner (builder: il figlio non puo' auto-staccarsi), irrevocabile.
    pub fn detached(mut self) -> Self {
        self.flags |= SPAWN_FLAG_DETACH;
        self
    }

    /// Stack largo per il figlio (S2.5d, `BIGSTACK_FRAMES`): per programmi
    /// che vogliono MB di stack (lo staticone rustc). Scelta dello spawner
    /// (builder, come `detached`), irrevocabile.
    pub fn bigstack(mut self) -> Self {
        self.flags |= SPAWN_FLAG_BIGSTACK;
        self
    }

    /// Argv/env packed per il figlio (S2.5e, builder come gli altri flag):
    /// `block` = `[argc:8][envc:8][stringhe NUL-terminate]` (stesso formato
    /// di exec; vedi `pack_args` per costruirlo). Il blocco deve restare vivo
    /// per tutta la `spawn_image` (il kernel lo copia dentro la syscall).
    pub fn argv(mut self, block: &[u8]) -> Self {
        self.args_ptr = block.as_ptr() as u64;
        self.args_len = block.len() as u64;
        self
    }
}

/// Costruisce il blocco packed `[argc:8][envc:8][stringhe NUL]` per
/// `SpawnMeta::argv` da fette di argomenti e ambiente (byte opachi, niente
/// `NAME=val` imposto: la convenzione vive qui, mai nel kernel — ADR-0025).
/// `None` se il totale supera `ARGS_MAX`.
pub fn pack_args(argv: &[&[u8]], env: &[&[u8]]) -> Option<alloc::vec::Vec<u8>> {
    let mut out = alloc::vec::Vec::new();
    let argc = argv.len() as u64;
    let envc = env.len() as u64;
    if argc > 1024 || envc > 1024 {
        return None;
    }
    out.extend_from_slice(&argc.to_le_bytes());
    out.extend_from_slice(&envc.to_le_bytes());
    for s in argv.iter().chain(env.iter()) {
        if s.contains(&0) {
            return None; // NUL interni proibiti (C-string)
        }
        out.extend_from_slice(s);
        out.push(0);
    }
    if out.len() as u64 > ARGS_MAX + 16 {
        return None;
    }
    Some(out)
}

/// `spawn_image(img, meta)`: come `spawn` ma il binario e' letto dalla memoria
/// del chiamante (Fase 21, servizi da disco e helper di test). Primitiva
/// generale: le porte I/O restano privilegio di init (pid 1, gli altri con
/// `io_count == 0` o rifiuto). Ritorna il channel di nascita; `Invalid` se
/// l'immagine/meta sono malformati (pre-validati qui, stesso bound del kernel
/// `SPAWN_IMAGE_MAX`), `NoMemory` se il kernel esaurisce PID/canali/frame
/// (Fase 39: i due rifiuti kernel collassano, la pre-validazione distingue).
#[inline]
pub fn spawn_image(img: &[u8], meta: &SpawnMeta) -> Result<i64, Error> {
    if img.is_empty() || img.len() > SPAWN_IMAGE_MAX {
        return Err(Error::Invalid);
    }
    let c = unsafe {
        syscall4(
            SYS_SPAWN_IMAGE,
            img.as_ptr() as u64,
            img.len() as u64,
            (meta as *const SpawnMeta) as u64,
            core::mem::size_of::<SpawnMeta>() as u64,
        )
    };
    if c < 0 { Err(Error::NoMemory) } else { Ok(c) }
}

/// `exec_image(img)`: sostituisce l'immagine del chiamante con l'ELF `img`
/// (Fase 37, `SYS_EXEC` in-place, senza argv → argc=0). Stesso PID/parent/
/// priorita'/canali; cade l'address space e ne viene caricato uno nuovo;
/// stack nuovo; `image_hash` rimisurato; porte I/O azzerate. NON ritorna mai
/// in caso di successo (salta all'entry della nuova immagine con `rax = 0`);
/// ritorna `Err(())` solo a validazione fallita (processo intatto,
/// completamente utilizzabile). Il FS va ri-fatto lazy: la nuova immagine
/// parte con stato `civis` pristine (BSS azzerato) e `fs_init` rifa' handshake
/// al primo uso. `Invalid` a validazione fallita (Fase 39).
#[inline]
pub fn exec_image(img: &[u8]) -> Result<(), Error> {
    exec_image_args(img, &[])
}

/// `exec_image_args(img, args)`: come `exec_image` ma con argv+env (37.1, env
/// in 43a). `args` = blocco `[argc:8][envc:8][payload NUL-separated]` entro
/// `ARGS_MAX` (normalmente costruito dalla personalita' POSIX,
/// `flavours/posix/libr`, non a mano);
/// vuoto = argc=0. `Invalid` a validazione fallita (Fase 39).
#[inline]
pub fn exec_image_args(img: &[u8], args: &[u8]) -> Result<(), Error> {
    let r = unsafe {
        syscall4(
            SYS_EXEC,
            img.as_ptr() as u64,
            img.len() as u64,
            args.as_ptr() as u64,
            args.len() as u64,
        )
    };
    // Successo = nessun ritorno (siamo nella nuova immagine); -1 = rifiuto.
    let _ = r;
    Err(Error::Invalid)
}

/// `service_register(service)`: occupa lo slot del servizio (ADR-0008). Il
/// chiamante diventa l'owner raggiungibile per nome. `Denied` a rifiuto
/// (Fase 39: gate non-figlio-di-init nel caso comune; slot occupato collassa
/// qui, indistinguibile dal client).
#[inline]
pub fn service_register(service: Service) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_SERVICE_REGISTER, service as u64, 0, 0, 0) };
    if r < 0 { Err(Error::Denied) } else { Ok(()) }
}

/// `service_lookup(service)`: risolve il servizio in un channel verso
/// l'attuale owner. Ritorna il channel id (>= 0); `NotFound` se non registrato
/// (Fase 39: il caso comune — lookup pre-server — e' preciso).
#[inline]
pub fn service_lookup(service: Service) -> Result<i64, Error> {
    let c = unsafe { syscall4(SYS_SERVICE_LOOKUP, service as u64, 0, 0, 0) };
    if c < 0 { Err(Error::NotFound) } else { Ok(c) }
}

/// Fase 14 (init-restart) — `service_pid(service)`: ritorna il pid
/// dell'attuale owner del servizio; `NotFound` se non registrato (Fase 39).
/// Usato per supervisione/diagnostica (es. verificare che un servizio
/// riavviato sia un processo NUOVO, pid diverso dal precedente).
#[inline]
pub fn service_pid(service: Service) -> Result<i64, Error> {
    let p = unsafe { syscall4(SYS_SERVICE_PID, service as u64, 0, 0, 0) };
    if p < 0 { Err(Error::NotFound) } else { Ok(p) }
}

/// Fase 35 (hardening) — `peer_pid(chan)`: pid del peer del canale `chan`
/// (0 = canale di nascita, come `send`/`recv`); `ServerDied` se il canale non
/// esiste o il peer e' morto (Fase 39). I server lo usano per attribuire una
/// richiesta a un processo (es. la policy `FS_REGISTER` di cardo distingue
/// i figli di init).
#[inline]
pub fn peer_pid(chan: u64) -> Result<i64, Error> {
    let p = unsafe { syscall4(SYS_PEER_PID, chan, 0, 0, 0) };
    if p < 0 { Err(Error::ServerDied) } else { Ok(p) }
}

/// Fase 36 (identita' misurata, Strato 2 di ADR-0026) — `peer_info(chan)`:
/// hash dell'immagine del peer del canale `chan` (0 = canale di nascita);
/// `ServerDied` se il canale non esiste o il peer e' morto (Fase 39). I server
/// lo usano per la policy su identita' (es. cardo accetta il replace di un
/// prefix solo dallo stesso binario; init verifica il manifest pre-spawn).
#[inline]
pub fn peer_info(chan: u64) -> Result<u64, Error> {
    let (rax, rdi, _, _, _) = unsafe { syscall4_out(SYS_PEER_INFO, chan, 0, 0, 0) };
    if rax != 0 { Err(Error::ServerDied) } else { Ok(rdi) }
}

/// Fase 35 (hardening) — `init_bounce(service)`: chiede a init (canale di
/// nascita, solo per figli di init) di uccidere+riavviare il servizio
/// supervisionato `service`. Uccidere un server supervisionato e' operazione
/// da supervisore: i test guidano il caos tramite init invece di killare
/// direttamente (il kill diretto e' parent-scoped). Ritorna il pid ucciso;
/// errori nativi (Fase 39: `ServerDied` se init irraggiungibile, `Failed` se
/// init rifiuta). La morte+restart si osservano poi via `service_pid`.
#[inline]
pub fn init_bounce(service: Service) -> Result<i64, Error> {
    match send(CHANNEL_PARENT, INIT_BOUNCE, service as u64, 0) {
        Ok(r) if r.w0 != u64::MAX => Ok(r.w0 as i64),
        Ok(_) => Err(Error::Failed),
        Err(e) => Err(e),
    }
}

/// `map_physical(phys, virt, count)`: mappa `count` pagine fisiche a partire
/// da `phys` all'indirizzo virtuale `virt` nello spazio del chiamante.
/// Usato dal console server per accedere al frame buffer VGA.
/// `Denied` se il frame non e' mappabile (gate Fase 35, Fase 39).
#[inline]
pub fn map_physical(phys: u64, virt: u64, count: usize) -> Result<(), Error> {
    let r = unsafe { syscall4(SYS_MAP_PHYSICAL, phys, virt, count as u64, 0) };
    if r < 0 {
        return Err(Error::Denied);
    }
    Ok(())
}
