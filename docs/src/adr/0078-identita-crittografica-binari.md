# ADR-0078: Identità crittografica dei binari — digest, manifest come dato, firma Ed25519

## Status

**Accepted** (2026-11). **Fase 1 implementata**: `ImageId` = BLAKE2s-128
(`syscall-numbers::ImageId`, calcolato una volta in `elf::load` e riusato da
text table e PCB), `peer_info` a 128 bit (`rdi`+`rsi`), manifest e policy come
**dati** (`sys:manifest` / `sys:policy` / `sys:test-policy`, letti a runtime da
init, cardo e usertests). **Fase 2 implementata** (`libs/sig`, `tools/sign`,
`scripts/sign-bins.sh`, verifica in cardo a `R_RIGHTS_EXECUTE`, `sys:anchor`,
t66): vedi §6. Dipende da ADR-0077; la classe arriva nel PCB in fase 3
(ADR-0079).

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

### 6. Fase 2 — implementazione

#### 6.1 La firma è sul contenuto, non sul path

Una firma non vive "accanto al file" nel record ArcaFS: il sidecar sta in un
**namespace dedicato indicizzato dal digest**.

```
sig:<blake2s-256 hex>  ->  [VSIG][key_id : u64 LE][sig : 64]      (76 byte)
```

Conseguenze dirette, e sono la ragione della scelta:

- gli stessi byte seedati in `sys`, `vela`, `usr` e nel mirror `ns` hanno
  **una** firma, non tre (e non possono divergere);
- una copia manomessa ha un digest diverso: **non eredita** la firma
  dell'originale, il sidecar per quel digest semplicemente non esiste;
- nessun ciclo di auto-riferimento: la firma non entra nell'oggetto firmato.

ArcaFS **non cambia**: il formato v4 resta com'è. Un sidecar nel record
(accanto al valore) sarebbe la strada del v5; qui era un costo senza vittoria —
il namespace costa una voce per immagine e zero modifiche a `arcafs`. Il
passaggio al v5 resta valutabile in un ADR suo.

#### 6.2 Dominio del messaggio

Il messaggio firmato è `DOMAIN || bytes`, con
`DOMAIN = b"VELORDO-IMAGE-V1\0"` (include il NUL). Senza prefisso la stessa
chiave firmerebbe indifferentemente un'immagine, un manifest o una riga di
configurazione, e una firma raccolta su un blob varrebbe su un eseguibile. Il
test host lo prova in negativo: la firma "con dominio" **non** verifica sui
byte grezzi.

#### 6.3 Anchor delle chiavi come dato

`sys:anchor` (seedato da `arca-tool.sh`, letto da cardo **prima del READY**
insieme a manifest e policy), una riga per chiave:

```
<key_id : 016x> <pubkey : 64 hex> <system|thirdparty> <nome>
```

- la `key_id` dichiarata è un **indice**, non una prova: cardo la
  **ricalcola** dalla pubkey e una riga incoerente viene scartata loud (una
  chiave non entra nell'anchor per un errore di trascrizione);
- `key_id` = primi 8 byte di BLAKE2s-256(pubkey) come u64 **LE**, la stessa
  derivazione del sidecar — quindi il verificatore sceglie la chiave in O(1)
  senza provarle tutte;
- al massimo 8 chiavi (`ANCHOR_MAX`), oltre si ignora il resto con un log;
- anchor assente = nessuna immagine firmata accettata: i bucket obbligatori
  negano (fail-closed), il boot **non** si rompe, perché i servizi sono
  spawati da init per object_id e non passano da `R_RIGHTS_EXECUTE`.

**Bug trovato implementando**: l'anchor scrive la `key_id` con `%016x` (cioè
big-endian nella stringa), mentre il sidecar la scrive LE. Il primo giro
parsava l'anchor con `from_le_bytes` e scartava **tutte** le righe
(`key_id 7a5eed0a74cbc435 != 35c4cb740aed5e7a`). Il parse corretto è
`from_be_bytes`; il sidecar resta LE. Due rappresentazioni, due conversioni:
non uniformarle senza toccare entrambi i lati.

#### 6.4 Semantica del verdetto

| Stato del sidecar | bucket `sys`/`vela`/`usr` | altri bucket (`tst`, `ns`, …) |
|---|---|---|
| assente | `ERR_DENIED` | `0`, classe `Untrusted` |
| presente, firma valida | `0`, classe della chiave | `0`, classe della chiave |
| presente ma malformato / chiave ignota / firma non valida | `ERR_DENIED` | `ERR_DENIED` |

Regola non negoziabile: un sidecar **presente ma sbagliato** non degrada mai a
"non firmato". Altrimenti basterebbe un byte di disturbo nel sidecar per
riportare un'immagine firmata al ramo permissivo. `Bad` nega ovunque.

Un oggetto **inesistente** non è "non firmato": è `ERR_NOTFOUND` (l'assenza di
firma è una proprietà di un oggetto, non di una chiave mancante).

#### 6.5 Pipeline: chi firma, con quale chiave

La mappa file→bucket esisteva già in un solo posto (`seed_bin` di
`scripts/arca-tool.sh`): la firma si aggancia lì, non in una seconda lista da
tenere d'accordo.

- `scripts/sign-bins.sh one <bucket> <file>` firma un file e restituisce
  `<digest> <sidecar>`; `build`/`anchor`/`digest` sono le altre modalità;
- la **chiave dipende dal bucket di spedizione**, cioè da chi decide la classe:
  `sys`/`vela` → chiave `system` (classe `System`), `usr` → `thirdparty`
  (classe `ThirdParty`); qualsiasi altro bucket **non ammette** firme;
- `seed_bin` chiama `seed_sig` per `sys|vela|usr` e aggiunge la voce `sig:` al
  seed, con dedupe per digest (lo stesso file in `ns` e in `usr` è una voce);
- le chiavi stanno in `tools/sign/keys/` e sono **di test**, con "test-only"
  nel nome; `VELORDO_SIGN_KEY_SYSTEM` / `VELORDO_SIGN_KEY_THIRDPARTY` le
  sostituiscono in release, `VELORDO_SIGN_KEYS_DIR` cambia la directory
  dell'anchor.

Il tool host (`tools/sign`) dipende da `libs/sig`: magic, dominio e layout del
sidecar sono **una** definizione, usata dai due lati. Formato del sidecar mai
duplicato a mano in uno script.

#### 6.6 Verifica e classe a `R_RIGHTS_EXECUTE`

`R_RIGHTS_EXECUTE` risponde `w0` = verdetto (`0` / `ERR_DENIED` /
`ERR_NOTFOUND`) e **`w1` = `ImageClass`** (`0` `Untrusted`, `1` `ThirdParty`,
`2` `System`, tipo condiviso in `syscall-numbers`, ri-esportato da `civis`).
La classe viaggia in `w1` perché il frame di risposta ha un solo registro per
il valore di ritorno: nessuna modifica al protocollo, nessun tag nuovo.

La verifica sta in `userland/cardo/src/verify.rs`:

- due letture dell'oggetto (digest, poi verifica incrementale: l'immagine non
  viene mai copiata). Se il volume non è montato, negato (fail-closed);
- **cache** su `(ImageId, key_id, sig)`: la parte crittografica si evita
  quando il sidecar non cambia. La chiave include il `key_id` e la firma,
  quindi sostituire un sidecar (revoca) invalida di fatto la entry **senza
  hook di purge**;
- `#[inline(never)]`: firewall LTO. Cardo ha 16 KiB di stack e questo è il
  punto in cui entrano LTO e la catena del motore; i byte dell'immagine vivono
  in un `Vec`, mai su stack.

Bucket a firma obbligatoria: `sys`, `vela`, `usr` (`MANDATORY_SIGNED` in
`verify.rs`). `vela` è nella lista pur essendo "driver": la firma senza
write-protection del bucket che la contiene sarebbe teatro, quindi `sig` e
`vela` sono entrate in `policy::protected_bucket` (ADR-0077) — solo init
scrive sidecar e driver.

#### 6.7 Vittoria misurata (t66)

`testland/usertests/src/t_sig.rs`, 7 casi, tutti host-independent (gli
artefatti li seeda `arca-tool.sh`):

1. `vela:bin/gpu.bin` firmato `system` → eseguibile, classe `System`;
2. `usr:runhello` firmato `thirdparty` → eseguibile, classe `ThirdParty`;
3. `tst:test/tests.bin` non firmato → eseguibile, classe `Untrusted`;
4. `vela:bin/unsigned.bin` (non firmato in bucket obbligatorio) → negato;
5. `tst:test/sigbad.bin` = copia di `gpu.bin` con **un byte flippato** e il
   sidecar valido di `gpu.bin` → negato (il digest cambia);
6. `tst:test/sigunk.bin` = sidecar con `key_id` azzerato → negato (chiave
   ignota all'anchor);
7. oggetto inesistente → `NotFound`.

#### 6.8 Limiti dichiarati di fase 2

- **`libr::exec` (POSIX) non passa da `R_RIGHTS_EXECUTE`**: legge per path e
  spawna i byte. La firma è obbligatoria sul path nativo (`interpres` → shell
  nativa); il buco si chiude in fase 3, quando la classe entra nel PCB e lo
  spawn attestato vale per **tutti** i chiamanti. Dichiararlo ora è meglio che
  scoprirlo dopo.
- **TOCTOU verifica→lettura**: tra il verdetto e la `spawn_image` l'oggetto
  può cambiare. La finestra è ristretta e il digest è ricalcolato dal kernel
  allo spawn, ma il confronto "classe del verdetto == classe dell'immagine
  caricata" diventa possibile solo in fase 3 (classe nel PCB).
- **Seed di fallback da FAT** (`seed_sys_disk`) scrive immagini **non
  firmate**: nel boot reale quelle chiavi esistono già (seed host) e il
  fallback non scatta; se scattasse, quelle immagini sarebbero `Untrusted`.
- **Rotazione/revoca** sono manuali: si sostituisce il sidecar o la riga
  dell'anchor e si riavvia cardo. Nessuna lista di revoca firmata, nessuna
  validità temporale.
- **Chiavi di test nel repo**: sono seed di sviluppo, non di produzione. In
  release si passano le chiavi vere via ambiente.

## Consequences

### Positive

- L'identità sopravvive ai rebuild: la policy si scrive su un'identità stabile.
- Aggiungere o aggiornare un servizio non ricompila il kernel.
- Il ciclo `rector`/`cardo` sparisce: la firma è esterna, nessun self-hash.
- L'impersonazione per collisione non è più praticabile (128 bit + firma).
- Il materiale di fiducia è ruotabile e revocabile senza toccare i sorgenti.
- La firma è indirizzata dal contenuto: una voce per immagine, non una per
  copia (`sys`/`vela`/`usr`/`ns`), e la manomissione di una copia rompe il
  legame con la firma invece di ereditarla.

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
- Due rappresentazioni della `key_id` (BE in `sys:anchor`, LE nel sidecar):
  costano una conversione a testa e un bug già pagato. Sono documentate qui
  perché "uniformarle" a metà è il modo di reintrodurlo.
- Fino alla fase 3 il controllo vale solo per il path nativo: `libr::exec`
  legge per path e non chiede il verdetto.

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
- Fase 2: `libs/sig` (formato, dominio, verifica), `tools/sign` (chiavi e
  firme), `scripts/sign-bins.sh`, `userland/cardo/src/verify.rs`,
  `libs/civis/src/fs/rights.rs`, `testland/usertests/src/t_sig.rs`
- RFC 8032 (Ed25519), RFC 7693 (BLAKE2)
