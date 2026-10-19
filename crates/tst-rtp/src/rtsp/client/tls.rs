//! `rtsps://` integration via sync rustls 0.23 (no tokio-rustls).
//!
//! Gated on cargo feature `tls`. The wrapper hides the rustls handshake
//! behind a [`TlsStream`] that implements `std::io::{Read, Write}` — the
//! rest of [`super::RtspClient`] talks to the connection through the
//! internal `Stream` enum (crate-private), so the per-method code stays
//! oblivious to whether the bytes go through plain TCP or TLS.
//!
//! Why sync rustls (not tokio-rustls): the RTSP client is sync by design
//! (std::thread + std::net). rustls 0.23 supports this directly via the
//! `read_tls`/`write_tls` + `process_new_packets` low-level API.

#![cfg(feature = "tls")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use crate::error::RtspError;
use crate::url::RtspUrl;

/// Wraps a [`TcpStream`] with rustls. Implements `Read + Write` so it
/// substitutes for plain `TcpStream` in the crate-internal `Stream`
/// enum (the `Tls` variant).
pub struct TlsStream {
    conn: rustls::ClientConnection,
    sock: TcpStream,
}

impl std::fmt::Debug for TlsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsStream")
            .field("peer", &self.sock.peer_addr().ok())
            .field("is_handshaking", &self.conn.is_handshaking())
            .finish()
    }
}

impl TlsStream {
    /// Wrap `sock` in rustls and drive the handshake to completion
    /// synchronously. `root_certs` defaults to the OS native trust store
    /// (via `rustls-native-certs`) when `None`.
    ///
    /// # Errors
    ///
    /// - [`RtspError::Tls`] on any rustls construction, handshake, or
    ///   server-name validation failure.
    pub fn connect(
        url: &RtspUrl,
        sock: TcpStream,
        root_certs: Option<rustls::RootCertStore>,
    ) -> Result<Self, RtspError> {
        let roots = root_certs.unwrap_or_else(default_roots);
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|e| RtspError::Tls(format!("invalid server name: {e}")))?;
        let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| RtspError::Tls(format!("rustls construct: {e}")))?;
        // Drive the handshake to completion synchronously.
        let mut sock = sock;
        while conn.is_handshaking() {
            // Order matters: flush any pending handshake bytes first,
            // then poll for the server's response. write_tls/read_tls
            // are no-ops when nothing is pending.
            if conn.wants_write() {
                conn.write_tls(&mut sock)
                    .map_err(|e| RtspError::Tls(format!("write_tls: {e}")))?;
            }
            if conn.wants_read() {
                conn.read_tls(&mut sock)
                    .map_err(|e| RtspError::Tls(format!("read_tls: {e}")))?;
                conn.process_new_packets()
                    .map_err(|e| RtspError::Tls(format!("process: {e}")))?;
            }
        }
        Ok(Self { conn, sock })
    }

    /// Set the read timeout of the TCP socket beneath rustls — the only
    /// place a TLS read blocks.
    pub(crate) fn set_read_timeout(&self, t: std::time::Duration) -> std::io::Result<()> {
        self.sock.set_read_timeout(Some(t))
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Pull any pending ciphertext from the wire before handing
        // plaintext up — post-handshake `wants_read()` toggles as TLS
        // records arrive out-of-order with application reads.
        //
        // At most ONE socket read per call: the socket read timeout
        // bounds a single `read_tls`, so a call that kept pulling until a
        // whole record decrypted could be held past the caller's
        // cancel/deadline check by a peer that dribbles a record a few
        // bytes at a time. After one pull with still no plaintext, report
        // `WouldBlock` — every caller treats it as "poll again".
        let mut pulled = false;
        loop {
            // Try to satisfy from the rustls buffered plaintext first;
            // this lets short application reads succeed without a
            // syscall when a record was already decrypted.
            match self.conn.reader().read(buf) {
                Ok(0) if self.conn.wants_read() => {
                    // No plaintext + wants more ciphertext: pull from
                    // the underlying socket and try again.
                }
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !self.conn.wants_read() {
                        return Err(e);
                    }
                    // wants more ciphertext; fall through to read_tls
                }
                Err(e) => return Err(e),
            }
            if pulled {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            // Bring more ciphertext in. read_tls forwards the underlying
            // socket's error (incl. WouldBlock / TimedOut), so callers
            // get the same cancel-loop behavior they get with plain TCP.
            self.conn.read_tls(&mut self.sock)?;
            self.conn
                .process_new_packets()
                .map_err(std::io::Error::other)?;
            pulled = true;
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.conn.writer().write(buf)?;
        // Flush the freshly-buffered plaintext to the wire — RTSP
        // requests are write-then-wait-for-response, so we MUST push
        // bytes out before the caller starts a blocking read.
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.sock)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.sock)?;
        }
        self.sock.flush()
    }
}

/// Build a [`rustls::RootCertStore`] from the OS native trust store.
/// Best-effort: silently skips certs that rustls rejects, which mirrors
/// the rustls-native-certs README pattern.
fn default_roots() -> rustls::RootCertStore {
    roots_from(rustls_native_certs::load_native_certs()).0
}

/// The root store built from the OS trust store's load result, with the
/// number of load errors. Load errors are logged and counted, and every
/// certificate that did load is kept; with none, the store is empty, as a
/// failed load gave before rustls-native-certs 0.8 reported partial results.
fn roots_from(native: rustls_native_certs::CertificateResult) -> (rustls::RootCertStore, usize) {
    let mut roots = rustls::RootCertStore::empty();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    let errors = native.errors.len();
    if let Some(first) = native.errors.first() {
        tracing::warn!(errors, error = %first, "rustls-native-certs: load_native_certs failed");
    }
    (roots, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_error() -> rustls_native_certs::Error {
        rustls_native_certs::Error {
            context: "test",
            kind: rustls_native_certs::ErrorKind::Io {
                inner: std::io::Error::other("unreadable"),
                path: "/nonexistent/cert.pem".into(),
            },
        }
    }

    #[test]
    fn native_roots_partial_load_counts_errors() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut partial = rustls_native_certs::CertificateResult::default();
        partial.certs.push(cert.cert.der().clone());
        partial.errors.push(load_error());
        let (roots, errors) = roots_from(partial);
        assert_eq!(roots.len(), 1);
        assert_eq!(errors, 1);

        // No certificate at all: an empty store with a warning, as a failed
        // load gave before.
        let mut failed = rustls_native_certs::CertificateResult::default();
        failed.errors.push(load_error());
        let (roots, errors) = roots_from(failed);
        assert!(roots.is_empty());
        assert_eq!(errors, 1);
    }

    /// The stream wrapper moves to the keepalive and pump threads, so it
    /// must stay `Send`.
    #[test]
    fn tls_stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<TlsStream>();
    }
}
