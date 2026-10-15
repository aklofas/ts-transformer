//! PEM loading for the rustls-based transports — `tcps://` (tst-tcp),
//! `hlss://` (tst-hls) and `rtsps://` (tst-rtp), and the bindings' trust
//! bundles. One loader, one error type; each caller maps [`TlsLoadError`]
//! into its own error. Behind the `tls` feature.

use std::path::Path;

/// Why a certificate, key or trust bundle could not be loaded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsLoadError {
    /// A PEM file could not be read.
    #[error("read {path}: {source}")]
    Read {
        /// The file as given.
        path: String,
        /// The read error.
        #[source]
        source: std::io::Error,
    },
    /// A PEM block did not parse.
    #[error("invalid PEM in {what}: {source}")]
    Pem {
        /// Which input (a path, or "the certificate PEM").
        what: String,
        /// The parse error.
        #[source]
        source: std::io::Error,
    },
    /// The input holds no certificate.
    #[error("no certificates in {what}")]
    NoCertificates {
        /// Which input.
        what: String,
    },
    /// The input holds no private key.
    #[error("no private key in {what}")]
    NoPrivateKey {
        /// Which input.
        what: String,
    },
    /// rustls refused a certificate as a trust anchor.
    #[error("certificate rejected as trust anchor: {0}")]
    TrustAnchor(rustls::Error),
    /// rustls refused the certificate chain and key as a server identity.
    #[error("ServerConfig build: {0}")]
    Config(rustls::Error),
}

/// Read a PEM certificate chain (leaf first) and a PEM private key (PKCS#8,
/// PKCS#1 or SEC1) from files and build a server config without client
/// authentication.
pub fn load_server_config(cert: &Path, key: &Path) -> Result<rustls::ServerConfig, TlsLoadError> {
    let read = |p: &Path| {
        std::fs::read(p).map_err(|source| TlsLoadError::Read {
            path: p.display().to_string(),
            source,
        })
    };
    let (cert_pem, key_pem) = (read(cert)?, read(key)?);
    build_server_config(
        &cert_pem,
        &cert.display().to_string(),
        &key_pem,
        &key.display().to_string(),
    )
}

/// [`load_server_config`] for PEM already in memory (the C binding's
/// `tst_rtsp_server_builder_tls_cert_pem` takes bytes).
pub fn server_config_from_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<rustls::ServerConfig, TlsLoadError> {
    build_server_config(cert_pem, "the certificate PEM", key_pem, "the key PEM")
}

fn build_server_config(
    cert_pem: &[u8],
    cert_what: &str,
    key_pem: &[u8],
    key_what: &str,
) -> Result<rustls::ServerConfig, TlsLoadError> {
    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsLoadError::Pem {
            what: cert_what.into(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsLoadError::NoCertificates {
            what: cert_what.into(),
        });
    }
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .map_err(|source| TlsLoadError::Pem {
            what: key_what.into(),
            source,
        })?
        .ok_or_else(|| TlsLoadError::NoPrivateKey {
            what: key_what.into(),
        })?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(TlsLoadError::Config)
}

/// Parse a PEM bundle of trust anchors (for verifying a private CA's server
/// certificate). A bundle with no certificate is an error.
pub fn load_root_store(pem: &[u8]) -> Result<rustls::RootCertStore, TlsLoadError> {
    let what = "the trust bundle";
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut &pem[..]) {
        let cert = cert.map_err(|source| TlsLoadError::Pem {
            what: what.into(),
            source,
        })?;
        roots.add(cert).map_err(TlsLoadError::TrustAnchor)?;
    }
    if roots.is_empty() {
        return Err(TlsLoadError::NoCertificates { what: what.into() });
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed() -> (String, String) {
        let c = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        (c.cert.pem(), c.key_pair.serialize_pem())
    }

    #[test]
    fn a_self_signed_pair_builds_a_server_config_from_bytes_and_files() {
        let (cert, key) = self_signed();
        server_config_from_pem(cert.as_bytes(), key.as_bytes()).expect("from bytes");
        let dir = tempfile::tempdir().unwrap();
        let (cp, kp) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        std::fs::write(&cp, &cert).unwrap();
        std::fs::write(&kp, &key).unwrap();
        load_server_config(&cp, &kp).expect("from files");
    }

    #[test]
    fn each_failure_has_its_own_variant() {
        let (cert, key) = self_signed();
        let missing = std::path::Path::new("/nonexistent/tst-core-tls-test.pem");
        assert!(matches!(
            load_server_config(missing, missing),
            Err(TlsLoadError::Read { .. })
        ));
        assert!(matches!(
            server_config_from_pem(b"", key.as_bytes()),
            Err(TlsLoadError::NoCertificates { .. })
        ));
        assert!(matches!(
            server_config_from_pem(cert.as_bytes(), b""),
            Err(TlsLoadError::NoPrivateKey { .. })
        ));
        let garbage = b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
        assert!(matches!(
            load_root_store(garbage),
            Err(TlsLoadError::Pem { .. })
        ));
        assert!(matches!(
            load_root_store(b""),
            Err(TlsLoadError::NoCertificates { .. })
        ));
        assert_eq!(load_root_store(cert.as_bytes()).unwrap().len(), 1);
    }
}
