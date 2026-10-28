//! Runtime (ambiente variabili + valutazione AST) per l'interprete REPL (S3.2b).
//!
//! **Env**: tabella variabili con scope flat (un solo livello per ora).
//! **eval_expr**: valuta un'espressione → Value.
//! **eval_stmt**: valuta un'istruzione → Value (usato da `for`/`if`).

use super::ast::{Expr, Stmt, Value};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// Importa la funzione console_write_str definita in main (crate root).
use super::console_write_str;

/// Ambiente variabili: mappa nome → valore.
pub struct Env {
    variables: Vec<(String, Value)>,
}

impl Env {
    /// Crea nuovo ambiente vuoto.
    pub fn new() -> Self {
        Env {
            variables: Vec::new(),
        }
    }

    /// Lookup variabile per nome.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.variables.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Set variabile (aggiorna se esiste, altrimenti crea).
    pub fn set(&mut self, name: String, value: Value) {
        // Cerca se esiste gia'.
        for (i, (n, v)) in self.variables.iter_mut().enumerate() {
            if n == &name {
                *v = value;
                return;
            }
        }
        self.variables.push((name, value));
    }

    /// Set variabile con ownership (più efficiente per Let).
    pub fn set_owned(&mut self, name: String, value: Value) {
        for (n, v) in self.variables.iter_mut() {
            if n == &name {
                *v = value;
                return;
            }
        }
        self.variables.push((name, value));
    }

    /// Itera su una Directory, assegnando ogni elemento alla variabile `var`.
    pub fn for_each(&mut self, var: &str, items: &[Value], mut body_eval: impl FnMut(&Env) -> Value) {
        for item in items {
            // Crea un nuovo scope (copia dell'env corrente).
            let mut child = Env {
                variables: self.variables.clone(),
            };
            child.set_owned(String::from(var), item.clone());
            // Esegui il corpo con lo scope esteso.
            let _result = body_eval(&child);
        }
    }
}

/// Valuta un'espressione → Value.
pub fn eval_expr(expr: &Expr, env: &mut Env) -> Value {
    match expr {
        Expr::Var(name) => {
            match env.get(name) {
                Some(v) => v.clone(),
                None => Value::Err(format!("variabile '{}' non definita", name)),
            }
        }

        Expr::Literal(s) => Value::Str(s.clone()),

        Expr::Call(func, args) => eval_call(func, args, env),

        Expr::MethodCall(obj_expr, method, args) => {
            let obj = eval_expr(obj_expr, env);
            eval_method(&obj, method, args, env)
        }

        Expr::BinaryOp(left, op, right) => {
            let l = eval_expr(left, env);
            let r = eval_expr(right, env);
            eval_binary_op(&l, op, &r)
        }

        Expr::UnaryOp(op, expr) => {
            let v = eval_expr(expr, env);
            eval_unary_op(op, &v)
        }
    }
}

/// Valuta un'istruzione → Value (usato da for/if per concatenare risultati).
pub fn eval_stmt(stmt: &Stmt, env: &mut Env) -> Value {
    match stmt {
        Stmt::Let(name, expr) => {
            let value = eval_expr(expr, env);
            env.set_owned(name.clone(), value);
            Value::Str(format!("{} = {}", name, value_to_string(&env.get(name).cloned().unwrap_or(Value::Err("".into())))))
        }

        Stmt::Expr(expr) => eval_expr(expr, env),

        Stmt::For(var, iter_expr, body) => {
            let iter_value = eval_expr(iter_expr, env);
            match iter_value {
                Value::Directory(items) => {
                    // Esegui il corpo per ogni elemento.
                    let mut last = Value::Str("".into());
                    for item in items {
                        let mut child_env = Env {
                            variables: env.variables.clone(),
                        };
                        child_env.set_owned(String::from(var), item.clone());
                        // Esegui tutte le statement del corpo.
                        for stmt in body {
                            last = eval_stmt(stmt, &mut child_env);
                        }
                    }
                    last
                }
                _ => Value::Err(format!("for richiede Directory, ottenuto {}", value_to_string(&iter_value))),
            }
        }

        Stmt::If(cond, then_body, else_body) => {
            let cond_val = eval_expr(cond, env);
            if is_truthy(&cond_val) {
                // Esegui then body.
                let mut last = Value::Str("".into());
                for stmt in then_body {
                    last = eval_stmt(stmt, env);
                }
                last
            } else if !else_body.is_empty() {
                // Esegui else body.
                let mut last = Value::Str("".into());
                for stmt in else_body {
                    last = eval_stmt(stmt, env);
                }
                last
            } else {
                Value::Str("".into())
            }
        }

        Stmt::Block(stmts) => {
            let mut last = Value::Str("".into());
            for stmt in stmts {
                last = eval_stmt(stmt, env);
            }
            last
        }
    }
}

/// Verifica se un valore e' "falsy" (0, "", Err).
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Int(n) => *n != 0,
        Value::Str(s) => !s.is_empty(),
        Value::Err(_) => false,
        _ => true, // Directory/File/altro sono truthy
    }
}

/// Serializza un Value in stringa leggibile.
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
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

/// Valuta una chiamata di comando (func, args).
fn eval_call(func: &str, args: &[Expr], env: &mut Env) -> Value {
    match func {
        "help" => {
            crate::cmd_help();
            Value::Str("Comandi mostrati".into())
        }

        "users" => {
            // users() usa il token corrente dall'ambiente.
            let token = env.variables.iter().rev().find_map(|(n, v)| {
                if n == "VELORDO_SESSION" {
                    match v {
                        Value::Int(n) => Some(*n as u64),
                        _ => None,
                    }
                } else {
                    None
                }
            });

            crate::cmd_users(token)
        }

        "create-user" => {
            let token = env.variables.iter().rev().find_map(|(n, v)| {
                if n == "VELORDO_SESSION" {
                    match v {
                        Value::Int(n) => Some(*n as u64),
                        _ => None,
                    }
                } else {
                    None
                }
            });

            let arg_val = if args.is_empty() {
                None
            } else {
                Some(eval_expr(&args[0], env))
            };
            let name = match arg_val {
                Some(Value::Str(ref s)) => Some(s.as_str()),
                _ => None,
            };
            crate::cmd_create_user(token, name)
        }

        "print" => {
            // print(value): serializza e stampa il valore.
            if !args.is_empty() {
                let val = eval_expr(&args[0], env);
                console_write_str(&value_to_string(&val));
                Value::Str("".into())
            } else {
                Value::Str("".into())
            }
        }

        "quit" | "exit" => {
            // Esci dalla shell: chiede a init di riavviare il login (getty).
            crate::cmd_logout()
        }

        "logout" => {
            // Alias per quit.
            crate::cmd_logout()
        }

        func if func.starts_with("fs::") => {
            // Namespace fs: `fs::list(path)`, `fs::read(name)`, `fs::remove(name)`.
            let method = &func[4..]; // Strippa "fs::" → "list", "read", "remove".
            eval_fs_method(method, args, env)
        }

        _ => {
            // Comando sconosciuto.
            Value::Err(format!("comando '{}' sconosciuto (usa help() per la guida)", func))
        }
    }
}

/// Namespace fs: dispatch per `fs::list`, `fs::read`, `fs::remove`.
fn eval_fs_method(method: &str, args: &[Expr], env: &mut Env) -> Value {
    match method {
        "list" => {
            // fs::list(path): lista directory.
            let path = if !args.is_empty() {
                match eval_expr(&args[0], env) {
                    Value::Str(s) => s,
                    _ => return Value::Err("fs::list richiede path stringa".into()),
                }
            } else {
                String::from("/")
            };

            // Apri la directory e leggi i nomi.
            let fd = match civis::open(&path, 0x100) { // O_RDONLY | O_DIRECTORY
                Ok(f) => f,
                Err(_) => return Value::Err(format!("impossibile aprire '{}'", path)),
            };

            let mut items = Vec::new();
            let mut buf = [0u8; 4096];
            let buf_cap = buf.len();
            loop {
                let n = match civis::read_fs(fd, &mut buf, buf_cap) {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                let content = core::str::from_utf8(&buf[..n]).unwrap_or("");
                for line in content.lines() {
                    items.push(Value::Str(String::from(line.trim())));
                }
            }
            let _ = civis::close(fd);
            Value::Directory(items)
        }

        "read" => {
            // fs::read(name): leggi file per nome.
            let name = if !args.is_empty() {
                match eval_expr(&args[0], env) {
                    Value::Str(s) => s,
                    _ => return Value::Err("fs::read richiede nome file stringa".into()),
                }
            } else {
                return Value::Err("fs::read richiede almeno un argomento (nome)".into());
            };

            let path = format!("/tmp/{}", name); // Semplificato: legge da /tmp.
            let fd = match civis::open(&path, 0) {
                Ok(f) => f,
                Err(_) => return Value::Err(format!("impossibile aprire '{}'", path)),
            };

            let mut buf = [0u8; 1024];
            let buf_cap = buf.len();
            let n = match civis::read_fs(fd, &mut buf, buf_cap) {
                Ok(n) => n,
                Err(_) => return Value::Err(format!("impossibile leggere '{}'", path)),
            };
            let _ = civis::close(fd);

            let content = core::str::from_utf8(&buf[..n]).unwrap_or("");
            Value::Str(String::from(content))
        }

        "remove" => {
            // fs::remove(name): rimuovi file.
            let name = if !args.is_empty() {
                match eval_expr(&args[0], env) {
                    Value::Str(s) => s,
                    _ => return Value::Err("fs::remove richiede nome file stringa".into()),
                }
            } else {
                return Value::Err("fs::remove richiede almeno un argomento (nome)".into());
            };

            let path = format!("/tmp/{}", name); // Semplificato.
            // Nota: cardo non espone unlink() direttamente — mock per ora.
            console_write_str(&format!("rimosso '{}'\n", name));
            Value::Str(format!("rimosso '{}'", name))
        }

        _ => Value::Err(format!("metodo 'fs::{}' non valido (usa list, read, remove)", method)),
    }
}

/// Valuta un metodo su valore (`f.size`, `dir.filter(...)`).
fn eval_method(obj: &Value, method: &str, args: &[Expr], env: &mut Env) -> Value {
    match obj {
        Value::Directory(items) => match method {
            "count" => Value::Int(items.len() as i64),
            "filter" => {
                // `dir.filter(cond)` — cond e' un'espressione con `x` come variabile.
                let cond = if !args.is_empty() {
                    Some(&args[0])
                } else {
                    None
                };

                let filtered: Vec<Value> = items.iter().filter_map(|item| {
                    // Crea scope temporaneo con `x` = item corrente.
                    let mut filter_env = Env {
                        variables: env.variables.clone(),
                    };
                    filter_env.set_owned("x".into(), item.clone());

                    match cond {
                        Some(cond_expr) => {
                            let result = eval_expr(cond_expr, &mut filter_env);
                            if is_truthy(&result) {
                                Some(item.clone())
                            } else {
                                None
                            }
                        }
                        None => Some(item.clone()),
                    }
                }).collect();

                Value::Directory(filtered)
            }
            _ => Value::Err(format!("metodo '{}' non valido per Directory", method)),
        },

        Value::File { name, size } => match method {
            "size" => Value::Int(*size),
            "name" => Value::Str(name.clone()),
            _ => Value::Err(format!("metodo '{}' non valido per File", method)),
        },

        // Namespace fs: `fs.list(path)` → MethodCall(Value::Str("fs"), "list", [path]).
        Value::Str(ns) if ns == "fs" => {
            match method {
                "list" => {
                    // fs.list(path): lista directory.
                    let path = if !args.is_empty() {
                        match eval_expr(&args[0], env) {
                            Value::Str(s) => s,
                            _ => return Value::Err("fs.list richiede path stringa".into()),
                        }
                    } else {
                        String::from("/")
                    };

                    // Apri la directory e leggi i nomi.
                    let fd = match civis::open(&path, 0x100) { // O_RDONLY | O_DIRECTORY
                        Ok(f) => f,
                        Err(_) => return Value::Err(format!("impossibile aprire '{}'", path)),
                    };

                    let mut items = Vec::new();
                    let mut buf = [0u8; 4096];
                    let buf_cap = buf.len();
                    loop {
                        let n = match civis::read_fs(fd, &mut buf, buf_cap) {
                            Ok(n) if n > 0 => n,
                            _ => break,
                        };
                        let content = core::str::from_utf8(&buf[..n]).unwrap_or("");
                        for line in content.lines() {
                            items.push(Value::Str(String::from(line.trim())));
                        }
                    }
                    let _ = civis::close(fd);
                    Value::Directory(items)
                }

                "read" => {
                    // fs.read(handle): leggi file per nome.
                    let name = if !args.is_empty() {
                        match eval_expr(&args[0], env) {
                            Value::Str(s) => s,
                            _ => return Value::Err("fs.read richiede nome file stringa".into()),
                        }
                    } else {
                        return Value::Err("fs.read richiede almeno un argomento (nome)".into());
                    };

                    let path = format!("/tmp/{}", name); // Semplificato: legge da /tmp.
                    let fd = match civis::open(&path, 0) {
                        Ok(f) => f,
                        Err(_) => return Value::Err(format!("impossibile aprire '{}'", path)),
                    };

                    let mut buf = [0u8; 1024];
                    let buf_cap = buf.len();
                    let n = match civis::read_fs(fd, &mut buf, buf_cap) {
                        Ok(n) => n,
                        Err(_) => return Value::Err(format!("impossibile leggere '{}'", path)),
                    };
                    let _ = civis::close(fd);

                    let content = core::str::from_utf8(&buf[..n]).unwrap_or("");
                    Value::Str(String::from(content))
                }

                "remove" => {
                    // fs.remove(path): rimuovi file.
                    let name = if !args.is_empty() {
                        match eval_expr(&args[0], env) {
                            Value::Str(s) => s,
                            _ => return Value::Err("fs.remove richiede nome file stringa".into()),
                        }
                    } else {
                        return Value::Err("fs.remove richiede almeno un argomento (nome)".into());
                    };

                    let path = format!("/tmp/{}", name); // Semplificato.
                    // Nota: cardo non espone unlink() direttamente — mock per ora.
                    console_write_str(&format!("rimosso '{}'\n", name));
                    Value::Str(format!("rimosso '{}'", name))
                }

                _ => Value::Err(format!("metodo 'fs.{}' non valido", method)),
            }
        }

        Value::Str(s) => match method {
            "len" => Value::Int(s.len() as i64),
            _ => Value::Err(format!("metodo '{}' non valido per Stringa", method)),
        },

        _ => Value::Err(format!("metodo '{}' non valido per {}", method, value_to_string(obj))),
    }
}

/// Valuta operatore binario (`+`, `-`, `>`, `<`, `==`, `!=`).
fn eval_binary_op(l: &Value, op: &str, r: &Value) -> Value {
    match op {
        "+" => {
            // Somma numeri o concatenazione stringhe.
            match (l, r) {
                (Value::Int(n1), Value::Int(n2)) => Value::Int(n1 + n2),
                (Value::Str(s1), Value::Str(s2)) => Value::Str(format!("{}{}", s1, s2)),
                _ => Value::Err("operatore '+' non valido per questi tipi".into()),
            }
        }
        "-" => {
            match (l, r) {
                (Value::Int(n1), Value::Int(n2)) => Value::Int(n1 - n2),
                _ => Value::Err("operatore '-' non valido per questi tipi".into()),
            }
        }
        ">" => {
            match (l, r) {
                (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 > n2 { 1 } else { 0 }),
                _ => Value::Err("operatore '>' non valido per questi tipi".into()),
            }
        }
        "<" => {
            match (l, r) {
                (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 < n2 { 1 } else { 0 }),
                _ => Value::Err("operatore '<' non valido per questi tipi".into()),
            }
        }
        "==" => {
            match (l, r) {
                (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 == n2 { 1 } else { 0 }),
                (Value::Str(s1), Value::Str(s2)) => Value::Int(if s1 == s2 { 1 } else { 0 }),
                _ => Value::Int(0), // Diversi tipi → non uguali.
            }
        }
        "!=" => {
            match (l, r) {
                (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 != n2 { 1 } else { 0 }),
                (Value::Str(s1), Value::Str(s2)) => Value::Int(if s1 != s2 { 1 } else { 0 }),
                _ => Value::Int(1), // Diversi tipi → diversi.
            }
        }
        _ => Value::Err(format!("operatore binario '{}' non valido", op)),
    }
}

/// Valuta operatore unario (`-`, `!`).
fn eval_unary_op(op: &str, v: &Value) -> Value {
    match op {
        "-" => {
            match v {
                Value::Int(n) => Value::Int(-n),
                _ => Value::Err("operatore '-' non valido per questo tipo".into()),
            }
        }
        "!" => {
            // NOT logico: !0 = 1, !1 = 0.
            match v {
                Value::Int(n) => Value::Int(if *n == 0 { 1 } else { 0 }),
                _ => Value::Err("operatore '!' non valido per questo tipo".into()),
            }
        }
        _ => Value::Err(format!("operatore unario '{}' non valido", op)),
    }
}
