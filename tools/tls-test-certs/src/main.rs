//! Genera i certificati del test TLS 1.3 (N6.2, ADR-0072).
//!
//! Produce in `<outdir>`:
//! - `ca.der` / `ca.pem`: la CA di test (self-signed, RSA-2048). Il DER e'
//!   la trust anchor che il guest riceve in `sys:ca.der`.
//! - `server.pem` + `server.key`: il leaf (RSA-2048, SAN `DNS:<sni>`,
//!   EKU serverAuth) firmato dalla CA, nel formato che `ssl.SSLContext`
//!   di Python legge (`load_cert_chain`).
//!
//! RSA e non ECDSA di proposito: e' la forma delle catene reali (ISRG Root
//! X1 e' RSA-4096) e prova la feature `rsa` di `embedded-tls`, sia sulla
//! catena sia sulla firma del `CertificateVerify` dell'handshake.
//!
//! Uso: `tls-test-certs <outdir> [sni]` (default `tls.test`).

use std::process::ExitCode;
use std::time::Duration;

use const_oid::db::rfc5280::ID_KP_SERVER_AUTH;
use der::asn1::Ia5String;
use der::{Encode, EncodePem};
use rsa::pkcs1v15::{Signature, SigningKey};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::signature::Keypair;
use rsa::RsaPrivateKey;
use sha2::Sha256;
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{ExtendedKeyUsage, SubjectAltName};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::Validity;

/// Validita' dei certificati di test: 30 giorni (non serve di piu').
const VALIDITY: Duration = Duration::from_secs(30 * 24 * 3600);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[tls-test-certs] errore: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| ".".into());
    let sni = args.next().unwrap_or_else(|| "tls.test".into());
    let dir = std::path::Path::new(&out);
    std::fs::create_dir_all(dir)?;

    let mut rng = rand::thread_rng();

    // ── CA (self-signed, RSA-2048) ────────────────────────────────────
    let ca_key = RsaPrivateKey::new(&mut rng, 2048)?;
    let ca_signer = SigningKey::<Sha256>::new(ca_key);
    let ca_name: Name = "CN=Velordo Test CA".parse()?;
    let ca_spki = spki::SubjectPublicKeyInfoOwned::from_key(ca_signer.verifying_key())?;
    let ca = CertificateBuilder::new(
        Profile::Root,
        SerialNumber::from(1u64),
        Validity::from_now(VALIDITY)?,
        ca_name.clone(),
        ca_spki,
        &ca_signer,
    )?
    .build::<Signature>()?;

    // ── Leaf (firmato dalla CA) ───────────────────────────────────────
    let leaf_key = RsaPrivateKey::new(&mut rng, 2048)?;
    let leaf_signer = SigningKey::<Sha256>::new(leaf_key.clone());
    let leaf_name: Name = format!("CN={sni}").parse()?;
    let leaf_spki = spki::SubjectPublicKeyInfoOwned::from_key(leaf_signer.verifying_key())?;
    let mut builder = CertificateBuilder::new(
        Profile::Leaf {
            issuer: ca_name,
            enable_key_agreement: false,
            enable_key_encipherment: true,
        },
        SerialNumber::from(2u64),
        Validity::from_now(VALIDITY)?,
        leaf_name,
        leaf_spki,
        &ca_signer,
    )?;
    builder.add_extension(&SubjectAltName(vec![GeneralName::DnsName(
        Ia5String::new(&sni)?,
    )]))?;
    builder.add_extension(&ExtendedKeyUsage(vec![ID_KP_SERVER_AUTH]))?;
    let leaf = builder.build::<Signature>()?;

    // ── Scrittura ─────────────────────────────────────────────────────
    std::fs::write(dir.join("ca.der"), ca.to_der()?)?;
    std::fs::write(dir.join("ca.pem"), ca.to_pem(LineEnding::LF)?)?;
    std::fs::write(dir.join("server.pem"), leaf.to_pem(LineEnding::LF)?)?;
    std::fs::write(
        dir.join("server.key"),
        leaf_key.to_pkcs8_pem(LineEnding::LF)?.as_bytes(),
    )?;

    println!(
        "[tls-test-certs] CA + leaf RSA-2048 per sni={sni} in {}",
        dir.display()
    );
    Ok(())
}
