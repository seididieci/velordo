# Velordo

Velordo è un sistema operativo sperimentale basato su microkernel, scritto in Rust per architettura x86_64 bare-metal. Il progetto punta a separare kernel, driver e servizi in userspace, con IPC per nome, scheduling RT e un design orientato alla sicurezza, all'isolamento e alla prova di concetti architetturali avanzati.

Questo repository contiene il kernel, i servizi nativi, il supporto di filesystem, gli strumenti di build/test e la documentazione di progettazione.

## Obiettivo

L'obiettivo dichiarato del progetto è costruire un microkernel capace di supportare:

- servizi e driver in userspace;
- IPC efficiente e robusto tra processi;
- filesystem e storage nativi con supporto avanzato;
- sviluppo di un ambiente OS in cui la complessità è separata in componenti isolati;
- progressi verso self-hosting, senza sacrificare la correttezza del sistema.

## Stato del progetto

Il progetto è in fase di sviluppo attivo. In base alla documentazione interna, il repository include:

- kernel x86_64 bare-metal;
- scheduler RT a 32 priorità con CBS;
- supporto a processi, IPC, syscalls e memory management;
- servizi userspace come console, filesystem, devfs, shell e test harness;
- supporto per FAT32 e ArcaFS;
- suite di test in QEMU.

Per la roadmap dettagliata e lo stato delle fasi, consulta:

- `ROADMAP.md`
- `AGENTS.md`
- `docs/`

## Architettura

Velordo segue un modello microkernel:

- il kernel contiene scheduling, IPC, gestione della memoria e routing degli interrupt;
- driver e servizi vivono in userspace;
- i componenti interagiscono tramite canali IPC e registri di servizio;
- le personalità POSIX sono separate dal meccanismo di sistema, in linea con l'architettura del progetto.

Il progetto usa Rust nightly e un target bare-metal `x86_64-unknown-none`.

## Repository layout

```text
velordo/
├── kernel/            # kernel del sistema
├── libs/              # librerie condivise / meccanismo del sistema
├── flavours/           # personalità e compatibilità (es. POSIX)
├── syscall-numbers/   # costanti syscall e ABI condivise
├── pal/               # PAL per il target Velordo
├── scripts/            # script di build e utilità
├── userland/          # servizi nativi in userspace
├── blake2s/           # implementazione BLAKE2s no_std
├── arcafs/            # crate e definizioni condivise per ArcaFS
├── tools/             # strumenti host e utility
├── testland/          # suite di test e dimostrazioni
├── docs/              # documentazione mdbook
├── Cargo.toml          # workspace Rust
├── run.sh             # build + boot di sistema in QEMU
├── run-tests.sh       # suite di regressione
├── ROADMAP.md         # stato, fasi pianificate e backlog
├── AGENTS.md          # regole e convenzioni per agenti AI
├── README.md          # documentazione del repository
└── ...
```

## Requisiti

Per costruire ed eseguire il progetto localmente sono necessari:

- Rust nightly
- QEMU (`qemu-system-x86_64`)
- toolchain di sistema per il build Rust bare-metal
- eventualmente `mdbook` per generare la documentazione

## Quick start

### Build del kernel

```bash
cargo build --release
```

### Esecuzione in QEMU

```bash
./run.sh
```

### Esecuzione con display grafico locale

```bash
RUN_DISPLAY=gtk ./run.sh
```

### Esecuzione della suite di test

```bash
./run-tests.sh
```

### Generazione della documentazione

```bash
cd docs && mdbook build
```

Oppure in modalità live:

```bash
cd docs && mdbook serve
```

## Documentazione

La documentazione ufficiale è organizzata in `docs/` e usa `mdbook`.

Documenti rilevanti:

- `ROADMAP.md` — stato e pianificazione del progetto
- `AGENTS.md` — convenzioni di sviluppo e struttura del repo
- `docs/src/` — capitoli e ADR del design tecnico

## Note importanti

- Il progetto è sperimentale e in evoluzione continua.
- Il target principale è un ambiente bare-metal x86_64.
- Non è pensato come sistema operativo general-purpose pronto all'uso;
  è un ambiente di ricerca e prototipazione architetturale.

## License

Il repository non specifica una licenza esplicita nel file di root osservato al momento della stesura di questo README. Verifica eventuali dettagli prima dell'uso commerciale o della distribuzione.

## Contatti e riferimenti

Per informazioni aggiornate su stato, decisioni, roadmap e dettagli architetturali, consulta i documenti interni del repo:

- `ROADMAP.md`
- `AGENTS.md`
- `docs/src/`


Questo README è pensato come punto di ingresso per chi vuole capire rapidamente cosa sia Velordo, come si compila e dove leggere la documentazione tecnica di riferimento.
