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

use std::io;
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
        let cert_file = std::fs::File::open(cert_pem).map_err(|e| {
            RtspServerError::Tls(format!(
                "failed to open cert file {}: {e}",
                cert_pem.display()
            ))
        })?;
        let mut cert_reader = io::BufReader::new(cert_file);
        let cert_chain: Vec<rustls_pki_types::CertificateDer<'static>> =
            rustls_pemfile::certs(&mut cert_reader)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| RtspServerError::Tls(format!("PEM cert parse failed: {e}")))?;
        if cert_chain.is_empty() {
            return Err(RtspServerError::Tls(format!(
                "no certificates found in {}",
                cert_pem.display()
            )));
        }

        let key_file = std::fs::File::open(key_pem).map_err(|e| {
            RtspServerError::Tls(format!(
                "failed to open key file {}: {e}",
                key_pem.display()
            ))
        })?;
        let mut key_reader = io::BufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut key_reader)
            .map_err(|e| RtspServerError::Tls(format!("PEM key parse failed: {e}")))?
            .ok_or_else(|| {
                RtspServerError::Tls(format!("no private key in {}", key_pem.display()))
            })?;

        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(|e| RtspServerError::Tls(format!("rustls config build failed: {e}")))?;

        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        Ok(Self { acceptor })
    }

    /// Perform the TLS handshake on an accepted TCP connection.
    ///
    /// Returns a [`TokioTlsServerStream`] that implements `AsyncRead +
    /// AsyncWrite`; per-session tasks treat it interchangeably with a
    /// plain [`TcpStream`].
    pub(crate) async fn accept(
        &self,
        tcp: TcpStream,
    ) -> Result<TokioTlsServerStream, RtspServerError> {
        let stream = self
            .acceptor
            .accept(tcp)
            .await
            .map_err(|e| RtspServerError::Tls(format!("TLS handshake failed: {e}")))?;
        Ok(TokioTlsServerStream { inner: stream })
    }
}

/// Server-side TLS stream wrapping `tokio_rustls::server::TlsStream<TcpStream>`.
/// Implements `AsyncRead + AsyncWrite` via the inner type — the per-session
/// task takes this and reads/writes RTSP requests + (for interleaved
/// transport) binary RTP frames over the same TLS session.
pub(crate) struct TokioTlsServerStream {
    pub(crate) inner: tokio_rustls::server::TlsStream<TcpStream>,
}

impl tokio::io::AsyncRead for TokioTlsServerStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for TokioTlsServerStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
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
