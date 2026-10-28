//! Parser ricorsivo discendente per l'interprete REPL (S3.2b).
//!
//! Grammar semplificata:
//! ```
//! stmt → let IDENT = expr NEWLINE
//!      → for IDENT in expr { stmts }
//!      → if expr { stmts } else { stmts }
//!      → expr SEMLINE
//!      → { stmts }
//! expr → IDENT ( . IDENT )* ( LPAREN expr_list RPAREN )?
//!      → expr OP expr  (per operatori binari)
//! ```

use super::ast::{Expr, Stmt};
use super::lexer;
use super::lexer::{Token, is_keyword};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Parser: tokenizza + parsifica → lista di istruzioni.
pub fn parse(input: &str) -> Result<Vec<Stmt>, String> {
    let tokens = lexer::tokenize(input);
    // Debug: stampa i token prodotti dal lexer.
    crate::debug_tokens(&tokens);
    let mut parser = Parser {
        tokens,
        pos: 0,
    };
    parser.parse_stmts()
}

/// Parser state.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    /// Parse statements until end or newline.
    fn parse_stmts(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        while self.pos < self.tokens.len() && !self.at_newline() {
            let stmt = self.parse_stmt()?;
            stmts.push(stmt);
            // Consuma separatore opzionale.
            if self.eat(Token::Semi) {
                continue;
            }
        }
        Ok(stmts)
    }

    /// Parse single statement: let / for / if / block / expr.
    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        // `let x = expr`
        if self.eat(Token::Name("let".into())) {
            return self.parse_let();
        }

        // `for var in expr { stmts }`
        if self.eat(Token::Name("for".into())) {
            return self.parse_for();
        }

        // `if expr { stmts } else { stmts }`
        if self.eat(Token::Name("if".into())) {
            return self.parse_if();
        }

        // `{ stmts }` (blocco)
        if self.eat(Token::LBrace) {
            let body = self.parse_stmts()?;
            self.expect(Token::RBrace)?;
            return Ok(Stmt::Block(body));
        }

        // Espressione sola.
        let expr = self.parse_expr()?;
        Ok(Stmt::Expr(expr))
    }

    /// Parse `let IDENT = expr`.
    fn parse_let(&mut self) -> Result<Stmt, String> {
        let name = match self.next() {
            Token::Name(n) => n,
            t => return Err(format!("expected name after 'let', got {:?}", t)),
        };
        self.expect(Token::Op("=".into()))?;
        let expr = self.parse_expr()?;
        Ok(Stmt::Let(name, Box::new(expr)))
    }

    /// Parse `for var in expr { stmts }`.
    fn parse_for(&mut self) -> Result<Stmt, String> {
        let var = match self.next() {
            Token::Name(n) => n,
            t => return Err(format!("expected name after 'for', got {:?}", t)),
        };
        self.expect(Token::Name("in".into()))?;
        let iter = self.parse_expr()?;

        // Parse body: either block or single expression.
        let body = if self.eat(Token::LBrace) {
            let stmts = self.parse_stmts()?;
            self.expect(Token::RBrace)?;
            stmts
        } else {
            // Single expression as body.
            vec![Stmt::Expr(self.parse_expr()?)]
        };

        Ok(Stmt::For(var, Box::new(iter), body))
    }

    /// Parse `if expr { stmts } else { stmts }`.
    fn parse_if(&mut self) -> Result<Stmt, String> {
        let cond = self.parse_expr()?;

        // Then body.
        let then_body = if self.eat(Token::LBrace) {
            let stmts = self.parse_stmts()?;
            self.expect(Token::RBrace)?;
            stmts
        } else {
            vec![Stmt::Expr(self.parse_expr()?)]
        };

        // Else body (optional).
        let else_body = if self.eat(Token::Name("else".into())) {
            if self.eat(Token::LBrace) {
                let stmts = self.parse_stmts()?;
                self.expect(Token::RBrace)?;
                stmts
            } else {
                vec![Stmt::Expr(self.parse_expr()?)]
            }
        } else {
            Vec::new()
        };

        Ok(Stmt::If(Box::new(cond), then_body, else_body))
    }

    /// Parse expression: call / method / binary / unary.
    fn parse_expr(&mut self) -> Result<Expr, String> {
        // Unary operators: `-x`, `!x`.
        let op = match self.peek() {
            Token::Op(op) => Some(op.clone()),
            _ => None,
        };
        if let Some(ref op) = op {
            if *op == "-" || *op == "!" {
                self.next(); // consume operator.
                let expr = self.parse_expr()?;
                return Ok(Expr::UnaryOp(op.clone(), Box::new(expr)));
            }
        }

        // Primary: IDENT or (expr) or string literal.
        let mut primary = match self.next() {
            Token::Name(n) => Expr::Var(n),
            Token::StringLiteral(s) => Expr::Literal(s),
            Token::LParen => {
                let expr = self.parse_expr()?;
                self.expect(Token::RParen)?;
                expr
            }
            t => return Err(format!("expected expression, got {:?}", t)),
        };

        // Method calls: `f.size`, `dir.filter(...)`.
        loop {
            if self.eat(Token::Dot) {
                let method = match self.next() {
                    Token::Name(n) => n,
                    t => return Err(format!("expected method name after '.', got {:?}", t)),
                };

                // Arguments?
                let args = if self.eat(Token::LParen) {
                    let a = self.parse_expr_list()?;
                    a
                } else {
                    Vec::new()
                };

                primary = Expr::MethodCall(
                    Box::new(primary),
                    method,
                    args,
                );
            } else if self.eat(Token::LParen) {
                // Function call: `help()` or `fs().list(path)` (strano ma possibile).
                let args = self.parse_expr_list()?;
                primary = Expr::Call(
                    match primary {
                        Expr::Var(n) => n,
                        _ => return Err("function name expected".into()),
                    },
                    args,
                );
            } else if self.eat(Token::Colon) && self.eat(Token::Colon) {
                // Namespace qualified call: `fs::list(path)` o `fs::read(name)`.
                let method = match self.next() {
                    Token::Name(n) => n,
                    t => return Err(format!("expected method name after '::', got {:?}", t)),
                };

                // Costruisci il nome completo: "fs::list".
                let full_name = primary.as_var().map(|n| format!("{}::{}", n, method))
                    .ok_or_else(|| String::from("expected namespace before '::'"))?;

                // Arguments?
                let args = if self.eat(Token::LParen) {
                    let a = self.parse_expr_list()?;
                    a
                } else {
                    Vec::new()
                };

                primary = Expr::Call(full_name, args);
            } else {
                break;
            }
        }

        // Binary operators (lowest precedence): `+`, `-`, `>`, `<`, `==`.
        // Single pass left-to-right.
        loop {
            let op = match self.peek() {
                Token::Op(op) => Some(op.clone()),
                _ => None,
            };
            if let Some(ref op) = op {
                if matches!(op.as_str(), "+" | "-" | ">" | "<" | "==" | "!=") {
                    self.next(); // consume operator.
                    let right = self.parse_expr()?;
                    primary = Expr::BinaryOp(
                        Box::new(primary),
                        op.clone(),
                        Box::new(right),
                    );
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(primary)
    }

    /// Parse comma-separated expression list inside `(...)`.
    fn parse_expr_list(&mut self) -> Result<Vec<Expr>, String> {
        let mut exprs = Vec::new();
        if !self.eat(Token::RParen) {
            exprs.push(self.parse_expr()?);
            while self.eat(Token::Op(",".into())) {
                exprs.push(self.parse_expr()?);
            }
            self.expect(Token::RParen)?;
        }
        Ok(exprs)
    }

    /// Consuma il token corrente se matches, altrimenti errore.
    fn expect(&mut self, expected: Token) -> Result<(), String> {
        let expected_clone = expected.clone();
        if self.eat(expected) {
            Ok(())
        } else {
            Err(format!("expected {:?}, got {:?}", expected_clone, self.peek()))
        }
    }

    /// Consuma il token corrente se matches.
    fn eat(&mut self, expected: Token) -> bool {
        if self.peek() == &expected {
            self.next();
            true
        } else {
            false
        }
    }

    /// Peek al prossimo token senza consumarlo.
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    /// Consuma e ritorna il prossimo token.
    fn next(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        self.pos += 1;
        token
    }

    /// Verifica se siamo alla fine o su newline.
    fn at_newline(&self) -> bool {
        matches!(self.peek(), Token::Newline)
    }
}
