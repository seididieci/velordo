//! Ambiente di valutazione + builtin (Fase 2, ADR-0065).
//!
//! Supporta: variabili `let`, assegnamento, scope annidati, operatori
//! aritmetici/relazionali/logici, chiamate funzione, builtin (`clear`, `dump`, `type`).

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::string::{String, ToString};
use alloc::format;

use crate::parser::{Expr, Stmt, Value, BinOp, UnaryOp};

// ── Runtime errors ─────────────────────────────────────────────────

/// Errore di runtime (variabile non trovata, tipo mismatch, ecc.).
#[derive(Debug)]
pub struct RuntimeError(String);

impl core::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "runtime error: {}", self.0)
    }
}

// ── Environment ────────────────────────────────────────────────────

/// Firma di una funzione builtin registrabile.
type BuiltinFn = Arc<dyn Fn(&Env, &[Expr]) -> Result<Value, RuntimeError> + 'static>;

/// Ambiente a scope annidati. Ogni blocco crea un nuovo `Env` con `parent`.
pub struct Env {
    parent:   Option<Rc<Env>>,
    vars:     BTreeMap<String, Value>,
    builtins: BTreeMap<String, BuiltinFn>,
}

impl Env {
    /// Crea un nuovo ambiente root (senza parent).
    pub fn new() -> Self {
        let mut env = Self {
            parent:   None,
            vars:     BTreeMap::new(),
            builtins: BTreeMap::new(),
        };
        // Registra i builtin.
        env.register_builtin("type", env_fn_type);
        env.register_builtin("dump", env_fn_dump);
        env.register_builtin("clear", env_fn_clear);
        env
    }

    /// Registra una funzione builtin.
    fn register_builtin<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&Env, &[Expr]) -> Result<Value, RuntimeError> + 'static,
    {
        self.builtins.insert(name.to_string(), Arc::new(f));
    }

    /// Cerca una funzione builtin: scope corrente → parent → …
    fn find_builtin(&self, name: &str) -> Option<&BuiltinFn> {
        if let Some(f) = self.builtins.get(name) {
            return Some(f);
        }
        if let Some(parent) = &self.parent {
            return parent.find_builtin(name);
        }
        None
    }

    /// Cerca una variabile: scope corrente → parent → ...
    pub fn get(&self, name: &str) -> Option<&Value> {
        if let Some(v) = self.vars.get(name) {
            return Some(v);
        }
        if let Some(parent) = &self.parent {
            return parent.get(name);
        }
        None
    }

    /// Imposta una variabile nello scope corrente.
    pub fn set(&mut self, name: String, value: Value) {
        self.vars.insert(name, value);
    }

    /// Valuta un'espressione e ritorna il valore.
    pub fn eval_expr(&self, expr: &Expr) -> Result<Value, RuntimeError> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Ident(name) => {
                match self.get(name) {
                    Some(v) => Ok(v.clone()),
                    None => Err(RuntimeError(format!("variabile non trovata: {}", name))),
                }
            }
            Expr::Binary(left, op, right) => {
                let lv = self.eval_expr(left)?;
                let rv = self.eval_expr(right)?;
                eval_binop(op, lv, rv)
            }
            Expr::Call(name, args) => {
                // 1. Builtin registrati (cercati anche nei parent scope).
                if let Some(f) = self.find_builtin(name) {
                    return f(self, args);
                }
                // 2. Variabile-funzione (Fase 4) o riferimento a variabile.
                match self.get(name) {
                    Some(v) => Ok(v.clone()),
                    None => Err(RuntimeError(format!("funzione non trovata: {}", name))),
                }
            }
            Expr::Unary(op, inner) => {
                match op {
                    UnaryOp::Not => {
                        let v = self.eval_expr(inner)?;
                        match v {
                            Value::Bool(b) => Ok(Value::Bool(!b)),
                            _ => Err(RuntimeError("! richiede un valore booleano".into())),
                        }
                    }
                }
            }
        }
    }

    /// Valuta una statement e ritorna il valore prodotto (se applicabile).
    pub fn eval_stmt(&mut self, stmt: &Stmt) -> Result<Option<Value>, RuntimeError> {
        match stmt {
            Stmt::Let(name, expr) => {
                let val = self.eval_expr(expr)?;
                self.vars.insert(name.clone(), val);
                Ok(None)
            }
            Stmt::Assign(name, expr) => {
                let val = self.eval_expr(expr)?;
                // Cerca nello scope corrente o in un parent (solo se il parent
                // e' mutabile tramite Rc::get_mut).
                if self.vars.contains_key(name.as_str()) {
                    self.vars.insert(name.clone(), val);
                    Ok(None)
                } else if let Some(parent) = &mut self.parent {
                    if let Some(inner) = Rc::get_mut(parent) {
                        inner.vars.insert(name.clone(), val);
                        return Ok(None);
                    }
                    // Non mutabile: errore.
                    Err(RuntimeError(format!("variabile non dichiarata: {}", name)))
                } else {
                    Err(RuntimeError(format!("variabile non dichiarata: {}", name)))
                }
            }
            Stmt::Expr(expr) => Ok(Some(self.eval_expr(expr)?)),
            Stmt::Block(stmts) => {
                // Crea un nuovo scope figlio che eredita variabili e catena dei
                // parent. Copia le variabili visibili in child.vars così le
                // assegnazioni a variabili esterne (es. `i = i + 1` in un while)
                // vengono trovate e propagate correttamente da merge_from.
                let mut child = Self {
                    parent:   self.parent.clone(),
                    vars:     self.vars.clone(),
                    builtins: self.builtins.clone(),
                };
                let mut last: Option<Value> = None;
                for s in stmts {
                    last = child.eval_stmt(s)?;
                }
                // Propaga le variabili dallo scope figlio al padre.
                self.merge_from(&child);
                Ok(last)
            }
            Stmt::If(cond, then_stmts, else_stmts) => {
                let cv = self.eval_expr(cond)?;
                let truthy = match cv {
                    Value::Bool(b) => b,
                    Value::Number(n) => n != 0.0,
                    _ => false,
                };
                if truthy {
                    self.eval_block(then_stmts)
                } else if !else_stmts.is_empty() {
                    self.eval_block(else_stmts)
                } else {
                    Ok(None)
                }
            }
            Stmt::While(cond, stmts) => {
                loop {
                    let cv = self.eval_expr(cond)?;
                    let truthy = match cv {
                        Value::Bool(b) => b,
                        Value::Number(n) => n != 0.0,
                        _ => false,
                    };
                    if !truthy {
                        break Ok(None);
                    }
                    // evaluate block; discard result
                    let _ = self.eval_block(stmts)?;
                }
            }

            Stmt::For(name, start_expr, end_expr, body) => {
                let start = match self.eval_expr(start_expr)? {
                    Value::Number(n) => n as i64,
                    _ => return Err(RuntimeError("for richiede un numero come inizio".into())),
                };
                let end = match self.eval_expr(end_expr)? {
                    Value::Number(n) => n as i64,
                    _ => return Err(RuntimeError("for richiede un numero come fine".into())),
                };
                let mut last: Option<Value> = None;
                let mut i = start;
                while i < end {
                    // Crea uno scope figlio che eredita variabili da self.
                    // La variabile di loop viene inserita in child.vars e le
                    // assegnazioni nel corpo la trovano e la aggiornano;
                    // merge_from (chiamato da eval_block) propaga il valore
                    // aggiornato di ritorno a child per l'iterazione successiva.
                    let mut child = Self {
                        parent:   self.parent.clone(),
                        vars:     self.vars.clone(),
                        builtins: self.builtins.clone(),
                    };
                    child.set(name.clone(), Value::Number(i as f64));
                    last = child.eval_block(body)?;
                    i += 1;
                }
                Ok(last)
            }
        }
    }

    /// Valuta un blocco di statement in uno scope figlio, coerente con `Stmt::Block`.
    /// Le variabili definite qui si propagano al parent (merge_from) così `if`/`else`
    /// si comporta come `{ }`.
    fn eval_block(&mut self, stmts: &[Stmt]) -> Result<Option<Value>, RuntimeError> {
        let mut child = Self {
            parent:   self.parent.clone(),
            vars:     self.vars.clone(),
            builtins: self.builtins.clone(),
        };
        let mut last: Option<Value> = None;
        for s in stmts {
            last = child.eval_stmt(s)?;
        }
        // Propaga le variabili dallo scope figlio al padre.
        self.merge_from(&child);
        Ok(last)
    }

    /// Propaga le variabili dallo scope figlio al padre (per `let` nel blocco).
    fn merge_from(&mut self, child: &Env) {
        // Copia solo le variabili dello scope corrente del child.
        for (k, v) in &child.vars {
            self.vars.insert(k.clone(), v.clone());
        }
    }

    /// Rimuove una variabile dato un `Expr::Ident` (usato da `clear`).
    pub fn clear_var(&mut self, expr: &Expr) -> Result<(), RuntimeError> {
        if let Expr::Ident(name) = expr {
            if self.vars.remove(name).is_some() {
                Ok(())
            } else {
                Err(RuntimeError(format!("variabile non trovata: {}", name)))
            }
        } else {
            Err(RuntimeError("clear richiede un identificatore".into()))
        }
    }
}

// ── Operator evaluation ────────────────────────────────────────────

/// Valuta un operatore binario su due valori.
fn eval_binop(op: &BinOp, lv: Value, rv: Value) -> Result<Value, RuntimeError> {
    match op {
        BinOp::Add => {
            // Concatenazione stringhe se entrambi gli operandi sono Str.
            if let (Value::Str(a), Value::Str(b)) = (&lv, &rv) {
                let mut out = a.clone();
                out.extend_from_slice(b);
                return Ok(Value::Str(out));
            }
            numeric_binop(lv, rv, |a, b| Ok(a + b))
        }
        BinOp::Sub => numeric_binop(lv, rv, |a, b| Ok(a - b)),
        BinOp::Mul => numeric_binop(lv, rv, |a, b| Ok(a * b)),
        BinOp::Div => numeric_binop(lv, rv, |a, b| {
            if b == 0.0 { Err(RuntimeError("divisione per zero".into())) } else { Ok(a / b) }
        }),
        BinOp::Mod => numeric_binop(lv, rv, |a, b| Ok(a % b)),
        BinOp::Eq   => Ok(Value::Bool(values_equal(&lv, &rv))),
        BinOp::Ne   => Ok(Value::Bool(!values_equal(&lv, &rv))),
        BinOp::Lt   => numeric_cmp(lv, rv, |a, b| a < b),
        BinOp::Le   => numeric_cmp(lv, rv, |a, b| a <= b),
        BinOp::Gt   => numeric_cmp(lv, rv, |a, b| a > b),
        BinOp::Ge   => numeric_cmp(lv, rv, |a, b| a >= b),
        BinOp::And  => bool_binop(lv, rv, |a, b| a && b),
        BinOp::Or   => bool_binop(lv, rv, |a, b| a || b),
    }
}

fn numeric_binop<F>(lv: Value, rv: Value, f: F) -> Result<Value, RuntimeError>
where
    F: FnOnce(f64, f64) -> Result<f64, RuntimeError>,
{
    let a = as_number(lv)?;
    let b = as_number(rv)?;
    Ok(Value::Number(f(a, b)?))
}

fn numeric_cmp<F>(lv: Value, rv: Value, cmp: F) -> Result<Value, RuntimeError>
where
    F: FnOnce(f64, f64) -> bool,
{
    let a = as_number(lv)?;
    let b = as_number(rv)?;
    Ok(Value::Bool(cmp(a, b)))
}

fn bool_binop<F>(lv: Value, rv: Value, cmp: F) -> Result<Value, RuntimeError>
where
    F: FnOnce(bool, bool) -> bool,
{
    let a = as_bool(lv)?;
    let b = as_bool(rv)?;
    Ok(Value::Bool(cmp(a, b)))
}

fn as_number(v: Value) -> Result<f64, RuntimeError> {
    match v {
        Value::Number(n) => Ok(n),
        _ => Err(RuntimeError(format!("numero atteso, trovato {}", type_name(&v)))),
    }
}

fn as_bool(v: Value) -> Result<bool, RuntimeError> {
    match v {
        Value::Bool(b) => Ok(b),
        _ => Err(RuntimeError(format!("booleano atteso, trovato {}", type_name(&v)))),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => a == b,
        (Value::Object(a), Value::Object(b)) => a == b,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

// ── Builtin helpers ────────────────────────────────────────────────

/// Restituisce il nome del tipo di un valore.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null     => "null",
        Value::Bool(_)  => "bool",
        Value::Number(_) => "number",
        Value::Str(_)   => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Formatta un valore per dump.
fn format_value(v: &Value) -> String {
    match v {
        Value::Null     => "null".into(),
        Value::Bool(b)  => format!("{}", b),
        Value::Number(n)=> format!("{}", n),
        Value::Str(s)   => {
            // Non aggiungo virgolette qui: lo fa Display per Value::Str.
            core::str::from_utf8(s).unwrap_or("").into()
        }
        Value::Array(items) => {
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 { out.push_str(", "); }
                out.push_str(&format!("{}", item));
            }
            out.push(']');
            out
        }
        Value::Object(map) => {
            let mut out = String::from("{");
            let mut first = true;
            for (k, v) in map {
                if !first { out.push_str(", "); }
                first = false;
                out.push_str(&format!("\"{}\": {}", k, v));
            }
            out.push('}');
            out
        }
    }
}

/// Valuta l'unico argomento di un builtin. Gli argomenti arrivano come AST:
/// la valutazione (call-by-value) spetta al builtin, non a `Expr::Call`
/// (`clear` ha bisogno dell'identificatore non valutato).
fn eval_builtin_arg(env: &Env, args: &[Expr], name: &str) -> Result<Value, RuntimeError> {
    match args {
        [arg] => env.eval_expr(arg),
        _ => Err(RuntimeError(format!("{} richiede 1 arg", name))),
    }
}

/// Funzione builtin `type(expr)` — chiamata da eval_expr.
fn env_fn_type(env: &Env, args: &[Expr]) -> Result<Value, RuntimeError> {
    let v = eval_builtin_arg(env, args, "type")?;
    Ok(Value::Str(type_name(&v).as_bytes().to_vec()))
}

/// Funzione builtin `dump(expr)` — chiamata da eval_expr.
fn env_fn_dump(env: &Env, args: &[Expr]) -> Result<Value, RuntimeError> {
    let v = eval_builtin_arg(env, args, "dump")?;
    Ok(Value::Str(format_value(&v).into_bytes()))
}

/// Funzione builtin `clear(name)` — chiamata da eval_expr.
fn env_fn_clear(_env: &Env, args: &[Expr]) -> Result<Value, RuntimeError> {
    if args.len() != 1 {
        return Err(RuntimeError("clear richiede 1 arg".into()));
    }
    // La rimozione vera e propria viene gestita da eval_stmt.
    Ok(Value::Null)
}

// ── Entry point pubblico ──────────────────────────────────────────

/// Valuta una lista di statement con un ambiente persistente.
/// Ritorna l'ultimo valore prodotto (se presente).
pub fn evaluate(env: &mut Env, stmts: &[Stmt]) -> Result<Option<Value>, RuntimeError> {
    let mut last: Option<Value> = None;
    for stmt in stmts {
        match env.eval_stmt(stmt) {
            Ok(val) => {
                // Se è un clear, applica la rimozione.
                if let (Some(Value::Null), Stmt::Expr(Expr::Call(name, args))) = (&val, stmt) {
                    if name == "clear" && args.len() == 1 {
                        env.clear_var(&args[0])?;
                    }
                }
                last = val;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(last) // Restituisce l'ultimo valore per la stampa nel REPL.
}
