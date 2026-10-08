//! RTSP server — accepts client connections, manages sessions, fans out
//! one Muxer's TS bytes to N connected peers.

pub mod auth;
pub mod fanout;
pub mod handlers;
pub mod listener;
pub mod mount;
pub mod multicast;
pub mod publish;
pub mod session;
#[cfg(feature = "rtsp-server-tls")]
pub mod tls;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::runtime::Runtime;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::builder::RtspServerBuilder;
use crate::cancel::RtspServerCancelHandle;
use crate::error::RtspServerError;
use crate::url::RtspScheme;

/// Bound of the on-demand publish-mount queue (see
/// [`RtspServerBuilder::accept_unregistered_publishers`]): handles created
/// by ANNOUNCE and not yet taken by [`RtspServer::next_publisher`]. An
/// ANNOUNCE that would create one more answers `503`.
pub(crate) const PUBLISH_QUEUE_BOUND: usize = 64;

/// Cap on on-demand publish mounts in the mount table (see
/// [`RtspServerBuilder::accept_unregistered_publishers`]), whether or not
/// their handles were taken: an ANNOUNCE that would create one more
/// answers `503`. Mounts registered with [`RtspServer::add_publish_mount`]
/// never count.
pub(crate) const ON_DEMAND_MOUNT_CAP: usize = 256;

/// Queue item: a publish mount an ANNOUNCE created on demand.
type PublishQueueItem = Arc<crate::rtsp::server::publish::mount::PublishMountState>;

/// Internal server state shared between the listener task, per-session
/// tasks, and mount handles. `Arc<ServerState>` lives as long as any
/// task references it; cloning is cheap.
pub(crate) struct ServerState {
    pub(crate) builder: RtspServerBuilder,
    /// Graceful-shutdown signal. `stop()` flips this; per-session tasks
    /// observe via `.cancelled()` await.
    pub(crate) cancel_token: CancellationToken,
    /// Hard-cancel signal (independent from graceful). Exposed publicly
    /// via [`RtspServer::cancel_handle`].
    pub(crate) hard_cancel: RtspServerCancelHandle,
    /// Mount path → mount entry; populated via `RtspServer::add_mount` /
    /// `add_multicast_mount` / `add_publish_mount`.
    pub(crate) mounts:
        std::sync::Mutex<std::collections::HashMap<String, crate::rtsp::server::mount::MountEntry>>,
    /// Live count of accepted (and not-yet-closed) client sessions.
    pub(crate) active_sessions: AtomicUsize,
    /// Cumulative RTP packets sent across all peers + all mounts.
    pub(crate) total_rtp_packets_sent: AtomicU64,
    /// Cumulative RTP bytes sent across all peers + all mounts.
    pub(crate) total_rtp_bytes_sent: AtomicU64,
    /// `start()` flips this once; `start()` returns AlreadyStarted on the
    /// second call.
    pub(crate) started: AtomicBool,
    /// `stop()` flips this. After this is true, public methods return
    /// `RtspServerError::Shutdown`.
    pub(crate) shutdown: AtomicBool,
    /// Bound address — set by the listener after kernel-assigns the port
    /// (when bind URL had `port = 0`). `start()` spin-waits on this.
    pub(crate) local_addr: std::sync::Mutex<Option<SocketAddr>>,
    /// Active session registry — populated by `session::handle_connection`
    /// on accept, removed on session end. Used by `stop()` to fan out the
    /// graceful-shutdown per-session cancel and the RFC 7826 §13.5.1
    /// Notice 5402 ("Server-Initiated TEARDOWN") message.
    pub(crate) sessions: std::sync::Mutex<Vec<Arc<ActiveSession>>>,
    /// Server-allocated CSeq counter for server-initiated requests
    /// (the only one today is the Notice 5402 ANNOUNCE in `stop()`).
    /// Seeded at 1_000_000 so it can't collide with client-allocated
    /// CSeqs (which always start at 1). The client doesn't ACK
    /// ANNOUNCE; this is a unidirectional notification.
    pub(crate) notice_cseq: AtomicU64,
    /// Pre-loaded TLS acceptor config for `rtsps://` binds. Loaded
    /// SYNCHRONOUSLY by `start()` — so bad cert/key paths fail `start()`
    /// itself instead of killing the listener task after `start()` has
    /// returned (the old silent-death wart). Taken by the listener task;
    /// `None` for plaintext binds.
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_config: std::sync::Mutex<Option<crate::rtsp::server::tls::TlsServerConfig>>,
    /// One-shot startup-result channel. `start()` installs the sender and
    /// blocks on the receiver; the listener task takes the sender and
    /// reports bind success (resolved local addr) or the typed bind error.
    /// This is what makes listener startup failures surface as `start()`
    /// errors instead of log-only silent death.
    pub(crate) startup_tx:
        std::sync::Mutex<Option<std::sync::mpsc::Sender<Result<SocketAddr, RtspServerError>>>>,
    /// Producer side of the on-demand publish-mount queue, bounded at
    /// [`PUBLISH_QUEUE_BOUND`]. The ANNOUNCE handler `try_send`s a mount
    /// it creates while holding the `mounts` lock (lock order: `mounts`,
    /// then this). `stop()` takes it, so a parked
    /// [`RtspServer::next_publisher`] wakes with `Shutdown` and a later
    /// on-demand ANNOUNCE answers `503`.
    pub(crate) publish_queue_tx:
        std::sync::Mutex<Option<std::sync::mpsc::SyncSender<PublishQueueItem>>>,
}

/// Lightweight per-session record kept on [`ServerState::sessions`] for
/// graceful-shutdown coordination. `session::handle_connection_inner`
/// populates `cancel` + `peer` at accept, mirrors `session_id` +
/// `mount_path` from [`session::ServerSessionState`] after each SETUP /
/// TEARDOWN, and stashes `tcp_write` after splitting the TCP. The
/// `tcp_write` mutex is the same `Arc` the per-peer fanout task uses for
/// RFC 7826 §14 interleaved RTP frames — `RtspServer::stop` takes the
/// same lock to write the Notice 5402 ANNOUNCE before cancelling the
/// session.
pub(crate) struct ActiveSession {
    /// RTSP session ID, once SETUP succeeded.
    pub(crate) session_id: std::sync::Mutex<Option<String>>,
    /// Mount path the client SETUP'd against.
    pub(crate) mount_path: std::sync::Mutex<Option<String>>,
    /// Per-session cancel — flipped by `stop()` to give an individual
    /// session a chance to flush and close cleanly. Observed by the
    /// per-session task's `tokio::select!` alongside `cancel_token`.
    pub(crate) cancel: CancellationToken,
    /// Peer address — captured at accept for logging + diagnostics.
    pub(crate) peer: SocketAddr,
    /// Async-locked write half of the per-session TCP. Set by
    /// `session::handle_connection_inner` once the `TcpStream` is split.
    /// `None` for unit-test constructions that don't drive a real
    /// connection (e.g. `register_session` in isolation). The fanout
    /// task + the per-session response writer share this same `Arc`;
    /// `RtspServer::stop` locks it to write the Notice 5402 ANNOUNCE.
    pub(crate) tcp_write: std::sync::Mutex<Option<Arc<AsyncMutex<OwnedWriteHalf>>>>,
}

impl ActiveSession {
    pub(crate) fn new(peer: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            session_id: std::sync::Mutex::new(None),
            mount_path: std::sync::Mutex::new(None),
            cancel: CancellationToken::new(),
            peer,
            tcp_write: std::sync::Mutex::new(None),
        })
    }
}

/// Register an active session with the server. Called by
/// `session::handle_connection` on accept; the returned `Arc` is held
/// by the per-session task for the connection's lifetime.
pub(crate) fn register_session(state: &Arc<ServerState>, peer: SocketAddr) -> Arc<ActiveSession> {
    let entry = ActiveSession::new(peer);
    if let Ok(mut g) = state.sessions.lock() {
        g.push(entry.clone());
    }
    entry
}

/// Remove an active session from the registry. Called by
/// `session::handle_connection` on disconnect / TEARDOWN / cancel.
pub(crate) fn unregister_session(state: &Arc<ServerState>, entry: &Arc<ActiveSession>) {
    if let Ok(mut g) = state.sessions.lock() {
        g.retain(|s| !Arc::ptr_eq(s, entry));
    }
}

/// RTSP server — accepts client connections, manages sessions, fans out
/// one Muxer's TS bytes to N connected peers.
///
/// Sync facade over an internal tokio `Runtime` (constructed in
/// `bind`/`build`, dropped in `Drop`). All public methods are sync; the
/// runtime is hidden from callers.
///
/// # Closing
///
/// Three shutdown patterns:
///
/// 1. **Drop** — fires the hard-cancel path: all per-session tasks abort
///    at their next poll, the runtime is shut down with a 5 s budget.
///    Implicit; no acknowledgement to connected clients.
/// 2. **Graceful — `stop()`** — sends an RTSP Notice (5402) to each
///    active session, allows up to
///    `RtspServerBuilder::graceful_shutdown_drain` for in-flight RTP to
///    drain, then closes the listener and runtime. Returns once drain
///    is done.
/// 3. **Hard cross-thread — `cancel_handle()`** — returns an
///    [`RtspServerCancelHandle`] that can be cancelled from any thread.
///    Equivalent to Drop's hard-cancel without dropping the handle.
pub struct RtspServer {
    pub(crate) state: Arc<ServerState>,
    pub(crate) runtime: Option<Runtime>,
    /// Consumer side of the on-demand publish-mount queue, read by
    /// [`Self::next_publisher`].
    publish_queue_rx: std::sync::Mutex<std::sync::mpsc::Receiver<PublishQueueItem>>,
}

impl std::fmt::Debug for RtspServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtspServer")
            .field("bind_url", &self.state.builder.bind_url)
            .field("started", &self.state.started.load(Ordering::Relaxed))
            .field("shutdown", &self.state.shutdown.load(Ordering::Relaxed))
            .field("local_addr", &*self.state.local_addr.lock().unwrap())
            .field(
                "active_sessions",
                &self.state.active_sessions.load(Ordering::Relaxed),
            )
            .field(
                "mounts",
                &self.state.mounts.lock().map(|m| m.len()).unwrap_or(0),
            )
            .finish()
    }
}

/// Shared path validation for `add_mount` / `add_multicast_mount` /
/// `add_publish_mount`: must start with `/` and avoid the URL-reserved
/// characters that would make the `extract_mount_path` lookup in
/// `handlers.rs` ambiguous.
pub(crate) fn validate_mount_path(path: &str) -> Result<(), RtspServerError> {
    if path.is_empty() || !path.starts_with('/') {
        return Err(RtspServerError::InvalidMountPath {
            detail: format!("path must start with '/'; got '{path}'"),
        });
    }
    if path.contains('?') || path.contains('#') {
        return Err(RtspServerError::InvalidMountPath {
            detail: format!("path contains URL-reserved character: '{path}'"),
        });
    }
    Ok(())
}

/// Random 32-bit SSRC seed for multicast mounts. Uses `getrandom`;
/// falls back to zero on the (impossible-in-practice) error path.
///
/// `pub(crate)` so that `handle_play` can seed a fresh per-peer SSRC
/// for each unicast subscriber.
pub(crate) fn rand_ssrc() -> u32 {
    let mut buf = [0u8; 4];
    let _ = getrandom::getrandom(&mut buf);
    u32::from_be_bytes(buf)
}

/// Random initial RTP sequence per RFC 3550 §5.1.
///
/// `pub(crate)` so that `handle_play` can seed a fresh per-peer initial
/// sequence number for each unicast subscriber.
pub(crate) fn rand_seq() -> u16 {
    let mut buf = [0u8; 2];
    let _ = getrandom::getrandom(&mut buf);
    u16::from_be_bytes(buf)
}

/// Build the wire bytes for a server-initiated `ANNOUNCE` request
/// carrying the RFC 7826 §13.5.1 Notice 5402 ("Server-Initiated
/// TEARDOWN") header. Hand-rolled rather than going through
/// [`crate::rtsp::message::RtspRequest`] because the latter normalises
/// headers via a HashMap (unordered iteration) and we want the wire
/// form to be deterministic + readable.
///
/// `host` is the server's bind host (interpolated into the request
/// URI); `port` is the listener's bound port. `mount_path` includes the
/// leading slash. The client doesn't ACK this request — there's no
/// response we wait for.
pub(crate) fn build_notice_5402_announce(
    host: &str,
    port: u16,
    mount_path: &str,
    session_id: &str,
    cseq: u64,
    server_value: &str,
) -> Vec<u8> {
    // RFC 7826 §13.5.1 / RFC 2326 §10.4 — Notice ("Notice") response
    // header on a server-initiated ANNOUNCE conveys an end-of-stream
    // condition. 5402 = "Server-Initiated TEARDOWN".
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(b"ANNOUNCE rtsp://");
    out.extend_from_slice(host.as_bytes());
    out.push(b':');
    out.extend_from_slice(port.to_string().as_bytes());
    out.extend_from_slice(mount_path.as_bytes());
    out.extend_from_slice(b" RTSP/1.0\r\n");
    out.extend_from_slice(b"CSeq: ");
    out.extend_from_slice(cseq.to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Notice: 5402 \"Server-Initiated TEARDOWN\"\r\n");
    out.extend_from_slice(b"Server: ");
    out.extend_from_slice(server_value.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Session: ");
    out.extend_from_slice(session_id.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"\r\n");
    out
}

/// Shared test-only `ServerState` builder. Both `handlers::tests` and
/// `publish::handlers::tests` need one; this is the single definition
/// so they don't duplicate the (fairly long) field list.
#[cfg(test)]
pub(crate) fn test_state() -> Arc<ServerState> {
    let builder = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    Arc::new(ServerState {
        builder,
        cancel_token: CancellationToken::new(),
        hard_cancel: RtspServerCancelHandle::new(),
        mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
        active_sessions: AtomicUsize::new(0),
        total_rtp_packets_sent: AtomicU64::new(0),
        total_rtp_bytes_sent: AtomicU64::new(0),
        started: AtomicBool::new(true),
        shutdown: AtomicBool::new(false),
        local_addr: std::sync::Mutex::new(Some("127.0.0.1:8554".parse().unwrap())),
        sessions: std::sync::Mutex::new(Vec::new()),
        notice_cseq: AtomicU64::new(1_000_000),
        #[cfg(feature = "rtsp-server-tls")]
        tls_config: std::sync::Mutex::new(None),
        startup_tx: std::sync::Mutex::new(None),
        publish_queue_tx: std::sync::Mutex::new(None),
    })
}

/// Test-only `ServerState` with `accept_unregistered_publishers` set to
/// `accept` and a live on-demand queue; returns the queue's receiver.
#[cfg(test)]
pub(crate) fn test_state_on_demand(
    accept: bool,
) -> (
    Arc<ServerState>,
    std::sync::mpsc::Receiver<PublishQueueItem>,
) {
    let mut builder = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    builder.accept_unregistered_publishers(accept);
    let (tx, rx) = std::sync::mpsc::sync_channel(PUBLISH_QUEUE_BOUND);
    let st = test_state();
    let st = Arc::new(ServerState {
        builder,
        publish_queue_tx: std::sync::Mutex::new(Some(tx)),
        ..Arc::try_unwrap(st).ok().expect("fresh test state")
    });
    (st, rx)
}

/// Shared test-only one-program H.264 `MuxerConfig` — the config a local
/// (muxer-backed) mount needs to construct against in a test. Shared by
/// `handlers::tests` and `publish::handlers::tests` for the same reason
/// as [`test_state`].
#[cfg(test)]
pub(crate) fn test_muxer_cfg() -> tst_core::mpegts::mux::MuxerConfig {
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

impl RtspServer {
    /// Internal — called from [`crate::builder::RtspServerBuilder::build`].
    /// Constructs the tokio Runtime and the shared `ServerState`.
    pub(crate) fn from_builder(b: RtspServerBuilder) -> Result<Self, RtspServerError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("tst-rtp-server")
            .build()
            .map_err(|e| RtspServerError::Io(e.kind()))?;
        let (publish_queue_tx, publish_queue_rx) =
            std::sync::mpsc::sync_channel(PUBLISH_QUEUE_BOUND);
        let state = Arc::new(ServerState {
            builder: b,
            cancel_token: CancellationToken::new(),
            hard_cancel: RtspServerCancelHandle::new(),
            mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_sessions: AtomicUsize::new(0),
            total_rtp_packets_sent: AtomicU64::new(0),
            total_rtp_bytes_sent: AtomicU64::new(0),
            started: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            local_addr: std::sync::Mutex::new(None),
            sessions: std::sync::Mutex::new(Vec::new()),
            notice_cseq: AtomicU64::new(1_000_000),
            #[cfg(feature = "rtsp-server-tls")]
            tls_config: std::sync::Mutex::new(None),
            startup_tx: std::sync::Mutex::new(None),
            publish_queue_tx: std::sync::Mutex::new(Some(publish_queue_tx)),
        });
        Ok(Self {
            state,
            runtime: Some(runtime),
            publish_queue_rx: std::sync::Mutex::new(publish_queue_rx),
        })
    }

    /// Convenience: `RtspServerBuilder::new(url)?.build()`.
    pub fn bind(url: &str) -> Result<Self, RtspServerError> {
        RtspServerBuilder::new(url)?.build()
    }

    /// Convenience: `RtspServerBuilder::with_url(url).build()`.
    pub fn bind_with(url: crate::url::RtspUrl) -> Result<Self, RtspServerError> {
        RtspServerBuilder::with_url(url).build()
    }

    /// Register a unicast mount under `path`. Returns a [`MountHandle`][crate::rtsp::server::mount::MountHandle]
    /// the caller pushes TS frames into. Multiple handles via
    /// [`MountHandle::clone`][crate::rtsp::server::mount::MountHandle::clone]
    /// can push from different threads.
    ///
    /// `path` must start with `/` and not contain URL-reserved characters
    /// like `?` or `#`.
    ///
    /// # Errors
    /// - [`RtspServerError::InvalidMountPath`] if `path` doesn't start with `/`
    ///   or is empty / contains URL-reserved characters.
    /// - [`RtspServerError::DuplicateMount`] if `path` is already registered.
    /// - [`RtspServerError::InvalidConfig`] if `MuxerConfig` validation fails.
    /// - [`RtspServerError::Shutdown`] if called after `stop()`.
    pub fn add_mount(
        &self,
        path: &str,
        cfg: tst_core::mpegts::mux::MuxerConfig,
    ) -> Result<crate::rtsp::server::mount::MountHandle, RtspServerError> {
        if self.state.shutdown.load(Ordering::Relaxed) {
            return Err(RtspServerError::Shutdown);
        }
        validate_mount_path(path)?;
        let mount_state = crate::rtsp::server::mount::MountState::new(
            path,
            crate::rtsp::server::mount::MountKind::Unicast,
            cfg,
            self.state.builder.fanout_capacity,
        )?;
        let mut mounts = self.state.mounts.lock().expect("mounts mutex");
        if mounts.contains_key(path) {
            return Err(RtspServerError::DuplicateMount {
                path: path.to_string(),
            });
        }
        mounts.insert(
            path.to_string(),
            crate::rtsp::server::mount::MountEntry::Local(mount_state.clone()),
        );
        Ok(crate::rtsp::server::mount::MountHandle { state: mount_state })
    }

    /// Register a publish mount under `path`. The server now accepts
    /// ANNOUNCE/RECORD against this path (RFC 2326 §10.3 / §10.11); the
    /// returned [`crate::rtsp::server::publish::PublishMountHandle`]
    /// hands the application an [`crate::transport::RtpRecvTransport`]
    /// fed by whichever publisher currently holds the mount, and the
    /// mount re-serves PLAY readers from the same TS bytes.
    ///
    /// # Errors
    /// - [`RtspServerError::InvalidMountPath`] — same rules as
    ///   [`Self::add_mount`].
    /// - [`RtspServerError::DuplicateMount`] — path already registered.
    /// - [`RtspServerError::Shutdown`] — server stopped.
    pub fn add_publish_mount(
        &self,
        path: &str,
    ) -> Result<crate::rtsp::server::publish::PublishMountHandle, RtspServerError> {
        if self.state.shutdown.load(Ordering::Relaxed) {
            return Err(RtspServerError::Shutdown);
        }
        validate_mount_path(path)?;
        let st = crate::rtsp::server::publish::mount::PublishMountState::new(
            path,
            self.state.builder.fanout_capacity,
        );
        let mut mounts = self.state.mounts.lock().expect("mounts mutex");
        if mounts.contains_key(path) {
            return Err(RtspServerError::DuplicateMount {
                path: path.to_string(),
            });
        }
        mounts.insert(
            path.to_string(),
            crate::rtsp::server::mount::MountEntry::Publish(st.clone()),
        );
        Ok(crate::rtsp::server::publish::PublishMountHandle { state: st })
    }

    /// Remove the mount at `path`, of any kind, and free the path for a
    /// later `add_mount` / `add_multicast_mount` / `add_publish_mount` (or
    /// an on-demand ANNOUNCE).
    ///
    /// Every session that has completed a SETUP on the mount, readers and
    /// a publish mount's publisher alike, is sent the RFC 7826 §13.5.1
    /// Notice 5402
    /// ("Server-Initiated TEARDOWN") ANNOUNCE and then cancelled, as
    /// [`Self::stop`] does for every session; each cancelled session closes
    /// its connection. A reader's stream therefore ends because its
    /// session ended. The Notice writes are best-effort and bounded at 1 s
    /// per session; there is no drain wait.
    ///
    /// - A publish mount's application transport (see
    ///   [`crate::rtsp::server::publish::PublishMountHandle::into_recv_transport`])
    ///   ends: a parked or later `recv_bytes` on it returns
    ///   `TransportError::Closed`. The mount's handles stay usable for
    ///   `stats()` and `generation()`; no publisher reaches it again.
    /// - A local mount's [`crate::rtsp::server::mount::MountHandle`] keeps
    ///   accepting pushes, which reach nobody: no session can find the
    ///   path any more. A mount added later at the same path is a new
    ///   mount; the old handle does not feed it.
    ///
    /// This is also how an application removes idle on-demand mounts
    /// (see [`RtspServerBuilder::accept_unregistered_publishers`]),
    /// including one whose ANNOUNCE created it and then failed later in
    /// the same request (for example a `500` while building the
    /// elementary-stream re-muxer): that mount stays in the table, idle,
    /// with its handle queued for [`Self::next_publisher`], until this
    /// call removes it.
    ///
    /// Sessions that have not completed a SETUP on the mount get no Notice
    /// and are not cancelled. A publisher between its ANNOUNCE and its first
    /// SETUP loses its publisher slot and is refused at SETUP (`404`; `455`
    /// if a publish mount has been registered at the path again, `461` if
    /// a local mount has). A SETUP answered while
    /// this call runs can still complete: a reader then keeps its
    /// subscription to the removed mount's fanout until it ends on its own,
    /// and a publisher's RECORD is refused with `455` because the mount no
    /// longer holds its publisher slot.
    ///
    /// # Errors
    /// - [`RtspServerError::MountNotFound`] — no mount is registered at
    ///   `path`.
    /// - [`RtspServerError::Shutdown`] — server stopped.
    pub fn remove_mount(&self, path: &str) -> Result<(), RtspServerError> {
        if self.state.shutdown.load(Ordering::Acquire) {
            return Err(RtspServerError::Shutdown);
        }
        // Take the entry under the mounts lock only; the Notice writes and
        // cancels below run with no server lock held.
        let entry = self
            .state
            .mounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(path)
            .ok_or_else(|| RtspServerError::MountNotFound {
                path: path.to_string(),
            })?;
        let sessions: Vec<Arc<ActiveSession>> = self
            .state
            .sessions
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|s| {
                        s.mount_path
                            .lock()
                            .is_ok_and(|m| m.as_deref() == Some(path))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.notify_and_cancel(&sessions);
        match entry {
            crate::rtsp::server::mount::MountEntry::Publish(m) => m.close(),
            // The broadcast sender lives in `MountState`, which outstanding
            // `MountHandle`s keep alive; readers ended with their sessions.
            crate::rtsp::server::mount::MountEntry::Local(_) => {}
        }
        Ok(())
    }

    /// Wait up to `timeout` for the next publish mount an ANNOUNCE created
    /// on demand (see
    /// [`RtspServerBuilder::accept_unregistered_publishers`]). Mounts come
    /// out in the order their ANNOUNCEs created them; the announcing
    /// publisher already holds each one's publisher slot. Returns
    /// `Ok(None)` when none arrived in time, and always does when the flag
    /// is off.
    ///
    /// Concurrent callers are served one at a time, each mount to exactly
    /// one of them. A call made while another caller is waiting can
    /// therefore wait longer than its own `timeout`.
    ///
    /// # Errors
    /// - [`RtspServerError::Shutdown`] — the server was stopped, including
    ///   while this call waited.
    pub fn next_publisher(
        &self,
        timeout: Duration,
    ) -> Result<Option<crate::rtsp::server::publish::PublishMountHandle>, RtspServerError> {
        if self.state.shutdown.load(Ordering::Acquire) {
            return Err(RtspServerError::Shutdown);
        }
        let rx = self
            .publish_queue_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match rx.recv_timeout(timeout) {
            Ok(state) => Ok(Some(crate::rtsp::server::publish::PublishMountHandle {
                state,
            })),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(RtspServerError::Shutdown),
        }
    }

    /// Register a multicast mount. The provided `group_url` is an
    /// `rtp://<mcast-ip>:<port>?ttl=N&iface=ethN` URL pointing at the
    /// multicast group + port to publish on. A single per-mount
    /// background task drains the broadcast and sends to the group;
    /// per-client SETUP responses point clients at the group.
    ///
    /// # Errors
    /// - [`RtspServerError::InvalidMountPath`] — same rules as
    ///   [`Self::add_mount`].
    /// - [`RtspServerError::InvalidMulticastGroup`] — `group_url` is
    ///   malformed or the host isn't multicast.
    /// - [`RtspServerError::DuplicateMount`] — path already registered.
    /// - [`RtspServerError::InvalidConfig`] — `MuxerConfig` validation
    ///   failed.
    /// - [`RtspServerError::Shutdown`] — server stopped.
    ///
    /// # Panics
    ///
    /// Only if the server's mount-table mutex is poisoned (a previous
    /// panic while it was held). The per-mount sender runs as a task on
    /// the server runtime: a panic inside it ends that task, not the
    /// server, and a failure to build the multicast socket is logged at
    /// `error` level and leaves the mount inactive.
    pub fn add_multicast_mount(
        &self,
        path: &str,
        cfg: tst_core::mpegts::mux::MuxerConfig,
        group_url: &str,
    ) -> Result<crate::rtsp::server::mount::MountHandle, RtspServerError> {
        if self.state.shutdown.load(Ordering::Relaxed) {
            return Err(RtspServerError::Shutdown);
        }
        validate_mount_path(path)?;
        let mcast = crate::url::MulticastGroup::parse(group_url).map_err(|e| {
            RtspServerError::InvalidMulticastGroup {
                addr: group_url.to_string(),
                detail: e.to_string(),
            }
        })?;
        let mount_state = crate::rtsp::server::mount::MountState::new(
            path,
            crate::rtsp::server::mount::MountKind::Multicast {
                group: mcast.addr,
                ttl: mcast.ttl,
                iface: mcast.iface.clone(),
            },
            cfg,
            self.state.builder.fanout_capacity,
        )?;
        let mut mounts = self.state.mounts.lock().expect("mounts mutex");
        if mounts.contains_key(path) {
            return Err(RtspServerError::DuplicateMount {
                path: path.to_string(),
            });
        }
        mounts.insert(
            path.to_string(),
            crate::rtsp::server::mount::MountEntry::Local(mount_state.clone()),
        );
        // Spawn the per-mount multicast sender task. The send socket is
        // built async on the runtime; we use spawn so add_multicast_mount
        // can return synchronously. If the socket build fails, the task
        // logs and exits — caller observes via tracing/stats, not via
        // the return value (matches the unicast handle pattern where
        // listener errors don't unwind to the caller).
        let rt = self.runtime.as_ref().expect("runtime present until Drop");
        let mount_clone = mount_state.clone();
        let cancel = self.state.cancel_token.clone();
        let group = mcast.addr;
        let ttl = mcast.ttl;
        let iface = mcast.iface.clone();
        let drop_counter = crate::rtsp::server::fanout::PeerDropCounter::with_mount_total(
            std::sync::Arc::clone(&mount_state.frames_dropped),
        );
        rt.spawn(async move {
            match crate::rtsp::server::multicast::build_multicast_send_socket(
                group,
                ttl,
                iface.as_deref(),
            )
            .await
            {
                Ok(sock) => {
                    let sock = Arc::new(sock);
                    let rx = mount_clone.fanout.subscribe();
                    let _join = crate::rtsp::server::multicast::spawn_multicast_sender(
                        rx,
                        sock,
                        cancel,
                        rand_ssrc(),
                        rand_seq(),
                        drop_counter,
                    );
                    // The spawn_multicast_sender returns a JoinHandle we
                    // intentionally drop — the task lives until cancel
                    // or broadcast::Closed (which happens when MountState
                    // is dropped → fanout sender drops → all subscribers
                    // get Closed).
                }
                Err(e) => {
                    tracing::error!(
                        target: "tst_rtp::server::multicast",
                        error = ?e,
                        group = ?group,
                        "failed to build multicast send socket; mount inactive"
                    );
                }
            }
        });
        Ok(crate::rtsp::server::mount::MountHandle { state: mount_state })
    }

    /// Begin accepting client connections. Loads + validates any TLS
    /// config synchronously, then spawns the listener task on the
    /// internal runtime and blocks (bounded) on its startup report.
    /// Returns once `local_addr()` reflects the bound port.
    ///
    /// # Errors
    /// - [`RtspServerError::AlreadyStarted`] if called twice.
    /// - [`RtspServerError::Shutdown`] if called after `stop()`.
    /// - [`RtspServerError::Tls`] on a missing/malformed cert or key path
    ///   for an `rtsps://` bind, if the `rtsp-server-tls` feature isn't
    ///   enabled, or if TLS paths are configured on a plaintext `rtsp://`
    ///   bind (refusing to start an accidentally-unencrypted server).
    /// - [`RtspServerError::BindAddrInUse`] / [`RtspServerError::Io`] on
    ///   listener bind failure.
    pub fn start(&self) -> Result<(), RtspServerError> {
        if self.state.shutdown.load(Ordering::Relaxed) {
            return Err(RtspServerError::Shutdown);
        }
        if self.state.started.swap(true, Ordering::AcqRel) {
            return Err(RtspServerError::AlreadyStarted);
        }
        // Load + validate the TLS config SYNCHRONOUSLY, before the
        // listener task exists: a bad cert/key path must fail start()
        // itself. The loaded config rides ServerState for the listener.
        let is_tls = matches!(self.state.builder.bind_url.scheme(), RtspScheme::Rtsps);
        // TLS material on a non-rtsps bind: refuse to start. The old
        // behavior silently ignored the configured TLS cert/key paths and came up
        // PLAINTEXT — the caller believed TLS was armed. Python feeds this
        // same builder, so erroring here closes both surfaces at once; the
        // JVM fails fast earlier at build(), and the C ABI — which cannot
        // reach this guard (tst-c builds without the `rtsp-server-tls`
        // feature) — carries its own equivalent refusal in build_server. The
        // fields only exist under the `rtsp-server-tls` feature, so no
        // cfg(not(rtsp-server-tls)) twin is needed.
        #[cfg(feature = "rtsp-server-tls")]
        if !is_tls
            && (self.state.builder.tls_cert_path.is_some()
                || self.state.builder.tls_key_path.is_some())
        {
            // Nothing was spawned — un-latch `started` so the caller
            // isn't left holding a wedged object (same contract as the
            // TLS-load failure path below).
            self.state.started.store(false, Ordering::Release);
            return Err(RtspServerError::Tls(
                "TLS cert/key paths are configured but the bind URL scheme is \
                 plaintext rtsp:// — bind an rtsps:// URL or drop the TLS config \
                 (refusing to start an accidentally-unencrypted server)"
                    .into(),
            ));
        }
        #[cfg(feature = "rtsp-server-tls")]
        if is_tls {
            let loaded = (|| {
                let cert = self.state.builder.tls_cert_path.as_ref().ok_or_else(|| {
                    RtspServerError::Tls("rtsps:// bind requires tls_cert() builder call".into())
                })?;
                let key = self.state.builder.tls_key_path.as_ref().ok_or_else(|| {
                    RtspServerError::Tls("rtsps:// bind requires tls_cert() builder call".into())
                })?;
                crate::rtsp::server::tls::TlsServerConfig::load(cert, key)
            })();
            match loaded {
                Ok(cfg) => *self.state.tls_config.lock().unwrap() = Some(cfg),
                Err(e) => {
                    // Nothing was spawned — un-latch `started` so the
                    // caller isn't left holding a wedged object.
                    self.state.started.store(false, Ordering::Release);
                    return Err(e);
                }
            }
        }
        #[cfg(not(feature = "rtsp-server-tls"))]
        if is_tls {
            self.state.started.store(false, Ordering::Release);
            return Err(RtspServerError::Tls(
                "rtsps:// bind requires the 'rtsp-server-tls' cargo feature".into(),
            ));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        *self.state.startup_tx.lock().unwrap() = Some(tx);
        let state = self.state.clone();
        let rt = self.runtime.as_ref().expect("runtime present until Drop");
        rt.spawn(async move {
            if let Err(e) = crate::rtsp::server::listener::run_listener(state).await {
                tracing::error!(target: "tst_rtp::server", error = ?e, "listener exited with error");
            }
        });
        // Block (bounded) on the listener's startup report. Ok(addr)
        // means bound + local_addr published (callers may local_addr()
        // immediately). Err is the typed bind failure — BindAddrInUse
        // stops being log-only silent death here.
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(_addr)) => Ok(()),
            Ok(Err(e)) => {
                // The listener reported and exited — provably dead, so
                // un-latch `started`.
                self.state.started.store(false, Ordering::Release);
                Err(e)
            }
            // Runtime never scheduled the bind within 5 s. `started`
            // stays latched: the task state is unknown and a retry
            // could double-spawn a listener.
            Err(_) => Err(RtspServerError::Io(std::io::ErrorKind::TimedOut)),
        }
    }

    /// Graceful shutdown. Sends an RFC 7826 §13.5.1 Notice 5402
    /// ("Server-Initiated TEARDOWN") ANNOUNCE over each session's TCP
    /// control channel, then cancels each session's per-session token,
    /// fires the global `cancel_token` so the listener stops accepting
    /// new connections, and sleeps `graceful_shutdown_drain + 1s` to
    /// let in-flight RTP drain. Idempotent — a second call after a
    /// completed first call is a no-op.
    ///
    /// The ANNOUNCE write is best-effort: a per-session write failure
    /// (peer already disconnected, TCP reset, etc.) logs a warning and
    /// continues. A 1 s per-session write timeout bounds the worst case
    /// (a stuck peer that has stopped reading) so `stop()` can't hang
    /// arbitrarily on a slow client.
    ///
    /// Sessions for which `mount_path` and `session_id` haven't been
    /// populated yet (the client hasn't completed a SETUP) are skipped
    /// for the ANNOUNCE write — there's no `Session:` value to put on
    /// the wire — and proceed straight to per-session cancel.
    ///
    /// Publisher sessions are treated like readers (notice, cancel).
    /// Every publish mount's application transport (see
    /// [`crate::rtsp::server::publish::PublishMountHandle::into_recv_transport`])
    /// is then ended: a parked or later `recv_bytes` on it returns
    /// `TransportError::Closed`. A [`Self::next_publisher`] call waiting
    /// on another thread wakes with [`RtspServerError::Shutdown`], and an
    /// ANNOUNCE that would create an on-demand mount answers `503`.
    ///
    /// # Errors
    /// - [`RtspServerError::NotStarted`] if called before `start()`.
    pub fn stop(&self) -> Result<(), RtspServerError> {
        if !self.state.started.load(Ordering::Relaxed) {
            return Err(RtspServerError::NotStarted);
        }
        // Close the on-demand queue first (idempotent): dropping its only
        // sender wakes a parked `next_publisher` with `Disconnected` →
        // `Shutdown`, and an on-demand ANNOUNCE racing the rest of `stop()`
        // finds no sender and answers 503 instead of creating a mount the
        // publish-mount close below would miss.
        self.state
            .publish_queue_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if self.state.shutdown.swap(true, Ordering::AcqRel) {
            // Idempotent: already shut down.
            return Ok(());
        }
        // Snapshot the active session list. Iterate + send Notice 5402
        // ANNOUNCE + cancel each. The per-session task is responsible
        // for flushing any in-flight RTP + closing the TCP cleanly
        // within graceful_shutdown_drain.
        let sessions: Vec<Arc<ActiveSession>> = self
            .state
            .sessions
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        self.notify_and_cancel(&sessions);
        // End every publish mount's application transport: a parked (or
        // later) `recv_bytes` on it reads `Closed`. Idempotent with the
        // publisher session's own `end_publisher` on its way out.
        let publish_mounts: Vec<_> = self
            .state
            .mounts
            .lock()
            .map(|g| {
                g.values()
                    .filter_map(|m| match m {
                        crate::rtsp::server::mount::MountEntry::Publish(p) => Some(p.clone()),
                        crate::rtsp::server::mount::MountEntry::Local(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for m in &publish_mounts {
            m.close();
        }
        // Also fire the global cancel so the listener stops accepting
        // new connections and any per-task observers exit promptly.
        self.state.cancel_token.cancel();
        // Wait for the drain window — sessions should observe their
        // per-session cancel, flush, and exit within this time.
        let drain = self.state.builder.graceful_shutdown_drain + Duration::from_secs(1);
        std::thread::sleep(drain);
        // Explicitly shutdown each per-session TCP write half so peers
        // see FIN immediately instead of relying on Arc-drop. Without
        // this, lingering `Arc<AsyncMutex<OwnedWriteHalf>>` clones (held
        // by `state.sessions`, the fanout task, and any in-flight
        // handler) keep the write half open after the per-session task
        // returns — the kernel never sends FIN, and clients block in
        // their pump's `read()` until their own read timeout fires.
        // `RtspClient::Drop`'s 500 ms `teardown_with_deadline` masks the
        // symptom but the root cause is here. Each shutdown is bounded
        // by a 500 ms timeout to keep `stop()` sync-bounded.
        if let Some(rt) = self.runtime.as_ref() {
            use tokio::io::AsyncWriteExt;
            for s in &sessions {
                let Some(write_half) = s.tcp_write.lock().ok().and_then(|g| g.clone()) else {
                    continue;
                };
                let peer = s.peer;
                rt.block_on(async {
                    let shutdown_fut = async {
                        let mut guard = write_half.lock().await;
                        guard.shutdown().await
                    };
                    match tokio::time::timeout(Duration::from_millis(500), shutdown_fut).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            tracing::debug!(
                                target: "tst_rtp::server",
                                peer = %peer,
                                error = %e,
                                "graceful shutdown: write half shutdown returned error (peer likely gone)"
                            );
                        }
                        Err(_) => {
                            tracing::warn!(
                                target: "tst_rtp::server",
                                peer = %peer,
                                "graceful shutdown: write half shutdown timed out"
                            );
                        }
                    }
                });
            }
        }
        Ok(())
    }

    /// Send the RFC 7826 §13.5.1 Notice 5402 ANNOUNCE over each of
    /// `sessions`' TCP control channel, then cancel each session. Shared
    /// by [`Self::stop`] (every session) and [`Self::remove_mount`] (the
    /// sessions on one mount). Callers hold no server lock.
    ///
    /// Sessions without a `mount_path` or `session_id` yet (no completed
    /// SETUP) or without a write half get no ANNOUNCE, only the cancel.
    fn notify_and_cancel(&self, sessions: &[Arc<ActiveSession>]) {
        // Send the Notice 5402 ANNOUNCE over each session's TCP control
        // channel BEFORE cancelling. The write needs an async context
        // (the write half lives behind a tokio AsyncMutex); we
        // block-on the runtime to keep the caller sync. Each per-session
        // write is bounded by a 1 s timeout so a stuck peer can't hang
        // the caller indefinitely.
        if let Some(rt) = self.runtime.as_ref() {
            let bind_host = self.state.builder.bind_url.host.clone();
            let bind_port = self
                .state
                .local_addr
                .lock()
                .ok()
                .and_then(|g| *g)
                .map(|a| a.port())
                .unwrap_or(self.state.builder.bind_url.port);
            let server_value = format!("tst-rtp/{}", env!("CARGO_PKG_VERSION"));
            for s in sessions {
                let Some(mount_path) = s.mount_path.lock().ok().and_then(|g| g.clone()) else {
                    continue;
                };
                let Some(session_id) = s.session_id.lock().ok().and_then(|g| g.clone()) else {
                    continue;
                };
                let Some(write_half) = s.tcp_write.lock().ok().and_then(|g| g.clone()) else {
                    continue;
                };
                let cseq = self.state.notice_cseq.fetch_add(1, Ordering::Relaxed);
                let bytes = build_notice_5402_announce(
                    &bind_host,
                    bind_port,
                    &mount_path,
                    &session_id,
                    cseq,
                    &server_value,
                );
                let peer = s.peer;
                rt.block_on(async {
                    let write_fut = async {
                        let mut guard = write_half.lock().await;
                        guard.write_all(&bytes).await?;
                        guard.flush().await
                    };
                    match tokio::time::timeout(Duration::from_secs(1), write_fut).await {
                        Ok(Ok(())) => {
                            tracing::info!(
                                target: "tst_rtp::server",
                                peer = %peer,
                                "Notice 5402 ANNOUNCE sent"
                            );
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(
                                target: "tst_rtp::server",
                                peer = %peer,
                                error = %e,
                                "Notice 5402 ANNOUNCE write failed"
                            );
                        }
                        Err(_) => {
                            tracing::warn!(
                                target: "tst_rtp::server",
                                peer = %peer,
                                "Notice 5402 ANNOUNCE timed out"
                            );
                        }
                    }
                });
            }
        }
        for s in sessions {
            tracing::info!(
                target: "tst_rtp::server",
                peer = %s.peer,
                "server-initiated teardown: signaling session"
            );
            s.cancel.cancel();
        }
    }

    /// Listener's bound address, populated once `start()` returns. `None`
    /// before `start()` is called, or before the listener task gets
    /// scheduled (rare race; spin-wait in `start()` makes this
    /// observationally rare).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        *self.state.local_addr.lock().unwrap()
    }

    /// Hard-cancel handle. Cloning is cheap; multiple holders can race
    /// the cancel call (idempotent).
    pub fn cancel_handle(&self) -> RtspServerCancelHandle {
        self.state.hard_cancel.clone()
    }

    /// Snapshot of aggregate server stats.
    pub fn stats(&self) -> ServerStats {
        ServerStats {
            active_sessions: self.state.active_sessions.load(Ordering::Relaxed),
            total_rtp_packets_sent: self.state.total_rtp_packets_sent.load(Ordering::Relaxed),
            total_rtp_bytes_sent: self.state.total_rtp_bytes_sent.load(Ordering::Relaxed),
            mounts: self.state.mounts.lock().map(|m| m.len()).unwrap_or(0),
        }
    }
}

impl Drop for RtspServer {
    fn drop(&mut self) {
        // Hard-cancel path on Drop — graceful shutdown blocks too long
        // for an implicit Drop.
        self.state.hard_cancel.cancel();
        self.state.cancel_token.cancel();
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

/// Aggregate server stats snapshot, returned from
/// [`RtspServer::stats`].
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServerStats {
    pub active_sessions: usize,
    pub total_rtp_packets_sent: u64,
    pub total_rtp_bytes_sent: u64,
    pub mounts: usize,
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn bind_returns_server_with_runtime() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        // local_addr not set until start()
        assert!(server.local_addr().is_none());
        let stats = server.stats();
        assert_eq!(stats.active_sessions, 0);
        assert_eq!(stats.mounts, 0);
    }

    #[test]
    fn start_binds_listener_and_populates_local_addr() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        server.start().unwrap();
        // After start() returns, local_addr() should reflect the
        // kernel-assigned port.
        let addr = server.local_addr().expect("listener bound");
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert!(addr.port() > 0);
    }

    #[test]
    fn start_twice_errors_second_time() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        server.start().unwrap();
        let e = server.start().unwrap_err();
        assert!(matches!(e, RtspServerError::AlreadyStarted));
    }

    #[test]
    fn stop_before_start_errors() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let e = server.stop().unwrap_err();
        assert!(matches!(e, RtspServerError::NotStarted));
    }

    #[test]
    fn stop_is_idempotent() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        // Override the drain timing for the test so it doesn't take 1+ s.
        // (We can't, because graceful_shutdown_drain is on the builder
        // not the server. We accept the ~1.1 s in this test for clarity.)
        server.start().unwrap();
        server.stop().unwrap();
        server.stop().unwrap(); // No-op the second time.
    }

    #[test]
    fn cancel_handle_clone_shares_flag() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let h1 = server.cancel_handle();
        let h2 = server.cancel_handle();
        h1.cancel();
        assert!(h2.is_cancelled());
    }

    #[test]
    fn drop_shuts_down_runtime() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        drop(server);
        // No panic / hang means shutdown_timeout completed cleanly.
    }

    #[test]
    fn server_stats_default() {
        let s = ServerStats::default();
        assert_eq!(s.active_sessions, 0);
        assert_eq!(s.total_rtp_packets_sent, 0);
    }
}

#[cfg(test)]
mod listener_tests {
    use super::*;

    #[test]
    fn double_start_errors() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        server.start().unwrap();
        let e = server.start().unwrap_err();
        assert!(matches!(e, RtspServerError::AlreadyStarted));
    }

    #[test]
    fn start_then_local_addr_returns_port() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        server.start().unwrap();
        assert!(server.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn second_bind_to_same_port_fails() {
        let first = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        first.start().unwrap();
        let port = first.local_addr().unwrap().port();
        // Try to bind ANOTHER server to the same port. start() now blocks
        // on the listener's startup-result channel, so the bind failure
        // surfaces as a typed error from start() itself instead of only
        // being observable by polling local_addr() afterward.
        let second = RtspServer::bind(&format!("rtsp://127.0.0.1:{port}")).unwrap();
        let e = second.start().unwrap_err();
        assert!(matches!(e, RtspServerError::BindAddrInUse), "got {e:?}");
        assert!(
            second.local_addr().is_none(),
            "second bind should have failed"
        );
    }
}

#[cfg(test)]
mod add_mount_tests {
    use super::*;
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

    fn make_muxer_cfg() -> MuxerConfig {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    }

    #[test]
    fn add_mount_returns_handle_with_path() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
        assert_eq!(mount.mount_path(), "/live");
        assert_eq!(server.stats().mounts, 1);
    }

    #[test]
    fn add_mount_rejects_empty_path() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let e = server.add_mount("", make_muxer_cfg()).unwrap_err();
        assert!(matches!(e, RtspServerError::InvalidMountPath { .. }));
    }

    #[test]
    fn add_mount_rejects_path_without_leading_slash() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let e = server.add_mount("live", make_muxer_cfg()).unwrap_err();
        assert!(matches!(e, RtspServerError::InvalidMountPath { .. }));
    }

    #[test]
    fn add_mount_rejects_duplicate_path() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        server.add_mount("/live", make_muxer_cfg()).unwrap();
        let e = server.add_mount("/live", make_muxer_cfg()).unwrap_err();
        assert!(matches!(e, RtspServerError::DuplicateMount { .. }));
    }

    #[test]
    fn add_mount_path_with_query_char_rejected() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let e = server.add_mount("/live?x=1", make_muxer_cfg()).unwrap_err();
        assert!(matches!(e, RtspServerError::InvalidMountPath { .. }));
    }
}

#[cfg(test)]
mod add_multicast_mount_tests {
    use super::*;
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

    fn make_muxer_cfg() -> MuxerConfig {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    }

    #[test]
    fn add_multicast_mount_returns_handle_for_v4_group() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let mount = server
            .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
            .unwrap();
        assert_eq!(mount.mount_path(), "/mc");
        assert!(matches!(
            mount.mount_kind(),
            crate::rtsp::server::mount::MountKind::Multicast { .. }
        ));
    }

    #[test]
    fn add_multicast_mount_rejects_unicast_group() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let e = server
            .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://10.0.0.1:5004")
            .unwrap_err();
        assert!(matches!(e, RtspServerError::InvalidMulticastGroup { .. }));
    }

    #[test]
    fn add_multicast_mount_rejects_malformed_url() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let e = server
            .add_multicast_mount("/mc", make_muxer_cfg(), "not-a-url")
            .unwrap_err();
        assert!(matches!(e, RtspServerError::InvalidMulticastGroup { .. }));
    }

    #[test]
    fn add_multicast_mount_rejects_duplicate_path() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        server
            .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
            .unwrap();
        let e = server
            .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.2:5004")
            .unwrap_err();
        assert!(matches!(e, RtspServerError::DuplicateMount { .. }));
    }

    #[test]
    fn add_multicast_mount_carries_ttl_and_iface() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let mount = server
            .add_multicast_mount(
                "/mc",
                make_muxer_cfg(),
                "rtp://239.0.0.1:5004?ttl=2&iface=127.0.0.1",
            )
            .unwrap();
        match mount.mount_kind() {
            crate::rtsp::server::mount::MountKind::Multicast { ttl, iface, .. } => {
                assert_eq!(*ttl, 2);
                assert_eq!(iface.as_deref(), Some("127.0.0.1"));
            }
            _ => panic!("expected Multicast"),
        }
    }
}

#[cfg(test)]
mod add_publish_mount_tests {
    use super::*;
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

    fn make_muxer_cfg() -> MuxerConfig {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    }

    #[test]
    fn add_publish_mount_registers_and_rejects_duplicates() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let h = server.add_publish_mount("/pub").unwrap();
        assert_eq!(h.mount_path(), "/pub");
        assert!(matches!(
            server.add_publish_mount("/pub"),
            Err(RtspServerError::DuplicateMount { .. })
        ));
        assert!(matches!(
            server.add_mount("/pub", make_muxer_cfg()),
            Err(RtspServerError::DuplicateMount { .. })
        ));
        assert!(matches!(
            server.add_publish_mount("nope"),
            Err(RtspServerError::InvalidMountPath { .. })
        ));
        assert_eq!(server.stats().mounts, 1);
    }
}

#[cfg(test)]
mod graceful_shutdown_tests {
    use super::*;

    #[test]
    fn stop_iterates_and_cancels_active_sessions() {
        // Wire up: register a session manually, then call stop() and
        // verify the session's per-session cancel was fired. We don't
        // need a real per-session task here — the unit-of-behavior is
        // "stop() walks state.sessions and cancels each".
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        server.start().unwrap();
        let peer: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let entry = register_session(&server.state, peer);
        assert_eq!(server.state.sessions.lock().unwrap().len(), 1);
        assert!(!entry.cancel.is_cancelled());
        server.stop().unwrap();
        assert!(entry.cancel.is_cancelled());
    }

    #[test]
    fn unregister_session_drops_from_list() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let peer: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let entry = register_session(&server.state, peer);
        assert_eq!(server.state.sessions.lock().unwrap().len(), 1);
        unregister_session(&server.state, &entry);
        assert_eq!(server.state.sessions.lock().unwrap().len(), 0);
    }

    #[test]
    fn register_session_records_peer() {
        let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
        let peer: std::net::SocketAddr = "10.0.0.1:12345".parse().unwrap();
        let entry = register_session(&server.state, peer);
        assert_eq!(entry.peer, peer);
    }
}
