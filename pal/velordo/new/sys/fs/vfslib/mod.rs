//! Hub del client FS vendored (S2.2): copie byte-identiche da `libs/vfscli`
//! + `abi.rs` generato + `base.rs` copiato. I file usano `super::...` come
//! nel repo (qui `super` = `vfslib`): stessi path, zero derive.

pub(crate) mod abi;
pub(crate) mod base;
pub(crate) mod cwd;
pub(crate) mod error;
pub(crate) mod ring;
pub(crate) mod session;
pub(crate) mod sync;

pub(crate) use base::{State, Sys};
