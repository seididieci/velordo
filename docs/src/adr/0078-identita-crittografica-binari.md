# ADR-0078: Identità crittografica dei binari — digest, manifest come dato, firma Ed25519

## Status

**Accepted** (2026-11). **Fase 1 implementata**: `ImageId` = BLAKE2s-128
(`syscall-numbers::ImageId`, calcolato una volta in `elf::load` e riusato da
text table e PCB), `peer_info` a 128 bit (`rdi`+`rsi`), manifest e policy come
**dati** (`sys:manifest` / `sys:policy` / `sys:test-policy`, letti a runtime da
init, cardo e usertests). Fase 2 (`libs/sig` + `tools/sign` + verifica in
cardo) da fare. Dipende da ADR-0077.

Vittoria misurata: `touch userland/netecho/src/main.rs` + build → il kernel
**non** viene ricompilato. Per ottenerla sono serviti due fix di build che
mascherano il risultato: `build_one` non riscrive i `.bin` identici (mtime
stabile, temp fuori dalla dir) e `kernel/build.rs` non osserva piu' la
*directory* `userland/build` (contiene `target/`, riscritto a ogni build).

**Bug trovato implementando**: il parser della policy richiedeva 16 cifre hex
anche per la mask (4 cifre) → 0 righe lette e ogni test ricadeva nel default
restrittivo. Un `if s.len() < 16` copiato da `hi`/`lo`.

## Context

L'identità di un processo è oggi `FNV-1a 64` sui byte dell'ELF, misurata dal
kernel allo spawn (`syscall_numbers::image_hash`) e confrontata con un manifest
**compilato**:

- `scripts/gen-service-hashes.sh` genera `service_hashes.rs` e
  `service_policy.rs`;
- init li include (`b"netecho" => Some(HASH_NETECHO)`) e verifica pre-spawn;
- cardo li usa come `SERVICE_POLICY` (tetto ops).

Tre problemi, di natura diversa:

1. **FNV-1a 64 non è crittografico.** `spawn_image` accetta byte arbitrari dal
   chiamante: per ottenere la policy di `netecho` basta una collisione. Con
   FNV-1a e padding controllabile è questione di lavoro, non di fortuna.
2. **L'identità è legata ai byte**, quindi cambia a ogni rebuild: si rigenera
   il manifest, si ricompilano init/cardo/usertests e — poiché init e cardo
   sono `include_bytes!` nel kernel — **si ricompila il kernel**. Aggiungere un
   servizio costa un rebuild del kernel.
3. **Ciclo di auto-riferimento**: `rector` e `cardo` sono esclusi dal manifest
   perché contengono l'hash di sé o di chi li embedda (wart documentato in
   Fase 36).

## Decision

### 1. Digest crittografico

`image_id` = **BLAKE2s-256 troncato a 128 bit** (`[u64; 2]`), calcolato una
volta in `elf::load` e riusato dalla tabella shared text e dal PCB. Il crate
`blake2s` è già nel repo (`no_std`, test host, usato da ArcaFS).

`peer_info` restituisce `lo` in `rdi` e `hi` in `rsi` (`rax` resta 0). 128 bit
danno resistenza alle collisioni 2⁶⁴: FNV a 64 bit ne dava 2³².

### 2. Manifest e policy come **dati**

`gen-service-hashes.sh` continua a generare i file Rust (compatibilità) ma
produce anche i dati autorevoli:

- `sys:manifest` — `nome id_hi id_lo` per riga;
- `sys:policy` — `id_hi id_lo mask` per riga.

init legge `sys:manifest` all'avvio, cardo legge `sys:policy`. Se mancano:
**fail-closed** (init non spawana servizi non elencati, cardo applica il
default restrittivo), mai fallback silenzioso.

Effetto misurabile: cambiare il codice di un servizio **non ricompila più il
kernel** (init e cardo non cambiano byte).

### 3. Firma Ed25519

Formato sidecar accanto all'oggetto (mai dentro l'ELF: self-reference):

```
[magic "VSIG" : 4][key_id : u64 LE][sig : 64]
```

- **Curva: Ed25519** (deciso). Deterministico (nessun nonce da proteggere),
  firma 64 B, chiave 32 B. P-256 sarebbe stato gratis in dipendenze ma ECDSA
  richiede un nonce e il suo uso errato è la causa classica di chiavi
  compromesse.
- **Crate**: `ed25519-compact` (pure Rust, `no_std`, nessuna dipendenza
  obbligatoria), `default-features = false` per non linkare `std`, `x25519`,
  `pem`. `random` serve solo al tool host per generare le chiavi.
- **`key_id`**: u64 derivato dalla pubkey (primi 8 byte di BLAKE2s della
  chiave), così il verificatore sceglie la chiave senza provarle tutte.
- **Nessun ASN.1/X.509**: X.509 resta confinato a `libs/tls` per TLS.

### 4. Verifica in userspace, a `R_RIGHTS_EXECUTE`

La verifica sta in **cardo**, dove i byte entrano nel sistema:

- `R_RIGHTS_EXECUTE` (0x37) è già il cancello che la shell usa prima di
  `spawn_image` di un oggetto: cardo verifica la firma del sidecar e risponde
  `0` solo se valida, `ERR_DENIED` altrimenti;
- la classe risultante (`System`/`ThirdParty`/`Untrusted`, ADR-0079) è
  calcolata da `key_id` → classe, e messa in cache con il digest.

Il kernel **non** contiene crypto: nessun parser di firme nel TCB, nessun costo
di verifica per spawn (Ed25519 ~1 ms nativo, decine di ms sotto TCG).

### 5. Chiavi

La **root resta offline** e firma solo le release key. In repo c'è una chiave
di **test**, con nome esplicito, usata da `tools/sign` solo se
`VELORDO_SIGN_KEY` non è impostata. Un binario senza sidecar non è un errore di
build: è un binario `Untrusted` (il codice dell'utente deve restare
eseguibile).

## Consequences

### Positive

- L'identità sopravvive ai rebuild: la policy si scrive su un'identità stabile.
- Aggiungere o aggiornare un servizio non ricompila il kernel.
- Il ciclo `rector`/`cardo` sparisce: la firma è esterna, nessun self-hash.
- L'impersonazione per collisione non è più praticabile (128 bit + firma).
- Il materiale di fiducia è ruotabile e revocabile senza toccare i sorgenti.

### Negative

- Costo digest per spawn: ~0,3 ms nativo, ~3–10 ms TCG su un'ELF da 300 KiB.
  Mitigazione se pesa: cache `(len, FNV) → digest` nel loader; il digest si
  calcola una volta per immagine distinta.
- Una dipendenza crittografica nuova in `libs/sig` (verificata: `ed25519-compact`
  2.6.1, `no_std`).
- Gestione chiavi reale: root offline, rotazione, revoca. Senza disciplina la
  root finisce sul portatile di tutti.
- cardo entra nel TCB della fiducia: se mente, la catena mente. È già il
  server FS, quindi il salto è piccolo, ma va dichiarato.
- `peer_info` cambia ABI (128 bit): vanno aggiornati cardo, net, siit,
  vestigia, il wrapper `civis` e t51.

### Neutral

- Nessuna nuova syscall in fase 1–2; nessun cambio al kernel oltre al digest e
  all'ABI di `peer_info`.
- `sys:manifest`/`sys:policy` protetti da ADR-0077.

## Alternatives Considered

- **Restare su FNV**: collisione scelta fattibile su byte craftati; non è
  un'identità, è un indice.
- **Digest a 256 bit**: inutile raddoppiare registri e confronti; 128 bit
  bastano per le collisioni, la firma copre il resto.
- **Verifica della firma nel kernel**: costo CPU per spawn (decine di ms in
  TCG) e parser crittografico nel TCB. Scartato.
- **X.509 per i binari**: riuserebbe `libs/tls`, ma porta ASN.1, validità
  temporale e name constraints dove non servono. Scartato: formato binario
  fisso.
- **Firma dentro l'ELF**: self-reference (firmare cambia i byte firmati).
  Scartato.
- **Manifest compilato ma firmato**: non risolve il rebuild del kernel.
  Scartato.

## References

- ADR-0026 (threat model), ADR-0027 (identità misurata, Fase 36),
  ADR-0077 (bucket protetti), ADR-0079 (classi e spawn attestato)
- `kernel/src/elf.rs`, `kernel/src/ordo/process.rs`, `kernel/src/text.rs`
- `scripts/gen-service-hashes.sh`, `userland/rector/src/main.rs`,
  `userland/cardo/src/policy.rs`
- RFC 8032 (Ed25519), RFC 7693 (BLAKE2)
