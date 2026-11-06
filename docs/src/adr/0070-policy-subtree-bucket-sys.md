# ADR-0070: Policy di subtree per hash in cardo e bucket `sys:*`

## Status

Proposed (2026-11). Prerequisito per la config dello stack di rete
(ADR-0069) e chiusura del debito `identis:config` (ADR-0058).

## Context

La policy di cardo (Fase 45) è **per hash ma solo su ops**: `SERVICE_POLICY`
→ mask. Il **subtree** è *self-restriction* del client (Fase 17), non imposto
dal server.

Per questo `identis:config` non è protetto: un servizio con `RIGHTS_ALL` (o un
ignoto con `DEFAULT_UNKNOWN_OPS`, che include READ/WRITE) può leggerlo/
scriverlo. È un debito dichiarato in ADR-0058.

Lo stack di rete ha bisogno di una config (`sys:net`) e non vogliamo un unico
`sys:config` condiviso: sarebbe un single point of failure, senza ACL per
chiave. La scelta è **un bucket per servizio** sotto il namespace `sys:`.

## Decision

1. **Policy di subtree lato server**: `SERVICE_POLICY` diventa
   `(hash, ops_mask, subtree)`. Al primo handshake il canale riceve il tetto
   `(ops, subtree)`; il choke point controlla **ops + subtree** per ogni op
   con path. Il client può solo **restringere** (DROP), mai allargare.
2. **Bucket per servizio** sotto il namespace `sys:`:
   - `sys:identis` (rinomina di `identis:config`): Identis legge/scrive;
   - `sys:net`: il servizio `net` legge (read-only);
   - init (TCB) può tutto.
3. **Migrazione**: aggiornare `userland/identis` (`BUCKET_PATH`), il seed
   (`scripts/arca-tool.sh`), i test e la documentazione; il vecchio nome
   `identis:config` sparisce (nessun alias: fail-loud se qualcuno lo usa).
4. **Config di rete**: `sys:net` contiene un file di config (indirizzo,
   prefix, gateway, MTU, policy ping/RA, upstream DNS). Il servizio `net` lo
   legge all'avvio; config invalida → fail-closed. Fallback compilato per i
   test.
5. **Default**: gli hash ignoti restano `DEFAULT_UNKNOWN_OPS` + subtree root
   (nessuna regressione); i servizi noti ricevono il loro subtree. Un
   servizio senza subtree dichiarato resta root (comportamento attuale).
6. **Config service**: non ora; un servizio `config` potrà stare sopra i
   bucket per policy/audit, senza cambiare il meccanismo.

## Consequences

### Positive

- Chiude il debito `identis:config`: solo Identis tocca il suo bucket.
- Config per servizio, least privilege, niente single point of failure.
- Il meccanismo è piccolo: tabella + choke point, nessuna nuova syscall.

### Negative / limiti

- La policy è per hash, non per utente/sessione: un binario con quel hash ha
  quel subtree (il manifest è la fonte di verità).
- Il subtree è controllato all'**open** (path), non su read/write degli fd
  già aperti (gli fd restano capability): è il modello attuale, dichiarato.
- Migrazione del bucket Identis: seed/test/docs da aggiornare nello stesso
  commit.

## Verifica

- Unit test host per `within_subtree` e per la policy (ops+subtree).
- Test in QEMU: un servizio con subtree `sys:net` non può aprire
  `sys:identis` (e viceversa); init può; un ignoto resta al default.
- `identistest` 11/11 invariato; gate 6 righe, zero FAIL.
- `sys:net` letto dal servizio `net` (N1) e config invalida → fail-closed.

## Riferimenti

- ADR-0058 (Identis, debito dichiarato), ADR-0026/0027 (hardening),
  ADR-0068 (threat model), ADR-0069 (stack di rete).
