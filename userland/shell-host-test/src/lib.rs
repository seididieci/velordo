//! Test host per la logica pura della shell Velordo (lexer, parser, runtime puro).
//!
//! Questo crate viene compilato per l'host (Linux) e testa la parte logica
//! dell'interprete REPL senza dipendere da civis o dal kernel.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod runtime_pure;
