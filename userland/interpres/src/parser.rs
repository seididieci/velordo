//! Parser ricorsivo per la sintassi Rust-like (Fase 1, ADR-0065).
//!
//! Trasforma i token prodotti dal lexer in un AST (`Expr` / `Stmt`).
//! Grammar semplificata: `let`, assegnamento, blocchi `{}`, `if/else`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use alloc::format;
use alloc::boxed::Box;

use crate::lexer::{Token, LexError};

// ── AST types ──────────────────────────────────────────────────────

/// Tipo di operatore binario.
#[derive(Clone, Debug, PartialEq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or, Not, // Not è unario, ma lo metto qui per semplicità
}

/// Espressione — nodo interno dell'AST.
#[derive(Clone, Debug)]
pub enum Expr {
    /// Letterale: numero o stringa.
    Literal(Value),
    /// Riferimento a variabile.
    Ident(String),
    /// Operazione binaria: operandi + operatore.
    Binary(Box<Expr>, BinOp, Box<Expr>),
    /// Chiamata di funzione: nome + argomenti.
    Call(String, Vec<Expr>),
    /// Operatore unario (!).
    Unary(BinOp, Box<Expr>),
}

/// Valore letterale — tipi supportati in fase 1.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Str(Vec<u8>),
}

impl core::fmt::Display for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::Null     => write!(f, "null"),
            Value::Bool(b)  => write!(f, "{}", b),
            Value::Number(n)=> write!(f, "{}", n),
            Value::Str(s)   => {
                let s = core::str::from_utf8(s).unwrap_or("");
                write!(f, "\"{}\"", s)
            }
        }
    }
}

/// Istruzione — nodo di statement dell'AST.
#[derive(Clone, Debug)]
pub enum Stmt {
    /// `let name = expr`
    Let(String, Expr),
    /// `name = expr` (assegnamento)
    Assign(String, Expr),
    /// Espressione come statement (es. `dump(x)`).
    Expr(Expr),
    /// Blocco `{ stmts... }`.
    Block(Vec<Stmt>),
    /// `if cond { then } else { otherwise }`.
    If(Box<Expr>, Vec<Stmt>, Vec<Stmt>),
    /// `for name in start..end { stmts... }` — placeholder per Fase 2+.
    For(String, Expr, Expr, Vec<Stmt>),
}

/// Errore di parsing con posizione.
#[derive(Debug)]
pub struct ParseError {
    pub pos: usize,
    pub msg: String,
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError { pos: e.pos, msg: e.msg }
    }
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "parse error at {}: {}", self.pos, self.msg)
    }
}

// ── Parser state ──────────────────────────────────────────────────

/// Stato condiviso tra le chiamate ricorsive del parser.
struct ParserState {
    tokens: Vec<Token>,
    pos:    usize,
}

impl ParserState {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Token corrente senza avanzare.
    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    /// Avanza di un token e ritorna quello vecchio.
    fn advance(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    /// Consuma il token corrente solo se matcha `expected`.
    fn expect(&mut self, expected: Token) -> Result<Token, ParseError> {
        let cur = self.peek().clone();
        if cur == expected {
            Ok(self.advance())
        } else {
            Err(ParseError { pos: self.pos, msg: format!("expected {:?}, got {:?}", expected, cur) })
        }
    }

    /// Consuma il token corrente solo se matcha `expected`, ritorna true/false.
    fn try_consume(&mut self, expected: Token) -> bool {
        if *self.peek() == expected {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consuma il token corrente solo se matcha la keyword data.
    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        self.expect(Token::Keyword(kw.into())).map(|_| ())
    }

    /// Consuma tutto lo whitespace e i separatori (;), ritorna true se c'è ancora input.
    fn skip_ws_and_semi(&mut self) -> bool {
        loop {
            match self.peek() {
                Token::Eof => return false,
                Token::Delim(';') => { self.advance(); continue; }
                _ => break,
            }
        }
        true
    }
}

// ── Parser entry point ───────────────────────────────────────────

/// Parse e valuta una stringa grezza.
/// Ritorna la lista di statement parsed (o errore).
pub fn parse(input: &str) -> Result<Vec<Stmt>, ParseError> {
    let tokens = crate::lexer::tokenize(input)?;
    let mut state = ParserState::new(tokens);
    let mut stmts = Vec::new();

    while state.skip_ws_and_semi() {
        let stmt = parse_stmt(&mut state)?;
        stmts.push(stmt);
    }

    Ok(stmts)
}

/// Parse e valutazione combinata: ritorna il valore dell'ultimo statement (se applicabile).
pub fn parse_and_eval(input: &str) -> Result<Option<Value>, ParseError> {
    let stmts = parse(input)?;
    // Per ora non valutiamo — la valutazione viene nella Fase 2.
    // Ritorna None per permettere il fallback al dispatcher esistente.
    let _ = stmts;
    Ok(None)
}

// ── Statement parsing ─────────────────────────────────────────────

fn parse_stmt(state: &mut ParserState) -> Result<Stmt, ParseError> {
    let kw = state.peek().as_keyword();
    match kw {
        Some("let") => return parse_let(state),
        Some("if")  => return parse_if(state),
        Some("for") => return parse_for(state), // placeholder per Fase 2+
        _ => {} // continua con il match principale
    }
    match state.peek() {
        Token::Delim('{') => parse_block(state),
        _                 => parse_expr_stmt(state),
    }
}

/// `let IDENT = expr`
fn parse_let(state: &mut ParserState) -> Result<Stmt, ParseError> {
    state.advance(); // consume "let"
    let name = match state.advance() {
        Token::Ident(s) => s,
        other => return Err(ParseError { pos: state.pos, msg: format!("expected ident after 'let', got {:?}", other) }),
    };
    state.expect(Token::Delim('='))?;
    let expr = parse_expr(state)?;
    Ok(Stmt::Let(name, expr))
}

/// `IDENT = expr` (assegnamento — solo se l'ident è seguito da `=`).
fn try_parse_assign(state: &mut ParserState) -> Result<Option<Stmt>, ParseError> {
    // Controlla se c'è un identificatore seguito da '=' (ma non ==).
    if let Token::Ident(ref name) = *state.peek() {
        let name_clone = name.clone();
        state.advance(); // consume ident
        match state.peek() {
            Token::Delim('=') => {
                state.advance(); // consume '='
                // È un assegnamento: nome = expr
                let expr = parse_expr(state)?;
                Ok(Some(Stmt::Assign(name_clone, expr)))
            }
            Token::Op(op) if op == "==" => {
                // Era `==`, torna indietro e lascia che lo gestisca parse_expr come confronto
                state.pos -= 2;
                Ok(None)
            }
            _ => {
                // Non è un assegnamento: torna indietro
                state.pos -= 1;
                Ok(None)
            }
        }
    } else {
        Ok(None)
    }
}

/// Blocco `{ stmts... }`
fn parse_block(state: &mut ParserState) -> Result<Stmt, ParseError> {
    state.advance(); // consume '{'
    let mut stmts = Vec::new();
    while !state.try_consume(Token::Delim('}')) {
        if state.peek() == &Token::Eof {
            return Err(ParseError { pos: state.pos, msg: "unclosed block".into() });
        }
        let stmt = parse_stmt(state)?;
        stmts.push(stmt);
        // Consuma il `;` opzionale dopo ogni statement (dentro blocchi).
        state.try_consume(Token::Delim(';'));
    }
    Ok(Stmt::Block(stmts))
}

/// `if expr { then } else { otherwise }`
fn parse_if(state: &mut ParserState) -> Result<Stmt, ParseError> {
    state.advance(); // consume "if"
    let cond = parse_expr(state)?;
    let then_block = parse_block(state)?;
    let else_block = if state.try_consume(Token::Keyword("else".into())) {
        let eb = parse_block(state)?;
        match eb {
            Stmt::Block(s) => s,
            _ => vec![eb],
        }
    } else {
        Vec::new()
    };
    Ok(Stmt::If(Box::new(cond), then_stmts(then_block), else_block))
}

/// Estrae gli statement da un blocco (se è un ExprStmt, lo avvolge in Block).
fn then_stmts(block: Stmt) -> Vec<Stmt> {
    match block {
        Stmt::Block(s) => s,
        other => vec![other],
    }
}

/// `for IDENT in expr { stmts... }` — placeholder per Fase 2+.
fn parse_for(state: &mut ParserState) -> Result<Stmt, ParseError> {
    state.advance(); // consume "for"
    let name = match state.advance() {
        Token::Ident(s) => s,
        other => return Err(ParseError { pos: state.pos, msg: format!("expected ident after 'for', got {:?}", other) }),
    };
    state.expect_keyword("in")?;
    let start = parse_expr(state)?;
    state.expect(Token::Op("..".into()))?; // range operator
    let end = parse_expr(state)?;
    let body = parse_block(state)?;
    // Per ora: restituisce un placeholder — la valutazione viene dopo.
    let _ = (name, start, end);
    Ok(body)
}

/// Expression statement: `expr` o `expr ; expr`.
fn parse_expr_stmt(state: &mut ParserState) -> Result<Stmt, ParseError> {
    // Prima prova assegnamento (che non può essere confuso con binary).
    if let Some(stmt) = try_parse_assign(state)? {
        return Ok(stmt);
    }
    // Altrimenti parse come espressione.
    let expr = parse_expr(state)?;
    Ok(Stmt::Expr(expr))
}

// ── Expression parsing (recursive descent, precedence climbing) ───

/// Espressione di livello più basso: `||`.
fn parse_expr(state: &mut ParserState) -> Result<Expr, ParseError> {
    parse_or(state)
}

/// Livello OR: `expr || expr`.
fn parse_or(state: &mut ParserState) -> Result<Expr, ParseError> {
    let mut left = parse_and(state)?;
    while state.try_consume(Token::Op("||".into())) {
        let right = parse_and(state)?;
        left = Expr::Binary(Box::new(left), BinOp::Or, Box::new(right));
    }
    Ok(left)
}

/// Livello AND: `expr && expr`.
fn parse_and(state: &mut ParserState) -> Result<Expr, ParseError> {
    let mut left = parse_equality(state)?;
    while state.try_consume(Token::Op("&&".into())) {
        let right = parse_equality(state)?;
        left = Expr::Binary(Box::new(left), BinOp::And, Box::new(right));
    }
    Ok(left)
}

/// Livello uguaglianza: `expr == expr` | `expr != expr`.
fn parse_equality(state: &mut ParserState) -> Result<Expr, ParseError> {
    let mut left = parse_comparison(state)?;
    loop {
        let op = match state.peek() {
            Token::Op(op) if op == "==" => BinOp::Eq,
            Token::Op(op) if op == "!=" => BinOp::Ne,
            _ => break,
        };
        state.advance();
        let right = parse_comparison(state)?;
        left = Expr::Binary(Box::new(left), op, Box::new(right));
    }
    Ok(left)
}

/// Livello confronto: `<` `<=` `>` `>=`.
fn parse_comparison(state: &mut ParserState) -> Result<Expr, ParseError> {
    let mut left = parse_additive(state)?;
    loop {
        let op = match state.peek() {
            Token::Op(op) if op == "<"  => BinOp::Lt,
            Token::Op(op) if op == "<=" => BinOp::Le,
            Token::Op(op) if op == ">"  => BinOp::Gt,
            Token::Op(op) if op == ">=" => BinOp::Ge,
            _ => break,
        };
        state.advance();
        let right = parse_additive(state)?;
        left = Expr::Binary(Box::new(left), op, Box::new(right));
    }
    Ok(left)
}

/// Livello additivo: `expr + expr` | `expr - expr`.
fn parse_additive(state: &mut ParserState) -> Result<Expr, ParseError> {
    let mut left = parse_multiplicative(state)?;
    loop {
        let op = match state.peek() {
            Token::Op(op) if op == "+" => BinOp::Add,
            Token::Op(op) if op == "-" => BinOp::Sub,
            _ => break,
        };
        state.advance();
        let right = parse_multiplicative(state)?;
        left = Expr::Binary(Box::new(left), op, Box::new(right));
    }
    Ok(left)
}

/// Livello moltiplicativo: `expr * expr` | `expr / expr` | `expr % expr`.
fn parse_multiplicative(state: &mut ParserState) -> Result<Expr, ParseError> {
    let mut left = parse_unary(state)?;
    loop {
        let op = match state.peek() {
            Token::Op(op) if op == "*" => BinOp::Mul,
            Token::Op(op) if op == "/" => BinOp::Div,
            Token::Op(op) if op == "%" => BinOp::Mod,
            _ => break,
        };
        state.advance();
        let right = parse_unary(state)?;
        left = Expr::Binary(Box::new(left), op, Box::new(right));
    }
    Ok(left)
}

/// Unary: `! expr` o primary.
fn parse_unary(state: &mut ParserState) -> Result<Expr, ParseError> {
    if state.try_consume(Token::Op("!".into())) {
        let expr = parse_unary(state)?; // right-associative
        return Ok(Expr::Unary(BinOp::Not, Box::new(expr)));
    }
    parse_primary(state)
}

/// Primary: literal, ident, func_call, `(expr)`.
fn parse_primary(state: &mut ParserState) -> Result<Expr, ParseError> {
    // Controlla keyword letterali (clona e confronta direttamente).
    let tok = state.peek().clone();
    if let Token::Keyword(s) = tok {
        if s == "true" { state.advance(); return Ok(Expr::Literal(Value::Bool(true))); }
        if s == "false" { state.advance(); return Ok(Expr::Literal(Value::Bool(false))); }
        if s == "null" { state.advance(); return Ok(Expr::Literal(Value::Null)); }
    }

    match state.peek() {
        Token::Number(n) => {
            let val = *n;
            state.advance();
            Ok(Expr::Literal(Value::Number(val)))
        }
        Token::Str(s) => {
            let data = s.clone();
            state.advance();
            Ok(Expr::Literal(Value::Str(data)))
        }
        Token::Ident(name) => {
            let name_clone = name.clone();
            state.advance();
            // Check for function call: `name(...)`
            if state.try_consume(Token::Delim('(')) {
                let mut args = Vec::new();
                while !state.try_consume(Token::Delim(')')) {
                    if state.peek() == &Token::Eof {
                        return Err(ParseError { pos: state.pos, msg: "unclosed function call".into() });
                    }
                    let arg = parse_expr(state)?;
                    args.push(arg);
                    state.try_consume(Token::Delim(','));
                }
                Ok(Expr::Call(name_clone, args))
            } else {
                // Variabile o builtin reference
                Ok(Expr::Ident(name_clone))
            }
        }
        Token::Delim('(') => {
            state.advance(); // consume '('
            let expr = parse_expr(state)?;
            state.expect(Token::Delim(')'))?;
            Ok(expr)
        }
        other => Err(ParseError { pos: state.pos, msg: format!("unexpected token {:?}", other) }),
    }
}

