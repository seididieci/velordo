//! F2a: sorgente compilato dal rustc nativo (S2.5f/F2).
//!
//! Volutamente piccolo ma non banale: usa `alloc` (Vec) e `std` (println)
//! cosi' la compilazione attraversa monomorfizzazione + metadata del
//! sysroot, non solo parsing. Non viene linkato in F2a (`--emit=obj`):
//! serve a provare che il compilatore produce codice, non solo usage.
#![feature(restricted_std)]

fn main() {
    let v: Vec<u32> = (0..4).map(|i| i * i).collect();
    println!("[f2] vec sum={}", v.iter().sum::<u32>());
}
