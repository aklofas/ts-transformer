//! `RtspClient` sync facade.
//!
//! Holds a single byte stream (plain TCP for `rtsp://`, rustls-wrapped
//! for `rtsps://`) for the control connection, behind an
//! `Arc<Mutex<Stream>>` so the main thread and the background
//! keepalive thread share the SAME stream — request/response exchanges
//! serialize under the mutex (RTSP isn't pipelined).

pub mod end_reason;
pub mod interleaved_pump;
pub mod keepalive;
pub mod options_describe;
pub mod play;
pub mod session;
pub mod setup;
pub mod teardown;
pub mod tls;
pub mod transport_negotiation;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;

use crate::error::RtspError;
use crate::url::{RtspScheme, RtspUrl, RtspVersion};

/// The control-plane byte stream — plain TCP for `rtsp://`, or
/// rustls-wrapped TCP for `rtsps://` when the `tls` cargo feature is
/// enabled.
///
/// Hidden behind the same `Read + Write` shape so per-method code in
/// `options_describe.rs`, `setup.rs`, etc. is agnostic to which
/// transport carries the bytes.
#[derive(Debug)]
pub(crate) enum Stream {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<tls::TlsStream>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.write_all(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.write_all(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Sync RTSP client. One instance per server.
///
/// Construct via [`Self::connect`] / [`Self::connect_with`], then drive
/// the session: `options` / `describe` / `setup_mp2t_auto` (or the
/// explicit `setup*` variants) / `play` / `pause` / `teardown`.
/// Keepalive pings run automatically on a background thread once SETUP
/// establishes a session, at the server-advertised `timeout=` cadence.
///
/// # Send
///
/// This type is `Send`: moving it to a dedicated receive/watchdog thread
/// is a supported, documented use — a regression here is a breaking
/// change.
#[derive(Debug)]
pub struct RtspClient {
    /// The control-plane byte stream — plain TCP for `rtsp://`, or
    /// rustls-wrapped TCP for `rtsps://` (under the `tls` feature).
    ///
    /// Wrapped in `Arc<Mutex<...>>` so the main thread + the background
    /// keepalive thread can share the SAME stream (no `try_clone` —
    /// rustls `ClientConnection` isn't clonable, so TLS keepalive would
    /// otherwise be impossible). RTSP isn't pipelined (one in-flight
    /// request at a time), so holding the lock through each
    /// request/response exchange is correct and the contention with the
    /// keepalive thread is negligible.
    pub(crate) stream: Arc<Mutex<Stream>>,
    /// Negotiated URL — caller can re-parse for re-connects.
    pub(crate) url: RtspUrl,
    /// Server's connection address as we resolved it.
    pub(crate) peer: SocketAddr,
    /// Monotonic CSeq counter; every outbound request bumps this.
    pub(crate) next_cseq: AtomicU32,
    /// Session ID from the most recent SETUP success. None before
    /// SETUP / after TEARDOWN.
    pub(crate) session_id: Option<String>,
    /// Server's `Session: ...;timeout=N` value (default 60 s if absent).
    pub(crate) session_timeout: Duration,
    /// Cancel flag — set by `RtspCancelHandle::cancel` to break out of
    /// blocking I/O loops.
    pub(crate) cancel: Arc<AtomicBool>,
    /// Last RTSP version observed in a server response.
    pub(crate) last_server_version: RtspVersion,
    /// Shared flag flipped when a keepalive observes session death: the
    /// [keepalive](crate::rtsp::client::keepalive) thread sets it when a
    /// control-TCP write fails, and the read sites (interleaved pump /
    /// non-pump `send_and_read`) set it when a keepalive ping is answered
    /// with `454 Session Not Found`. The main thread polls it via
    /// [`Self::is_session_alive`]. `None` until
    /// [`Self::spawn_keepalive_if_needed`] runs.
    pub(crate) session_dead: Option<Arc<AtomicBool>>,
    /// Shared cell the main thread updates after SETUP so the keepalive
    /// thread can emit `Session: <id>` headers. `None` until
    /// [`Self::spawn_keepalive_if_needed`] runs.
    pub(crate) session_id_shared: Option<Arc<std::sync::Mutex<Option<String>>>>,
    /// Keepalive ping cadence in milliseconds, shared with the running
    /// keepalive thread (which re-reads it at every wake). SETUP retunes
    /// it to the server-advertised `Session: <id>;timeout=N` unless the
    /// caller supplied an explicit interval override. `None` until
    /// [`Self::spawn_keepalive_if_needed`] runs.
    pub(crate) keepalive_interval_shared: Option<Arc<AtomicU64>>,
    /// True when the keepalive was spawned with a caller-supplied
    /// interval override ([`crate::RtspClientBuilder::keepalive_interval`])
    /// — SETUP must not clobber an explicit override with the cadence
    /// derived from the server-advertised timeout.
    pub(crate) keepalive_interval_overridden: bool,
    /// Value sent in the `User-Agent:` header on every outbound request.
    /// Set at connect time from `RtspClientBuilder::user_agent`; defaults
    /// to `"tst-rtp/0.1"` when using the bare `connect`/`connect_with`
    /// entry points.
    pub(crate) user_agent: String,
    /// Per-request response deadline
    /// ([`crate::RtspClientBuilder::request_timeout`]); `None` = unbounded.
    /// Applied by `send_and_read` and `teardown`.
    pub(crate) request_timeout: Option<Duration>,
    /// JoinHandle for the rtsp-keepalive thread — joined in [`Drop`].
    /// `None` when keepalive is disabled or hasn't been spawned yet.
    pub(crate) keepalive_thread: Option<std::thread::JoinHandle<()>>,
    /// Interleaved-pump state — `Some` after a successful TCP-interleaved
    /// SETUP has activated the producer thread that drains the control
    /// TCP into [`mpsc`] channels (data / rtcp / ctrl). When this is
    /// `Some`, `send_and_read` writes the outbound request under the
    /// stream mutex but reads the response from
    /// [`InterleavedPumpState::ctrl_rx`] (matching by CSeq) — reading the
    /// stream directly would race against the pump.
    pub(crate) pump_state: Option<InterleavedPumpState>,
    /// Cached auth state (challenge + `qop=auth` nonce-count), shared
    /// with the keepalive thread. Once a challenge is learned (first
    /// 401, usually at DESCRIBE), SETUP / PLAY / PAUSE / TEARDOWN
    /// attach an `Authorization` header pre-emptively — servers such as
    /// gortsplib / MediaMTX require auth on every method and reject an
    /// unauthenticated SETUP even after an authenticated DESCRIBE.
    pub(crate) auth: Arc<Mutex<AuthState>>,
    /// Stream-write hand-off gate, shared by every writer that contends
    /// with the interleaved pump for the stream mutex: the count of
    /// writers currently waiting for (or holding) the lock. A writer
    /// `fetch_add(1)`s before locking and `fetch_sub(1)`s after
    /// releasing; the pump skips its blocking read while the count is
    /// nonzero, so a writer acquires within at most one in-flight read
    /// cycle. A counter (not a bool) because two writers — a control
    /// request on the main thread and a keepalive ping — can overlap,
    /// and a bool cleared by whichever finishes first would drop the
    /// other's yield request. Created at connect time (before the pump
    /// exists) so the keepalive thread can hold a clone from its spawn;
    /// gate traffic is harmless while no pump is running.
    pub(crate) write_gate: Arc<AtomicUsize>,
    /// Structured record of why the session ended — first-writer-wins,
    /// written by the interleaved pump's exit sites and the keepalive
    /// thread's failure sites (see [`end_reason::StreamEndReason`]).
    /// Created unconditionally at connect time (unlike `session_dead`,
    /// which stays `None` until a keepalive is spawned) so a client that
    /// never spawns a keepalive or pump still has a slot to clone into
    /// [`session::RtspSession`] — closed only by that transport's own
    /// cancel/close path.
    pub(crate) end_reason: end_reason::EndReasonSlot,
}

/// `WWW-Authenticate` challenge cache + `qop=auth` nonce-count pair.
///
/// The two live under ONE mutex — never split them: an `nc` allocation
/// must pair atomically with the challenge (nonce) it is sent with, or
/// a challenge rotation racing the keepalive thread could repeat an
/// `nc` for a reused nonce, which a `qop=auth` server rejects as a
/// replay (RFC 7616 §3.4).
#[derive(Debug, Default)]
pub(crate) struct AuthState {
    /// Cached challenge from the most recent 401.
    pub(crate) challenge: Option<String>,
    /// Monotonic nonce-count for the CURRENT challenge; reset to 0
    /// when a *different* challenge is cached (a new nonce restarts
    /// the count at 1).
    pub(crate) nc: u32,
}

impl AuthState {
    /// Cache `www_auth` as the current challenge, resetting the
    /// nonce-count when the challenge changed (a new nonce restarts the
    /// count at 1 — RFC 7616 §3.4). Shared by the main-thread 401 path
    /// ([`RtspClient::cache_auth_challenge`]) and the keepalive-response
    /// handler ([`keepalive::handle_keepalive_response`]).
    pub(crate) fn cache_challenge(&mut self, www_auth: String) {
        if self.challenge.as_deref() != Some(www_auth.as_str()) {
            self.nc = 0;
        }
        self.challenge = Some(www_auth);
    }
}

/// RAII participant in [`RtspClient::write_gate`]: increments the
/// waiting-writers count on construction, decrements on drop — INCLUDING
/// panic unwind. A writer that panics between increment and decrement
/// would otherwise leave the gate stuck nonzero, and the pump — which
/// skips its read while the gate is nonzero — would spin forever without
/// ever touching the mutex, wedging the session instead of failing it.
/// (Locking itself can no longer panic on a poisoned mutex — see
/// [`lock_unpoisoned`] — but the guard still protects any other panic
/// that might occur while the gate is held, e.g. inside `write_all`.)
pub(crate) struct WriteGateGuard<'a>(&'a AtomicUsize);

impl<'a> WriteGateGuard<'a> {
    pub(crate) fn enter(gate: &'a AtomicUsize) -> Self {
        gate.fetch_add(1, Ordering::Relaxed);
        Self(gate)
    }
}

impl Drop for WriteGateGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Poison-recovering lock for the RTSP client's internal mutexes.
///
/// Crate policy (mirrors the tst-pipeline shells): these mutexes guard an
/// I/O stream and plain state cells with no torn-state invariants — a
/// panic in a peer thread (e.g. the keepalive pinger) must not cascade
/// into a panic here. Recovering keeps every path panic-free, including
/// the best-effort TEARDOWN inside `Drop` (a panic there during an unwind
/// would be a process abort no consumer `catch_unwind` can contain). A
/// half-written request left by the panicked thread surfaces as a normal
/// protocol/timeout error on the next exchange.
pub(crate) fn lock_unpoisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Millisecond count of `d`, saturating at `u64::MAX`. A plain
/// `as_millis() as u64` cast silently truncates for durations past
/// ~584 million years — a hostile-but-spec-legal `Session: timeout=`
/// near `u64::MAX` seconds would wrap the derived keepalive cadence
/// into a tiny value and turn the pinger into a hot loop.
pub(crate) fn duration_ms_saturating(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// State the main thread keeps about the interleaved producer thread.
///
/// Owned by [`RtspClient`] (one pump per client, since one TCP control
/// connection per client). The pump thread is reaped in `Drop`.
#[derive(Debug)]
pub(crate) struct InterleavedPumpState {
    /// Pump-only cancel flag (separate from `RtspClient::cancel` so we
    /// can stop the pump without stopping the rest of the client; in
    /// practice they're flipped together at `Drop`).
    pub(crate) cancel: Arc<AtomicBool>,
    /// Control-write hand-off gate — clone of [`RtspClient::write_gate`]
    /// (see its doc for the counter protocol). Kept here so the pump
    /// thread and the send path resolve it without reaching back into
    /// the client.
    pub(crate) write_gate: Arc<AtomicUsize>,
    /// Receiver for RTSP responses parsed by the pump. The pump pushes
    /// each `CRLFCRLF`+body-bounded RTSP response here; `send_and_read`
    /// polls this matching by CSeq once pump mode is active.
    pub(crate) ctrl_rx: mpsc::Receiver<Bytes>,
    /// Pump-thread handle; joined in `Drop` after `cancel` is flipped.
    pub(crate) thread: Option<std::thread::JoinHandle<()>>,
}

/// Cancel handle for the RTSP client. Covers the control plane; the
/// transport plane (post-PLAY RTP data) uses its own
/// [`crate::RtpCancelHandle`] returned from the
/// [`crate::RtpRecvTransport`].
#[derive(Clone)]
pub struct RtspCancelHandle {
    cancel: Arc<AtomicBool>,
}

impl RtspCancelHandle {
    /// Signal the client to break out of blocking I/O at the next poll.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Has [`Self::cancel`] been called?
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Per-connection knobs carried from [`RtspClientBuilder`](crate::RtspClientBuilder)
/// into the connect path. Internal — the public `connect`/`connect_with`/
/// `connect_with_roots` entry points use [`ConnectParams::default`].
pub(crate) struct ConnectParams {
    /// TCP connect timeout.
    pub(crate) connect_timeout: Duration,
    /// Per-read socket timeout (cancel/interleaved-frame poll interval).
    pub(crate) read_timeout: Duration,
    /// Per-request response deadline (`RtspClientBuilder::request_timeout`);
    /// `None` = unbounded.
    pub(crate) request_timeout: Option<Duration>,
    /// `User-Agent:` header value sent on every outbound request.
    pub(crate) user_agent: String,
}

impl Default for ConnectParams {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_millis(100),
            request_timeout: Some(Duration::from_secs(10)),
            user_agent: "tst-rtp/0.1".into(),
        }
    }
}

impl RtspClient {
    /// Connect to an `rtsp://` or `rtsps://` URL. The `rtsps://` scheme
    /// requires the `tls` cargo feature; otherwise it returns
    /// [`RtspError::Tls`].
    ///
    /// # Errors
    ///
    /// - [`RtspError::Url`] if the URL cannot be parsed.
    /// - [`RtspError::Io`] on socket-level failure (DNS, refused, etc.).
    /// - [`RtspError::Tls`] if the URL scheme is `rtsps://` and the
    ///   `tls` cargo feature is not enabled, or on rustls handshake
    ///   failure (server name validation, untrusted cert, etc.).
    pub fn connect(url: &str) -> Result<Self, RtspError> {
        let parsed = RtspUrl::parse(url)?;
        Self::connect_with(&parsed)
    }

    /// Connect using an already-parsed URL.
    ///
    /// `rtsps://` URLs are not supported by this entry point on a build
    /// without the `tls` cargo feature; they return
    /// [`RtspError::Tls`] in that case.
    ///
    /// # Errors
    ///
    /// See [`Self::connect`].
    pub fn connect_with(url: &RtspUrl) -> Result<Self, RtspError> {
        Self::connect_with_params(url, None, ConnectParams::default())
    }

    /// Connect with an optional client-side TLS root-cert store.
    ///
    /// `roots = None` falls back to the platform native trust roots
    /// (loaded via `rustls-native-certs`). `roots = Some(custom)` is
    /// used by `RtspClientBuilder::tls_root_certs` callers that need
    /// to trust a self-signed cert (e.g., test fixtures).
    ///
    /// For plain `rtsp://` URLs the roots argument is ignored.
    ///
    /// Uses the default connect/read timeouts and User-Agent. To
    /// customize those, use [`RtspClientBuilder`](crate::RtspClientBuilder).
    ///
    /// # Errors
    ///
    /// See [`Self::connect`].
    pub fn connect_with_roots(
        url: &RtspUrl,
        #[cfg(feature = "tls")] roots: Option<rustls::RootCertStore>,
        #[cfg(not(feature = "tls"))] roots: Option<()>,
    ) -> Result<Self, RtspError> {
        Self::connect_with_params(url, roots, ConnectParams::default())
    }

    /// Connect with explicit per-connection parameters (timeouts +
    /// User-Agent). Internal: callers use [`Self::connect_with_roots`]
    /// for defaults or [`RtspClientBuilder`](crate::RtspClientBuilder) to
    /// override `params`. Holds the real connect logic that the public
    /// entry points delegate to.
    pub(crate) fn connect_with_params(
        url: &RtspUrl,
        #[cfg(feature = "tls")] roots: Option<rustls::RootCertStore>,
        #[cfg(not(feature = "tls"))] roots: Option<()>,
        params: ConnectParams,
    ) -> Result<Self, RtspError> {
        let _ = &roots; // silence unused on non-tls builds
        let is_tls = matches!(url.scheme(), RtspScheme::Rtsps);
        #[cfg(not(feature = "tls"))]
        if is_tls {
            return Err(RtspError::Tls(
                "TLS support requires the 'tls' cargo feature".into(),
            ));
        }

        let host_port = (url.host.as_str(), url.port);
        let mut addrs = host_port
            .to_socket_addrs()
            .map_err(|e| RtspError::Io(e.kind()))?;
        let peer = addrs
            .next()
            .ok_or(RtspError::Io(std::io::ErrorKind::AddrNotAvailable))?;
        let tcp = TcpStream::connect_timeout(&peer, params.connect_timeout)
            .map_err(|e| RtspError::Io(e.kind()))?;
        tcp.set_read_timeout(Some(params.read_timeout))
            .map_err(|e| RtspError::Io(e.kind()))?;
        tcp.set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| RtspError::Io(e.kind()))?;
        tcp.set_nodelay(true).ok();
        // `?tcp_keepalive=` / `RtspClientBuilder::tcp_keepalive`: applied
        // to the raw TCP socket before any TLS wrap so it covers rtsps://
        // too. Unlike nodelay above this propagates failure — the caller
        // explicitly asked for keepalive, so silently running without it
        // would defeat the dead-peer detection they configured.
        if let Some(idle) = url.tcp_keepalive {
            let ka = socket2::TcpKeepalive::new().with_time(idle);
            socket2::SockRef::from(&tcp)
                .set_tcp_keepalive(&ka)
                .map_err(|e| RtspError::Io(e.kind()))?;
        }

        // Branch the stream construction. For `rtsps://` we hand the
        // TCP socket to the rustls handshake; the resulting TlsStream
        // exposes the same Read+Write shape the rest of the client
        // expects.
        let stream = if is_tls {
            #[cfg(feature = "tls")]
            {
                Stream::Tls(Box::new(tls::TlsStream::connect(url, tcp, roots)?))
            }
            #[cfg(not(feature = "tls"))]
            {
                // Unreachable: the early-return above already short-
                // circuited. Kept as a compile-time guard.
                unreachable!("tls feature disabled but rtsps:// reached connect path")
            }
        } else {
            Stream::Plain(tcp)
        };

        Ok(Self {
            stream: Arc::new(Mutex::new(stream)),
            url: url.clone(),
            peer,
            next_cseq: AtomicU32::new(1),
            session_id: None,
            session_timeout: Duration::from_secs(60),
            cancel: Arc::new(AtomicBool::new(false)),
            last_server_version: RtspVersion::V1_0,
            session_dead: None,
            session_id_shared: None,
            keepalive_interval_shared: None,
            keepalive_interval_overridden: false,
            user_agent: params.user_agent,
            request_timeout: params.request_timeout,
            keepalive_thread: None,
            pump_state: None,
            auth: Arc::new(Mutex::new(AuthState::default())),
            write_gate: Arc::new(AtomicUsize::new(0)),
            end_reason: end_reason::EndReasonSlot::default(),
        })
    }

    /// Get a clone-able cancel handle.
    pub fn cancel_handle(&self) -> RtspCancelHandle {
        RtspCancelHandle {
            cancel: self.cancel.clone(),
        }
    }

    /// Server's reported RTSP version from the last response we parsed.
    pub fn last_server_version(&self) -> RtspVersion {
        self.last_server_version
    }

    /// Internal helper: get the next CSeq value.
    pub(crate) fn bump_cseq(&self) -> u32 {
        self.next_cseq.fetch_add(1, Ordering::Relaxed)
    }

    /// Spawn the background OPTIONS-pinger.
    ///
    /// `override_interval` lets callers force a specific cadence
    /// (typically supplied by `RtspClientBuilder::keepalive_interval`);
    /// when `None`, the cadence starts at `session_timeout / 2` and is
    /// retuned in place when SETUP learns the server-advertised timeout
    /// (the thread re-reads the shared interval cell at every wake, so
    /// there is never a reason to respawn).
    ///
    /// Idempotent: a no-op once a pinger is running. Spawning another
    /// thread would NOT stop the first (only close/`Drop` flips the
    /// shared `cancel` flag), so a second spawn would otherwise leave
    /// two pingers running — duplicate OPTIONS traffic — until the
    /// client closes; the first spawn's cadence therefore wins.
    //
    // Exposed `#[doc(hidden)] pub` so the integration test in
    // `tests/rtsp_client_keepalive.rs` can drive it without going
    // through `RtspClientBuilder`. The builder also calls this.
    ///
    /// # Errors
    ///
    /// Returns [`RtspError::Io`] if the OS refuses to spawn the keepalive
    /// thread (resource exhaustion). The error is propagated rather than
    /// panicked because this runs on the RTSP connect path and the JVM/C
    /// bindings do not catch unwinds across the FFI boundary.
    #[doc(hidden)]
    pub fn spawn_keepalive_if_needed(
        &mut self,
        override_interval: Option<Duration>,
    ) -> Result<(), RtspError> {
        if self.keepalive_thread.is_some() {
            return Ok(());
        }
        let interval = override_interval.unwrap_or(self.session_timeout / 2);
        // The cadence lives in a shared cell the thread re-reads at every
        // wake: SETUP retunes it to the server-advertised session timeout
        // (unless `override_interval` pinned it), because at spawn time —
        // the connect path — only the 60 s default is known.
        let interval_ms = Arc::new(AtomicU64::new(duration_ms_saturating(interval).max(1)));
        self.keepalive_interval_shared = Some(interval_ms.clone());
        self.keepalive_interval_overridden = override_interval.is_some();
        // Share the same `Arc<Mutex<Stream>>` with the keepalive thread.
        // Per-ping the thread locks the mutex, writes the OPTIONS bytes,
        // unlocks. Works uniformly for `Stream::Plain` AND `Stream::Tls`
        // — pre-T21 the Tls variant skipped keepalive entirely because
        // rustls `ClientConnection` isn't clonable.
        let write_half = self.stream.clone();
        let cancel = self.cancel.clone();
        let session_dead = Arc::new(AtomicBool::new(false));
        let session_id = Arc::new(Mutex::new(self.session_id.clone()));
        self.session_dead = Some(session_dead.clone());
        self.session_id_shared = Some(session_id.clone());
        // The keepalive thread shares the auth state directly (single
        // source of truth) plus a credential snapshot, so its OPTIONS
        // pings authenticate on servers that challenge them. The main
        // thread populates the state via `cache_auth_challenge` as the
        // challenge is learned (at DESCRIBE, after this spawn).
        let handle = keepalive::spawn(
            write_half,
            cancel,
            session_dead,
            interval_ms,
            self.url.render_no_credentials(),
            self.url.rtsp_version,
            session_id,
            self.user_agent.clone(),
            self.auth.clone(),
            self.url.username.clone(),
            self.url.password.clone(),
            self.write_gate.clone(),
            self.end_reason.clone(),
        )
        .map_err(|e| RtspError::Io(e.kind()))?;
        self.keepalive_thread = Some(handle);
        Ok(())
    }

    /// Spawn the interleaved producer thread (TCP-interleaved transport).
    ///
    /// Called from SETUP after a successful TCP-interleaved negotiation
    /// (see [`crate::rtsp::client::setup`]). The pump owns reads from
    /// the control TCP from this point on: it parses `$<ch><len><data>`
    /// frames, routes RTP payloads to `data_rx` (one of the channels
    /// returned here — the session hands it to `RtpRecvTransport`),
    /// routes RTCP payloads to `rtcp_rx` (the other channel returned
    /// here — T28 plumbs it into the `RtcpReporterHandle`), and routes
    /// RTSP responses to `InterleavedPumpState::ctrl_rx` so subsequent
    /// [`Self::send_and_read`] calls can match by CSeq.
    ///
    /// Idempotent in the sense that calling it twice produces a fresh
    /// pump and drops the previous one (the previous pump's `cancel`
    /// flips, its `data_rx` becomes unfed and the receiver-transport
    /// side will see `mpsc::RecvError`).
    ///
    /// Returns `(data_rx, rtcp_rx)`. Prior to Phase 4 Stage 3 (T27) the
    /// pump's RTCP receiver was consumed by a tiny `rtsp-rtcp-drain`
    /// std::thread that discarded everything; that drain has been
    /// removed and the receiver is now returned upward so a caller (T28)
    /// can route RTCP frames into the existing `RtcpReporterHandle`
    /// instead of black-holing them.
    ///
    /// # Errors
    ///
    /// Returns [`RtspError::Io`] if the OS refuses to spawn the pump thread
    /// (resource exhaustion). Propagated rather than panicked: this runs on
    /// the RTSP SETUP path and the JVM/C bindings do not catch unwinds
    /// across the FFI boundary.
    pub(crate) fn activate_interleaved_pump(
        &mut self,
        channels: interleaved_pump::InterleavedChannels,
    ) -> Result<(mpsc::Receiver<Bytes>, mpsc::Receiver<Bytes>), RtspError> {
        // Reap any prior pump (replacement semantics — should not
        // happen in normal SETUP flow, but be defensive).
        if let Some(prev) = self.pump_state.take() {
            prev.cancel.store(true, Ordering::Relaxed);
            if let Some(t) = prev.thread {
                let _ = t.join();
            }
        }

        // Bounded hand-off queues (B3 / T1-RTSP-QUEUE): a fast or malicious
        // server cannot flood these to OOM the client. Media drops newest +
        // counters on overflow; RTCP/control fail the session on overflow.
        // See the per-class `*_QUEUE_BOUND` rationale in `interleaved_pump`.
        let (data_tx, data_rx) = mpsc::sync_channel::<Bytes>(interleaved_pump::DATA_QUEUE_BOUND);
        let (rtcp_tx, rtcp_rx) = mpsc::sync_channel::<Bytes>(interleaved_pump::RTCP_QUEUE_BOUND);
        let (ctrl_tx, ctrl_rx) = mpsc::sync_channel::<Bytes>(interleaved_pump::CTRL_QUEUE_BOUND);
        let pump_cancel = Arc::new(AtomicBool::new(false));
        // Shared with the keepalive thread (which got its clone at spawn,
        // possibly before this pump existed) and the control send path.
        let write_gate = self.write_gate.clone();
        let stats = Arc::new(interleaved_pump::PumpStats::default());

        let reader = interleaved_pump::SharedStreamReader::new(self.stream.clone());
        let thread = interleaved_pump::spawn_client_pump(
            reader,
            data_tx,
            rtcp_tx,
            ctrl_tx,
            channels,
            pump_cancel.clone(),
            write_gate.clone(),
            // Shared with the main thread + keepalive thread: the pump
            // consumes keepalive responses, and a 401 among them must
            // refresh the same challenge cache the next ping signs with.
            self.auth.clone(),
            // `Some` iff the keepalive was spawned (its death flag) — the
            // pump flips it on a 454 keepalive response.
            self.session_dead.clone(),
            stats,
            self.end_reason.clone(),
        )
        .map_err(|e| RtspError::Io(e.kind()))?;

        self.pump_state = Some(InterleavedPumpState {
            cancel: pump_cancel,
            write_gate,
            ctrl_rx,
            thread: Some(thread),
        });

        Ok((data_rx, rtcp_rx))
    }

    /// Returns false once a keepalive has observed session death — a
    /// control-TCP write failed, or the server answered a keepalive ping
    /// with `454 Session Not Found`. Returns true when keepalive hasn't
    /// been started or hasn't observed a failure.
    pub fn is_session_alive(&self) -> bool {
        match &self.session_dead {
            Some(flag) => !flag.load(Ordering::Relaxed),
            None => true,
        }
    }
}

impl Drop for RtspClient {
    fn drop(&mut self) {
        // Signal cancel to the pump + keepalive threads FIRST, before the
        // best-effort TEARDOWN. The interleaved pump's `SharedStreamReader`
        // re-acquires the shared `Mutex<Stream>` on every read cycle
        // (~100 ms); if we attempt TEARDOWN while the pump is running, the
        // TEARDOWN write-lock acquisition is starved for a variable,
        // *unbounded* time — `std::sync::Mutex` is unfair and the lock
        // acquisition happens before (and so is not covered by) the
        // teardown deadline. That stalls Drop for seconds locally and >60 s
        // on the slower CI runners (it cancelled linux-aarch64's job and
        // unmasked on x86_64 once the cdylib-clobber fix let the test run).
        // Setting cancel first lets the pump exit at its next loop-top check
        // (within one read cycle) and stop re-locking, so the TEARDOWN write
        // acquires the lock promptly and Drop stays bounded.
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(pump) = self.pump_state.as_ref() {
            pump.cancel.store(true, Ordering::Relaxed);
        }
        // Best-effort TEARDOWN if a session is still active, now on the
        // uncontended stream. Bounded to 500 ms via teardown_with_deadline
        // so Drop stays fast even when the peer silently half-closed (e.g.
        // RtspServer::stop cancels per-session tasks but leaves the write
        // half open via lingering ActiveSession Arcs — TEARDOWN write
        // succeeds into the kernel buffer but no response ever comes back).
        if self.session_id.is_some() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            let _ = self.teardown_with_deadline(Some(deadline));
        }
        // Reap the threads (already winding down from the cancel above) so
        // they're joined before the TcpStream FD they hold is closed by the
        // main thread's `Drop`.
        if let Some(t) = self.keepalive_thread.take() {
            let _ = t.join();
        }
        if let Some(mut pump) = self.pump_state.take() {
            if let Some(t) = pump.thread.take() {
                let _ = t.join();
            }
            // The pump's RTCP `mpsc::Sender` is dropped along with the
            // pump thread that just exited; the rtcp_rx end was returned
            // upward at activate time (T27) and consumed by
            // `RtpRecvTransport::from_mpsc_with_rtcp` (T28).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn connect_to_loopback_listener_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept in background so connect_with doesn't hang.
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let url = format!("rtsp://127.0.0.1:{}/test", port);
        let c = RtspClient::connect(&url).unwrap();
        assert_eq!(c.peer.port(), port);
        assert!(matches!(c.url.scheme(), RtspScheme::Rtsp));
    }

    /// `?tcp_keepalive=` must actually reach the OS: SO_KEEPALIVE reads
    /// back enabled on the connected control socket, and stays at the OS
    /// default (off) when the knob is absent.
    #[test]
    fn tcp_keepalive_query_applies_to_control_socket() {
        let keepalive_state = |query: &str| -> bool {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let _ = listener.accept();
            });
            let url = format!("rtsp://127.0.0.1:{port}/test{query}");
            let c = RtspClient::connect(&url).unwrap();
            let stream = c.stream.lock().unwrap();
            match &*stream {
                Stream::Plain(tcp) => socket2::SockRef::from(tcp).keepalive().unwrap(),
                #[cfg(feature = "tls")]
                Stream::Tls(_) => unreachable!("plain rtsp:// connects yield Stream::Plain"),
            }
        };
        assert!(
            keepalive_state("?tcp_keepalive=30"),
            "knob set -> SO_KEEPALIVE enabled"
        );
        assert!(
            !keepalive_state(""),
            "knob absent -> SO_KEEPALIVE stays off"
        );
    }

    #[test]
    #[cfg(not(feature = "tls"))]
    fn rtsps_without_tls_feature_errors() {
        let e = RtspClient::connect("rtsps://localhost:322/test").unwrap_err();
        assert!(matches!(e, RtspError::Tls(_)));
    }

    /// A hostile-but-parseable `Session: timeout=` near `u64::MAX`
    /// seconds must saturate, not wrap through `as u64` into a tiny
    /// cadence (a hot ping loop).
    #[test]
    fn duration_ms_saturates_instead_of_truncating() {
        assert_eq!(
            duration_ms_saturating(Duration::from_secs(u64::MAX)),
            u64::MAX
        );
        assert_eq!(duration_ms_saturating(Duration::from_secs(30)), 30_000);
    }

    /// The gate decrement must survive a panic between enter and the end
    /// of the write scope — a stuck-nonzero gate makes the pump skip
    /// reads forever without ever observing the panic itself, wedging the
    /// session instead of failing it.
    #[test]
    fn write_gate_guard_decrements_on_panic() {
        let gate = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _gate = WriteGateGuard::enter(&gate);
            assert_eq!(gate.load(Ordering::Relaxed), 1);
            panic!("simulated poisoned-mutex expect");
        }));
        assert!(result.is_err());
        assert_eq!(
            gate.load(Ordering::Relaxed),
            0,
            "gate must be released on unwind"
        );
    }

    /// A second spawn while a pinger is already running must be a no-op —
    /// replacing the JoinHandle would NOT stop the first thread (only
    /// close/Drop flips the shared cancel flag), so two pingers would
    /// run until the client closes. Observable via the interval cell:
    /// the first spawn's cadence wins.
    #[test]
    fn spawn_keepalive_if_needed_is_noop_when_already_running() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let url = format!("rtsp://127.0.0.1:{}/test", port);
        let mut c = RtspClient::connect(&url).unwrap();
        c.spawn_keepalive_if_needed(Some(Duration::from_secs(2)))
            .unwrap();
        c.spawn_keepalive_if_needed(Some(Duration::from_secs(5)))
            .unwrap();
        let iv = c
            .keepalive_interval_shared
            .as_ref()
            .expect("interval cell set by the first spawn");
        assert_eq!(
            iv.load(Ordering::Relaxed),
            2000,
            "second spawn must not replace the running pinger or its cadence"
        );
    }

    #[test]
    fn cancel_handle_toggles_flag() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let url = format!("rtsp://127.0.0.1:{}/test", port);
        let c = RtspClient::connect(&url).unwrap();
        let h = c.cancel_handle();
        assert!(!h.is_cancelled());
        h.cancel();
        assert!(h.is_cancelled());
    }
}

#[cfg(test)]
mod poison_policy {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::{Duration, Instant};

    /// Fake peer: accepts one TCP connection, holds it open briefly, never
    /// replies, then closes. Enough for `connect()`; TEARDOWN writes land
    /// in the kernel buffer. Both `teardown_with_deadline` read paths
    /// honor the deadline (`send_and_read_with_deadline`); in this test
    /// the 200 ms peer close usually races ahead of the deadline — the
    /// client's next 100 ms-timeout read poll observes EOF — and either
    /// bound keeps the test prompt.
    fn client_against_silent_peer() -> RtspClient {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(200));
            drop(sock);
        });
        RtspClient::connect(&format!("rtsp://{addr}/s")).unwrap()
    }

    fn poison_stream_mutex(client: &RtspClient) {
        let stream = client.stream.clone();
        let _ = std::thread::spawn(move || {
            let _g = stream.lock().unwrap();
            panic!("deliberate poison");
        })
        .join(); // joins the Err — the mutex is now poisoned
    }

    #[test]
    fn drop_does_not_panic_on_poisoned_stream_mutex() {
        let mut client = client_against_silent_peer();
        // Make Drop take the best-effort TEARDOWN path (the `mod.rs`
        // Drop impl gates on `session_id.is_some()`).
        client.session_id = Some("12345".into());
        poison_stream_mutex(&client);
        let r = catch_unwind(AssertUnwindSafe(move || drop(client)));
        assert!(r.is_ok(), "Drop panicked on a poisoned stream mutex");
    }

    #[test]
    fn request_path_survives_poisoned_stream_mutex() {
        let mut client = client_against_silent_peer();
        client.session_id = Some("12345".into());
        poison_stream_mutex(&client);
        let deadline = Instant::now() + Duration::from_millis(300);
        let r = catch_unwind(AssertUnwindSafe(|| {
            client.teardown_with_deadline(Some(deadline))
        }));
        let inner = r.expect("request path panicked on a poisoned mutex");
        // The silent peer never replies — an Err is expected; a panic is the bug.
        assert!(inner.is_err());
    }
}
