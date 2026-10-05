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
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
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
    let mut roots = rustls::RootCertStore::empty();
    match rustls_native_certs::load_native_certs() {
        Ok(certs) => {
            for cert in certs {
                let _ = roots.add(cert);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "rustls-native-certs: load_native_certs failed");
        }
    }
    roots
}
