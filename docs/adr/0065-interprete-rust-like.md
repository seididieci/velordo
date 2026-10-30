# ADR-0065: Interprete “Rust‑like” per la shell di Velordo

## 1. Motivazione
La shell nativa di Velordo è un REPL *bare‑metal* che, pur essendo estremamente snella, non offre alcuna possibilità di **persistenza, introspezione o scripting**.  I comandi vengono eseguiti come binari nativi e il loro output è stampato sulla console, ma non può essere assegnato a una variabile o usato in espressioni successive.

Per trasformare la shell in un ambiente di *scripting* completo, è necessario:

* Aggiungere un **tokenizer, parser e valutatore** basati su una sintassi Rust‑like minimale.
* Permettere l’**assegnazione di valori** (numeri, stringhe, array, oggetti, blob binari) a variabili di sessione.
* Offrire funzioni builtin (`clear`, `dump`, `type`) e la possibilità di **eseguire binari nativi** con la stessa semantica di un comando shell.
* Fornire *serializzazione* JSON (per oggetti) e *blob* binari (`Vec<u8>`) per l’output di programmi non testuali.

Con queste funzionalità, l’utente può scrivere script semplici, manipolare dati direttamente nella REPL, e sfruttare i comandi nativi senza affidarsi a `grep/awk/sed`.


## 2. Ambito

| Includere | Esplicitamente escludere |
|-----------|--------------------------|
| Tokenizer, Parser (grammar semplice) | Macro, generics, `async`/`await`, pattern matching |
| Abstract Syntax Tree (AST) | Pipe di I/O, I/O avanzato |
| Ambiente di valutazione con scope annidati | Funzioni built‑in non documentate |
| Built‑ins: `clear`, `dump`, `type`, opzionale `exec` | Espressioni regolari, punteggiatura complessa |
| Serializzazione JSON + `Blob(Vec<u8>)` | `serde_json` (dipende da std) |
| Esecuzione di binari come comandi | Lanci di processi con `spawn_image` solo, nessuno pipe di stdout (facoltativo) |


## 3. Assunzioni

* La shell è `no_std` ma può usare `alloc`.
* Gli output di programmi esterni possono essere **capturati** mediante un pipe (se supportato); altrimenti il valore restituito è `Value::Null`.
* I **blob binari** sono rappresentati con `Value::Blob(Vec<u8>)`.
* Le variabili definite **con `let`** hanno vita **finita allo scope**. Se fuori da un blocco `{}` diventano globali fino alla fine della sessione o `clear`.
* Una funzione inline conserva lo **scope lexicale** della dichiarazione; può essere chiamata in qualsiasi posizione successiva nella stessa sessione.
* `eval` è un *pure function* salvo l’ambiente di variabili.


## 4. Terminologia

| Term | Definizione |
|------|-------------|
| **Token** | Unità di base del lexer (`Ident`, `Number`, `String`, `Operator`, `Delim`). |
| **AST** | Rappresentazione in memoria della sintassi (enum `Expr`, `Stmt`). |
| **Env** | Mappa `HashMap<String, Value>` che rappresenta lo scope corrente; può avere un `parent`. |
| **Value** | Varianti: `Null`, `Bool`, `I64`, `F64`, `String`, `Array(Vec<Value>)`, `Object(HashMap<String, Value>)`, `Blob(Vec<u8>)`. |
| **Builtin** | Funzione predefinita esposta dallo shell (es. `clear`, `dump`, `type`). |


## 5. Grammatica (semplificata)
```text
script     ::= { stmt }
stmt       ::= let_stmt | var_assign | expr_stmt | block | if_stmt | for_stmt
let_stmt   ::= "let" IDENT "=" expr
var_assign ::= IDENT "=" expr
expr_stmt  ::= expr
block      ::= "{" { stmt } "}"
if_stmt    ::= "if" expr "{" { stmt } "}" ["else" "{" { stmt } "}"]
for_stmt   ::= "for" IDENT "in" range "{" { stmt } "}"
range      ::= expr ".." expr | "0.." expr
expr       ::= literal | IDENT | binary_expr | func_call
literal    ::= NUMBER | STRING | "true" | "false" | "null"
binary_expr ::= expr OP expr
func_call  ::= IDENT "(" [ expr { "," expr } ")"
```
*Operatori supportati*: `+ - * / %`, `== != < <= > >=`, `&& || !`.


## 6. AST (enum in Rust)
```rust
#[derive(Clone, Debug)]
pub enum Expr {
    Literal(Value),
    Ident(String),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Call(String, Vec<Expr>),
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let(String, Expr),
    Assign(String, Expr),
    Expr(Expr),
    Block(Vec<Stmt>),
    If(Expr, Vec<Stmt>, Option<Vec<Stmt>>),
    For(String, Expr, Expr, Vec<Stmt>),
}
```


## 7. Ambiente & Valori
```rust
pub struct Env {
    parent: Option<Box<Env>>,
    vars:   HashMap<String, Value>,
}
```
*La valutazione di un blocco* crea un nuovo `Env` con `parent` = current env; al termine del blocco l’`Env` viene rilasciato.  
*Built‑ins* sono implementati come funzioni che ricevono `&mut Env` e `&[Expr]`.  
L’`eval_expr` restituisce `Result<Value, RuntimeError>`; `eval_stmt` restituisce `Result<Option<Value>, RuntimeError>` (opzionalmente un valore per `expr_stmt`).


## 8. Built‑ins principali
| Nome | Argomenti | Ritorna | Descrizione |
|------|-----------|--------|-------------|
| `clear` | `name` | `()` | Rimuove la variabile dal *scope* corrente. |
| `dump` | `name` | `String` | Serializza il valore come JSON/hex e stampa. |
| `type` | `name` | `String` | Restituisce il tipo (`Number`, `String`, `Array`, `Blob`, etc.). |
| `exec` *(opzionale)* | `cmd [args …]` | `Value::String` (output) | Avvia un binario, cattura stdout. |


## 9. Esecuzione di comandi esterni
* Se un identificatore non è un builtin e non è una variabile, viene trattato come **nome di un binario**. 
* Si chiama `civis::spawn_image` con la path derivata dall’`BinList` di cardo. 
* Se è possibile catturare l’output (`stdout`) tramite pipe, il valore restituito è `Value::String`; altrimenti `Value::Null`. 
* Gli errori di spawn o runtime (es. `ENOENT`) sono tradotti in `RuntimeError`.


## 10. Gestione degli errori
```rust
#[derive(Debug)]
pub enum ParseError { ... }

#[derive(Debug)]
pub enum RuntimeError(String);
```
* `ParseError` è restituito dal parser. 
* `RuntimeError` include una stringa descrittiva; la REPL la stampa e riparte.


## 11. Serializzazione
Un modulo `json_util` (no‑std) fornisce `to_json(&Value) -> String`. 
* `Value::Blob(Vec<u8>)` viene serializzato in hex (`0x...`). 
* `dump` builtin utilizza questo serializzatore.


## 12. Persistenza delle variabili
* **Locali**: dichiarate con `let` all’interno di un blocco `{}`; visibili solo fino al chiudere il blocco. 
* **Globali**: dichiarate fuori da blocchi; persistono fino alla fine della sessione o fino a `clear`. 
* Funzioni inline: salvate come `Value::Object` con campo `__function` (parametri + AST). 
* Il comando `clear name` può essere usato per rimuovere variabili o funzioni.


## 13. Implementazione preliminare
| Fase | Attività | Note |
|------|----------|------|
| 1 | Lexer + Parser base | Unit test di tokenizzazione e parsing. |
| 2 | Ambiente e valutatore semplice | Aritmetica, let, if, for. |
| 3 | Built‑ins + cattura output | Test con `echo`, `cat`. |
| 4 | Funzioni inline | Scope, chiusura. |
| 5 | Serializzazione JSON + blob | json_util. |
| 6 | Integrazione shell | Aggiornare REPL, sostituire dispatcher. |
| 7 | Test di integrazione | Script di esempio, `dump`, `type`. |
| 8 | Documentazione | Aggiornare `docs/adr/` e help output. |


## 14. Riferimenti
* ADR‑0064 — **Bucket `binary`** (nome binari piatti). 
* ADR‑0043 — **Object buckets** (`sys`, `vela`, `usr`). 
* ADR‑0039 — **Log** (per output di pipe). 
* ADR‑0035 — **Job control** (per cancellare processi). 
* ADR‑0026 — **Threat model & hardening** (per sicurezza di exec). 


> **Nota**: l'implementazione di `exec` è opzionale. Se l'infrastruttura di pipe di cardo non è pronta, la funzione può restituire `Value::Null`.
