//! Runtime puro (senza civis) per test host dell'interprete REPL (S3.2b).
//!
//! Contiene solo la logica di valutazione AST che non dipende dal kernel:
//! eval_expr, eval_stmt, eval_method, eval_binary_op, eval_unary_op,
//! value_to_string, is_truthy, Env.
//!
//! Le parti che usano civis (eval_fs_method, eval_call con IPC) sono
//! sostituite da stub che restituiscono errore descrittivo.

use crate::ast::{Expr, Stmt, Value};
use std::boxed::Box;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

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
}

/// Valuta un'espressione → Value.
pub fn eval_expr(expr: &Expr, env: &mut Env) -> Value {
    match expr {
        Expr::Var(name) => match env.get(name) {
            Some(v) => v.clone(),
            None => Value::Err(format!("variabile '{}' non definita", name)),
        },

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

/// Valuta un'istruzione → Value.
pub fn eval_stmt(stmt: &Stmt, env: &mut Env) -> Value {
    match stmt {
        Stmt::Let(name, expr) => {
            let value = eval_expr(expr, env);
            env.set_owned(name.clone(), value);
            let current = env.get(name).cloned().unwrap_or(Value::Err("".into()));
            Value::Str(format!("{} = {}", name, super::ast::value_to_string(&current)))
        }

        Stmt::Expr(expr) => eval_expr(expr, env),

        Stmt::For(var, iter_expr, body) => {
            let iter_value = eval_expr(iter_expr, env);
            match iter_value {
                Value::Directory(items) => {
                    let mut last = Value::Str("".into());
                    for item in items {
                        let mut child_env = Env {
                            variables: env.variables.clone(),
                        };
                        child_env.set_owned(String::from(var), item.clone());
                        for stmt in body {
                            last = eval_stmt(stmt, &mut child_env);
                        }
                    }
                    last
                }
                _ => Value::Err(format!(
                    "for richiede Directory, ottenuto {}",
                    super::ast::value_to_string(&iter_value)
                )),
            }
        }

        Stmt::If(cond, then_body, else_body) => {
            let cond_val = eval_expr(cond, env);
            if is_truthy(&cond_val) {
                let mut last = Value::Str("".into());
                for stmt in then_body {
                    last = eval_stmt(stmt, env);
                }
                last
            } else if !else_body.is_empty() {
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
/// Versione host: le chiamate che richiedono civis (users, create-user, fs::*)
/// restituiscono errore descrittivo invece di fare IPC.
pub fn eval_call(func: &str, args: &[Expr], env: &mut Env) -> Value {
    match func {
        "help" => {
            Value::Str("Comandi: help, users, create-user, fs::list, fs::read, fs::remove, logout/exit".into())
        }

        "users" => {
            Value::Err("users richiede servizio Identis (non disponibile su host)".into())
        }

        "create-user" => {
            Value::Err("create-user richiede servizio Identis (non disponibile su host)".into())
        }

        "print" => {
            if !args.is_empty() {
                let val = eval_expr(&args[0], env);
                Value::Str(format!("print: {}", value_to_string(&val)))
            } else {
                Value::Str("".into())
            }
        }

        "quit" | "exit" | "logout" => {
            Value::Str("quit/exit/logout (host stub)".into())
        }

        func if func.starts_with("fs::") => {
            Value::Err(format!("fs::{} richiede cardo (non disponibile su host)", &func[4..]))
        }

        _ => {
            Value::Err(format!("comando '{}' sconosciuto", func))
        }
    }
}

/// Valuta un metodo su valore (`f.size`, `dir.filter(...)`).
pub fn eval_method(obj: &Value, method: &str, args: &[Expr], env: &mut Env) -> Value {
    match obj {
        Value::Directory(items) => match method {
            "count" => Value::Int(items.len() as i64),
            "filter" => {
                let cond = if !args.is_empty() {
                    Some(&args[0])
                } else {
                    None
                };

                let filtered: Vec<Value> = items.iter().filter_map(|item| {
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

        Value::Str(ns) if ns == "fs" => {
            Value::Err("fs namespace richiede cardo (non disponibile su host)".into())
        }

        Value::Str(s) => match method {
            "len" => Value::Int(s.len() as i64),
            _ => Value::Err(format!("metodo '{}' non valido per Stringa", method)),
        },

        _ => Value::Err(format!("metodo '{}' non valido per {}", method, value_to_string(obj))),
    }
}

/// Valuta operatore binario (`+`, `-`, `>`, `<`, `==`, `!=`).
pub fn eval_binary_op(l: &Value, op: &str, r: &Value) -> Value {
    match op {
        "+" => match (l, r) {
            (Value::Int(n1), Value::Int(n2)) => Value::Int(n1 + n2),
            (Value::Str(s1), Value::Str(s2)) => Value::Str(format!("{}{}", s1, s2)),
            _ => Value::Err("operatore '+' non valido per questi tipi".into()),
        },
        "-" => match (l, r) {
            (Value::Int(n1), Value::Int(n2)) => Value::Int(n1 - n2),
            _ => Value::Err("operatore '-' non valido per questi tipi".into()),
        },
        ">" => match (l, r) {
            (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 > n2 { 1 } else { 0 }),
            _ => Value::Err("operatore '>' non valido per questi tipi".into()),
        },
        "<" => match (l, r) {
            (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 < n2 { 1 } else { 0 }),
            _ => Value::Err("operatore '<' non valido per questi tipi".into()),
        },
        "==" => match (l, r) {
            (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 == n2 { 1 } else { 0 }),
            (Value::Str(s1), Value::Str(s2)) => Value::Int(if s1 == s2 { 1 } else { 0 }),
            _ => Value::Int(0),
        },
        "!=" => match (l, r) {
            (Value::Int(n1), Value::Int(n2)) => Value::Int(if n1 != n2 { 1 } else { 0 }),
            (Value::Str(s1), Value::Str(s2)) => Value::Int(if s1 != s2 { 1 } else { 0 }),
            _ => Value::Int(1),
        },
        _ => Value::Err(format!("operatore binario '{}' non valido", op)),
    }
}

/// Valuta operatore unario (`-`, `!`).
pub fn eval_unary_op(op: &str, v: &Value) -> Value {
    match op {
        "-" => match v {
            Value::Int(n) => Value::Int(-n),
            _ => Value::Err("operatore '-' non valido per questo tipo".into()),
        },
        "!" => match v {
            Value::Int(n) => Value::Int(if *n == 0 { 1 } else { 0 }),
            _ => Value::Err("operatore '!' non valido per questo tipo".into()),
        },
        _ => Value::Err(format!("operatore unario '{}' non valido", op)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::value_to_string;

    #[test]
    fn test_eval_literal() {
        let mut env = Env::new();
        let result = eval_expr(&Expr::Literal("hello".into()), &mut env);
        assert_eq!(result, Value::Str("hello".into()));
    }

    #[test]
    fn test_eval_int_literal() {
        let mut env = Env::new();
        let result = eval_expr(&Expr::Literal("42".into()), &mut env);
        assert_eq!(result, Value::Str("42".into()));
    }

    #[test]
    fn test_eval_undefined_var() {
        let mut env = Env::new();
        let result = eval_expr(&Expr::Var("foo".into()), &mut env);
        assert!(matches!(result, Value::Err(_)));
    }

    #[test]
    fn test_eval_let_and_var() {
        let mut env = Env::new();
        let _ = eval_stmt(
            &Stmt::Let("x".into(), Box::new(Expr::Literal("42".into()))),
            &mut env,
        );
        let result = eval_expr(&Expr::Var("x".into()), &mut env);
        assert_eq!(result, Value::Str("42".into()));
    }

    #[test]
    fn test_eval_binary_add_strings() {
        let mut env = Env::new();
        let expr = Expr::BinaryOp(
            Box::new(Expr::Literal("10".into())),
            "+".into(),
            Box::new(Expr::Literal("20".into())),
        );
        let result = eval_expr(&expr, &mut env);
        assert_eq!(result, Value::Str("1020".into())); // string concatenation
    }

    #[test]
    fn test_eval_binary_int_add() {
        let mut env = Env::new();
        env.set_owned("x".into(), Value::Int(1));
        env.set_owned("y".into(), Value::Int(2));
        let expr = Expr::BinaryOp(
            Box::new(Expr::Var("x".into())),
            "+".into(),
            Box::new(Expr::Var("y".into())),
        );
        let result = eval_expr(&expr, &mut env);
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn test_eval_unary_neg() {
        let mut env = Env::new();
        env.set_owned("x".into(), Value::Int(42));
        let expr = Expr::UnaryOp("-".into(), Box::new(Expr::Var("x".into())));
        let result = eval_expr(&expr, &mut env);
        assert_eq!(result, Value::Int(-42));
    }

    #[test]
    fn test_eval_unary_not() {
        let mut env = Env::new();
        env.set_owned("x".into(), Value::Int(0));
        let expr = Expr::UnaryOp("!".into(), Box::new(Expr::Var("x".into())));
        let result = eval_expr(&expr, &mut env);
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn test_eval_stmt_block() {
        let mut env = Env::new();
        let stmts = vec![
            Stmt::Let("a".into(), Box::new(Expr::Literal("1".into()))),
            Stmt::Let("b".into(), Box::new(Expr::Literal("2".into()))),
        ];
        let result = eval_stmt(&Stmt::Block(stmts), &mut env);
        assert_eq!(result, Value::Str("b = 2".into()));
    }

    #[test]
    fn test_eval_stmt_if_true() {
        let mut env = Env::new();
        env.set_owned("x".into(), Value::Int(1));
        let then_stmts = vec![Stmt::Let("y".into(), Box::new(Expr::Literal("yes".into())))];
        let stmt = Stmt::If(
            Box::new(Expr::Var("x".into())),
            then_stmts,
            vec![],
        );
        let result = eval_stmt(&stmt, &mut env);
        assert_eq!(env.get("y").map(|v| v.clone()), Some(Value::Str("yes".into())));
        assert_eq!(result, Value::Str("y = yes".into()));
    }

    #[test]
    fn test_eval_stmt_if_false_with_else() {
        let mut env = Env::new();
        env.set_owned("x".into(), Value::Int(0));
        let then_stmts = vec![Stmt::Let("y".into(), Box::new(Expr::Literal("yes".into())))];
        let else_stmts = vec![Stmt::Let("y".into(), Box::new(Expr::Literal("no".into())))];
        let stmt = Stmt::If(
            Box::new(Expr::Var("x".into())),
            then_stmts,
            else_stmts,
        );
        let result = eval_stmt(&stmt, &mut env);
        assert_eq!(env.get("y").map(|v| v.clone()), Some(Value::Str("no".into())));
    }

    #[test]
    fn test_eval_stmt_for_directory() {
        let mut env = Env::new();
        let items = vec![
            Value::Str("a".into()),
            Value::Str("bb".into()),
            Value::Str("ccc".into()),
        ];
        env.set_owned("items".into(), Value::Directory(items));
        let stmt = Stmt::For(
            "x".into(),
            Box::new(Expr::Var("items".into())),
            vec![Stmt::Let(
                "result".into(),
                Box::new(Expr::MethodCall(
                    Box::new(Expr::Var("x".into())),
                    "len".into(),
                    vec![],
                )),
            )],
        );
        let result = eval_stmt(&stmt, &mut env);
        // L'ultimo elemento e' "ccc" con len 3, quindi result = "result = 3".
        assert_eq!(result, Value::Str("result = 3".into()));
    }

    #[test]
    fn test_eval_method_directory_count() {
        let items = vec![
            Value::Str("a".into()),
            Value::Str("b".into()),
            Value::Str("c".into()),
        ];
        let obj = Value::Directory(items);
        let result = eval_method(&obj, "count", &[], &mut Env::new());
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn test_eval_method_directory_filter() {
        let mut env = Env::new();
        let items = vec![
            Value::Str("apple".into()),
            Value::Str("banana".into()),
            Value::Str("apricot".into()),
        ];
        let obj = Value::Directory(items);
        // filter: x == "banana"
        let cond = Expr::BinaryOp(
            Box::new(Expr::Var("x".into())),
            "==".into(),
            Box::new(Expr::Literal("banana".into())),
        );
        let result = eval_method(&obj, "filter", &[cond], &mut env);
        match result {
            Value::Directory(filtered) => {
                assert_eq!(filtered.len(), 1);
                assert!(filtered.iter().any(|v| matches!(v, Value::Str(s) if s == "banana")));
            }
            _ => panic!("expected Directory, got {:?}", result),
        }
    }

    #[test]
    fn test_eval_method_file_size_name() {
        let obj = Value::File { name: "test.txt".into(), size: 1024 };
        assert_eq!(eval_method(&obj, "size", &[], &mut Env::new()), Value::Int(1024));
        assert_eq!(
            eval_method(&obj, "name", &[], &mut Env::new()),
            Value::Str("test.txt".into())
        );
    }

    #[test]
    fn test_eval_method_str_len() {
        let obj = Value::Str("hello".into());
        assert_eq!(eval_method(&obj, "len", &[], &mut Env::new()), Value::Int(5));
    }

    #[test]
    fn test_value_to_string() {
        assert_eq!(value_to_string(&Value::Str("hi".into())), "hi");
        assert_eq!(value_to_string(&Value::Int(42)), "42");
        assert_eq!(
            value_to_string(&Value::File { name: "f.txt".into(), size: 100 }),
            "File('f.txt', 100 bytes)"
        );
        assert_eq!(value_to_string(&Value::Directory(vec![])), "(vuoto)");
        assert_eq!(
            value_to_string(&Value::Err("something wrong".into())),
            "ERRORE: something wrong"
        );
    }

    #[test]
    fn test_is_truthy() {
        assert!(!is_truthy(&Value::Int(0)));
        assert!(is_truthy(&Value::Int(1)));
        assert!(!is_truthy(&Value::Str("".into())));
        assert!(is_truthy(&Value::Str("x".into())));
        assert!(!is_truthy(&Value::Err("err".into())));
        assert!(is_truthy(&Value::Directory(vec![])));
    }

    #[test]
    fn test_eval_call_help() {
        let mut env = Env::new();
        let result = eval_call("help", &[], &mut env);
        assert!(matches!(result, Value::Str(_)));
    }

    #[test]
    fn test_eval_call_print() {
        let mut env = Env::new();
        let result = eval_call(
            "print",
            &[Expr::Literal("test".into())],
            &mut env,
        );
        assert_eq!(result, Value::Str("print: test".into()));
    }

    #[test]
    fn test_eval_call_quit() {
        let mut env = Env::new();
        let result = eval_call("quit", &[], &mut env);
        assert!(matches!(result, Value::Str(_)));
    }

    #[test]
    fn test_eval_call_fs_not_available() {
        let mut env = Env::new();
        let result = eval_call(
            "fs::list",
            &[Expr::Literal("/".into())],
            &mut env,
        );
        assert!(matches!(result, Value::Err(_)));
        let msg = value_to_string(&result);
        assert!(msg.contains("cardo"));
    }

    #[test]
    fn test_eval_call_unknown() {
        let mut env = Env::new();
        let result = eval_call("nonexistent", &[], &mut env);
        assert!(matches!(result, Value::Err(_)));
    }
}
