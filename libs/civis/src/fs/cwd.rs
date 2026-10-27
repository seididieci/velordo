//! Wrapper concreti sopra `vfscli::cwd` (S2.2): stessa API di prima,
//! delega con lo stato del processo. Generati meccanicamente: non modificare
//! a mano (fonte in `libs/vfscli/src/cwd.rs`).

use crate::Error;
use alloc::borrow::Cow;
use alloc::vec::Vec;


pub fn getcwd() -> Result<Vec<u8>, Error> {
    vfscli::cwd::getcwd::<super::CivisSys>()
}


pub fn resolve(path: &str) -> Result<Cow<[u8]>, Error> {
    vfscli::cwd::resolve::<super::CivisSys>(path)
}


pub fn chdir(path: &str) -> Result<(), Error> {
    vfscli::cwd::chdir(&super::FSTATE, path)
}
