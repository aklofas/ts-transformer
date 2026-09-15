//! [`TcpTransport`] — TCP transport implementing both Transport + RecvTransport.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::net::{SendClass, classify_send_error};
use tst_core::transport::{
    BrokenCause, RecvTransport, SocketStats, Transport, TransportCancel, TransportError,
};

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
            Self::Tls(s) => {
                // Send close_notify and shut the socket here rather than
                // leaving it to Drop: a caller that closes but retains the
                // transport would otherwise keep the peer parked on a read.
                s.shutdown();
            }
        }
    }
}

/// Cheap cloneable handle that unblocks a parked [`TcpTransport::recv_bytes`]
/// (or a [`crate::listener::TcpListener::accept_blocking`]) from another thread.
///
/// Obtained via [`TcpTransport::cancel_handle`] or
/// [`crate::listener::TcpListener::cancel_handle`]. Cancelling does **not**
/// shut down the underlying socket — [`tst_core::transport::Transport::close`]
/// still does (a listener's socket stays bound until the `TcpListener` is
/// dropped). Cancellation is cooperative:
///
/// - a parked `recv_bytes` observes the flag at its next poll boundary
///   (~100 ms) and returns [`tst_core::transport::TransportError::Closed`];
/// - calls started after `cancel()` return `Closed` on their entry check;
/// - a call already past its entry check may still complete its current I/O
///   first (a recv that receives data in that window returns it; a send that
///   has not yet committed any byte finishes its bounded ≤~100 ms write
///   attempt) — the *next* call then returns `Closed`;
/// - a send that has already committed a partial prefix keeps writing the
///   remainder and observes the flag at its next ~100 ms write-timeout tick,
///   returning `Closed` (the prefix stays on the wire; the transport is dead);
/// - a parked `accept_blocking` observes the flag at its next ~5 ms poll
///   (`ACCEPT_POLL_INTERVAL`, shorter than the ~100 ms above — see that
///   constant's doc for why) and returns [`crate::error::TcpError::Closed`];
///   later accepts return it at their entry check.
///
/// `TcpCancelHandle` is `Clone + Send + Sync`; multiple holders can race
/// `cancel()` safely (the flag is an `Arc<AtomicBool>`, idempotent).
#[derive(Clone, Debug)]
pub struct TcpCancelHandle {
    alive: Arc<AtomicBool>,
}

impl TcpCancelHandle {
    /// Build a handle over a shared flag — used by [`TcpTransport::cancel_handle`]
    /// and [`crate::listener::TcpListener::cancel_handle`].
    pub(crate) fn from_flag(alive: Arc<AtomicBool>) -> Self {
        Self { alive }
    }
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
        TcpCancelHandle::from_flag(self.alive.clone())
    }
}

/// Drive a manual write loop so partial progress is observable.
///
/// `Write::write_all` hides how many bytes it consumed before failing, so it
/// cannot distinguish a zero-progress `WouldBlock` (the slice is intact — safe
/// to retry per the [`Transport`] contract) from a partial-prefix-then-
/// `WouldBlock` (the prefix is already on the wire, so the *caller* must not
/// retry the slice — but the stream itself is intact: the kernel accepted the
/// prefix in order, and the `WouldBlock` (`TimedOut` on Windows —
/// [`classify_send_error`]) is only the 100 ms send deadline (`apply_knobs`)
/// ticking over). In that second case the loop keeps writing
/// `&msg[written..]`, re-checking `alive` at every tick so a cancel or close
/// from another thread bounds the wait — the same poll shape `recv_bytes`
/// uses. Tearing the connection down here instead is what used to desync
/// the peer's 188-byte TS framing: the managed reconnect started a fresh
/// connection mid-message.
///
/// Returns `Ok(())` on a full write. On error the `bool` is `true` when the
/// transport must be marked dead (`Ok(0)` or a hard error) and `false` for a
/// clean zero-progress `Backpressure` the caller may retry, or a `Closed`
/// after a mid-message cancel (the canceller already dropped the flag).
fn write_loop<W: FnMut(&[u8]) -> std::io::Result<usize>>(
    msg: &[u8],
    alive: &AtomicBool,
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
                        cause: BrokenCause::Unspecified,
                    },
                    true,
                ));
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => { /* EINTR: retry */ }
            Err(e) if classify_send_error(&e) == SendClass::Transient => {
                if written == 0 {
                    // Nothing was consumed — the slice is intact and safe to
                    // retry per the Transport contract.
                    return Err((
                        TransportError::Backpressure {
                            msg: format!("write deadline expired: {e}"),
                            errno_code: e.raw_os_error(),
                        },
                        false,
                    ));
                }
                // A partial prefix is on the wire in order; the deadline
                // ticked over. Keep writing the remainder unless a cancel or
                // close landed while we were parked (the write timeout is
                // the poll cadence, so this check runs every ~100 ms).
                if !alive.load(Ordering::Acquire) {
                    return Err((TransportError::Closed, false));
                }
            }
            Err(e) => {
                return Err((
                    TransportError::Broken {
                        msg: format!("write error: {e}"),
                        errno_code: e.raw_os_error(),
                        cause: BrokenCause::Unspecified,
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
        match write_loop(msg, &self.alive, |b| self.inner.write(b)) {
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

/// What the receive loop does with a failed `read`.
///
/// Split out of `recv_bytes` so the classification is unit-testable: the
/// read itself goes straight at `InnerStream` with no injectable seam, and
/// `EINTR` in particular cannot be provoked deterministically from a plain
/// socket. This mirrors what `write_loop` gets from its scripted writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecvAction {
    /// Transient — poll again; the transport stays alive.
    Retry,
    /// Terminal — report `Broken` and latch the transport dead.
    Fatal,
}

/// Classify a failed receive-side `read` by its [`std::io::ErrorKind`].
///
/// `WouldBlock` / `TimedOut` are the read-deadline poll ticking over.
/// `Interrupted` is `EINTR` — a signal landed on the thread parked in
/// `read` (which is exactly what the SIGINT-handler shutdown pattern the
/// docs steer callers toward delivers). `std`'s `TcpStream::read` surfaces
/// it rather than retrying internally, and the send path's `write_loop`
/// already retries it, so the receive path does too — otherwise a signal
/// meant to be handled and resumed would latch the transport permanently
/// dead. Everything else is terminal.
pub(crate) fn classify_recv_error(kind: std::io::ErrorKind) -> RecvAction {
    match kind {
        std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::Interrupted => RecvAction::Retry,
        _ => RecvAction::Fatal,
    }
}

impl RecvTransport for TcpTransport {
    /// Receive into `buf`. An **empty** `buf` is a documented no-op: it
    /// returns `Ok(0)` without touching the socket or the liveness flag.
    /// `TcpStream::read(&mut [])` returns `Ok(0)` on an open peer, and the
    /// `Ok(0)` arm below is the peer-EOF discriminator — letting an empty
    /// read reach it would report a clean EOF (`Broken { cause: CleanEof }`)
    /// and latch the transport dead while the peer is still connected
    /// (X-CORR-07). The guard sits above `InnerStream`, so `tcps://` follows
    /// the same rule.
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if buf.is_empty() {
            return Ok(0);
        }
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
                        cause: BrokenCause::CleanEof,
                    });
                }
                Ok(n) => {
                    self.stats.recv_calls = self.stats.recv_calls.saturating_add(1);
                    self.stats.bytes_received = self.stats.bytes_received.saturating_add(n as u64);
                    return Ok(n);
                }
                Err(e) if classify_recv_error(e.kind()) == RecvAction::Retry => {
                    continue;
                }
                Err(e) => {
                    // Fatal read (anything `classify_recv_error` does not send
                    // back through the retry arm above) — same terminal contract
                    // as the EOF arm: report Broken *and* mark the transport
                    // dead so is_alive() cannot claim otherwise.
                    self.alive.store(false, Ordering::Release);
                    self.stats.recv_errors = self.stats.recv_errors.saturating_add(1);
                    return Err(TransportError::Broken {
                        msg: format!("read error: {e}"),
                        errno_code: e.raw_os_error(),
                        cause: BrokenCause::Unspecified,
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
mod recv_classify_tests {
    use super::{RecvAction, classify_recv_error};
    use std::io::ErrorKind;

    #[test]
    fn wouldblock_and_timedout_retry() {
        // The read-deadline poll ticking over: transient by construction.
        assert_eq!(
            classify_recv_error(ErrorKind::WouldBlock),
            RecvAction::Retry
        );
        assert_eq!(classify_recv_error(ErrorKind::TimedOut), RecvAction::Retry);
    }

    #[test]
    fn interrupted_retries_like_the_send_path() {
        // EINTR: a signal landed on the thread parked in `read`. `write_loop`
        // retries it; treating it as fatal here would latch `alive = false`
        // and kill the transport for good on a signal meant to be resumed.
        assert_eq!(
            classify_recv_error(ErrorKind::Interrupted),
            RecvAction::Retry
        );
    }

    #[test]
    fn genuine_failures_are_fatal() {
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::NotConnected,
        ] {
            assert_eq!(classify_recv_error(kind), RecvAction::Fatal, "{kind:?}");
        }
    }
}

#[cfg(test)]
mod write_loop_tests {
    use super::write_loop;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tst_core::transport::TransportError;

    /// An `alive` flag that never drops — the default for scripted writers.
    fn live() -> AtomicBool {
        AtomicBool::new(true)
    }

    /// Scripted writer: pops one outcome per call from a queue.
    fn scripted(steps: Vec<io::Result<usize>>) -> impl FnMut(&[u8]) -> io::Result<usize> {
        let mut it = steps.into_iter();
        move |_buf| it.next().expect("write called more times than scripted")
    }

    #[test]
    fn full_write_in_one_call_is_ok() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, &live(), scripted(vec![Ok(188)]));
        assert!(r.is_ok());
    }

    #[test]
    fn full_write_across_multiple_calls_is_ok() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, &live(), scripted(vec![Ok(100), Ok(88)]));
        assert!(r.is_ok());
    }

    #[test]
    fn zero_progress_wouldblock_is_backpressure_not_dead() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock");
        let r = write_loop(&msg, &live(), scripted(vec![Err(err)]));
        match r {
            Err((TransportError::Backpressure { .. }, mark_dead)) => {
                assert!(!mark_dead, "zero-progress backpressure must NOT mark dead");
            }
            other => panic!("expected Backpressure, got {other:?}"),
        }
    }

    /// CORR-22 / Q6: the kernel accepted the first 100 bytes *in order*, so
    /// the stream is intact — a `WouldBlock` after that is the 100 ms send
    /// deadline ticking over, not a desync. `write_loop` must keep writing
    /// `&msg[written..]` and finish when the peer drains.
    #[test]
    fn partial_then_wouldblock_keeps_writing() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock");
        let r = write_loop(&msg, &live(), scripted(vec![Ok(100), Err(err), Ok(88)]));
        assert!(
            r.is_ok(),
            "expected Ok after the remainder drains, got {r:?}"
        );
    }

    /// The remainder loop is bounded only by cancel/close: once the flag
    /// drops mid-message the loop stops and reports `Closed` (the
    /// transport was already latched dead by the canceller).
    #[test]
    fn partial_then_cancel_returns_closed() {
        let msg = vec![0u8; 188];
        let alive = AtomicBool::new(true);
        let mut calls = 0;
        let r = write_loop(&msg, &alive, |_buf| {
            calls += 1;
            match calls {
                1 => Ok(100),
                _ => {
                    // The canceller flips the flag while the write is parked.
                    alive.store(false, Ordering::Release);
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock"))
                }
            }
        });
        match r {
            Err((TransportError::Closed, mark_dead)) => {
                assert!(!mark_dead, "cancel already latched the flag; no re-latch")
            }
            other => panic!("expected Closed after a mid-message cancel, got {other:?}"),
        }
    }

    /// CORR-24: `TimedOut` is what Windows reports when `SO_SNDTIMEO` expires
    /// (`WSAETIMEDOUT`); Linux/macOS report `WouldBlock` (`EAGAIN`) for the
    /// same event. Both are the deadline ticking over: zero progress is
    /// `Backpressure`, progress keeps writing — never `Broken`.
    #[test]
    fn timed_out_is_the_deadline_tick_like_wouldblock() {
        let msg = vec![0u8; 188];
        let zero = io::Error::new(io::ErrorKind::TimedOut, "wsaetimedout");
        match write_loop(&msg, &live(), scripted(vec![Err(zero)])) {
            Err((TransportError::Backpressure { .. }, mark_dead)) => assert!(!mark_dead),
            other => panic!("zero-progress TimedOut must be Backpressure, got {other:?}"),
        }
        let partial = io::Error::new(io::ErrorKind::TimedOut, "wsaetimedout");
        let r = write_loop(&msg, &live(), scripted(vec![Ok(100), Err(partial), Ok(88)]));
        assert!(
            r.is_ok(),
            "TimedOut with progress must keep writing, got {r:?}"
        );
    }

    #[test]
    fn write_returns_zero_is_broken_and_dead() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, &live(), scripted(vec![Ok(0)]));
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
        let r = write_loop(&msg, &live(), scripted(vec![Ok(50), Err(eintr), Ok(138)]));
        assert!(r.is_ok());
    }

    #[test]
    fn hard_error_is_broken_and_dead() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::ConnectionReset, "reset");
        let r = write_loop(&msg, &live(), scripted(vec![Err(err)]));
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
