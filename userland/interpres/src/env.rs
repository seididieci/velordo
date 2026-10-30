//! Ambiente di valutazione + builtin (Fase 2, ADR-0065).
//!
//! Supporta: variabili `let`, assegnamento, scope annidati, operatori
//! aritmetici/relazionali/logici, chiamate funzione, builtin (`clear`, `dump`, `type`).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;

use crate::parser::{Expr, Stmt, Value, BinOp};

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

/// Ambiente a scope annidati. Ogni blocco crea un nuovo `Env` con `parent`.
pub struct Env {
    parent: Option<Box<Env>>,
    vars:   BTreeMap<String, Value>,
}

impl Env {
    /// Crea un nuovo ambiente root (senza parent).
    pub fn new() -> Self {
        let mut env = Self { parent: None, vars: BTreeMap::new() };
        // Registra i builtin.
        env.register_builtin("type", env_fn_type);
        env.register_builtin("dump", env_fn_dump);
        env.register_builtin("clear", env_fn_clear);
        env
    }

    /// Crea un nuovo scope con questo ambiente come parent.
    pub fn child(&self) -> Self {
        Self {
            parent: Some(Box::new(Self {
                parent: None,
                vars: BTreeMap::new(),
            })),
            vars: BTreeMap::new(),
        }
    }

    /// Registra una funzione builtin.
    fn register_builtin<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&Env, &[Expr]) -> Result<Value, RuntimeError> + 'static,
    {
        // Salvata come Value::Object con campo speciale (placeholder per Fase 4).
        let _ = name;
        let _ = f;
        // Per ora i builtin sono gestiti direttamente in eval_expr.
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

    /// Rimuove una variabile dallo scope corrente.
    pub fn remove(&mut self, name: &str) -> bool {
        self.vars.remove(name).is_some()
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
                // Funzioni builtin o variabili-funzione (Fase 4).
                match name.as_str() {
                    "type" => {
                        if args.len() == 1 {
                            let v = self.eval_expr(&args[0])?;
                            Ok(Value::Str(type_name(&v).as_bytes().to_vec()))
                        } else {
                            Err(RuntimeError("type richiede 1 arg".into()))
                        }
                    }
                    "dump" => {
                        if args.len() == 1 {
                            let v = self.eval_expr(&args[0])?;
                            Ok(Value::Str(format_value(&v).into_bytes()))
                        } else {
                            Err(RuntimeError("dump richiede 1 arg".into()))
                        }
                    }
                    "clear" => {
                        // clear name — rimuove la variabile.
                        if let Expr::Ident(n) = &args[0] {
                            // Non posso mutare self qui (borrow checker).
                            // La rimozione viene gestita in eval_stmt.
                            Ok(Value::Null)
                        } else {
                            Err(RuntimeError("clear richiede un identificatore".into()))
                        }
                    }
                    _ => {
                        // Nome non riconosciuto: variabile o riferimento a builtin futuro.
                        match self.get(name) {
                            Some(v) => Ok(v.clone()),
                            None => Err(RuntimeError(format!("funzione non trovata: {}", name))),
                        }
                    }
                }
            }
            Expr::Unary(op, inner) => {
                match op {
                    BinOp::Not => {
                        let v = self.eval_expr(inner)?;
                        match v {
                            Value::Bool(b) => Ok(Value::Bool(!b)),
                            _ => Err(RuntimeError("! richiede un valore booleano".into())),
                        }
                    }
                    _ => Err(RuntimeError(format!("operatore unario non supportato: {:?}", op))),
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
                // Cerca nello scope corrente o in un parent.
                if self.vars.contains_key(name.as_str()) {
                    self.vars.insert(name.clone(), val);
                    Ok(None)
                } else {
                    // Non presente: errore (non posso risalire ai parent per set).
                    Err(RuntimeError(format!("variabile non dichiarata: {}", name)))
                }
            }
            Stmt::Expr(expr) => Ok(Some(self.eval_expr(expr)?)),
            Stmt::Block(stmts) => {
                // Crea un nuovo scope.
                let mut child = Env {
                    parent: Some(Box::new(Self {
                        parent: None,
                        vars: BTreeMap::new(),
                    })),
                    vars: self.vars.clone(), // eredita le variabili visibili
                };
                let mut last: Option<Value> = None;
                for s in stmts {
                    last = child.eval_stmt(s)?;
                }
                // Propaga le variabili dallo scope figlio al padre (per `let` nel blocco).
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
            Stmt::For(..) => {
                // Placeholder per Fase 2+.
                Err(RuntimeError("for non ancora implementato".into()))
            }
        }
    }

    /// Valuta un blocco di statement (senza creare scope — usato per if/else).
    fn eval_block(&mut self, stmts: &[Stmt]) -> Result<Option<Value>, RuntimeError> {
        let mut last: Option<Value> = None;
        for s in stmts {
            last = self.eval_stmt(s)?;
        }
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

    /// Reset completo dell'ambiente (per `clear` senza argomenti).
    pub fn clear_all(&mut self) {
        self.vars.clear();
        if let Some(parent) = &mut self.parent {
            parent.clear_all();
        }
    }
}

// ── Operator evaluation ────────────────────────────────────────────

/// Valuta un operatore binario su due valori.
fn eval_binop(op: &BinOp, lv: Value, rv: Value) -> Result<Value, RuntimeError> {
    match op {
        BinOp::Add => numeric_binop(lv, rv, |a, b| Ok(a + b)),
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
        _ => Err(RuntimeError(format!("operatore non supportato: {:?}", op))),
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
        _               => "unknown",
    }
}

/// Formatta un valore per dump.
fn format_value(v: &Value) -> String {
    match v {
        Value::Null     => "null".into(),
        Value::Bool(b)  => format!("{}", b),
        Value::Number(n)=> format!("{}", n),
        Value::Str(s)   => {
            let s = core::str::from_utf8(s).unwrap_or("");
            format!("\"{}\"", s)
        }
        _ => "[non serializzato]".into(),
    }
}

/// Funzione builtin `type(name)` — chiamata da eval_expr.
fn env_fn_type(_env: &Env, args: &[Expr]) -> Result<Value, RuntimeError> {
    if args.len() != 1 {
        return Err(RuntimeError("type richiede 1 arg".into()));
    }
    // La valutazione avviene nel chiamante. Qui restituiamo il tipo del primo arg non valutato.
    Ok(Value::Str(match &args[0] {
        Expr::Literal(v) => type_name(v).as_bytes().to_vec(),
        Expr::Ident(name) => match _env.get(name) {
            Some(v) => type_name(v).as_bytes().to_vec(),
            None => b"undefined".to_vec(),
        },
        _ => b"expr".to_vec(),
    }))
}

/// Funzione builtin `dump(name)` — chiamata da eval_expr.
fn env_fn_dump(_env: &Env, args: &[Expr]) -> Result<Value, RuntimeError> {
    if args.len() != 1 {
        return Err(RuntimeError("dump richiede 1 arg".into()));
    }
    // La valutazione avviene nel chiamante. Qui restituiamo la stringa formattata.
    Ok(Value::Str(match &args[0] {
        Expr::Literal(v) => format_value(v).into_bytes(),
        Expr::Ident(name) => match _env.get(name) {
            Some(v) => format_value(v).into_bytes(),
            None => b"[undefined]".to_vec(),
        },
        _ => b"[non dumpabile]".to_vec(),
    }))
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
