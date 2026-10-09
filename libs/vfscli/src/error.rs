//! Errore nativo del sistema (vocabolario condiviso, Fase 39/ADR-0030).
//!
//! Traslocato qui da `civis` (S2.2, `vfscli`): modulo neutro — NON in `posix`
//! — perche' lo usano tutti i wrapper, meccanismo e personalita' senza
//! distinzione (`open` come `spawn`, `mmap` come l'IPC). La traduzione in
//! numeri errno POSIX vive SOLO in `flavours/posix/libr` (modulo `posix`,
//! `to_errno`) al bordo: le decisioni usano le varianti, mai i numeri.
//! `civis::Error` e' un re-export di questo (zero churn ai chiamanti); la
//! conversione da `WaitReplyError` resta in `civis` (la' vive il tipo).

/// Errore nativo di un'operazione OS (Fase 39). `Copy` + payload minimi: viaggia
/// per valore nei `Result` senza allocare (hot path IPC/FS invariato).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Server non pronto/raggiungibile: handshake fallito, lookup bound scaduto,
    /// ring mai allocati, retry init-restart esaurito.
    NotReady,
    /// Altra operazione FS in volo (guard 1-in-volo, Fase 13): raccogliere prima.
    Pending,
    /// Richiesta non recapitata: frame non scritto nel ring, coda del peer piena
    /// (backpressure), `send_async` rifiutata. In Fase 39 "coda piena" e "canale
    /// morto" sono indistinguibili dal client: collassano qui.
    RingFull,
    /// Il peer che doveva rispondere e' morto (EXIT_NOTIFY osservata). Senza
    /// payload: chi serve pid/code usa `WaitReplyError` direttamente.
    ServerDied,
    /// Rifiuto di policy senza dettaglio attribuibile dal client (gate
    /// non-figlio-di-init su kill/register/map, porte negate, ...).
    Denied,
    /// Risorse esaurite: PID/canali/pagine (spawn, fork, mmap, sbrk, DMA, ...).
    NoMemory,
    /// Risorsa occupata (slot servizio occupato, mount con fd aperti, CBS cap).
    /// In Fase 39 alcuni rifiuti indistinguibili (es. register: occupato vs
    /// gate) collassano in `Denied`; qui solo l'occupazione certa.
    Busy,
    /// Argomenti o validazione rifiutati (exec oltre bound, meta invalide,
    /// munmap/mprotect parziali, frame malformati, messaggi fuori ordine).
    Invalid,
    /// Rifiuto del server senza dettaglio (Fase 39: cardo risponde solo `ERR`;
    /// la Fase 40 tipizzera' questi casi producendo le varianti di dominio).
    Failed,
    // ── Dominio FS: mapping fissato qui, produttori in Fase 40 ──
    /// Path inesistente.
    NotFound,
    /// Path non directory dove serviva una directory.
    NotDir,
    /// Path directory dove serviva un file.
    IsDir,
    /// Esiste gia' (O_EXCL futuro, mkdir su esistente, ...).
    Exists,
    /// Scrittura/mutazione su volume read-only (FAT senza permessi, ...).
    ReadOnly,
    /// Oltre il limite di dimensione (spawn_image/exec bound, file troppo grosso).
    TooBig,
    // ── Pipe (Fase 42): dal server, mai dal kernel ──
    /// Pipe vuota con writer ancora aperti: i wrapper `read_fs`/`write_fs` la
    /// riprovano throttled da soli (I/O bloccante); la vede solo chi parla il
    /// protocollo ring a mano. Al bordo: EAGAIN.
    Empty,
    /// Estremita' opposta chiusa (write senza lettori): al bordo EPIPE.
    /// Niente segnali in Fase 42: il chiamante vede l'errore e decide.
    Closed,
    // ── Quota (A3, ADR-0044) ──
    /// Budget quota del bucket superato (put/clone/rollback oltre
    /// `quota_blocks`): al bordo ENOSPC. Rifiuto pre-allocazione: mai mezze
    /// scritte, i dati precedenti restano intatti.
    NoSpace,
}


