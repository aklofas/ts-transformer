//! Self-signed cert + key fixture for TLS integration tests.
//!
//! Uses rcgen to generate a fresh keypair per test invocation. The
//! returned PEM paths live in a tempdir that the caller MUST hold for
//! the test's duration (auto-cleanup on drop).
//!
//! Feature-gated via `fixtures/mod.rs` (which only declares
//! `pub mod tls_certs` under `#[cfg(feature = "tls")]`) — no duplicate
//! cfg on this module.

#![allow(dead_code)]

use std::path::PathBuf;

/// Self-signed cert + matching private key, written to disk in PEM form.
///
/// `cert_path` + `key_path` point at files inside a tempdir that lives
/// as long as this struct. Drop this struct after the test completes to
/// clean up.
pub struct SelfSignedCert {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// PEM-encoded cert (the "root") for the client trust store. Same
    /// bytes as the file at `cert_path`; kept in-memory to avoid a
    /// re-read in the test body.
    pub root_pem: String,
    _dir: tempfile::TempDir,
}

impl SelfSignedCert {
    /// Generate a fresh self-signed cert for `localhost` + `127.0.0.1`
    /// SANs. Both SANs are required so the client can connect by either
    /// hostname or IP literal without rustls rejecting on name mismatch.
    pub fn generate() -> Self {
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen self-signed cert generation");
        let cert_pem = cert.cert.pem();
        let key_pem = cert.signing_key.serialize_pem();

        let dir = tempfile::tempdir().expect("create tempdir for TLS fixture");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, &cert_pem).expect("write cert.pem");
        std::fs::write(&key_path, &key_pem).expect("write key.pem");

        Self {
            cert_path,
            key_path,
            root_pem: cert_pem,
            _dir: dir,
        }
    }
}
