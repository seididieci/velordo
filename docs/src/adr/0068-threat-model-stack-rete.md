# ADR-0068: Threat model e invarianti dello stack di rete

## Status

Proposed (2026-11). Primo artefatto della fase rete: fissa **cosa** deve
essere sicuro e **come** si verifica, prima del piano di implementazione. Le
scelte architetturali stanno in ADR-0069; la policy di configurazione in
ADR-0070.

## Context

Velordo non ha rete. L'obiettivo è uno stack nativo, IPv6-first, con una
postura di sicurezza **almeno** pari a FreeBSD e, dove possibile, più
restrittiva. Il confronto va fatto sul modello, non sulle feature:

- **FreeBSD**: stack in-kernel in C, decenni di hardening, ma una superficie
  enorme (opzioni IP, source routing, broadcast, compatibilità, VNET, ...).
  Un bug è compromissione del kernel.
- **Velordo**: stack in **userspace**, isolato dal kernel e dagli altri
  servizi; core in **Rust `no_std`** con `#![forbid(unsafe_code)]`; modello a
  **capability** (canali, diritti, manifest con hash); **minimalismo** come
  mitigazione (si toglie superficie invece di aggiungere feature).

Il vantaggio strutturale è l'isolamento: un bug dello stack compromette il
servizio `net`, non il kernel. Il limite strutturale è che **non c'è IOMMU**:
un driver/NIC compromesso può fare DMA ovunque. VT-d è rimandato (ADR-0069);
fino ad allora il device è nel TCB.

## Decision

### 1. Asset da proteggere

1. **Kernel** e altri servizi (integrità, disponibilità).
2. **Identità di rete** del sistema (indirizzi, MAC, nomi).
3. **Configurazione** (`sys:net`, `sys:identis`, ADR-0070).
4. **Credenziali** (Identis: hash password, token di sessione).
5. **Log/audit** (Vestigia).
6. **Risorse** (memoria, code, CPU): nessun client deve poterle esaurire.

### 2. Avversari e scope

| Avversario | In scope | Note |
|---|---|---|
| Attaccante **remoto** (pacchetti malformati, flood, spoofing) | **sì** | è il threat model principale dello stack |
| **Client locale malevolo** (programma che chiede rete) | **sì** | capability + policy per binario |
| **Stack compromesso** | **sì, confinato** | il danno resta nel servizio `net` e nei suoi endpoint |
| **Driver/NIC compromesso** | **no, finché niente VT-d** | il DMA può scrivere ovunque; device fidato (virtio/QEMU) |
| Attaccante **fisico**, side-channel, supply chain | no | fuori scope dichiarato |

**Assunzioni**: kernel e init sono TCB; gli hash del manifest sono corretti;
il device è fidato fino a VT-d; l'host/passt è fidato nei test.

### 3. Confini di fiducia

1. **kernel ↔ driver `netdev`**: MMIO e DMA. Il driver è l'unico a toccare il
   device; il kernel gli dà solo grant espliciti (IOMMU-ready).
2. **driver ↔ stack `net`**: ring virtio e ring pacchetti. **Diffidenza
   reciproca**: il driver valida gli indici del used ring; lo stack valida
   ogni pacchetto e ogni descrittore che riceve.
3. **stack ↔ client**: endpoint a capability. Il client non ha autorità
   implicita; la policy è per binario (hash) e per endpoint.
4. **stack ↔ config**: `sys:net` in lettura, con policy di subtree lato
   server (ADR-0070).

### 4. Invarianti (testabili)

**I1 — Niente panic/UB su input arbitrario.** Tutti i parser restituiscono
`Result`; nessun `unwrap`/`expect`/indicizzazione non controllata su dati di
rete; nessuna ricorsione non limitata (extension header: contatore massimo);
`libs/net` è `#![forbid(unsafe_code)]`.

**I2 — Risorse limitate.** Pool a dimensione fissa; nessuna allocazione nel
percorso caldo; limiti per coda, per connessione, per client e globali;
niente reassembly/frammentazione illimitati; cache NDP limitata; half-open
TCP limitati; rate limit su ICMP/NDP/RST.

**I3 — Default-deny.** Niente raw socket, niente ping, niente RA/redirect,
niente IPv4, niente DNS, niente connessioni in ingresso se la policy non lo
dichiara. Ogni capability di rete è esplicita.

**I4 — Policy per binario.** Ogni endpoint è dichiarato nel manifest del
binario e applicato dal servizio `net` (hash del peer via `SYS_PEER_INFO`).
Nessuna autorità ambientale: un programma ignoto non ha rete.

**I5 — Niente protocolli legacy pericolosi.** IPv4: niente opzioni, source
routing, broadcast, redirect. IPv6: niente routing header, niente jumbogram,
catena di extension header limitata; frammenti **droppati** nella prima
versione (reassembly bounded solo se servirà).

**I6 — TCP minimale e resistente.** SYN cookies sempre; RFC 5961 (challenge
ACK/RST); ISN e porte randomizzati; niente SACK/timestamps nella prima
versione; niente urgent; RST rate-limited; niente dati nel SYN oltre il
consentito; half-open limitati.

**I7 — ICMPv6 controllato.** Echo solo se la policy lo consente (default:
no; test: sì); errori validati sul pacchetto che li ha generati;
rate-limited; niente redirect; niente amplificazione.

**I8 — NDP con RA guard.** RA accettati solo da router in allowlist; redirect
ignorati; cache neighbor limitata; NA unsolicited non sovrascrive un entry
statico; NS/NA rate-limited.

**I9 — Privacy.** IID **randomizzato** (RFC 4941), mai EUI-64 derivato dal
MAC. **DAD disabilitato** (vettore DoS): l'indirizzo è considerato unico per
configurazione; il trade-off è dichiarato (un conflitto è un errore di
configurazione, non un attacco da gestire in linea).

**I10 — DMA IOMMU-ready.** Il driver riceve solo buffer grantati in un
dominio per device; nessuna mappatura fisica arbitraria; il kernel non è nel
percorso dati. VT-d dietro la stessa interfaccia.

**I11 — Kernel neutro.** Nessuna syscall di rete, nessun parsing di
protocolli nel kernel: solo MMIO/DMA/MSI-X/IPC/registro servizi (ADR-0025,
ADR-0048).

**I12 — Audit.** Eventi di sicurezza (pacchetti invalidi, rate limit, policy
denied) al servizio `Log` (Vestigia), rate-limited; tap opzionale per un
monitor con capability esplicita.

**I13 — Nessuna fiducia nel device.** Offload disabilitati (checksum/GSO/
MRG_RXBUF): lo stack calcola i checksum e valida le lunghezze; il driver
valida il used ring.

**I14 — Nessuna fiducia nel client.** Indici e lunghezze dei ring validati;
niente TOCTOU: o copia al confine, o payload read-only con lifetime.

**I15 — Verifica.** Fuzzing host dei parser; harness deterministico
packet-in/packet-out; test QEMU con `passt` fuori dal gate; gate invariato.

### 5. Verifica

- **Host**: `cargo test` + `cargo-fuzz` su `libs/net` (parser Ethernet/IPv6/
  ICMPv6/NDP/UDP/TCP), harness deterministico (input → output, nessun I/O).
- **QEMU**: `scripts/run-net-tests.sh` con `-netdev passt` (IPv6 reale,
  unprivileged), fuori dal gate perché passt è una dipendenza host.
- **Gate**: invariato (6 righe, zero FAIL/PANIC/FAULT); la rete non entra nel
  gate finché non è matura.
- **Metriche**: nessun panic su corpus fuzz; memoria bounded; latenza/
  throughput misurati (non nel gate).

## Consequences

### Positive

- Isolamento userspace + Rust + minimalismo: una postura più restrittiva di
  FreeBSD su input remoto, senza ereditarne la superficie.
- Policy per binario: nessun programma ignoto ha rete.
- IOMMU-ready: VT-d non richiede un rifacimento.

### Negative / limiti

- Fino a VT-d il device è nel TCB: un NIC ostile è fuori scope.
- Niente DAD: un conflitto di indirizzo è un errore di configurazione.
- Niente frammentazione nella prima versione: alcuni peer IPv6 che
  frammentano non saranno raggiungibili (accettato; si aggiunge bounded).
- Il minimalismo TCP esclude SACK/timestamps: performance inferiore su reti
  lossy (accettato; si aggiungono solo se necessari).

## Riferimenti

- ADR-0005 (microkernel), ADR-0008 (IPC per nome), ADR-0025/0041/0048
  (meccanismo/personalità, niente POSIX dentro), ADR-0026/0027 (hardening),
  ADR-0047 (PAL), ADR-0055/0058 (Identis), ADR-0057 (grant MMIO),
  ADR-0060 (console), ADR-0064 (bucket binari).
- ADR-0069 (architettura dello stack), ADR-0070 (policy subtree/bucket).
