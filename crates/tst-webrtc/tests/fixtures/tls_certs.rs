//! A self-signed certificate and key for the `wheps://` tests, generated
//! per call with rcgen and written under the temp directory. Hold the
//! struct for the test's duration; the files go when it drops.

use std::path::PathBuf;

pub struct SelfSignedCert {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// The certificate PEM, for the client's trust store.
    pub root_pem: String,
    _dir: tempfile::TempDir,
}

impl SelfSignedCert {
    /// A certificate for `localhost` and `127.0.0.1`.
    pub fn generate() -> Self {
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("generate a self-signed certificate");
        let cert_pem = cert.cert.pem();
        let dir = tempfile::tempdir().expect("create a temp directory");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, &cert_pem).expect("write cert.pem");
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key.pem");
        Self {
            cert_path,
            key_path,
            root_pem: cert_pem,
            _dir: dir,
        }
    }

    /// The `wheps://` URL on an ephemeral loopback port with this pair.
    pub fn url(&self, extra: &str) -> String {
        format!(
            "wheps://127.0.0.1:0?cert={}&key={}{extra}",
            self.cert_path.display(),
            self.key_path.display()
        )
    }
}
