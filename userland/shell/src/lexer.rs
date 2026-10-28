//! Tokenizzatore per l'interprete REPL (S3.2b).
//!
//! Produce una lista di token dalla stringa di input:
//! - `NAME`: identificatore o nome comando (`let`, `for`, `fs`, `users`, ...)
//! - `LPAREN`/`RPAREN`: parentesi tonde `(`, `)`
//! - `LBRACE`/`RBRACE`: parentesi graffe `{`, `}`
//! - `DOT`: punto `.` (per metodi: `f.size`)
//! - `COLON`: due punti `:` (per namespace: `fs::list`)
//! - `SEMI`: punto e virgola `;` (separatore opzionale)
//! - `NEWLINE`: fine istruzione
//! - `EQ`/`GT`/`LT`/`PLUS`/`MINUS`/`BANG`/`STAR`/`SLASH`: operatori
//! - `StringLiteral`: stringa tra doppi apici `"..."`

use alloc::string::String;
use alloc::vec::Vec;

/// Token prodotto dal lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Identificatore o parola chiave (`let`, `for`, `if`, `else`, `fs`, ...).
    Name(String),
    /// Stringa letterale tra doppi apici.
    StringLiteral(String),
    /// Parentesi tonde.
    LParen,
    RParen,
    /// Parentesi graffe.
    LBrace,
    RBrace,
    /// Punto per metodi (`f.size`).
    Dot,
    /// Due punti per namespace (`fs::list`).
    Colon,
    /// Separatore di istruzioni.
    Semi,
    /// Fine riga (implicito).
    Newline,
    /// Operatore: `=`, `>`, `<`, `+`, `-`, `!`, `*`, `/`.
    Op(String),
}

/// Tokenizza la stringa di input in una lista di token.
pub fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            '{' => tokens.push(Token::LBrace),
            '}' => tokens.push(Token::RBrace),
            '.' => tokens.push(Token::Dot),
            ';' => tokens.push(Token::Semi),
            '=' => tokens.push(Token::Op("=".into())),
            '>' => tokens.push(Token::Op(">".into())),
            '<' => tokens.push(Token::Op("<".into())),
            '+' => tokens.push(Token::Op("+".into())),
            '-' => tokens.push(Token::Op("-".into())),
            '!' => tokens.push(Token::Op("!".into())),
            '*' => tokens.push(Token::Op("*".into())),
            '/' => tokens.push(Token::Op("/".into())),
            ':' => tokens.push(Token::Colon),
            '"' => {
                // String literal: accumula caratteri fino alla prossima ".
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break, // fine stringa.
                        Some(c) => s.push(c),
                        None => break, // senza closing quote: prende tutto quanto.
                    }
                }
                tokens.push(Token::StringLiteral(s));
            }
            c if c.is_whitespace() => {
                // Ignora spazi e tab (ma non newline — gestiti da read_line)
                continue;
            }
            c if c.is_alphanumeric() || c == '_' => {
                // Identificatore: accumula caratteri alfanumerici.
                let mut name = String::new();
                name.push(c);
                while let Some(&next) = chars.peek() {
                    if next.is_alphanumeric() || next == '_' {
                        name.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Name(name));
            }
            c if c.is_ascii_digit() => {
                // Numero intero: accumula cifre.
                let mut num = String::new();
                num.push(c);
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_digit() {
                        num.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Name(num)); // Trattato come Name (Int nel parser)
            }
            c => {
                // Carattere non riconosciuto: ignora (potrebbe essere parte di stringhe).
            }
        }
    }

    tokens.push(Token::Newline);
    tokens
}

/// Verifica se un token e' una parola chiave.
pub fn is_keyword(name: &str) -> bool {
    matches!(name, "let" | "for" | "if" | "else" | "in")
}
