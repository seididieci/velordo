# ADR-0072: TLS per DoT — `embedded-tls` dietro `libs/tls`, fiducia e limiti

## Status

Proposed (2026-11). **N6.2 completato** (crate `libs/tls` + prova handshake
TLS 1.3 reale in QEMU, `scripts/run-tls-tests.sh`). Diventa Accepted a
chiusura di N6.3 (DoT dentro `dns`).

## Context

N6.0 ha costruito l'inoltro DNS upstream "nella forma che avrà il DoT"
(endpoint effimero, single-flight, deadline, txid rimappato) e riconosce
`mode = dot` degradandolo con un log. N6.1 ha dato il TCP client reale. Manca
il trasporto TLS.

Requisiti: TLS 1.3 client, `no_std`, trasporto iniettato (il nostro TCP a
endpoint non è un socket), verifica dei certificati con clock reale
(`usertime`), nessuna allocazione obbligatoria, codice di sicurezza nostro il
meno possibile.

Cosa esiste (verificato compilando per il target, ADR-0071):

- **RustCrypto**: `sha2`, `hmac`, `hkdf`, `chacha20poly1305`, `aes-gcm`,
  `x25519-dalek`, `p256`, `rsa`, `der`, `x509-cert`, `rustls-webpki` — tutti
  `no_std`. Ottimi come primitive, ma handshake e policy X.509 restano
  nostri.
- **`embedded-tls` 0.19**: client TLS 1.3 `no_std` (con o senza alloc),
  blocking e async, **cipher suite incluse** (`Aes128GcmSha256`,
  `Aes256GcmSha384`, …), verifica catena con `pki::CertVerifier` e trait
  `TlsClock`, feature `rsa` per le catene RSA, ALPN. Trasporto = trait
  `embedded-io`, quindi si adatta al nostro endpoint senza toccare il core.
- **`rustls`**: `no_std` solo in `0.24.0-dev.2`; il provider
  `rustls-rustcrypto` è `0.1.0-pre.0` e pinna rustls 0.23 (std-only).
  Inutilizzabile oggi.
- **`ring`/`aws-lc-rs`**: C/asm con `build.rs` e dipendenze di piattaforma.
  Fuori dal bare metal.
- **`libcrux`**: primitive verificate, non un client TLS completo.

Scrivere handshake + X.509 da zero significherebbe ~1700 righe di codice di
sicurezza nostro, la parte dove un errore costa di più.

## Decision

1. **`libs/tls` è un adattatore**, non un'implementazione: dentro c'è
   `embedded-tls` 0.19 (versione pinnata `=0.19.0` in `Cargo.toml`), fuori
   un'API piccola e testabile. `dns` non vede `embedded-tls`.
   Feature abilitate: `default-features = false` + `["rustpki", "rsa",
   "alloc"]` — `rustpki` è la strada `no_std` (`webpki` è std-only), `rsa`
   implica `rustpki`+`alloc` ed è **necessario** per le catene firmate RSA
   (ISRG Root X1 è RSA-4096).
2. **Il crate è puro**: nessuna syscall, nessuna dipendenza da `civis`.
   Trasporto **e** orologio sono iniettati: trait `Transport { read; write;
   flush }` + adapter `embedded_io::Read/Write`, e `TlsClock` generico. Così
   il crate si prova sull'host in millisecondi (`cargo test`, 12 test) senza
   boot. L'unico aggancio al sistema è la feature `civis-clock` (dipendenza
   opzionale) che fornisce `tls::SystemClock` (`civis::time::wall_secs()`):
   i test host girano senza, il sistema reale la attiva.
   In `dns`/`tlsprobe` il trasporto è l'endpoint TCP di `net`
   (`NET_EP_OPEN` + `NET_EP_FLAG_CONNECT`, ring 16×2048, `NET_EP_TX`,
   attesa RX con deadline via `futex_wait`, ADR-0067).
3. **Suite**: `Aes128GcmSha256` (inclusa da `embedded-tls`); ALPN dal
   chiamante (`dot` per DoT). Un solo named group: `secp256r1` (è ciò che
   `embedded-tls` offre).
4. **Fiducia**: `pki::CertVerifier` (rustpki) con `TlsClock` iniettato,
   `set_hostname_verification(sni)` (lo fa `TlsConnection::open` col
   `server_name` del `TlsConfig`), catena bounded (`CERT_SIZE` 4 KiB,
   fail-loud oltre: `OutOfMemory`, mai handshake "riuscito" senza verifica).
   Buffer record `RECORD_READ_BUF` 16640 / `RECORD_WRITE_BUF` 16384, nel
   `Box` (heap civis), mai sullo stack. Trust anchor obbligatoria: vuota →
   `Error::Config` (fail-closed). In N6.3 la CA arriverà da `sys:ca.der`.
5. **Entropia**: DRBG ChaCha20 (`rand_chacha`, `rand_core` 0.6 — lo stesso
   di `embedded-tls`) seminato da RDRAND (gate CPUID leaf 1 ECX bit 30:
   senza supporto l'istruzione fa `#UD`); fallback `rdtsc ^ mix`. Reseed
   ogni MiB. **Sotto QEMU TCG RDRAND non funziona**: il fallback è
   dichiarato non crypto-grade e va scritto nel threat model (ADR-0068).
   Nota di implementazione: `ChaCha20Rng` ha align 16, l'heap di civis
   supporta solo align ≤ 8 → il DRBG lo avvolge in un wrapper a align 8
   (`ChaCha8`), sicuro perché ChaCha lavora su `u32`.
6. **Semantica DoT invariata rispetto a N6.0**: tabella statica vincente;
   nome fuori tabella + upstream configurato → inoltro; TLS rotto o
   certificato non valido → **SERVFAIL**, mai NXDOMAIN silenzioso;
   `mode = dot` senza `sni` valido → fail-closed con log (mai degrado
   silenzioso a plain).
7. **Una connessione per query** (apertura, query, risposta, `close_notify`):
   niente keep-alive né resumption in questa fase.

## Consequences

### Positive

- Handshake, key schedule, record layer e parsing dei certificati sono codice
  mantenuto da terzi, non nostro; `libs/tls` resta piccolo e verificabile.
- Il trasporto e l'orologio iniettati rendono il client testabile su host con
  un mock (12 unit test, senza boot) **e** in QEMU con un server TLS locale
  (`scripts/run-tls-tests.sh`: `tools/tls-test-certs` genera CA+leaf RSA-2048,
  Python `ssl` fa da server TLS 1.3 con ALPN `dot`). La prova QEMU copre
  handshake, verifica catena+hostname e un record applicativo (HTTP).
- Il clock reale c'è già (`usertime`), quindi la validità dei certificati è
  vera e non "ignorata".
- **Bug TCP trovato dalla prova reale**: `Conn::next_tx` emetteva i dati
  bufferizzati anche in `SynSent` (ACK non ancora noto → segmento con ACK=0,
  che passt resetta). Sul loopback non si vedeva (SYN+ACK processato in modo
  sincrono). Corretto: dati solo in `Established`/`CloseWait`, FIN solo a
  buffer di invio vuoto. Regressione coperta da un unit test host
  (`no_data_before_handshake_completes`).

### Negative / limiti dichiarati

- TLS 1.3 only; nessun fallback a 1.2.
- Un solo named group (secp256r1): un server che accettasse **solo** x25519
  non verrebbe servito. Da rivedere se emerge un resolver reale così.
- Niente resumption, early data, OCSP/CRL, client certificate.
- `embedded-tls` è un progetto piccolo (un maintainer principale): la
  versione è pinnata e l'upgrade è una decisione esplicita, non un
  `cargo update` distratto.
- Sotto TCG l'entropia è debole (dichiarato); su hardware con RDRAND è ok.
- `CERT_SIZE` bounded: catene più lunghe di 4 KiB vengono rifiutate.
- Il server di prova è `ssl` di Python (non c'è il binario `openssl` nella
  sandbox): va bene per la prova, ma i certificati di test li genera il tool
  Rust `tools/tls-test-certs` per non dipendere da `openssl` in nessun passo.
- La prova QEMU dipende da `passt` (come la suite di rete) e da un server TLS
  sull'host: fuori dal gate, come N1–N6.

### Neutral

- Il kernel resta neutro: nessuna syscall nuova, TLS è tutto userspace.
- `tlsprobe` (testland) usa lo stack grande (`SPAWN_FLAG_BIGSTACK`): il
  verifier contiene `OwnedCertificate<CERT_SIZE>` (~8 KiB). La lista è per
  nome (`rector::wants_big_stack`), i manifest da disco non hanno un campo
  dedicato.

## Alternatives Considered

- **Primitive RustCrypto + handshake nostro**: scartata per costo/rischio
  (~1700 righe di codice di sicurezza nostro) senza vantaggi di fiducia.
- **`rustls` + `rustls-rustcrypto`**: scartata perché `no_std` esiste solo
  in una dev release e il provider è una pre-release che pinna una rustls
  std-only.
- **`ring`/`aws-lc-rs`**: scartata (C/asm, `build.rs`, niente bare metal).
- **Primitive nostre senza SSE2** (piano iniziale N6.2a): scartata dopo la
  misura di ADR-0071 — con SSE2 si ottengono primitive mantenute e
  l'handshake, a costo di un cambio di target già sicuro per il kernel.
- **Fiducia TOFU (pin al primo uso)**: scartata: un DoT che non verifica la
  catena non aggiunge garanzie rispetto al plain, e il clock reale rende la
  verifica vera praticabile.

## References

- ADR-0068 (threat model), ADR-0069 (stack di rete), ADR-0070 (policy
  subtree), ADR-0071 (SSE2 nello userland).
- RFC 7858 (DNS over TLS), RFC 8446 (TLS 1.3), RFC 5280 (X.509).
- `libs/tls/` (N6.2: `transport`/`drbg`/`trust`/`session`),
  `testland/tlsprobe/`, `tools/tls-test-certs/`,
  `scripts/run-tls-tests.sh`.
- `userland/dns/src/main.rs` (N6.0: `forward_plain`, config `sys:dns.conf`).
