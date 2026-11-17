//! `sign` host tool (ADR-0078, fase 2): chiavi e firme delle immagini.
//!
//! Host-side e `std` (come `tools/arca`): genera chiavi, firma i `.bin` che
//! finiscono nel volume, verifica in diagnostica. Il formato del sidecar e
//! il prefisso di dominio vivono in `libs/sig` (single source col guest).
//!
//! Uso:
//!   sign keygen <out.key>                     # 32-byte seed casuale, file 0600
//!   sign pubkey <key.key>                     # "<key_id:016x> <pubkey:64hex>"
//!   sign pubkey-hex <pubkey:64hex>            # "<key_id:016x>" (per l'anchor)
//!   sign digest <file>                        # BLAKE2s-256 hex
//!   sign sign <key.key> <file> <out.sig>      # scrive il sidecar (76 B)
//!   sign verify <key.key|pubkeyhex> <file> <sidecar>
//!
//! Il messaggio firmato e' `VELORDO-IMAGE-V1\0 || bytes`, quindi una firma
//! vale per un'immagine e non per altri formati.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::exit;

use sig::{
    digest, digest_hex, hex_decode, key_id, public_key, sign as sign_image, Sidecar, DIGEST_HEX_LEN,
    SEED_LEN, SIDECAR_LEN,
};

fn die(msg: &str) -> ! {
    eprintln!("[sign] errore: {msg}");
    exit(1);
}

fn usage() -> ! {
    eprintln!("uso: sign keygen <out.key>");
    eprintln!("     sign pubkey <key.key>");
    eprintln!("     sign pubkey-hex <pubkey-hex64>");
    eprintln!("     sign digest <file>");
    eprintln!("     sign sign <key.key> <file> <out.sig>");
    eprintln!("     sign verify <key.key|pubkey-hex64> <file> <sidecar>");
    exit(2);
}

/// Seed da un file chiave: prima riga non vuota e non commento, 64 cifre hex.
fn read_seed(path: &str) -> [u8; SEED_LEN] {
    let text = fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut seed = [0u8; SEED_LEN];
        if !hex_decode(line.as_bytes(), &mut seed) {
            die(&format!("{path}: seed non valido (attese 64 cifre hex)"));
        }
        return seed;
    }
    die(&format!("{path}: nessun seed nel file"));
}

/// Pubkey da un argomento che puo' essere un file `.key` o 64 cifre hex.
fn read_pubkey(arg: &str) -> [u8; 32] {
    if arg.len() == 64 {
        let mut pk = [0u8; 32];
        if hex_decode(arg.as_bytes(), &mut pk) {
            return pk;
        }
    }
    let seed = read_seed(arg);
    public_key(&seed).unwrap_or_else(|| die("derivazione pubkey fallita"))
}

fn keygen(out: &str) {
    let mut seed = [0u8; SEED_LEN];
    let mut f = fs::File::open("/dev/urandom").unwrap_or_else(|e| die(&format!("/dev/urandom: {e}")));
    std::io::Read::read_exact(&mut f, &mut seed).unwrap_or_else(|e| die(&format!("lettura urandom: {e}")));
    if seed.iter().fold(0u8, |a, &b| a | b) == 0 {
        die("seed tutto zero: ritenta");
    }
    let pk = public_key(&seed).unwrap_or_else(|| die("derivazione pubkey fallita"));
    let mut body = String::new();
    body.push_str("# Velordo image-signing key (seed Ed25519). NON committare chiavi di produzione.\n");
    for b in seed {
        body.push_str(&format!("{b:02x}"));
    }
    body.push('\n');
    fs::write(out, body).unwrap_or_else(|e| die(&format!("{out}: {e}")));
    let _ = fs::set_permissions(out, fs::Permissions::from_mode(0o600));
    println!("key_id {:016x} pubkey {}", key_id(&pk), hex_str(&pk));
}

fn hex_str(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn do_digest(path: &str) -> [u8; 32] {
    let data = fs::read(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    digest(&data)
}

fn do_sign(key: &str, file: &str, out: &str) {
    let seed = read_seed(key);
    let pk = public_key(&seed).unwrap_or_else(|| die("derivazione pubkey fallita"));
    let data = fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
    let image_sig = sign_image(&seed, &data).unwrap_or_else(|| die("firma fallita"));
    let sc = Sidecar { key_id: key_id(&pk), sig: image_sig };
    let bytes = sc.encode();
    debug_assert_eq!(bytes.len(), SIDECAR_LEN);
    // Temp + rename: un sidecar a meta' scrittura non deve mai esistere.
    let tmp = format!("{out}.tmp");
    fs::write(&tmp, bytes).unwrap_or_else(|e| die(&format!("{tmp}: {e}")));
    if let Err(e) = fs::rename(&tmp, out) {
        let _ = fs::remove_file(&tmp);
        die(&format!("{out}: {e}"));
    }
    println!("[sign] {file} -> {out} (key_id {:016x}, digest {})", sc.key_id, String::from_utf8_lossy(&digest_hex(&digest(&data))));
}

fn do_verify(key: &str, file: &str, sidecar: &str) {
    let pk = read_pubkey(key);
    let data = fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
    let raw = fs::read(sidecar).unwrap_or_else(|e| die(&format!("{sidecar}: {e}")));
    let sc = Sidecar::parse(&raw).unwrap_or_else(|| die("sidecar malformato"));
    if sc.key_id != key_id(&pk) {
        die(&format!("key_id del sidecar {:016x} != chiave {:016x}", sc.key_id, key_id(&pk)));
    }
    if !sig::verify(&pk, &data, &sc.sig) {
        die("firma NON valida");
    }
    println!("[sign] OK: firma valida ({file})");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match a.as_slice() {
        ["keygen", out] => keygen(out),
        ["pubkey", key] => {
            let seed = read_seed(key);
            let pk = public_key(&seed).unwrap_or_else(|| die("derivazione pubkey fallita"));
            println!("{:016x} {}", key_id(&pk), hex_str(&pk));
        }
        ["pubkey-hex", hexpk] => {
            let mut pk = [0u8; 32];
            if !hex_decode(hexpk.as_bytes(), &mut pk) {
                die("pubkey-hex: attese 64 cifre hex");
            }
            println!("{:016x}", key_id(&pk));
        }
        ["digest", file] => {
            let d = do_digest(file);
            debug_assert_eq!(DIGEST_HEX_LEN, 64);
            println!("{}", String::from_utf8_lossy(&digest_hex(&d)).as_ref());
        }
        ["sign", key, file, out] => do_sign(key, file, out),
        ["verify", key, file, sidecar] => do_verify(key, file, sidecar),
        _ => usage(),
    }
}
