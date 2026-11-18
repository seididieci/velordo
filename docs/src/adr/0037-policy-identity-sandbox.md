# ADR-0037: Policy su identita' + sandbox build (Fase 45)

**Status**: Implemented (Fase 45 — gate 5/5 + 7/7 + 57/57 + shell 179 check).

## Context

Dopo la Fase 44b il sistema ha:
- Self-restriction sui canali (Fase 17): DROP irrevocabile, mai widen.
- Identita' misurata (Fase 36): hash FNV-1a nel PCB, manifest a build-time verificato da init, policy `FS_REGISTER` su identita'.
- POSIX in userspace (Fasi 39-44b): fd virtuali, pipe, redirect, job control, segnali.

Ma il modello resta cooperativo: un canale sconosciuto ha default `{ALL, root}` — nessun tetto server-side per hash ignoto. Per self-hosting serve che i binari non verificati non possano montare filesystem, creare pipe o concedere fd ad altri processi.

## Decision

**Policy su identita' a build-time, enforcement in userfs** (zero kernel).

### 45.0 — Bit nuovi
- `RIGHTS_GRANT (0x200)` per `R_DUP_GRANT` (handoff capability): senza, il grant e' negato (CLAIM/CANCEL restano liberi: consumano grant propri o del parent).
- `RIGHTS_PIPE (0x400)` per `R_PIPE_CREATE`: senza, pipe_create e' negato.
- `ALL = 0x7FF` (esteso da 0x1FF): retrocompatibilita' — default ALL invariato dove la policy non dice altro.

### 45.1 — Tabella policy a build-time
Due tabelle generate con lo stesso pattern del manifest (Fase 36):

- `SERVICE_POLICY` (`gen-service-hashes.sh`, `build-meta/service_policy.rs`): hash→ops per i servizi userland (esclusi init/userfs: ciclo hash-di-se'). TCB → ALL. Programmi di terzi → mask restrittiva.
  - Oggi: `userrunhello` = `0x00F` (OPEN|READ|WRITE|READDIR; senza MKDIR/DELETE/SEEK/MOUNT/UMOUNT/GRANT/PIPE).
- `TEST_POLICY` (`gen-test-policy.sh`, `build-meta/test_policy.rs`): hash→ops per i binari testland. Tutti ALL (la suite esercita ogni op). I negativi stanno in t57 sul default ignoto. Escluso `userforeign.bin` (attore "ignoto" di t57: DEVE restare fuori da ogni tabella).

### 45.2 — Enforcement
- `ceiling_for(chan)` in `policy.rs`: classifica il peer UNA volta all'handshake (`FS_BUF_REG`).
  - init → ALL.
  - hash noto al manifest → mask della riga.
  - hash noto ai test → ALL.
  - ignoto → `DEFAULT_UNKNOWN_OPS = 0x19F` (niente MOUNT/UMOUNT/GRANT/PIPE).
- Nel choke point: `drop_mask & ceiling & bit == 0` → `ERR_DENIED` (un permesso mancante è un verdetto, non un guasto: con `ERR` il client vedeva `Error::Failed` e la causa era indistinguibile da un errore interno — corretto in ADR-0080 fase 6). La policy non allarga mai: DROP resta irrevocabile, GET riporta il tetto.
- Carve-out: init-child (parent==1, difesa in profondita'), slot Test aperto.

### 45.3 — Test
- t57: helper noto (GET default ALL, drop GRANT→negato, drop PIPE→negato, op valida dopo); attore ignoto `foreign.bin` (mount/grant/pipe_create negati dal default 0x19F, open+read/write+seek lecite).
- t34 resta per ultimo: nessun drop sul canale di usertests.

## Consequences

### Positive
- Zero cambi di protocollo (solo dinieghi ERR in piu'); la suite gira a policy attive.
- Estendibile: il futuro toolchain self-hosted avra' righe esplicite, zero redesign.
- `foreign.bin` prova il default fail-closed senza toccare nessun binario TCB.

### Negative
- Carve-out init-child riapre un buco formale (un figlio di init compromesso e' full): dichiarato, lo stesso principio Strato 1+2.
- Diritti restano effimeri (restart userfs = re-handshake).
- Generazioni PID e Strato 3 rimangono parcheggiati.

### Neutral
- `0x19F` come default ignoto: OPEN/READ/WRITE/READDIR/MKDIR/DELETE/SEEK restano; niente MOUNT/UMOUNT/GRANT/PIPE (le op che creano stato globale o capability per altri).
- Tabella test separata (`TEST_POLICY`): mai inclusa dai binari test (niente ciclo, come l'esclusione rector/userfs in Fase 36).

## Alternatives Considered

- **Policy nel kernel**: scartata — romperebbe la neutralita' ADR-0025; il kernel conosce solo meccanismo.
- **Default restrittivo per TUTTI (no carve-out)**: scarterebbe init+figli, non self-hosting. Il carve-out e' una difesa in profondita', non alternativa.
- **Hardening lato shell**: la shell droppa i propri diritti — ma un binario ostile non li dropperebbe mai. Serve il tetto server-side.

## References

- `userland/fs/src/policy.rs`, `userland/fs/src/server.rs` (choke point + handshake), `userland/fs/src/rights.rs` (drop/get con ceiling)
- `scripts/gen-service-hashes.sh`, `scripts/gen-test-policy.sh` (generazione tabelle)
- `testland/foreign/` (attore ignoto di t57)
- ADR-0026 (hardening), ADR-0027 (identita' misurata), ADR-0014 (diritti per-canale), Fase 36 (manifest)
