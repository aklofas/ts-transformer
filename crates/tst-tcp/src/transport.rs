//! [`TcpTransport`] — TCP transport implementing both Transport + RecvTransport.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::transport::{RecvTransport, SocketStats, Transport, TransportCancel, TransportError};

use crate::config::SocketConfig;
use crate::error::TcpError;
use crate::recv_knobs::apply_knobs;
use crate::stats::TcpStats;
use crate::url::TcpUrl;

/// Inner stream — Plain for `tcp://`, Tls for `tcps://`.
///
/// The TLS variant is boxed because `rustls::ClientConnection` /
/// `ServerConnection` carry several KB of session state; without the Box,
/// every `TcpTransport` would pay the TLS-sized footprint regardless of which
/// variant is active (clippy `large_enum_variant`).
pub(crate) enum InnerStream {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<crate::tls::TlsStream>),
}

impl InnerStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.read(buf),
        }
    }
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.write(buf),
        }
    }
    fn shutdown(&mut self) {
        match self {
            Self::Plain(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            #[cfg(feature = "tls")]
            Self::Tls(_) => {
                // TLS shutdown handled in TlsStream's StreamOwned/socket drop.
            }
        }
    }
}

/// Cheap cloneable handle that unblocks a parked [`TcpTransport::recv_bytes`]
/// from another thread.
///
/// Obtained via [`TcpTransport::cancel_handle`]. Cancelling does **not** shut
/// down the underlying socket — [`tst_core::transport::Transport::close`] still
/// does. Cancellation is cooperative:
///
/// - a parked `recv_bytes` observes the flag at its next poll boundary
///   (~100 ms) and returns [`tst_core::transport::TransportError::Closed`];
/// - calls started after `cancel()` return `Closed` on their entry check;
/// - a call already past its entry check may still complete its current I/O
///   first (a recv that receives data in that window returns it; a send
///   finishes its bounded ≤~100 ms write attempt) — the *next* call then
///   returns `Closed`.
///
/// `TcpCancelHandle` is `Clone + Send + Sync`; multiple holders can race
/// `cancel()` safely (the flag is an `Arc<AtomicBool>`, idempotent).
#[derive(Clone, Debug)]
pub struct TcpCancelHandle {
    alive: Arc<AtomicBool>,
}

impl TcpCancelHandle {
    /// Signal any parked `recv_bytes` (or subsequent `send_bytes`/`recv_bytes`)
    /// to return [`tst_core::transport::TransportError::Closed`] at its next
    /// ~100 ms poll boundary. Idempotent — repeated calls are a no-op.
    pub fn cancel(&self) {
        self.alive.store(false, Ordering::Release);
    }

    /// `true` if [`Self::cancel`] has been called on any clone of this handle.
    pub fn is_cancelled(&self) -> bool {
        !self.alive.load(Ordering::Acquire)
    }
}

impl TransportCancel for TcpCancelHandle {
    fn cancel(&self) {
        TcpCancelHandle::cancel(self)
    }
}

/// TCP transport. Implements both Transport (sender) and RecvTransport (receiver).
///
/// Build via [`TcpTransport::connect`] (caller) or via
/// [`crate::listener::TcpListener::accept_blocking`] (server-side).
pub struct TcpTransport {
    pub(crate) inner: InnerStream,
    pub(crate) pkt_size: usize,
    pub(crate) peer: SocketAddr,
    pub(crate) stats: TcpStats,
    pub(crate) alive: Arc<AtomicBool>,
}

/// Resolve `host:port` (IP literal or DNS name) and connect with `timeout`
/// applied per candidate address, returning the stream + the address that
/// accepted. DA-NET-9: hostnames resolve here, never at URL-parse time.
pub(crate) fn connect_stream(
    host: &str,
    port: u16,
    timeout: std::time::Duration,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    use std::net::ToSocketAddrs;
    let mut last_err = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok((s, addr)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no addresses resolved for {host}:{port}"),
        )
    }))
}

impl TcpTransport {
    /// Build a caller-side `TcpTransport` from a URL (TLS automatically
    /// applied for `tcps://`).
    pub fn connect(url: &str) -> Result<Self, TcpError> {
        let url = TcpUrl::parse(url)?;
        if url.listen {
            return Err(TcpError::InvalidConfig(
                "URL has ?listen=1 — use TcpListener::bind".into(),
            ));
        }
        let mut cfg = SocketConfig::default();
        cfg.merge_from_url(&url);
        Self::connect_with_config(&url, &cfg)
    }

    /// Build a caller-side `TcpTransport` from an already-parsed URL + config.
    pub fn connect_with_config(url: &TcpUrl, cfg: &SocketConfig) -> Result<Self, TcpError> {
        if url.tls {
            #[cfg(feature = "tls")]
            {
                return crate::tls::connect_tls(url, cfg);
            }
            #[cfg(not(feature = "tls"))]
            {
                return Err(TcpError::TlsDisabled);
            }
        }

        let (socket, peer) = connect_stream(&url.host, url.port, cfg.connect_timeout_or_default())
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    TcpError::ConnectTimeout {
                        seconds: cfg.connect_timeout_or_default().as_secs(),
                    }
                } else {
                    TcpError::Io(e)
                }
            })?;
        apply_knobs(&socket, cfg).map_err(TcpError::Io)?;

        Ok(Self {
            inner: InnerStream::Plain(socket),
            pkt_size: cfg.pkt_size_or_default(),
            peer,
            stats: TcpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Build from an accepted plain socket (called by TcpListener::accept_blocking).
    pub(crate) fn from_accepted_plain(
        socket: TcpStream,
        peer: SocketAddr,
        cfg: &SocketConfig,
    ) -> Result<Self, TcpError> {
        apply_knobs(&socket, cfg).map_err(TcpError::Io)?;
        Ok(Self {
            inner: InnerStream::Plain(socket),
            pkt_size: cfg.pkt_size_or_default(),
            peer,
            stats: TcpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Build from a TLS-wrapped stream (called by tls::connect_tls / tls::accept_tls).
    #[cfg(feature = "tls")]
    pub(crate) fn from_tls(
        tls: crate::tls::TlsStream,
        peer: SocketAddr,
        cfg: &SocketConfig,
    ) -> Self {
        Self {
            inner: InnerStream::Tls(Box::new(tls)),
            pkt_size: cfg.pkt_size_or_default(),
            peer,
            stats: TcpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Peer address.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Snapshot stats.
    pub fn stats(&self) -> TcpStats {
        self.stats
    }

    /// Return a cloneable handle that can cancel a `recv_bytes` parked in
    /// another thread. See [`TcpCancelHandle`] for the full contract.
    pub fn cancel_handle(&self) -> TcpCancelHandle {
        TcpCancelHandle {
            alive: self.alive.clone(),
        }
    }
}

/// Drive a manual write loop so partial progress is observable.
///
/// `Write::write_all` hides how many bytes it consumed before failing, so it
/// cannot distinguish a zero-progress `WouldBlock` (the slice is intact — safe
/// to retry per the [`Transport`] contract) from a partial-prefix-then-
/// `WouldBlock` (the prefix is already on the wire — retrying would duplicate
/// it and desync the receiver's 188-byte TS framing).
///
/// Returns `Ok(())` on a full write. On error the `bool` is `true` when the
/// transport must be marked dead (any partial/broken outcome) and `false` for
/// a clean zero-progress `Backpressure` that the caller may retry.
fn write_loop<W: FnMut(&[u8]) -> std::io::Result<usize>>(
    msg: &[u8],
    mut write: W,
) -> Result<(), (TransportError, bool)> {
    let mut written = 0usize;
    while written < msg.len() {
        match write(&msg[written..]) {
            Ok(0) => {
                // Peer closed mid-message: stream is now desynced — undefined state.
                return Err((
                    TransportError::Broken {
                        msg: "write returned 0 (peer closed mid-message)".to_string(),
                        errno_code: None,
                    },
                    true,
                ));
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => { /* EINTR: retry */ }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if written == 0 {
                    // Nothing was consumed — the slice is intact and safe to
                    // retry per the Transport contract.
                    return Err((
                        TransportError::Backpressure {
                            msg: format!("write WouldBlock: {e}"),
                            errno_code: e.raw_os_error(),
                        },
                        false,
                    ));
                }
                // A partial prefix is already committed to the stream. We cannot
                // report Backpressure (a retry would duplicate the prefix and
                // desync the receiver's TS framing). The stream is in an undefined
                // state — mark the transport dead and report Broken so the caller
                // rebuilds rather than re-sending onto a corrupted stream.
                return Err((
                    TransportError::Broken {
                        msg: format!(
                            "partial write then WouldBlock ({written}/{} bytes); stream desynced, rebuild required",
                            msg.len()
                        ),
                        errno_code: e.raw_os_error(),
                    },
                    true,
                ));
            }
            Err(e) => {
                return Err((
                    TransportError::Broken {
                        msg: format!("write error: {e}"),
                        errno_code: e.raw_os_error(),
                    },
                    true,
                ));
            }
        }
    }
    Ok(())
}

impl Transport for TcpTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        if msg.len() > self.pkt_size {
            self.stats.send_errors = self.stats.send_errors.saturating_add(1);
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.pkt_size,
            });
        }
        match write_loop(msg, |b| self.inner.write(b)) {
            Ok(()) => {
                self.stats.send_calls = self.stats.send_calls.saturating_add(1);
                self.stats.bytes_sent = self.stats.bytes_sent.saturating_add(msg.len() as u64);
                Ok(())
            }
            Err((err, mark_dead)) => {
                if mark_dead {
                    self.alive.store(false, Ordering::Release);
                    self.stats.send_errors = self.stats.send_errors.saturating_add(1);
                }
                Err(err)
            }
        }
    }

    fn max_payload(&self) -> usize {
        self.pkt_size
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn close(&mut self) {
        self.alive.store(false, Ordering::Release);
        self.inner.shutdown();
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(self.cancel_handle()))
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        Some(self.stats.to_socket_stats())
    }
}

impl RecvTransport for TcpTransport {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        loop {
            if !self.alive.load(Ordering::Acquire) {
                return Err(TransportError::Closed);
            }
            match self.inner.read(buf) {
                Ok(0) => {
                    // Peer EOF is terminal: nothing more will ever arrive on
                    // this stream, so the transport must stop reporting alive
                    // (the send path already marks dead on its terminal arms).
                    self.alive.store(false, Ordering::Release);
                    return Err(TransportError::Broken {
                        msg: "peer closed connection".into(),
                        errno_code: None,
                    });
                }
                Ok(n) => {
                    self.stats.recv_calls = self.stats.recv_calls.saturating_add(1);
                    self.stats.bytes_received = self.stats.bytes_received.saturating_add(n as u64);
                    return Ok(n);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => {
                    // Fatal read (anything that is not the retryable
                    // WouldBlock/TimedOut poll above) — same terminal contract
                    // as the EOF arm: report Broken *and* mark the transport
                    // dead so is_alive() cannot claim otherwise.
                    self.alive.store(false, Ordering::Release);
                    self.stats.recv_errors = self.stats.recv_errors.saturating_add(1);
                    return Err(TransportError::Broken {
                        msg: format!("read error: {e}"),
                        errno_code: e.raw_os_error(),
                    });
                }
            }
        }
    }

    fn max_payload(&self) -> usize {
        self.pkt_size
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn close(&mut self) {
        self.alive.store(false, Ordering::Release);
        self.inner.shutdown();
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(self.cancel_handle()))
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        Some(self.stats.to_socket_stats())
    }
}

#[cfg(test)]
mod write_loop_tests {
    use super::write_loop;
    use std::io;
    use tst_core::transport::TransportError;

    /// Scripted writer: pops one outcome per call from a queue.
    fn scripted(steps: Vec<io::Result<usize>>) -> impl FnMut(&[u8]) -> io::Result<usize> {
        let mut it = steps.into_iter();
        move |_buf| it.next().expect("write called more times than scripted")
    }

    #[test]
    fn full_write_in_one_call_is_ok() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, scripted(vec![Ok(188)]));
        assert!(r.is_ok());
    }

    #[test]
    fn full_write_across_multiple_calls_is_ok() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, scripted(vec![Ok(100), Ok(88)]));
        assert!(r.is_ok());
    }

    #[test]
    fn zero_progress_wouldblock_is_backpressure_not_dead() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock");
        let r = write_loop(&msg, scripted(vec![Err(err)]));
        match r {
            Err((TransportError::Backpressure { .. }, mark_dead)) => {
                assert!(!mark_dead, "zero-progress backpressure must NOT mark dead");
            }
            other => panic!("expected Backpressure, got {other:?}"),
        }
    }

    #[test]
    fn partial_then_wouldblock_is_broken_and_dead() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock");
        // First write commits a partial prefix, then the next blocks.
        let r = write_loop(&msg, scripted(vec![Ok(100), Err(err)]));
        match r {
            Err((TransportError::Broken { msg, .. }, mark_dead)) => {
                assert!(mark_dead, "partial-write Broken must mark dead");
                assert!(
                    msg.contains("partial write") && msg.contains("100/188"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn write_returns_zero_is_broken_and_dead() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, scripted(vec![Ok(0)]));
        match r {
            Err((TransportError::Broken { msg, .. }, mark_dead)) => {
                assert!(mark_dead);
                assert!(msg.contains("peer closed mid-message"), "got: {msg}");
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn interrupted_is_retried() {
        let msg = vec![0u8; 188];
        let eintr = io::Error::new(io::ErrorKind::Interrupted, "eintr");
        // EINTR mid-flight must be transparently retried, not surfaced.
        let r = write_loop(&msg, scripted(vec![Ok(50), Err(eintr), Ok(138)]));
        assert!(r.is_ok());
    }

    #[test]
    fn hard_error_is_broken_and_dead() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::ConnectionReset, "reset");
        let r = write_loop(&msg, scripted(vec![Err(err)]));
        match r {
            Err((TransportError::Broken { .. }, mark_dead)) => assert!(mark_dead),
            other => panic!("expected Broken, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod connect_stream_tests {
    use super::connect_stream;

    #[test]
    fn connect_stream_loopback_resolves_localhost() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        // "localhost" may resolve to ::1 first — the per-address loop must
        // fall through to 127.0.0.1.
        let (s, peer) =
            connect_stream("localhost", port, std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(peer.port(), port);
        drop((s, l));
    }

    #[test]
    fn connect_stream_resolution_failure_is_clean_io_error() {
        // An empty host fails getaddrinfo's argument preprocessing before any
        // resolver query is issued, so this stays hermetic even on a runner
        // with blocked or misconfigured DNS. (A real NXDOMAIN path is
        // resolver-dependent and deliberately not exercised in unit tests.)
        let err = connect_stream("", 7001, std::time::Duration::from_secs(5)).unwrap_err();
        let _ = err; // any io::Error is acceptable; must not panic
    }
}
