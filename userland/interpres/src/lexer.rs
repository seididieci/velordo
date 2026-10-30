//! Tokenizzatore per la sintassi Rust-like (Fase 1, ADR-0065).
//!
//! Converte una stringa grezza in un vettore di token.
//! Supporto: identificatori, numeri, stringhe letterali, operatori, delimitatori, keyword.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

/// Token prodotto dal lexer.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// Identificatore (variabile o nome funzione).
    Ident(String),
    /// Numero intero o float.
    Number(f64),
    /// Stringa letterale tra doppi apici.
    Str(Vec<u8>),
    /// Operatore binario/relazionale/logico.
    Op(String),
    /// Delimitatore: `{ } ( ) [ ] ; , =`.
    Delim(char),
    /// Keyword: `let` `if` `else` `for` `in` `fn` `true` `false` `null`.
    Keyword(String),
    /// Fine file — sentinella.
    Eof,
}

impl Token {
    /// Estrae il contenuto della keyword come `&str`, se presente.
    pub fn as_keyword(&self) -> Option<&str> {
        if let Token::Keyword(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }
}

/// Errore di tokenizzazione con posizione e messaggio descrittivo.
#[derive(Debug)]
pub struct LexError {
    pub pos:   usize,
    pub msg:   String,
}

impl core::fmt::Display for LexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "lexer error at {}: {}", self.pos, self.msg)
    }
}

/// Tokenizza `input` in vettore di token.
/// Ritorna `Ok(Vec<Token>)` o un `LexError` con posizione esatta.
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    let mut pos: usize = 0;

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' => {} // skip whitespace
            '\n' => {
                tokens.push(Token::Delim(';')); // newline → statement separator
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let ident = read_ident(&mut chars, c);
                // Check keyword
                match ident.as_str() {
                    "let"     => tokens.push(Token::Keyword("let".into())),
                    "if"      => tokens.push(Token::Keyword("if".into())),
                    "else"    => tokens.push(Token::Keyword("else".into())),
                    "for"     => tokens.push(Token::Keyword("for".into())),
                    "in"      => tokens.push(Token::Keyword("in".into())),
                    "fn"      => tokens.push(Token::Keyword("fn".into())),
                    "true"    => tokens.push(Token::Keyword("true".into())),
                    "false"   => tokens.push(Token::Keyword("false".into())),
                    "null"    => tokens.push(Token::Keyword("null".into())),
                    _         => tokens.push(Token::Ident(ident)),
                }
            }
            c if c.is_ascii_digit() => {
                let num = read_number(&mut chars, c);
                tokens.push(Token::Number(num));
            }
            '"' => {
                let s = read_string(&mut chars)?;
                tokens.push(Token::Str(s));
            }
            // Operatori composti a 2 char (==, !=, <=, >=, &&, ||)
            '<' => {
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op("<=".into()));
                } else {
                    tokens.push(Token::Op("<".into()));
                }
            }
            '>' => {
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op(">=".into()));
                } else {
                    tokens.push(Token::Op(">".into()));
                }
            }
            '=' => {
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op("==".into()));
                } else {
                    tokens.push(Token::Delim('='));
                }
            }
            '!' => {
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(Token::Op("!=".into()));
                } else {
                    tokens.push(Token::Op("!".into()));
                }
            }
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push(Token::Op("&&".into()));
                } else {
                    return Err(LexError { pos, msg: format!("unexpected '&'",) });
                }
            }
            '|' => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(Token::Op("||".into()));
                } else {
                    return Err(LexError { pos, msg: format!("unexpected '|'",) });
                }
            }
            '+' | '-' | '*' | '/' | '%' => {
                let op = String::from(c);
                tokens.push(Token::Op(op));
            }
            '{' | '}' | '(' | ')' | '[' | ']' | ';' | ',' => {
                tokens.push(Token::Delim(c));
            }
            _ => {
                return Err(LexError { pos, msg: format!("unexpected character '{}'", c) });
            }
        }
        pos += 1;
    }

    tokens.push(Token::Eof);
    Ok(tokens)
}

/// Legge un identificatore (lettere, cifre, underscore).
/// `first` e' il primo carattere gia' consumato dal lexer.
fn read_ident(chars: &mut core::iter::Peekable<core::str::Chars>, first: char) -> String {
    let mut s = String::new();
    s.push(first); // Include il primo carattere perso dal lexer
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
            chars.next();
        } else {
            break;
        }
    }
    s
}

/// Legge un numero (intero o float).
/// `first` e' il primo carattere gia' consumato dal lexer.
fn read_number(chars: &mut core::iter::Peekable<core::str::Chars>, first: char) -> f64 {
    let mut int_part = String::new();
    int_part.push(first); // Include il primo carattere perso dal lexer
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() || c == '.' {
            int_part.push(c);
            chars.next();
        } else {
            break;
        }
    }
    // Parsing sicuro: fallback a 0.0 se fallisce
    int_part.parse::<f64>().unwrap_or(0.0)
}

/// Legge una stringa letterale tra doppi apici (senza escape).
fn read_string(chars: &mut core::iter::Peekable<core::str::Chars>) -> Result<Vec<u8>, LexError> {
    let mut s = Vec::new();
    for c in chars.by_ref() {
        if c == '"' {
            break; // chiusura
        }
        s.push(c as u8);
    }
    Ok(s)
}

