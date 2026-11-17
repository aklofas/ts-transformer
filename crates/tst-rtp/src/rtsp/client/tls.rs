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

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::CONTROL_WRITE_TIMEOUT;
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
    /// (via `rustls-native-certs`, kept for the process by
    /// [`native_roots`] once a load yields a certificate) when `None`.
    ///
    /// The handshake shares the connect budget: it must finish before
    /// `deadline` (`None` = unbounded, the budget overflowed `Instant`),
    /// and one that completes on a read made after it is still timed out.
    /// Each socket read is bounded by `read_timeout` or the time left,
    /// whichever is shorter, and a read that times out is a poll tick that
    /// comes back to the deadline check, not a failure. Each socket write
    /// is bounded by [`CONTROL_WRITE_TIMEOUT`] or the time left, whichever
    /// is shorter, and a write that times out is a tick too. A peer FIN
    /// reaches rustls only as `read_tls` returning 0, so that is an error here
    /// rather than another poll. `ClientConnection::complete_io` is not
    /// used: it loops internally until the handshake completes, so a peer
    /// that drips a byte inside every read timeout would hold it past any
    /// deadline checked between calls. On success the socket's read timeout
    /// is `read_timeout` again and its write timeout
    /// [`CONTROL_WRITE_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// - [`RtspError::Tls`] on any rustls construction, handshake, or
    ///   server-name validation failure, and when the peer closes the
    ///   connection mid-handshake.
    /// - [`RtspError::Io`] with [`io::ErrorKind::TimedOut`] when `deadline`
    ///   passes first, including when the handshake completes on a read
    ///   that ended after it.
    pub fn connect(
        url: &RtspUrl,
        sock: TcpStream,
        root_certs: Option<rustls::RootCertStore>,
        read_timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<Self, RtspError> {
        Self::connect_with_clock(
            url,
            sock,
            root_certs,
            read_timeout,
            deadline,
            &mut Instant::now,
        )
    }

    /// [`Self::connect`] with the clock the deadline is read from.
    fn connect_with_clock(
        url: &RtspUrl,
        sock: TcpStream,
        root_certs: Option<rustls::RootCertStore>,
        read_timeout: Duration,
        deadline: Option<Instant>,
        now: &mut dyn FnMut() -> Instant,
    ) -> Result<Self, RtspError> {
        let roots = root_certs.map_or_else(native_roots, Arc::new);
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|e| RtspError::Tls(format!("invalid server name: {e}")))?;
        let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| RtspError::Tls(format!("rustls construct: {e}")))?;
        let mut sock = sock;
        // Unix reports a socket timeout as WouldBlock, Windows as TimedOut.
        let is_tick = |e: &io::Error| {
            matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            )
        };
        while conn.is_handshaking() {
            // `remaining` is recomputed after every write, so each write and
            // the read below start from a fresh reading of the clock.
            let mut remaining = time_left(deadline, now)?;
            // Flush everything rustls has queued (ClientHello, Finished, …)
            // before polling for the server's answer.
            let mut flushed = true;
            while conn.wants_write() {
                sock.set_write_timeout(Some(write_wait(remaining)))
                    .map_err(|e| RtspError::Io(e.kind()))?;
                match conn.write_tls(&mut sock) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if is_tick(&e) => {
                        flushed = false;
                        break;
                    }
                    Err(e) => return Err(RtspError::Tls(format!("write_tls: {e}"))),
                }
                remaining = time_left(deadline, now)?;
            }
            if !flushed || !conn.wants_read() {
                continue;
            }
            // A zero read timeout is an error, so floor the time left.
            let wait = remaining.map_or(read_timeout, |left| {
                read_timeout.min(left.max(Duration::from_millis(1)))
            });
            sock.set_read_timeout(Some(wait))
                .map_err(|e| RtspError::Io(e.kind()))?;
            match conn.read_tls(&mut sock) {
                Ok(0) => {
                    return Err(RtspError::Tls(
                        "peer closed the connection during the TLS handshake".into(),
                    ));
                }
                Ok(_) => {
                    if let Err(e) = conn.process_new_packets() {
                        // Let the alert rustls queued for the peer go out
                        // (best effort, within the budget) before reporting
                        // the failure.
                        if let Ok(left) = time_left(deadline, now) {
                            let _ = sock.set_write_timeout(Some(write_wait(left)));
                            let _ = conn.write_tls(&mut sock);
                        }
                        return Err(RtspError::Tls(format!("process: {e}")));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted || is_tick(&e) => {}
                Err(e) => return Err(RtspError::Tls(format!("read_tls: {e}"))),
            }
        }
        // The read that completed the handshake may have ended after the
        // deadline.
        time_left(deadline, now)?;
        sock.set_read_timeout(Some(read_timeout))
            .map_err(|e| RtspError::Io(e.kind()))?;
        sock.set_write_timeout(Some(CONTROL_WRITE_TIMEOUT))
            .map_err(|e| RtspError::Io(e.kind()))?;
        Ok(Self { conn, sock })
    }

    /// Set the read timeout of the TCP socket beneath rustls — the only
    /// place a TLS read blocks.
    pub(crate) fn set_read_timeout(&self, t: std::time::Duration) -> std::io::Result<()> {
        self.sock.set_read_timeout(Some(t))
    }

    /// Set the write timeout of the TCP socket beneath rustls, where each
    /// `write_tls` pass blocks.
    pub(crate) fn set_write_timeout(&self, t: std::time::Duration) -> std::io::Result<()> {
        self.sock.set_write_timeout(Some(t))
    }

    /// The rustls connection, with whatever it has already decrypted or
    /// still has to write, and the socket beneath it.
    pub(crate) fn into_parts(self) -> (rustls::ClientConnection, TcpStream) {
        (self.conn, self.sock)
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

/// The time left before `deadline`: `None` without a deadline, and
/// `Io(TimedOut)` once it has passed.
fn time_left(
    deadline: Option<Instant>,
    now: &mut dyn FnMut() -> Instant,
) -> Result<Option<Duration>, RtspError> {
    match deadline {
        Some(d) => match d.checked_duration_since(now()) {
            Some(left) if !left.is_zero() => Ok(Some(left)),
            _ => Err(RtspError::Io(io::ErrorKind::TimedOut)),
        },
        None => Ok(None),
    }
}

/// The write timeout for a handshake write with `left` remaining: at most
/// [`CONTROL_WRITE_TIMEOUT`], floored at 1 ms because a zero timeout is an
/// error.
fn write_wait(left: Option<Duration>) -> Duration {
    left.map_or(CONTROL_WRITE_TIMEOUT, |left| {
        left.min(CONTROL_WRITE_TIMEOUT)
            .max(Duration::from_millis(1))
    })
}

/// The root store built from the OS native trust store. The first load
/// that yields a certificate is kept for the rest of the process:
/// certificates added to the OS store later are not seen. A load that
/// yields none is returned uncached, so the next call loads again.
/// Best-effort: silently skips certs that rustls rejects, which mirrors
/// the rustls-native-certs README pattern.
pub(crate) fn native_roots() -> Arc<rustls::RootCertStore> {
    static ROOTS: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();
    roots_cached(&ROOTS, || {
        roots_from(rustls_native_certs::load_native_certs()).0
    })
}

/// The store kept in `cell`, or a fresh `load` that is kept only if it is
/// not empty: an empty store would fail every later handshake.
fn roots_cached(
    cell: &OnceLock<Arc<rustls::RootCertStore>>,
    load: impl FnOnce() -> rustls::RootCertStore,
) -> Arc<rustls::RootCertStore> {
    if let Some(kept) = cell.get() {
        return kept.clone();
    }
    let roots = Arc::new(load());
    if roots.is_empty() {
        return roots;
    }
    // A concurrent first load may have been kept meanwhile: share that one.
    cell.get_or_init(|| roots).clone()
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
        tracing::warn!(errors, first = %first, "some native certificates failed to load");
    }
    (roots, errors)
}

/// A TLS peer for the client's unit tests.
#[cfg(test)]
pub(crate) mod test_peer {
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// A TLS server on `127.0.0.1` with a fresh self-signed certificate for
    /// that address, returned with a root store that trusts it. It reads the
    /// ClientHello, calls `before_first_write`, completes the handshake and
    /// hands the stream to `then`. A handshake that fails ends the peer
    /// without calling `then`. Plaintext `before_first_write` gives the
    /// connection's writer goes out right after the server's Finished, in
    /// the same flight (TLS 1.3 half-RTT data). The peer sends no session
    /// tickets, so nothing reaches the client after the handshake unless
    /// `then` writes it.
    pub(crate) fn tls_test_peer(
        then: impl FnOnce(rustls::StreamOwned<rustls::ServerConnection, TcpStream>) + Send + 'static,
        before_first_write: impl FnOnce(&mut rustls::ServerConnection) + Send + 'static,
    ) -> (u16, rustls::RootCertStore, JoinHandle<()>) {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)
            .unwrap();
        config.send_half_rtt_data = true;
        config.send_tls13_tickets = 0;
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
            let Ok(mut conn) = rustls::ServerConnection::new(config) else {
                return;
            };
            // The server's first flight is queued once the ClientHello has
            // been processed.
            while !conn.wants_write() {
                match conn.read_tls(&mut sock) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if conn.process_new_packets().is_err() {
                    return;
                }
            }
            before_first_write(&mut conn);
            while conn.is_handshaking() {
                if conn.complete_io(&mut sock).is_err() {
                    return;
                }
            }
            then(rustls::StreamOwned::new(conn, sock));
        });
        (port, roots, peer)
    }
}

#[cfg(test)]
mod tests {
    use super::test_peer::tls_test_peer;
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(target_os = "linux")]
    use tst_test_helpers::timing::scaled;

    fn url(port: u16) -> RtspUrl {
        RtspUrl::parse(&format!("rtsps://127.0.0.1:{port}/x")).unwrap()
    }

    /// A ClientHello write parked on a full send buffer ends at the connect
    /// deadline, not at the socket's 5 s write timeout. Linux only, like the
    /// fill recipe.
    #[cfg(target_os = "linux")]
    #[test]
    fn handshake_write_is_bounded_by_the_connect_deadline() {
        use crate::rtsp::client::test_sockets::{fill_send_buffer, non_reading_peer};
        let (port, done_tx, peer) = non_reading_peer();
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        fill_send_buffer(&sock);
        let t0 = Instant::now();
        let r = TlsStream::connect(
            &url(port),
            sock,
            Some(rustls::RootCertStore::empty()),
            Duration::from_millis(100),
            Some(t0 + Duration::from_millis(300)),
        );
        let elapsed = t0.elapsed();
        let _ = done_tx.send(());
        peer.join().unwrap();
        assert!(
            matches!(r, Err(RtspError::Io(io::ErrorKind::TimedOut))),
            "{r:?}"
        );
        assert!(elapsed < scaled(Duration::from_secs(2)), "{elapsed:?}");
    }

    /// A handshake whose last server flight is read after the deadline has
    /// passed is timed out, not returned as a success.
    #[test]
    fn a_handshake_completing_after_the_deadline_is_timed_out() {
        let late = Arc::new(AtomicBool::new(false));
        let set_late = late.clone();
        let (port, roots, peer) =
            tls_test_peer(drop, move |_| set_late.store(true, Ordering::SeqCst));
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let t0 = Instant::now();
        let mut clock = || {
            if late.load(Ordering::SeqCst) {
                t0 + Duration::from_secs(3600)
            } else {
                t0
            }
        };
        let r = TlsStream::connect_with_clock(
            &url(port),
            sock,
            Some(roots),
            Duration::from_millis(100),
            Some(t0 + Duration::from_secs(10)),
            &mut clock,
        );
        assert!(
            matches!(r, Err(RtspError::Io(io::ErrorKind::TimedOut))),
            "{r:?}"
        );
        drop(r);
        peer.join().unwrap();
    }

    fn one_cert_store() -> rustls::RootCertStore {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        roots
    }

    /// A non-empty load is kept: a later call shares it without loading.
    /// Asserted on a local cell with a non-empty loader, because the
    /// machine's own store may be empty, and an empty load is not kept.
    #[test]
    fn native_roots_are_loaded_once() {
        let cell = OnceLock::new();
        let first = roots_cached(&cell, one_cert_store);
        let second = roots_cached(&cell, || panic!("loaded twice"));
        assert!(Arc::ptr_eq(&first, &second));
    }

    /// A load that yields no certificate is not kept: the next call loads
    /// again, and its non-empty result is the one kept.
    #[test]
    fn an_empty_root_load_is_not_cached() {
        let cell = OnceLock::new();
        assert!(roots_cached(&cell, rustls::RootCertStore::empty).is_empty());
        let second = roots_cached(&cell, one_cert_store);
        assert_eq!(second.len(), 1);
        let third = roots_cached(&cell, || panic!("loaded after a kept load"));
        assert!(Arc::ptr_eq(&second, &third));
    }

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
