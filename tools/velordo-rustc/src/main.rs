//! rustc nativo per Velordo — staticone (S2.5b).
//!
//! Bin crate minima sopra `rustc_driver_impl` (stessa nightly pinnata,
//! sorgenti in `../../rust-src`) + backend Cranelift linkato statico
//! (niente `.so`, niente dlopen): `Callbacks::config` imposta
//! `make_codegen_backend` al costruttore statico.
//!
//! `main` gira su un thread da 64M (stack main 16K insufficiente per rustc;
//! niente lavoro kernel: gli stack thread sono sized dal chiamante).
//! Exit code onesto senza abort: i callback fermano alla prima fase con
//! errori (`Compilation::Stop`) e alzano flag (con `panic=abort` un fatal
//! dopo l'analisi abortirebbe 134 senza messaggio utile).
//! `RUSTC_BOOTSTRAP=1` obbligatorio (rustc_private).

#![feature(rustc_private)]

extern crate rustc_codegen_cranelift;
extern crate rustc_driver_impl;
extern crate rustc_interface;

use rustc_driver_impl::{Callbacks, Compilation};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

struct VelordoCallbacks {
    failed: AtomicBool,
}

impl Callbacks for VelordoCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        config.make_codegen_backend =
            Some(Box::new(|_sess| rustc_codegen_cranelift::__rustc_codegen_backend()));
    }

    fn after_crate_root_parsing(
        &mut self,
        compiler: &rustc_interface::interface::Compiler,
        _krate: &mut rustc_ast::ast::Crate,
    ) -> Compilation {
        self.check(compiler.sess.dcx().has_errors().is_some())
    }

    fn after_expansion<'tcx>(
        &mut self,
        compiler: &rustc_interface::interface::Compiler,
        tcx: rustc_middle::ty::TyCtxt<'tcx>,
    ) -> Compilation {
        let _ = compiler;
        self.check(tcx.dcx().has_errors().is_some())
    }

    fn after_analysis<'tcx>(
        &mut self,
        compiler: &rustc_interface::interface::Compiler,
        tcx: rustc_middle::ty::TyCtxt<'tcx>,
    ) -> Compilation {
        let _ = compiler;
        self.check(tcx.dcx().has_errors().is_some())
    }
}

impl VelordoCallbacks {
    fn check(&self, failed: bool) -> Compilation {
        if failed {
            self.failed.store(true, Ordering::Relaxed);
            Compilation::Stop
        } else {
            Compilation::Continue
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Thread 64M per il driver (rustc vuole MB di stack; vedi sopra).
    let child = std::thread::Builder::new()
        .name("velordo-rustc-driver".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let mut callbacks = VelordoCallbacks { failed: AtomicBool::new(false) };
            rustc_driver_impl::run_compiler(&args, &mut callbacks);
            if callbacks.failed.load(Ordering::Relaxed) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        });
    match child {
        Ok(h) => h.join().unwrap_or(ExitCode::FAILURE),
        Err(_) => ExitCode::FAILURE,
    }
}
