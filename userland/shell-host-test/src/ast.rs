//! AST (Albero Sintattico) e tipi valori per l'interprete REPL (S3.2b).
//!
//! **Tipi valore**: `Str`, `Int`, `File(Path)`, `Directory(Vec<Value>)`, `Err(String)`.
//! **Istruzioni**: `Let(name, expr)`, `Expr(expr)`, `For(var, iter, body)`,
//! `If(cond, then_body, else_body)`, `Block(Vec<Stmt>)`.
//! **Espressioni**: `Var(name)`, `Call(func, args)`, `MethodCall(obj, name, args)`,
//! `BinaryOp(left, op, right)`, `UnaryOp(op, expr)`.

use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

/// Tipo valore strutturato restituito dai comandi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Stringa.
    Str(String),
    /// Intero.
    Int(i64),
    /// File: nome + dimensione in byte.
    File { name: String, size: i64 },
    /// Directory: lista di valori (tipicamente `Str` con nomi utente o file).
    Directory(Vec<Value>),
    /// Errore: messaggio di errore (incluse TOKEN_EXPIRED).
    Err(String),
}

/// Istruzione dell'interprete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// Assegnamento: `let x = expr`.
    Let(String, Box<Expr>),
    /// Espressione sola (`fs.list("/tmp")`): valuta e restituisce il valore.
    Expr(Expr),
    /// Loop: `for var in iter { body }`.
    For(String, Box<Expr>, Vec<Stmt>),
    /// Condizionale: `if cond { then } else { else_ }`.
    If(Box<Expr>, Vec<Stmt>, Vec<Stmt>),
    /// Blocco: `{ stmt1; stmt2; ... }`.
    Block(Vec<Stmt>),
}

/// Espressione valutabile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Variabile (`x`).
    Var(String),
    /// Letterale stringa (`"hello"`).
    Literal(String),
    /// Chiamata di comando (`fs.list(path)`).
    Call(String, Vec<Expr>),
    /// Metodo su valore (`dir.filter(...)` o `f.size`).
    MethodCall(Box<Expr>, String, Vec<Expr>),
    /// Operatore binario (`x + y`, `x > y`).
    BinaryOp(Box<Expr>, String, Box<Expr>),
    /// Operatore unario (`-x`, `!x`).
    UnaryOp(String, Box<Expr>),
}

impl Expr {
    /// Estrae il nome se questa espressione e' una Var pura.
    pub fn as_var(&self) -> Option<&str> {
        match self {
            Expr::Var(n) => Some(n.as_str()),
            _ => None,
        }
    }
}

/// Serializza un Value in stringa leggibile.
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("{}", s),
        Value::Int(n) => format!("{}", n),
        Value::File { name, size } => format!("File('{}', {} bytes)", name, size),
        Value::Directory(items) => {
            if items.is_empty() {
                String::from("(vuoto)")
            } else {
                let mut out = String::new();
                for item in items {
                    out.push_str("  ");
                    out.push_str(&value_to_string(item));
                    out.push('\n');
                }
                out
            }
        }
        Value::Err(msg) => format!("ERRORE: {}", msg),
    }
}

/// Confronta due valori per `==` (usato da `if`).
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(n1), Value::Int(n2)) => n1 == n2,
        (Value::Str(s1), Value::Str(s2)) => s1 == s2,
        _ => false,
    }
}

/// Verifica se un valore e' "falsy" (0, "", Err).
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Int(n) => *n != 0,
        Value::Str(s) => !s.is_empty(),
        Value::Err(_) => false,
        _ => true, // Directory/File/altro sono truthy
    }
}
