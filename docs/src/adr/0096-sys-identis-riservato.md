# ADR-0096: `sys:identis` — le credenziali sono un dato riservato, e l'elenco lo serve il servizio

## Status

**Accepted** (2026-11-26). Implementata. Gate: `[identistest] PASS 12/12` (era
11/11: +1 caso), le altre suite invariate, `[shell-native] 33/33`.

Chiude il debito (a) di ADR-0058/ADR-0055 («proteggere il bucket
`identis:config`») e prepara il namespace `sys:*` che servirà al tool di
configurazione. Dipende da ADR-0077 (bucket protetti), ADR-0078/0079
(classi/firma), ADR-0094/0095 (identità attestata).

## Contesto

Le credenziali stavano nel namespace `ns` come `identis:config`, con due
guarigie a metà:

1. **scrittura**: protetta (owner `Identis`, ADR-0077) — il `foreign` la prova
   dal bit 7;
2. **lettura**: **non** protetta. Chiunque aprisse `identis:config` vedeva gli
   hash di tutti. Non era teorico: **`users` lo faceva davvero**, perché il
   `LIST` restituiva solo il *numero* di utenti e l'unico modo di stampare i
   nomi era leggere il bucket. Il test lo dichiarava come debito: «legge diretta
   del bucket — valida finché il bucket non è protetto».

Inoltre il dato era fuori posto: tutta la configurazione di sistema vive in
`sys:*` (`manifest`, `policy`, `caps`, `anchor`, `ca.der`, `dns.conf`), e le
credenziali no.

## Decisione

**Il dato va dove sono gli altri, e chi elenca non lo legge.**

1. **`identis:config` → `sys:identis`** (bucket `sys`, già protetto da init).
   Writer: **carve-out `identis_writer`** (init o Identis), che non passa da
   `allowed()`: un `ceiling == RIGHTS_ALL` dice «puoi scrivere», non «questo
   dato è tuo» (stessa regola di `caps_writer`/`consent_writer`).
2. **Guardia in lettura** (`policy::read_allowed`, chiamata in
   `cardo/src/server.rs` accanto a `write_allowed`): `R_OPEN` di `sys:identis`
   e `R_OBJ_GET` di `(sys, identis)` solo init/Identis. È la parte che chiude
   il buco: la protezione dei bucket parla di **scritture**, e le credenziali
   avevano un buco in lettura.
3. **`IDENTIS_LIST` restituisce i nomi** (`nome:ruoli` per riga, **mai
   l'hash**), a **pull da 4 byte**: `w0` = offset, reply `w1` = 4 byte
   impacchettati, `R_DONE` (codice 9) a fine elenco.
   - **Perché pull e non push**: la coda IPC ha 8 slot; una lista spinta in N
     messaggi si perde a metà (l'async reply si scarta quando la coda è piena).
     Il pull ha una richiesta per blocco, non dipende dalla coda e non richiede
     memoria condivisa.
   - **Perché il formato lo decide il servizio**: l'elenco è una *proiezione*
     del bucket (nome + ruoli, senza hash). Chi elenca non deve poter dedurre
     le credenziali, e il servizio resta l'unico punto che conosce il formato.
4. **`users` non apre più il bucket**: tira l'elenco da `IDENTIS_LIST` e
   stampa `nome  [ruoli]` esattamente come prima (l'utente non vede cambiare
   nulla, smette solo di esistere la strada che esponeva gli hash).

## Conseguenze

- **Il buco è chiuso**: un `ThirdParty` non legge più le credenziali, nemmeno
  per sbaglio (e `users` è la prova che prima lo faceva).
- **Namespace coerente**: `sys:*` è la casa della configurazione di sistema,
  con un writer per dato. È il terreno del tool di configurazione (desiderata
  #5 in ROADMAP).
- **Nasce un pattern riusabile**: «elenco = proiezione servita dal servizio,
  pull a blocchi». Serve identico a S3.2b (risultati strutturati che non
  possono stare in due registri).
- **Costo**: `users` fa ~4-6 IPC in più (un blocco da 4 byte per nome+ruoli);
  `identistest` un caso in più (12/12).
- **Limite dichiarato**: la cache dell'elenco si ricostruisce a `offset == 0`
  e non è transazionale; due client che elencano mentre un terzo crea un utente
  possono vedere la lista a metà. È una vista per la UI, non una decisione:
  chi deve decidere (autorità, ruoli) usa `WHOAMI`, che è live.

## Trappola pagata

Il primo giro del pull aveva un difetto da un byte: il client aggiungeva
**sempre 4 byte** per blocco, anche all'ultimo parziale (57 byte → 60, con 3
NUL). Invisibile a occhio, ma `str::trim` **non** toglie i NUL (non sono
spazio): la coda diventava una riga in più, e `users` stampava un utente
fantasma con il nome vuoto — il gate è passato lo stesso, perché i check
guardavano il *contenuto* e non la *forma*. Fix: il **totale** viaggia nel
primo reply (`w1` = totale nei bit 32.., i 4 byte nei bit 0..32), così il
client sa quanti byte sono validi nell'ultimo blocco. Guardia: `LIST` non
contiene NUL e finisce con `\n` (`identistest` t11).

## Test

- `identistest` t11: l'elenco arriva da `LIST`, contiene `nome:ruoli` e **non**
  contiene un prefisso dell'hash.
- `identistest` t12: `open("sys:identis")` e `obj_get(sys, identis)` **negati**
  a un `tst`.
- `foreign` bit8 (nuovo): la lettura riservata è negata (t57 di `usertests`
  passa a 511 = 9 bit).
- Shell 33/33: `users` stampa gli stessi nomi, con i ruoli, senza aprire il
  bucket.
