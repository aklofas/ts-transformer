//! Server-side TLS handshake for `rtsps://` listeners.
//!
//! Uses tokio-rustls 0.26 (built on rustls 0.23) for the async handshake.
//! Server-side analog to `crate::rtsp::client::tls`.
//!
//! Feature-gated behind `rtsp-server-tls` — entire module compiles to
//! nothing when the feature is off.
//!
//! Why tokio-rustls (server) vs sync rustls (client): the client is a
//! sync facade, so it drives the rustls state machine
//! manually via `read_tls`/`write_tls`. The server is
//! fundamentally async (tokio Runtime), and native async TLS via
//! `TlsAcceptor::accept` is much cleaner than wiring sync rustls into
//! tokio via `spawn_blocking`.
//!
//! Note: the cfg gate lives on the `pub mod tls;` declaration in
//! `super::mod`; no inner `#![cfg(...)]` here (clippy flags that as a
//! `duplicated_attributes` warning under `-D warnings`).

use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

use crate::error::RtspServerError;

/// Server-side TLS config — wraps a [`TlsAcceptor`] (which itself owns an
/// `Arc<rustls::ServerConfig>`) ready to be applied to accepted TCP
/// connections. Built at server-bind time from cert + key file paths.
#[derive(Clone)]
pub(crate) struct TlsServerConfig {
    pub(crate) acceptor: TlsAcceptor,
}

impl std::fmt::Debug for TlsServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsServerConfig")
            .field("acceptor", &"<rustls::ServerConfig>")
            .finish()
    }
}

impl TlsServerConfig {
    /// Load a cert chain (PEM) + private key (PEM) from disk and build a
    /// rustls [`rustls::ServerConfig`] with no client cert verification
    /// (mTLS is not supported). Returns
    /// [`RtspServerError::Tls`] on any file open / PEM parse / keypair
    /// validation failure.
    pub(crate) fn load(cert_pem: &Path, key_pem: &Path) -> Result<Self, RtspServerError> {
        let config = tst_core::net::tls::load_server_config(cert_pem, key_pem)
            .map_err(|e| RtspServerError::Tls(e.to_string()))?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }

    /// Perform the TLS handshake on an accepted TCP connection.
    ///
    /// Returns the TLS stream; the session splits it like a TCP stream.
    pub(crate) async fn accept(
        &self,
        tcp: TcpStream,
    ) -> Result<tokio_rustls::server::TlsStream<TcpStream>, RtspServerError> {
        let stream = self
            .acceptor
            .accept(tcp)
            .await
            .map_err(|e| RtspServerError::Tls(format!("TLS handshake failed: {e}")))?;
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Generate a self-signed cert + key PEM in a tempdir with rcgen (a
    /// dev-dep), load them via [`TlsServerConfig::load`], and verify the
    /// acceptor config builds.
    #[test]
    fn load_self_signed_cert_succeeds() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("rcgen self-signed cert");
        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

        TlsServerConfig::load(&cert_path, &key_path)
            .expect("loading a valid self-signed cert + key should succeed");
    }

    /// Load with a non-existent cert file — surfaces
    /// [`RtspServerError::Tls`] from the file-open `?`.
    #[test]
    fn load_missing_cert_errors() {
        let dir = tempdir().unwrap();
        let cert = dir.path().join("missing.pem");
        let key = dir.path().join("missing.key");
        let e = TlsServerConfig::load(&cert, &key).unwrap_err();
        assert!(matches!(e, RtspServerError::Tls(_)));
    }

    /// Load with empty cert PEM — surfaces [`RtspServerError::Tls`] via
    /// the "no certificates found" check.
    #[test]
    fn load_empty_cert_errors() {
        let dir = tempdir().unwrap();
        let cert = dir.path().join("empty.pem");
        let key = dir.path().join("empty.key");
        std::fs::write(&cert, b"").unwrap();
        std::fs::write(&key, b"").unwrap();
        let e = TlsServerConfig::load(&cert, &key).unwrap_err();
        assert!(matches!(e, RtspServerError::Tls(_)));
    }
}
