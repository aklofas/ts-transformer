//! [`TcpTransport`] — TCP transport implementing both Transport + RecvTransport.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::cancel::FlagCancel;
use tst_core::net::{RecvAction, SendClass, classify_recv_error, classify_send_error};
use tst_core::transport::{
    BrokenCause, EndReason, EndReasonHandle, RecvTransport, SocketStats, Transport,
    TransportCancel, TransportError,
};

use crate::config::TcpConfig;
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
///   (~100 ms) and returns
///   [`tst_core::transport::TransportError::ExplicitClose`];
/// - calls started after `cancel()` return `ExplicitClose` on their entry
///   check;
/// - a read or write that FAILS while the cancel is latched (peer EOF, a
///   reset, a zero-length write) returns `ExplicitClose` instead of
///   `Broken`, and is not counted as an error;
/// - a call already past its entry check may still complete its current I/O
///   first (a recv that receives data in that window returns it; a send that
///   has not yet committed any byte finishes its bounded ≤~100 ms write
///   attempt) — the *next* call then returns `ExplicitClose`;
/// - a send that has already committed a partial prefix keeps writing the
///   remainder and observes the flag at its next ~100 ms write-timeout tick,
///   returning `ExplicitClose` (the prefix stays on the wire; the transport
///   is dead);
/// - an accepted `tcps://` transport's first send drives the lazy TLS server
///   handshake one socket step at a time and re-reads the flag between
///   steps, so a cancel during a peer's slow handshake returns
///   `ExplicitClose` within ~100 ms; a tick with no bytes from the peer is a
///   zero-progress `Backpressure`;
/// - a parked `accept_blocking` observes the flag at its next ~5 ms poll
///   (`ACCEPT_POLL_INTERVAL`, shorter than the ~100 ms above — see that
///   constant's doc for why) and returns [`crate::error::TcpError::Closed`]
///   (a listener is not a `Transport` — the `TcpError` layer carries no
///   cancel variant); later accepts return it at their entry check.
///
/// The transport's own [`tst_core::transport::Transport::close`] drops
/// `alive` WITHOUT latching the cancel, so a post-close operation reports
/// `Closed` and a post-cancel one `ExplicitClose`.
///
/// `TcpCancelHandle` is `Clone + Send + Sync`; multiple holders can race
/// `cancel()` safely (the flag is an `Arc<AtomicBool>`, idempotent).
#[derive(Clone, Debug)]
pub struct TcpCancelHandle {
    alive: Arc<AtomicBool>,
    /// The cancel latch proper, SEPARATE from `alive`.
    ///
    /// `alive` is a liveness flag: the transport drops it on a clean peer
    /// EOF and on a broken read as well as on a cancel, so `!alive` cannot
    /// answer "did the caller cancel?". The bindings relabel a
    /// caller-initiated end from exactly that answer (`Owned::is_cancelled`
    /// ORs the transport's latch in), so reading `!alive` would turn every
    /// clean TCP peer EOF into `TST_E_CLOSED` instead of
    /// `TST_E_END_OF_STREAM`. Shared through the same `Arc` chain as
    /// `alive` so a handle minted after the cancel still reads `true`.
    cancel: FlagCancel,
}

impl TcpCancelHandle {
    /// Build a handle over the shared liveness + cancel flags — used by
    /// [`TcpTransport::cancel_handle`] and
    /// [`crate::listener::TcpListener::cancel_handle`].
    pub(crate) fn from_flags(alive: Arc<AtomicBool>, cancel: FlagCancel) -> Self {
        Self { alive, cancel }
    }
}

impl TcpCancelHandle {
    /// Signal any parked `recv_bytes` (or subsequent `send_bytes`/`recv_bytes`)
    /// to return [`tst_core::transport::TransportError::ExplicitClose`] at its
    /// next ~100 ms poll boundary; a parked `accept_blocking` returns
    /// [`crate::error::TcpError::Closed`]. Idempotent — repeated calls are a
    /// no-op.
    pub fn cancel(&self) {
        // Latch the cancel BEFORE dropping liveness: the woken thread's next
        // act is to ask `is_cancelled()` whether the failure it just saw was
        // caller-initiated, and it must not read `false` there.
        self.cancel.cancel();
        self.alive.store(false, Ordering::Release);
    }

    /// `true` if [`Self::cancel`] has been called on any clone of this handle
    /// (or on any handle minted from the same transport/listener).
    ///
    /// A peer EOF or a wire failure does NOT set this — those drop the
    /// transport's liveness flag only, which [`crate::TcpTransport`]'s
    /// `is_alive()` reports. Neither does the transport's own `close()`:
    /// a post-close operation reads `Closed`, a post-cancel one
    /// `ExplicitClose`.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

impl TransportCancel for TcpCancelHandle {
    fn cancel(&self) {
        TcpCancelHandle::cancel(self)
    }
    fn is_cancelled(&self) -> bool {
        TcpCancelHandle::is_cancelled(self)
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
    /// Set only by [`TcpCancelHandle::cancel`] — see that field's doc for
    /// why it is not `!alive`.
    pub(crate) cancel: FlagCancel,
    /// Why the receive side ended, shared with every handle from
    /// [`RecvTransport::end_reason_handle`]; written first-writer-wins
    /// through [`record_tcp_end`]. A send-only user never reads it.
    end_reason: EndReasonHandle,
}

/// The reasons a TCP receive stream can end with: a cancel or close, the
/// peer's orderly close (EOF), or a hard read failure. Every recording
/// site goes through [`record_tcp_end`].
fn tcp_receiver_can_end_with(r: &EndReason) -> bool {
    matches!(
        r,
        EndReason::Cancelled | EndReason::CleanTeardown | EndReason::TransportFailed { .. }
    )
}

/// Record why a TCP receive stream ended (first writer wins).
fn record_tcp_end(h: &EndReasonHandle, r: EndReason) {
    debug_assert!(
        tcp_receiver_can_end_with(&r),
        "a TCP receiver cannot end with {r:?}"
    );
    h.record(r);
}

/// Resolve `host:port` (IP literal or DNS name) and connect with `timeout`
/// applied per candidate address, returning the stream + the address that
/// accepted. Hostnames resolve here, never at URL-parse time.
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

/// The one connect-error mapping for `tcp://` and `tcps://`: a connect
/// that ran out `connect_timeout` is `ConnectTimeout`
/// (`TCP_CONNECT_TIMEOUT`, -32); anything else from the socket is `Io`.
pub(crate) fn map_connect_err(e: std::io::Error, cfg: &TcpConfig) -> TcpError {
    if e.kind() == std::io::ErrorKind::TimedOut {
        TcpError::ConnectTimeout {
            seconds: cfg.connect_timeout_or_default().as_secs(),
        }
    } else {
        TcpError::Io(e)
    }
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
        Self::connect_with_config(&url, &url.config)
    }

    /// Build a caller-side `TcpTransport` from an already-parsed URL + config.
    pub fn connect_with_config(url: &TcpUrl, cfg: &TcpConfig) -> Result<Self, TcpError> {
        cfg.check_pkt_size()?;
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
            .map_err(|e| map_connect_err(e, cfg))?;
        apply_knobs(&socket, cfg).map_err(TcpError::Io)?;

        Ok(Self {
            inner: InnerStream::Plain(socket),
            pkt_size: cfg.pkt_size_or_default(),
            peer,
            stats: TcpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
            cancel: FlagCancel::new(),
            end_reason: EndReasonHandle::default(),
        })
    }

    /// Build from an accepted plain socket (called by TcpListener::accept_blocking).
    pub(crate) fn from_accepted_plain(
        socket: TcpStream,
        peer: SocketAddr,
        cfg: &TcpConfig,
    ) -> Result<Self, TcpError> {
        apply_knobs(&socket, cfg).map_err(TcpError::Io)?;
        Ok(Self {
            inner: InnerStream::Plain(socket),
            pkt_size: cfg.pkt_size_or_default(),
            peer,
            stats: TcpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
            cancel: FlagCancel::new(),
            end_reason: EndReasonHandle::default(),
        })
    }

    /// Build from a TLS-wrapped stream (called by tls::connect_tls / tls::accept_tls).
    #[cfg(feature = "tls")]
    pub(crate) fn from_tls(tls: crate::tls::TlsStream, peer: SocketAddr, cfg: &TcpConfig) -> Self {
        Self {
            inner: InnerStream::Tls(Box::new(tls)),
            pkt_size: cfg.pkt_size_or_default(),
            peer,
            stats: TcpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
            cancel: FlagCancel::new(),
            end_reason: EndReasonHandle::default(),
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
        TcpCancelHandle::from_flags(self.alive.clone(), self.cancel.clone())
    }

    /// What a dead transport reports: the cancel if one fired, else the
    /// plain close (own `close()`, or latched dead by a terminal error —
    /// those callers return `Broken` directly and never reach this).
    fn dead_error(&self) -> TransportError {
        dead_kind(&self.cancel)
    }
}

/// What `is_alive()` reports, and what a call checks before it starts: live,
/// and no cancel latched.
///
/// A canceller latches the cancel before it drops liveness, and a call that
/// fails while the latch is set already reports `ExplicitClose`. The latch on
/// its own therefore reads as dead, or a caller that asks right after that
/// call would be told the transport still lives; and a call that starts
/// while it is set is refused, so a transport that reads dead takes no more
/// I/O. Liveness is read first, as in `write_loop`.
fn reports_alive(alive: &AtomicBool, cancel: &FlagCancel) -> bool {
    alive.load(Ordering::Acquire) && !cancel.is_cancelled()
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
/// uses. Tearing the connection down here instead would desync the peer's
/// 188-byte TS framing: the managed reconnect starts a fresh connection
/// mid-message.
///
/// Returns `Ok(())` on a full write. On error the `bool` is `true` when the
/// transport must be marked dead (`Ok(0)` or a hard error, with the transport
/// still live and no cancel latched) and `false` for a clean zero-progress
/// `Backpressure` the caller may retry, or an `ExplicitClose` (cancel) /
/// `Closed` (close) after a mid-message wake (the canceller already dropped
/// the flag).
fn write_loop<W: FnMut(&[u8]) -> std::io::Result<usize>>(
    msg: &[u8],
    alive: &AtomicBool,
    cancel: &FlagCancel,
    mut write: W,
) -> Result<(), (TransportError, bool)> {
    let mut written = 0usize;
    while written < msg.len() {
        match write(&msg[written..]) {
            // The cancel preference below covers this arm too: a peer that
            // closes as the cancel lands reports the cancel, not `Broken`.
            Ok(0) if !alive.load(Ordering::Acquire) || cancel.is_cancelled() => {
                return Err((dead_kind(cancel), false));
            }
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
            // A cancel or close that landed while the write was parked wins
            // over whatever the interrupted syscall reported, at any progress.
            // Liveness is read BEFORE the latch because the canceller stores
            // them in the opposite order (latch, then liveness): a dead flag
            // seen here guarantees a cancel's latch is visible, while reading
            // the latch first lets a cancel land between the two loads and
            // pass for a plain close. The latch is also consulted on its own
            // so a cancel caught between its two stores (latched, still
            // live) wins too.
            Err(_) if !alive.load(Ordering::Acquire) || cancel.is_cancelled() => {
                return Err((dead_kind(cancel), false));
            }
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
                // ticked over. Keep writing the remainder (the write timeout
                // is the poll cadence for the cancel/close checks above).
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

/// What a transport that is no longer live reports: the cancel if one
/// latched, else the plain close.
fn dead_kind(cancel: &FlagCancel) -> TransportError {
    if cancel.is_cancelled() {
        TransportError::ExplicitClose
    } else {
        TransportError::Closed
    }
}

/// The read loop behind `recv_bytes`, shaped like [`write_loop`]. On error
/// the `bool` is `true` when the transport must be marked dead (peer EOF or
/// a hard read error) and `false` for a cancel or close.
///
/// A read that fails while the cancel is latched reports the cancel, with the
/// same read order as `write_loop` (liveness, then the latch), and so does a
/// read that has not started yet. A read that succeeds returns its data
/// without consulting the latch.
fn read_loop<R: FnMut(&mut [u8]) -> std::io::Result<usize>>(
    buf: &mut [u8],
    alive: &AtomicBool,
    cancel: &FlagCancel,
    mut read: R,
) -> Result<usize, (TransportError, bool)> {
    loop {
        if !reports_alive(alive, cancel) {
            return Err((dead_kind(cancel), false));
        }
        match read(buf) {
            Ok(0) | Err(_) if !alive.load(Ordering::Acquire) || cancel.is_cancelled() => {
                return Err((dead_kind(cancel), false));
            }
            Ok(0) => {
                // Peer EOF is terminal: nothing more will ever arrive on
                // this stream.
                return Err((
                    TransportError::Broken {
                        msg: "peer closed connection".into(),
                        errno_code: None,
                        cause: BrokenCause::CleanEof,
                    },
                    true,
                ));
            }
            Ok(n) => return Ok(n),
            Err(e) if classify_recv_error(e.kind()) == RecvAction::Retry => continue,
            Err(e) => {
                // Fatal read (anything `classify_recv_error` does not send
                // back through the retry arm above).
                return Err((
                    TransportError::Broken {
                        msg: format!("read error: {e}"),
                        errno_code: e.raw_os_error(),
                        cause: BrokenCause::Unspecified,
                    },
                    true,
                ));
            }
        }
    }
}

/// Record why a receive ended from the result of `recv_bytes`.
fn record_recv_end(h: &EndReasonHandle, r: &Result<usize, TransportError>) {
    match r {
        Err(TransportError::ExplicitClose) => record_tcp_end(h, EndReason::Cancelled),
        Err(TransportError::Broken {
            cause: BrokenCause::CleanEof,
            ..
        }) => record_tcp_end(h, EndReason::CleanTeardown),
        Err(TransportError::Broken { msg, .. }) => {
            record_tcp_end(h, EndReason::TransportFailed { msg: msg.clone() });
        }
        _ => {}
    }
}

impl Transport for TcpTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if !reports_alive(&self.alive, &self.cancel) {
            return Err(self.dead_error());
        }
        if msg.len() > self.pkt_size {
            self.stats.send_errors = self.stats.send_errors.saturating_add(1);
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.pkt_size,
            });
        }
        #[cfg(feature = "tls")]
        self.drive_tls_handshake()?;
        match write_loop(msg, &self.alive, &self.cancel, |b| self.inner.write(b)) {
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
        reports_alive(&self.alive, &self.cancel)
    }

    fn close(&mut self) {
        // No end reason is recorded here: the shells call the RecvTransport
        // close, and the send side has no end reason to report.
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

impl TcpTransport {
    /// Finish a pending TLS handshake before the first application write,
    /// one socket step at a time with the cancel and liveness flags read
    /// between steps. `Ok` at once when the handshake is done (every
    /// connected caller, and an accepted stream after its first I/O).
    #[cfg(feature = "tls")]
    fn drive_tls_handshake(&mut self) -> Result<(), TransportError> {
        use crate::tls::{HandshakeStep, is_poll_timeout};
        loop {
            let InnerStream::Tls(tls) = &mut self.inner else {
                return Ok(());
            };
            if !self.alive.load(Ordering::Acquire) || self.cancel.is_cancelled() {
                return Err(self.dead_error());
            }
            match tls.handshake_step() {
                Ok(HandshakeStep::Done) => return Ok(()),
                Ok(HandshakeStep::Progress) => continue,
                // A cancel or close that landed while the step was parked
                // wins over whatever the interrupted syscall reported, as in
                // `write_loop`: liveness first, then the latch on its own.
                Err(_) if !self.alive.load(Ordering::Acquire) || self.cancel.is_cancelled() => {
                    return Err(self.dead_error());
                }
                Err(e) if is_poll_timeout(&e) => {
                    // Zero progress, nothing wrong with the socket: the same
                    // Backpressure a silent peer produced inside the lazy
                    // handshake of the first write.
                    return Err(TransportError::Backpressure {
                        msg: format!("TLS handshake waiting on the peer: {e}"),
                        errno_code: e.raw_os_error(),
                    });
                }
                Err(e) => {
                    self.alive.store(false, Ordering::Release);
                    self.stats.send_errors = self.stats.send_errors.saturating_add(1);
                    return Err(TransportError::Broken {
                        msg: format!("TLS handshake failed: {e}"),
                        errno_code: e.raw_os_error(),
                        cause: BrokenCause::Unspecified,
                    });
                }
            }
        }
    }

    /// The receive loop behind `recv_bytes`; the caller records the end.
    fn recv_once(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let inner = &mut self.inner;
        match read_loop(buf, &self.alive, &self.cancel, |b| inner.read(b)) {
            Ok(n) => {
                self.stats.recv_calls = self.stats.recv_calls.saturating_add(1);
                self.stats.bytes_received = self.stats.bytes_received.saturating_add(n as u64);
                Ok(n)
            }
            Err((err, mark_dead)) => {
                if mark_dead {
                    // Both terminal arms stop reporting alive (the send path
                    // already marks dead on its terminal arms).
                    self.alive.store(false, Ordering::Release);
                    if !matches!(
                        err,
                        TransportError::Broken {
                            cause: BrokenCause::CleanEof,
                            ..
                        }
                    ) {
                        self.stats.recv_errors = self.stats.recv_errors.saturating_add(1);
                    }
                }
                Err(err)
            }
        }
    }
}

impl RecvTransport for TcpTransport {
    /// Receive into `buf`. An **empty** `buf` is a documented no-op: it
    /// returns `Ok(0)` without touching the socket or the liveness flag.
    /// `TcpStream::read(&mut [])` returns `Ok(0)` on an open peer, and the
    /// `Ok(0)` arm below is the peer-EOF discriminator — letting an empty
    /// read reach it would report a clean EOF (`Broken { cause: CleanEof }`)
    /// and latch the transport dead while the peer is still connected.
    /// The guard sits above `InnerStream`, so `tcps://` follows
    /// the same rule.
    ///
    /// A dead transport reports WHICH death at the loop's entry check (the
    /// private `dead_error` helper): a cancel → `ExplicitClose`, the
    /// transport's own `close()` → `Closed`. A parked receive observes either
    /// at its next ~100 ms poll boundary. A read that fails (peer EOF or a
    /// hard error) while the cancel is latched reports `ExplicitClose` too,
    /// and the end reason is then `Cancelled`.
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let r = self.recv_once(buf);
        record_recv_end(&self.end_reason, &r);
        r
    }

    fn max_payload(&self) -> usize {
        self.pkt_size
    }

    fn is_alive(&self) -> bool {
        reports_alive(&self.alive, &self.cancel)
    }

    fn close(&mut self) {
        self.alive.store(false, Ordering::Release);
        record_tcp_end(&self.end_reason, EndReason::Cancelled);
        self.inner.shutdown();
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(self.cancel_handle()))
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        Some(self.stats.to_socket_stats())
    }

    fn end_reason_handle(&self) -> Option<EndReasonHandle> {
        Some(self.end_reason.clone())
    }
}

#[cfg(test)]
mod write_loop_tests {
    use super::{reports_alive, write_loop};
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tst_core::cancel::FlagCancel;
    use tst_core::transport::TransportError;

    /// An `alive` flag that never drops — the default for scripted writers.
    fn live() -> AtomicBool {
        AtomicBool::new(true)
    }

    /// A `cancelled` flag that never fires — the default for scripted
    /// writers (only the cancel/close rows below flip it).
    fn uncancelled() -> FlagCancel {
        FlagCancel::new()
    }

    /// Scripted writer: pops one outcome per call from a queue.
    fn scripted(steps: Vec<io::Result<usize>>) -> impl FnMut(&[u8]) -> io::Result<usize> {
        let mut it = steps.into_iter();
        move |_buf| it.next().expect("write called more times than scripted")
    }

    #[test]
    fn full_write_in_one_call_is_ok() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, &live(), &uncancelled(), scripted(vec![Ok(188)]));
        assert!(r.is_ok());
    }

    #[test]
    fn full_write_across_multiple_calls_is_ok() {
        let msg = vec![0u8; 188];
        let r = write_loop(
            &msg,
            &live(),
            &uncancelled(),
            scripted(vec![Ok(100), Ok(88)]),
        );
        assert!(r.is_ok());
    }

    #[test]
    fn zero_progress_wouldblock_is_backpressure_not_dead() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock");
        let r = write_loop(&msg, &live(), &uncancelled(), scripted(vec![Err(err)]));
        match r {
            Err((TransportError::Backpressure { .. }, mark_dead)) => {
                assert!(!mark_dead, "zero-progress backpressure must NOT mark dead");
            }
            other => panic!("expected Backpressure, got {other:?}"),
        }
    }

    /// The kernel accepted the first 100 bytes *in order*, so
    /// the stream is intact — a `WouldBlock` after that is the 100 ms send
    /// deadline ticking over, not a desync. `write_loop` must keep writing
    /// `&msg[written..]` and finish when the peer drains.
    #[test]
    fn partial_then_wouldblock_keeps_writing() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock");
        let r = write_loop(
            &msg,
            &live(),
            &uncancelled(),
            scripted(vec![Ok(100), Err(err), Ok(88)]),
        );
        assert!(
            r.is_ok(),
            "expected Ok after the remainder drains, got {r:?}"
        );
    }

    /// The remainder loop is bounded only by cancel/close: once `alive`
    /// drops mid-message the loop stops. With no cancel latched this is the
    /// transport's own `close()`, which reports `Closed` (the flag was
    /// already dropped by the closer).
    #[test]
    fn partial_then_close_returns_closed() {
        let msg = vec![0u8; 188];
        let alive = AtomicBool::new(true);
        let mut calls = 0;
        let r = write_loop(&msg, &alive, &uncancelled(), |_buf| {
            calls += 1;
            match calls {
                1 => Ok(100),
                _ => {
                    // The transport's own close() drops liveness (and only
                    // liveness) while the write is parked.
                    alive.store(false, Ordering::Release);
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock"))
                }
            }
        });
        match r {
            Err((TransportError::Closed, mark_dead)) => {
                assert!(!mark_dead, "close already dropped the flag; no re-latch")
            }
            other => panic!("expected Closed after a mid-message close, got {other:?}"),
        }
    }

    /// A cross-thread cancel mid-message is reported as the cancel
    /// (`ExplicitClose`); the transport's own close() keeps reporting
    /// `Closed` (`partial_then_close_returns_closed` above).
    #[test]
    fn partial_then_cross_thread_cancel_returns_explicit_close() {
        let msg = vec![0u8; 188];
        let alive = AtomicBool::new(true);
        let cancelled = FlagCancel::new();
        let mut calls = 0;
        let r = write_loop(&msg, &alive, &cancelled, |_buf| {
            calls += 1;
            match calls {
                1 => Ok(100),
                _ => {
                    // A canceller latches the cancel BEFORE dropping alive,
                    // exactly as `TcpCancelHandle::cancel` does.
                    cancelled.cancel();
                    alive.store(false, Ordering::Release);
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "ewouldblock"))
                }
            }
        });
        match r {
            Err((TransportError::ExplicitClose, mark_dead)) => assert!(!mark_dead),
            other => panic!("expected ExplicitClose after a mid-message cancel, got {other:?}"),
        }
    }

    /// A cancel that fully landed (latch, then liveness, in
    /// `TcpCancelHandle::cancel`'s order) before the write woke reports
    /// `ExplicitClose`. With `zero_progress_timeout_after_close_is_closed`
    /// this pins the two outcomes the loop's liveness-then-latch read order
    /// must preserve.
    #[test]
    fn zero_progress_timeout_after_cancel_is_explicit_close() {
        let alive = AtomicBool::new(true);
        let cancel = FlagCancel::new();
        let r = write_loop(&[0u8; 188], &alive, &cancel, |_| {
            cancel.cancel();
            alive.store(false, Ordering::Release);
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        });
        assert!(
            matches!(r, Err((TransportError::ExplicitClose, false))),
            "{r:?}"
        );
    }

    #[test]
    fn hard_error_racing_a_cancel_is_explicit_close() {
        let alive = AtomicBool::new(true);
        let cancel = FlagCancel::new();
        let r = write_loop(&[0u8; 188], &alive, &cancel, |_| {
            cancel.cancel();
            alive.store(false, Ordering::Release);
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        });
        assert!(
            matches!(r, Err((TransportError::ExplicitClose, false))),
            "{r:?}"
        );
    }

    /// A cancel caught between its two stores (latch set, liveness not yet
    /// dropped) still wins over a hard error: the send reports the cancel,
    /// not a `Broken` a managed wrapper would treat as a reconnect cause.
    #[test]
    fn hard_error_with_cancel_latched_but_still_live_is_explicit_close() {
        let alive = live();
        let cancel = FlagCancel::new();
        let r = write_loop(&[0u8; 188], &alive, &cancel, |_| {
            cancel.cancel();
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        });
        assert!(
            matches!(r, Err((TransportError::ExplicitClose, false))),
            "{r:?}"
        );
    }

    /// In that window `is_alive()` already reads dead, so a caller that asks
    /// right after a call reported the cancel is not told the transport lives.
    #[test]
    fn a_cancel_latched_but_still_live_reads_as_dead() {
        let alive = live();
        let cancel = FlagCancel::new();
        assert!(reports_alive(&alive, &cancel));
        cancel.cancel();
        assert!(!reports_alive(&alive, &cancel));
    }

    /// The transport's own `close()` and a terminal error drop liveness
    /// without a cancel: dead, and not cancelled.
    #[test]
    fn a_dropped_liveness_flag_reads_as_dead_without_a_cancel() {
        let alive = AtomicBool::new(false);
        assert!(!reports_alive(&alive, &uncancelled()));
    }

    /// The same window with a zero-progress deadline tick reports the
    /// cancel, not a retryable `Backpressure`.
    #[test]
    fn zero_progress_timeout_with_cancel_latched_but_still_live_is_explicit_close() {
        let alive = live();
        let cancel = FlagCancel::new();
        let r = write_loop(&[0u8; 188], &alive, &cancel, |_| {
            cancel.cancel();
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        });
        assert!(
            matches!(r, Err((TransportError::ExplicitClose, false))),
            "{r:?}"
        );
    }

    /// The transport's own `close()` drops liveness only, so a write it
    /// wakes reports `Closed`, never the cancel's `ExplicitClose`.
    #[test]
    fn zero_progress_timeout_after_close_is_closed() {
        let alive = AtomicBool::new(true);
        let r = write_loop(&[0u8; 188], &alive, &uncancelled(), |_| {
            alive.store(false, Ordering::Release);
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        });
        assert!(matches!(r, Err((TransportError::Closed, false))), "{r:?}");
    }

    #[test]
    fn zero_progress_timeout_without_cancel_stays_backpressure() {
        let r = write_loop(
            &[0u8; 188],
            &live(),
            &uncancelled(),
            scripted(vec![Err(io::Error::from(io::ErrorKind::WouldBlock))]),
        );
        assert!(
            matches!(r, Err((TransportError::Backpressure { .. }, false))),
            "{r:?}"
        );
    }

    /// `TimedOut` is what Windows reports when `SO_SNDTIMEO` expires
    /// (`WSAETIMEDOUT`); Linux/macOS report `WouldBlock` (`EAGAIN`) for the
    /// same event. Both are the deadline ticking over: zero progress is
    /// `Backpressure`, progress keeps writing — never `Broken`.
    #[test]
    fn timed_out_is_the_deadline_tick_like_wouldblock() {
        let msg = vec![0u8; 188];
        let zero = io::Error::new(io::ErrorKind::TimedOut, "wsaetimedout");
        match write_loop(&msg, &live(), &uncancelled(), scripted(vec![Err(zero)])) {
            Err((TransportError::Backpressure { .. }, mark_dead)) => assert!(!mark_dead),
            other => panic!("zero-progress TimedOut must be Backpressure, got {other:?}"),
        }
        let partial = io::Error::new(io::ErrorKind::TimedOut, "wsaetimedout");
        let r = write_loop(
            &msg,
            &live(),
            &uncancelled(),
            scripted(vec![Ok(100), Err(partial), Ok(88)]),
        );
        assert!(
            r.is_ok(),
            "TimedOut with progress must keep writing, got {r:?}"
        );
    }

    #[test]
    fn write_returns_zero_is_broken_and_dead() {
        let msg = vec![0u8; 188];
        let r = write_loop(&msg, &live(), &uncancelled(), scripted(vec![Ok(0)]));
        match r {
            Err((TransportError::Broken { msg, .. }, mark_dead)) => {
                assert!(mark_dead);
                assert!(msg.contains("peer closed mid-message"), "got: {msg}");
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn write_zero_racing_a_cancel_is_explicit_close() {
        let alive = AtomicBool::new(true);
        let cancel = FlagCancel::new();
        let r = write_loop(&[0u8; 188], &alive, &cancel, |_| {
            cancel.cancel();
            alive.store(false, Ordering::Release);
            Ok(0)
        });
        assert!(
            matches!(r, Err((TransportError::ExplicitClose, false))),
            "{r:?}"
        );
    }

    /// The cancel caught between its two stores (latched, still live).
    #[test]
    fn write_zero_with_cancel_latched_but_still_live_is_explicit_close() {
        let alive = live();
        let cancel = FlagCancel::new();
        let r = write_loop(&[0u8; 188], &alive, &cancel, |_| {
            cancel.cancel();
            Ok(0)
        });
        assert!(
            matches!(r, Err((TransportError::ExplicitClose, false))),
            "{r:?}"
        );
    }

    #[test]
    fn interrupted_is_retried() {
        let msg = vec![0u8; 188];
        let eintr = io::Error::new(io::ErrorKind::Interrupted, "eintr");
        // EINTR mid-flight must be transparently retried, not surfaced.
        let r = write_loop(
            &msg,
            &live(),
            &uncancelled(),
            scripted(vec![Ok(50), Err(eintr), Ok(138)]),
        );
        assert!(r.is_ok());
    }

    #[test]
    fn hard_error_is_broken_and_dead() {
        let msg = vec![0u8; 188];
        let err = io::Error::new(io::ErrorKind::ConnectionReset, "reset");
        let r = write_loop(&msg, &live(), &uncancelled(), scripted(vec![Err(err)]));
        match r {
            Err((TransportError::Broken { .. }, mark_dead)) => assert!(mark_dead),
            other => panic!("expected Broken, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod read_loop_tests {
    use super::{read_loop, record_recv_end};
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tst_core::cancel::FlagCancel;
    use tst_core::transport::{BrokenCause, EndReason, EndReasonHandle, TransportError};

    /// Run one read whose I/O call cancels the transport, then fails or
    /// reads EOF.
    fn cancelled_read(
        outcome: impl Fn() -> io::Result<usize>,
        drop_alive: bool,
    ) -> Result<usize, (TransportError, bool)> {
        let alive = AtomicBool::new(true);
        let cancel = FlagCancel::new();
        read_loop(&mut [0u8; 188], &alive, &cancel, |_| {
            cancel.cancel();
            if drop_alive {
                alive.store(false, Ordering::Release);
            }
            outcome()
        })
    }

    #[test]
    fn eof_racing_a_cancel_is_explicit_close() {
        for drop_alive in [true, false] {
            let r = cancelled_read(|| Ok(0), drop_alive);
            assert!(
                matches!(r, Err((TransportError::ExplicitClose, false))),
                "{r:?}"
            );
        }
    }

    #[test]
    fn hard_error_racing_a_cancel_is_explicit_close() {
        for drop_alive in [true, false] {
            let r = cancelled_read(|| Err(io::ErrorKind::ConnectionReset.into()), drop_alive);
            assert!(
                matches!(r, Err((TransportError::ExplicitClose, false))),
                "{r:?}"
            );
        }
    }

    /// A read that starts with the cancel already latched (liveness dropped
    /// or not yet) touches no socket and reports the cancel.
    #[test]
    fn a_read_that_starts_under_a_latched_cancel_reads_nothing() {
        for drop_alive in [true, false] {
            let alive = AtomicBool::new(!drop_alive);
            let cancel = FlagCancel::new();
            cancel.cancel();
            let r = read_loop(&mut [0u8; 188], &alive, &cancel, |_| {
                panic!("a cancelled transport must not read")
            });
            assert!(
                matches!(r, Err((TransportError::ExplicitClose, false))),
                "{r:?}"
            );
        }
    }

    #[test]
    fn data_racing_a_cancel_is_returned() {
        let r = cancelled_read(|| Ok(188), true);
        assert!(matches!(r, Ok(188)), "{r:?}");
    }

    #[test]
    fn eof_without_cancel_is_clean_eof_and_dead() {
        let alive = AtomicBool::new(true);
        let r = read_loop(&mut [0u8; 188], &alive, &FlagCancel::new(), |_| Ok(0));
        assert!(
            matches!(
                r,
                Err((
                    TransportError::Broken {
                        cause: BrokenCause::CleanEof,
                        ..
                    },
                    true
                ))
            ),
            "{r:?}"
        );
    }

    #[test]
    fn hard_error_without_cancel_is_broken_and_dead() {
        let alive = AtomicBool::new(true);
        let r = read_loop(&mut [0u8; 188], &alive, &FlagCancel::new(), |_| {
            Err(io::ErrorKind::ConnectionReset.into())
        });
        assert!(
            matches!(r, Err((TransportError::Broken { .. }, true))),
            "{r:?}"
        );
    }

    #[test]
    fn closed_transport_reports_closed_at_entry() {
        let alive = AtomicBool::new(false);
        let r = read_loop(&mut [0u8; 188], &alive, &FlagCancel::new(), |_| {
            panic!("a dead transport must not read")
        });
        assert!(matches!(r, Err((TransportError::Closed, false))), "{r:?}");
    }

    #[test]
    fn a_cancelled_read_ends_the_stream_as_cancelled() {
        let h = EndReasonHandle::default();
        let r = cancelled_read(|| Ok(0), true).map_err(|(e, _)| e);
        record_recv_end(&h, &r);
        assert_eq!(h.get(), Some(EndReason::Cancelled));
    }
}

#[cfg(test)]
mod cancel_window_tests {
    use super::TcpTransport;
    use tst_core::transport::{RecvTransport, Transport, TransportError};

    /// A canceller caught between its two stores (the latch set, liveness not
    /// yet dropped), on a real transport: both traits read dead, and a send
    /// that starts now reports the cancel and writes nothing. The cancel is
    /// judged before the message's size, as it is once liveness has dropped.
    #[test]
    fn a_latched_cancel_reads_as_dead_and_refuses_a_send_loopback() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut t = TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).unwrap();
        let _peer = l.accept().unwrap();
        assert!(Transport::is_alive(&t) && RecvTransport::is_alive(&t));
        t.cancel.cancel();
        assert!(!Transport::is_alive(&t));
        assert!(!RecvTransport::is_alive(&t));
        let r = t.send_bytes(&[0x47; 188]);
        assert!(matches!(r, Err(TransportError::ExplicitClose)), "{r:?}");
        let r = t.send_bytes(&vec![0x47; t.pkt_size + 1]);
        assert!(matches!(r, Err(TransportError::ExplicitClose)), "{r:?}");
        assert_eq!(t.stats().bytes_sent, 0);
        assert_eq!(t.stats().send_errors, 0);
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
    fn map_connect_err_maps_timed_out_to_connect_timeout_and_the_rest_to_io() {
        let cfg = crate::config::TcpConfig {
            connect_timeout: Some(std::time::Duration::from_secs(3)),
            ..Default::default()
        };
        let e = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(matches!(
            super::map_connect_err(e, &cfg),
            crate::error::TcpError::ConnectTimeout { seconds: 3 }
        ));
        let e = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(matches!(
            super::map_connect_err(e, &cfg),
            crate::error::TcpError::Io(_)
        ));
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
