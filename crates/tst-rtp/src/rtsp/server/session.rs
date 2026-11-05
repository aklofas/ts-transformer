//! Per-connection async task — runs the RTSP request/response loop
//! against one client. Reads requests via tokio AsyncRead, dispatches
//! to handlers, writes responses back.
//!
//! Per-connection state lives in [`ServerSessionState`], whose [`Role`] is one
//! of `Idle` (before SETUP/ANNOUNCE and after TEARDOWN), `Reader` (SETUP done;
//! its `Playback` is `Ready`, `Playing` with the fanout task, or `Paused`) or
//! `Publisher` (ANNOUNCE claimed a publish mount). The reader's mount, session
//! id and transport (a UDP socket pair, interleaved channels or multicast)
//! live in the `Reader` variant; a SETUP while a reader is `Playing` or
//! `Paused` is refused with 455. TEARDOWN or a disconnect retires the fanout
//! and ends a publisher. DESCRIBE is stateless. A connection holds one role.
//!
//! Plain `rtsp://` and `rtsps://` connections share one request loop:
//! `handle_tcp_connection` and the `rtsp-server-tls`-gated
//! `handle_tls_connection` each box their write half as a [`SessionWriter`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::error::RtspServerError;
use crate::packet::RtpHeader;
use crate::rtsp::message::{RtspFraming, RtspMethod, RtspRequest};
use crate::rtsp::server::ServerState;
use crate::rtsp::server::auth::generate_nonce;
use crate::rtsp::server::fanout::PeerDropCounter;
use crate::rtsp::server::handlers;
use crate::rtsp::server::publish;
use crate::rtsp::server::publish::session::PublishSession;
use crate::rtsp::transport_negotiation::TransportResponse;

/// How long a session's exit path waits to shut its write half down.
const EXIT_SHUTDOWN_BOUND: std::time::Duration = std::time::Duration::from_millis(500);

/// The write half of one RTSP session's connection — plain TCP or TLS —
/// shared by the response writer, the interleaved fanout and
/// `RtspServer::stop`'s Notice 5402. The mutex keeps an RTSP response and
/// a `$`-framed RTP packet from interleaving mid-bytes.
///
/// Every writer of a `SessionWriter` flushes after its write, because the
/// TLS half may hold encoded records until flushed; [`write_flushed`] does
/// both under the caller's lock.
pub(crate) type SessionWriter = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// `write_all` then `flush` on a locked session writer (a no-op flush on
/// plain TCP).
pub(crate) async fn write_flushed<W: AsyncWrite + Unpin + ?Sized>(
    w: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    w.write_all(bytes).await?;
    w.flush().await
}

/// The session's publish idle window: its publisher and the configured
/// timeout, when `publish_idle_timeout` is set and the connection holds a
/// publisher. Only a recording publisher has an idle time
/// ([`PublishSession::idle_for`]); before RECORD the window never fires.
fn publish_window<'a>(
    state: &ServerState,
    session: &'a ServerSessionState,
) -> Option<(&'a PublishSession, std::time::Duration)> {
    Some((session.publisher()?, state.builder.publish_idle_timeout?))
}

/// A session deadline that has run out.
enum Expired {
    /// No SETUP and no request that passed a credential check within
    /// `READ_IDLE_TIMEOUT` of the request loop starting.
    Admission,
    /// A recording publisher received no media for its publish idle timeout.
    PublishIdle(std::time::Duration),
}

/// Which of the admission deadline and the publish idle window has run out,
/// read from the clocks themselves. The session loop checks this before
/// every read and every request, so a deadline holds even when a peer keeps
/// the read ready and a timer racing it in a `select!` never gets to fire.
fn expired_deadline(
    admission_deadline: Option<tokio::time::Instant>,
    state: &ServerState,
    session: &ServerSessionState,
) -> Option<Expired> {
    if admission_deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
        return Some(Expired::Admission);
    }
    let t = state.builder.publish_idle_timeout?;
    let idle = session.publisher()?.idle_for()?;
    (idle >= t).then_some(Expired::PublishIdle(t))
}

/// Log why an expired deadline closes the session.
fn warn_expired(peer: SocketAddr, expired: &Expired) {
    match expired {
        Expired::Admission => tracing::warn!(
            target: "tst_rtp::server",
            peer = %peer,
            timeout_secs = READ_IDLE_TIMEOUT.as_secs(),
            "no SETUP or authenticated request within the admission deadline; closing session"
        ),
        Expired::PublishIdle(t) => tracing::warn!(
            target: "tst_rtp::server",
            peer = %peer,
            timeout_ms = t.as_millis() as u64,
            "publish idle timeout; closing session"
        ),
    }
}

/// The bound for a reply write: the idle bound, cut to what is left of the
/// admission deadline while that deadline stands, so a reply parked on a
/// peer that stopped reading cannot carry the connection past it.
fn reply_bound(
    idle_bound: std::time::Duration,
    admission_deadline: Option<tokio::time::Instant>,
) -> std::time::Duration {
    admission_deadline.map_or(idle_bound, |d| {
        idle_bound.min(d.saturating_duration_since(tokio::time::Instant::now()))
    })
}

/// The one bounded writer for every reply a session sends: take the
/// writer lock, write and flush `bytes` and, when `shutdown_after`, shut
/// the write half, all raced against `bound`, the publish idle window and
/// both cancel tokens. `None` means a bound or a cancel won; the caller then
/// leaves the serve loop so the session drops (its Drop aborts a fanout
/// parked on the lock).
///
/// The lock is shared with the fanout task, which holds it for RFC 7826
/// §14 interleaved frames and may be parked mid-frame on a peer that never
/// reads. Unbounded, a reply would wait on it out of reach of the idle
/// reaper and the server's cancel.
///
/// With `publish` set, a recording publisher that receives no media for its
/// timeout ends the wait too, so a publisher that stops reading replies
/// cannot outlast the publish idle timeout. When that window runs out but
/// media arrived meanwhile, the write keeps waiting, up to `bound`. The
/// write future is created once and never restarted, so a partial reply is
/// never sent twice.
async fn write_bounded(
    write_half: &SessionWriter,
    bytes: &[u8],
    bound: std::time::Duration,
    publish: Option<(&PublishSession, std::time::Duration)>,
    server_cancel: &CancellationToken,
    session_cancel: &CancellationToken,
    shutdown_after: bool,
) -> Option<std::io::Result<()>> {
    let write = async {
        let mut guard = write_half.lock().await;
        write_flushed(&mut **guard, bytes).await?;
        if shutdown_after {
            guard.shutdown().await?;
        }
        Ok(())
    };
    tokio::pin!(write);
    let bound_at = tokio::time::Instant::now() + bound;
    loop {
        let mut wait = bound_at.saturating_duration_since(tokio::time::Instant::now());
        if let Some((p, t)) = publish
            && let Some(idle) = p.idle_for()
        {
            wait = wait.min(t.saturating_sub(idle));
        }
        tokio::select! {
            r = &mut write => return Some(r),
            _ = tokio::time::sleep(wait) => {
                if tokio::time::Instant::now() >= bound_at {
                    return None;
                }
                if publish.is_some_and(|(p, t)| p.idle_for().is_some_and(|d| d >= t)) {
                    return None;
                }
                // Media arrived during the wait: the window moved.
            }
            _ = server_cancel.cancelled() => return None,
            _ = session_cancel.cancelled() => return None,
        }
    }
}

pub(crate) fn session_writer<W: AsyncWrite + Send + Unpin + 'static>(w: W) -> SessionWriter {
    Arc::new(AsyncMutex::new(Box::new(w)))
}

/// RAII guard for one reserved `active_sessions` slot.
///
/// The accept loop reserves a slot atomically (a compare-exchange loop that
/// increments only while below `max_sessions`, so the counter never
/// overshoots the cap — over-cap connections are refused without touching
/// it) *before* spawning the per-session task, then moves this guard into
/// the task. `Drop` releases the slot (`fetch_sub`) on EVERY task exit path —
/// normal close, session error, or (for `rtsps://`) a TLS-handshake
/// failure that returns before the session loop ever runs. Centralizing
/// the decrement here is what prevents a leaked slot on the handshake-fail
/// path, where the old in-handler `fetch_sub` was never
/// reached.
pub(crate) struct SessionSlot(Arc<ServerState>);

impl SessionSlot {
    /// Construct a guard for an already-reserved slot (the accept loop has
    /// done the CAS reserve). Dropping it releases the slot.
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        SessionSlot(state)
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.active_sessions.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Per-read idle bound BEFORE a session exists (pre-SETUP): closes the
/// slow-loris window on a connection that has nothing to keep alive yet.
/// It is also the admission deadline: a connection that has neither
/// completed a SETUP nor passed a credential check this long after its
/// request loop started is closed, whatever it sends meanwhile.
/// Once SETUP has advertised a timeout the window is deliberately the
/// longer `timeout + grace` of [`post_setup_idle_bound`] (90 s at the
/// default) per RFC 7826 §18.49 — do not shorten it back to this constant;
/// memory is still bounded by the request-buffer cap, not by this timer.
const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Idle bound once SETUP has advertised `Session: <id>;timeout=N`:
/// RFC 7826 §18.49 lets the server reap a session that sent
/// nothing for `timeout`; clients ping at `timeout / 2` (ours, ffmpeg) or
/// close to `timeout` (live555 derivatives), so the bound is
/// `timeout + max(timeout / 2, 2 s)` — never less than advertised. A fixed
/// 30 s bound would reap every default (60 s) session at its second ping.
fn post_setup_idle_bound(session_timeout: std::time::Duration) -> std::time::Duration {
    session_timeout.saturating_add((session_timeout / 2).max(std::time::Duration::from_secs(2)))
}

/// How a reader's SETUP-allocated transport reaches the client.
#[derive(Clone)]
pub(crate) enum ReaderTransport {
    /// TCP-interleaved: RTP and RTCP ride the control connection on these
    /// channel numbers.
    Interleaved { channels: (u8, u8) },
    /// Unicast UDP: the server-allocated RTP+RTCP socket pair, plus the
    /// negotiated transport (it carries the client's ports).
    Udp {
        sockets: (Arc<tokio::net::UdpSocket>, Arc<tokio::net::UdpSocket>),
        negotiated: TransportResponse,
    },
    /// Multicast: the mount's own sender drives the group; nothing is
    /// allocated per session.
    Multicast,
}

/// Where a reader is in SETUP / PLAY / PAUSE.
pub(crate) enum Playback {
    /// SETUP done, no PLAY yet (also what a multicast PLAY leaves, since no
    /// per-session fanout exists).
    Ready,
    /// PLAY spawned the per-peer fanout task.
    Playing {
        fanout: tokio::task::JoinHandle<()>,
        /// Kept so the counter outlives the task's own clone for as long as
        /// the session is playing.
        #[allow(dead_code)]
        drop_counter: Arc<PeerDropCounter>,
    },
    /// PAUSE retired the fanout; a later PLAY spawns a new one.
    Paused,
}

/// A reader session: set up against one mount.
pub(crate) struct ReaderSession {
    pub(crate) session_id: String,
    pub(crate) mount_path: String,
    pub(crate) transport: ReaderTransport,
    pub(crate) playback: Playback,
}

/// The one role a connection holds. A connection is a reader or a publisher,
/// never both; TEARDOWN returns it to `Idle`.
pub(crate) enum Role {
    /// Before SETUP / ANNOUNCE, and after TEARDOWN. DESCRIBE is stateless
    /// and leaves it here.
    Idle,
    /// SETUP made this connection a reader of one mount.
    Reader(ReaderSession),
    /// ANNOUNCE claimed a publish mount's publisher slot.
    Publisher(PublishSession),
}

/// Per-session state. Lives for the duration of one client's TCP
/// connection: the connection-scoped auth fields, the writer and peer
/// address, and the connection's [`Role`].
pub struct ServerSessionState {
    /// Nonce for Digest auth — generated per connection and emitted in
    /// every WWW-Authenticate; rotated by `challenge_response` on a
    /// stale re-challenge (which also resets `auth_nc_hwm`).
    pub auth_nonce: String,
    /// Digest nc (nonce-count) high-water mark for replay detection.
    /// Tracks the highest nc observed/consumed under the
    /// current nonce — it advances even when the digest-response check
    /// subsequently fails (see `verify_digest`), so a captured header
    /// can never be replayed. Reset to 0 whenever the nonce is rotated.
    pub(crate) auth_nc_hwm: u32,
    /// Count of consecutive 401-bounced auth requests on this connection
    /// since the last successful authentication. Increments on every 401;
    /// resets to 0 on a successful (2xx) response to a request that passed a
    /// credential check (DESCRIBE/SETUP/PLAY/PAUSE/TEARDOWN/ANNOUNCE/RECORD
    /// under a `Require` or builder-auth `Inherit` policy; never `Open`). Non-auth-gated methods
    /// (OPTIONS, GET_PARAMETER) never reset it, so an attacker cannot bypass
    /// the 3-strike limit by interleaving OPTIONS between bad-auth requests.
    /// After 3 consecutive failures the session closes. A mount rule's
    /// `Refuse` status is not a 401 and leaves the count unchanged.
    pub auth_failures: u8,
    /// Set by the auth check when the request in flight passed a credential
    /// check (`Require`, or `Inherit` with builder auth); cleared before each
    /// dispatch. An `Open` mount or credential-less `Inherit` leaves it false,
    /// so its 2xx does not reset [`Self::auth_failures`].
    pub(crate) request_authenticated: bool,
    /// CancellationToken for the per-peer fanout task. PAUSE cancels + replaces
    /// with a fresh token so a subsequent PLAY can re-spawn, and so does a
    /// mount rule's refusal that ends the role; TEARDOWN cancels it without
    /// replacement.
    pub peer_cancel: tokio_util::sync::CancellationToken,
    /// Fanouts retired by PAUSE, TEARDOWN or a second PLAY (see
    /// [`Self::retire_fanout`]). Their token is cancelled and they exit at
    /// their next frame boundary — which an interleaved peer that never
    /// reads defers indefinitely (the task stays parked inside its frame
    /// write, holding the session writer). The session's `Drop` aborts whatever
    /// is left here so a retired fanout never outlives its session.
    /// Finished entries are pruned on each retire.
    pub(crate) retired_fanouts: Vec<tokio::task::JoinHandle<()>>,
    /// The session's writer; `None` only in unit tests that build a session without a connection.
    pub(crate) writer: Option<SessionWriter>,
    /// The TCP peer address for this session. Set by `serve_connection`
    /// from the accepted socket's peer address. Used by `handle_play` to
    /// direct UDP RTP packets to the client's actual IP, not loopback.
    /// Defaults to `0.0.0.0:0` for unit-test sessions that don't go
    /// through `serve_connection`.
    pub(crate) peer_addr: std::net::SocketAddr,
    /// The connection's role. A publisher's `PublishSession` is ended
    /// exactly once however the connection goes away: TEARDOWN ends it
    /// explicitly, and any other exit runs the session's own `Drop`
    /// (which also calls `end`, idempotently).
    pub(crate) role: Role,
    /// Set by every change of role or of the ids the registry mirrors;
    /// `serve_requests` copies the ids onto the `ActiveSession` entry when
    /// it is set (see [`Self::take_role_change`]).
    role_changed: bool,
}

impl ServerSessionState {
    pub(crate) fn new() -> Self {
        Self {
            auth_nonce: generate_nonce(),
            auth_nc_hwm: 0,
            auth_failures: 0,
            request_authenticated: false,
            peer_cancel: tokio_util::sync::CancellationToken::new(),
            retired_fanouts: Vec::new(),
            writer: None,
            peer_addr: std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            role: Role::Idle,
            role_changed: false,
        }
    }

    /// The RTSP session id: a reader's from SETUP, a publisher's from its
    /// first SETUP. `None` while `Idle` and for an announced publisher that
    /// has not set a track up.
    pub(crate) fn session_id(&self) -> Option<&str> {
        match &self.role {
            Role::Idle => None,
            Role::Reader(r) => Some(&r.session_id),
            Role::Publisher(p) => p.session_id.as_deref(),
        }
    }

    /// The mount path the session was set up against (same rule as
    /// [`Self::session_id`]).
    pub(crate) fn mount_path(&self) -> Option<&str> {
        match &self.role {
            Role::Idle => None,
            Role::Reader(r) => Some(&r.mount_path),
            Role::Publisher(p) => p.mount_path.as_deref(),
        }
    }

    pub(crate) fn publisher(&self) -> Option<&PublishSession> {
        match &self.role {
            Role::Publisher(p) => Some(p),
            _ => None,
        }
    }

    pub(crate) fn publisher_mut(&mut self) -> Option<&mut PublishSession> {
        match &mut self.role {
            Role::Publisher(p) => Some(p),
            _ => None,
        }
    }

    /// Whether a fanout task is running for this reader.
    pub(crate) fn is_playing(&self) -> bool {
        matches!(
            &self.role,
            Role::Reader(ReaderSession {
                playback: Playback::Playing { .. },
                ..
            })
        )
    }

    /// Enter a role from `Idle` or a reader's SETUP.
    pub(crate) fn set_role(&mut self, role: Role) {
        self.role = role;
        self.role_changed = true;
    }

    /// A publisher's first track SETUP gives the session its id and mount.
    pub(crate) fn publisher_set_up(&mut self, session_id: String, mount_path: String) {
        if let Role::Publisher(p) = &mut self.role {
            p.session_id = Some(session_id);
            p.mount_path = Some(mount_path);
            self.role_changed = true;
        }
    }

    /// Leave the current role for `Idle`: retire the reader's fanout, end a
    /// publisher (once), and drop the transport state with it.
    pub(crate) fn end_role(&mut self) {
        self.retire_fanout();
        if let Role::Publisher(mut p) = std::mem::replace(&mut self.role, Role::Idle) {
            // Frees the mount's publisher slot (bumping its generation) so a
            // fresh ANNOUNCE can claim it right away; the `Drop` that follows
            // finds it ended.
            p.end();
        }
        self.role_changed = true;
    }

    /// Unit tests: make the reader `Playing` a stand-in fanout task (an
    /// `Idle` session becomes a bare multicast reader first).
    #[cfg(test)]
    pub(crate) fn install_test_fanout(&mut self, fanout: tokio::task::JoinHandle<()>) {
        if matches!(self.role, Role::Idle) {
            self.role = Role::Reader(ReaderSession {
                session_id: String::new(),
                mount_path: String::new(),
                transport: ReaderTransport::Multicast,
                playback: Playback::Ready,
            });
        }
        if let Role::Reader(r) = &mut self.role {
            r.playback = Playback::Playing {
                fanout,
                drop_counter: Arc::new(PeerDropCounter::default()),
            };
        }
    }

    /// True once after a change of role or of the ids the registry mirrors.
    pub(crate) fn take_role_change(&mut self) -> bool {
        std::mem::take(&mut self.role_changed)
    }

    /// Retire the current fanout: cancel its token so it exits at its next
    /// frame boundary and keep its handle in `retired_fanouts` for `Drop`
    /// to abort if it is still running when the session ends. Never
    /// `abort()`s here — an interleaved fanout may be parked inside a
    /// multi-poll `write_all` under the shared session-writer mutex, and
    /// aborting it there releases the lock with a partial `$` frame on the
    /// wire, which the next RTSP response then completes with RTSP text
    /// (framing desync). A reader that was playing becomes `Paused`.
    /// Callers that allow a later PLAY replace `peer_cancel` with a fresh
    /// token afterwards.
    pub(crate) fn retire_fanout(&mut self) {
        self.peer_cancel.cancel();
        if let Role::Reader(r) = &mut self.role
            && let Playback::Playing { fanout, .. } =
                std::mem::replace(&mut r.playback, Playback::Paused)
        {
            self.retired_fanouts.retain(|h| !h.is_finished());
            self.retired_fanouts.push(fanout);
        }
    }
}

impl Drop for ServerSessionState {
    /// Every exit of `serve_requests` — clean EOF, read error, idle reap,
    /// graceful or per-session cancel — drops the session; without this the
    /// fanout task would keep streaming RTP to the departed peer and hold its
    /// UDP port pair until the server was dropped.
    /// TEARDOWN and PAUSE still cancel explicitly; this is the safety net.
    fn drop(&mut self) {
        self.peer_cancel.cancel();
        if let Role::Reader(ReaderSession {
            playback: Playback::Playing { fanout, .. },
            ..
        }) = &self.role
        {
            fanout.abort();
        }
        // Fanouts retired by PAUSE, TEARDOWN or a second PLAY exit at their
        // next frame boundary — which a peer that never reads defers
        // indefinitely. This terminal path (FIN follows) is their backstop;
        // a frame cut here is acceptable exactly as for the current one.
        for handle in self.retired_fanouts.drain(..) {
            handle.abort();
        }
    }
}

/// Serve one plain-TCP connection until disconnect or cancel.
///
/// `slot` is the [`SessionSlot`] guard for the `active_sessions` slot the
/// accept loop reserved before spawning this task; it is dropped when this
/// future ends, releasing the slot on every exit path.
pub(crate) async fn handle_tcp_connection(
    state: Arc<ServerState>,
    tcp: TcpStream,
    peer: SocketAddr,
    slot: SessionSlot,
) -> Result<(), RtspServerError> {
    let (read_half, write_half) = tcp.into_split();
    serve_connection(
        state,
        read_half,
        session_writer(write_half),
        peer,
        slot,
        "session opened",
    )
    .await
}

/// Serve one connection whose TLS handshake completed. The accept loop
/// reserved the slot before the handshake and moved it here, so a failed
/// handshake released it without reaching this function.
#[cfg(feature = "rtsp-server-tls")]
pub(crate) async fn handle_tls_connection(
    state: Arc<ServerState>,
    tls: tokio_rustls::server::TlsStream<TcpStream>,
    peer: SocketAddr,
    slot: SessionSlot,
) -> Result<(), RtspServerError> {
    let (read_half, write_half) = tokio::io::split(tls);
    serve_connection(
        state,
        read_half,
        session_writer(write_half),
        peer,
        slot,
        "TLS session opened",
    )
    .await
}

async fn serve_connection<R: AsyncRead + Unpin + Send>(
    state: Arc<ServerState>,
    read_half: R,
    writer: SessionWriter,
    peer: SocketAddr,
    _slot: SessionSlot,
    opened: &'static str,
) -> Result<(), RtspServerError> {
    tracing::info!(target: "tst_rtp::server", peer = %peer, "{opened}");
    // The registry entry holds the same writer so `RtspServer::stop` can
    // send the Notice 5402 ANNOUNCE and shut the write side down.
    let entry = crate::rtsp::server::register_session(&state, peer, writer.clone());
    let mut session = ServerSessionState::new();
    // PLAY clones it into `PeerTransport::Interleaved` (plain TCP and TLS).
    session.writer = Some(writer.clone());
    // `handle_play`'s UDP branch sends RTP to the client's real address.
    session.peer_addr = peer;
    let res = serve_requests(
        state.clone(),
        peer,
        entry.clone(),
        session,
        read_half,
        writer,
    )
    .await;
    crate::rtsp::server::unregister_session(&state, &entry);
    res
}

/// The RTSP request/response loop, generic over the read half so the plain
/// `rtsp://` (TCP) and `rtsps://` (TLS) sessions share one implementation.
///
/// Reads requests, dispatches each to its handler, writes the response,
/// and enforces the auth-failure + TEARDOWN close semantics. The connection
/// handler owns the writer; plain TCP and TLS sessions get the same one.
async fn serve_requests<R>(
    state: Arc<ServerState>,
    peer: SocketAddr,
    session_entry: Arc<crate::rtsp::server::ActiveSession>,
    mut session: ServerSessionState,
    mut read_half: R,
    write_half: SessionWriter,
) -> Result<(), RtspServerError>
where
    R: AsyncRead + Unpin,
{
    // Bounded read buffer — RTSP requests are typically << 4 KiB.
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    // When the request (or `$` frame) now at the head of `buf` started
    // arriving. `None` while `buf` is empty. The read-idle bound below is
    // per READ; without this a peer trickling one byte per 29 s holds its
    // session slot (and, after ANNOUNCE, a mount's publisher slot) until
    // the 64 KiB header cap — about 22 days — and `max_sessions` such
    // connections refuse every new peer before any of them authenticates.
    // A message must frame within the same bound a silent peer gets.
    let mut msg_started: Option<tokio::time::Instant> = None;
    // Until SETUP gives the connection a session, or one of its requests
    // passes a credential check, it holds a session slot on nothing but its
    // own say-so. This absolute deadline bounds that however many OPTIONS,
    // GET_PARAMETERs or interleaved frames it sends, however slowly it reads
    // the replies: `expired_deadline` is checked before every read and every
    // request, and reply writes are cut to what is left of it. Once cleared
    // it stays cleared for the connection.
    let mut admission_deadline = Some(tokio::time::Instant::now() + READ_IDLE_TIMEOUT);
    // Base of the per-read idle wait while no message is in progress. Every
    // read moves it, except a read that an `Idle` connection spent only on
    // interleaved frames: those belong to no session and are dropped, so
    // they are not activity.
    let mut quiet_since = tokio::time::Instant::now();

    'serve: loop {
        // Cancellation guard — hard cancel exits the loop immediately,
        // graceful cancel observed via the tokio::select! below.
        if state.hard_cancel.is_cancelled() {
            break;
        }
        if let Some(expired) = expired_deadline(admission_deadline, &state, &session) {
            warn_expired(peer, &expired);
            break;
        }

        // Read more bytes. Use a small per-iteration buffer so we can
        // respond promptly to cancellation. tokio::select! with the
        // cancel_token gives prompt graceful-shutdown response.
        //
        // The idle-read timeout arm closes a slow-loris window: a client
        // that advertises a large Content-Length but then sends bytes very
        // slowly would cause the buffer cap (below) to trigger eventually,
        // but could hold the connection open for a long time before it does.
        // The per-read bound closes that window: `READ_IDLE_TIMEOUT` before
        // SETUP, `post_setup_idle_bound` (the advertised session timeout
        // plus grace) once a session exists — so a conformant client's
        // keepalives at `timeout / 2` are never raced. Before SETUP or a
        // credential check, the admission deadline caps the whole
        // connection at `READ_IDLE_TIMEOUT` from the start of this loop.
        let idle_bound = if session.session_id().is_some() {
            post_setup_idle_bound(state.builder.session_timeout)
        } else {
            READ_IDLE_TIMEOUT
        };
        // A message in progress shortens the wait to what is left of its own
        // deadline; a peer with nothing in progress gets what is left of the
        // bound since its last activity.
        // An opt-in publish idle timeout shortens the wait while a
        // recording publisher exists; the timeout arm decides whether the
        // publisher really was idle.
        // The wait is what is left of the window, so control-channel traffic
        // that is not media (keepalives) cannot postpone it.
        let publish_idle = state.builder.publish_idle_timeout;
        let publish_left = publish_idle.and_then(|t| {
            session
                .publisher()
                .and_then(|p| p.idle_for())
                .map(|d| t.saturating_sub(d))
        });
        // The two deadlines are independent: a partial request must not
        // suspend the publish idle timeout, and media must not extend a
        // request's framing deadline.
        let framing_left = match msg_started {
            Some(t0) => idle_bound.saturating_sub(t0.elapsed()),
            None => idle_bound.saturating_sub(quiet_since.elapsed()),
        };
        let mut read_bound = publish_left.map_or(framing_left, |left| framing_left.min(left));
        if let Some(d) = admission_deadline {
            read_bound = read_bound.min(d.saturating_duration_since(tokio::time::Instant::now()));
        }
        let mut chunk = [0u8; 4096];
        let n = tokio::select! {
            r = read_half.read(&mut chunk) => match r {
                Ok(0) => {
                    // Clean EOF — client disconnected.
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(target: "tst_rtp::server", peer = %peer, error = %e, "read failed");
                    break;
                }
            },
            _ = tokio::time::sleep(read_bound) => {
                // A publisher's media is liveness too: a TCP-interleaved
                // publisher already re-arms this sleep by landing bytes on
                // the read above (next iteration starts a fresh timer), but
                // a UDP-transport publisher sends its RTP on a
                // different socket this read loop never touches — without
                // this check such a publisher would be reaped on schedule
                // despite media flowing.
                if let Some(expired) = expired_deadline(admission_deadline, &state, &session) {
                    warn_expired(peer, &expired);
                    break;
                }
                if let Some(t0) = msg_started {
                    if t0.elapsed() >= idle_bound {
                        tracing::warn!(
                            target: "tst_rtp::server",
                            peer = %peer,
                            buffered = buf.len(),
                            timeout_secs = idle_bound.as_secs(),
                            "request not completed within the idle bound; closing session"
                        );
                        break;
                    }
                    // Woken by the publish deadline with media still
                    // flowing: the request has time left.
                    continue 'serve;
                }
                let media_recent = session
                    .publisher()
                    .is_some_and(|p| p.media_within(idle_bound));
                if media_recent {
                    quiet_since = tokio::time::Instant::now();
                    continue 'serve;
                }
                tracing::warn!(
                    target: "tst_rtp::server",
                    peer = %peer,
                    timeout_secs = idle_bound.as_secs(),
                    "idle read timeout; closing session"
                );
                break;
            },
            _ = state.cancel_token.cancelled() => {
                tracing::info!(target: "tst_rtp::server", peer = %peer, "server cancel token observed (stop or hard cancel)");
                break;
            }
            _ = session_entry.cancel.cancelled() => {
                tracing::info!(target: "tst_rtp::server", peer = %peer, "per-session cancel observed");
                break;
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        if msg_started.is_none() {
            msg_started = Some(tokio::time::Instant::now());
        }
        let len_after_read = buf.len();

        // Process the buffer head until neither an interleaved frame nor
        // a complete RTSP request can be taken from it. One loop, because
        // the two interleave freely on a publisher's connection (RFC 2326
        // §10.12): `[REQ][$frame][REQ]` can land in one read, and the
        // frame between the requests must be drained before the second
        // request is framed.
        //
        // - Interleaved frames, every session: a `$`-headed buffer is an
        //   interleaved frame. On a publisher session (ANNOUNCE done) a
        //   complete one is routed to its track (unknown channels are
        //   counted and dropped, never fatal); on a reader session it is
        //   dropped (RTCP receiver reports). Either way the loop goes round
        //   again, and a partial one waits for the next read. The RTSP
        //   framing below — and its 64 KiB unterminated-header cap — never
        //   sees a `$`-headed buffer, so binary payload (which may well
        //   contain `\r\n\r\n`) is never parsed as a request.
        //
        // - RTSP direction: body-aware cap, coherent with the client
        //   `send_and_read` loop and both interleaved pumps (all share
        //   `rtsp_frame_decision` + the same MAX_RTSP_MESSAGE_BYTES /
        //   MAX_RTSP_BODY_BYTES constants). A blanket "buf.len() > 64 KiB
        //   → 413" cap would wrongly reject a valid request whose body
        //   legitimately runs up to MAX_RTSP_BODY_BYTES (1 MiB). Instead:
        //
        //   Headers: 413 once they exceed MAX_RTSP_MESSAGE_BYTES (64 KiB),
        //     before or after the terminator arrives — the header DoS bound.
        //   After the terminator (CRLFCRLF seen): parse the declared
        //     Content-Length up front. An over-cap (> 1 MiB) / malformed /
        //     duplicate value is a 413 NOW (don't read toward EOF). A
        //     legitimate body up to 1 MiB is awaited in full; the exact
        //     header + 4 + content_length ceiling bounds a peer dribbling
        //     past its declared body.
        //
        //   The cap is checked on every pass, so an oversized pipelined
        //   request whose entirety already arrived behind a good one gets
        //   its 413 immediately rather than waiting on another socket read
        //   (or the idle timeout) that may never come. A complete frame
        //   that does not parse is answered — 501 for a method we don't
        //   implement, 400 otherwise — and DRAINED, so a `SET_PARAMETER`
        //   never wedges every request queued behind it until the idle
        //   timeout. `NeedMore` loops back to read more (bounded: header
        //   ≤ 64 KiB, body ≤ 1 MiB).
        let mut rtsp_consumed = false;
        loop {
            let drained = match session.publisher() {
                Some(publish) => {
                    let mut route = |ch: u8, payload: &[u8]| match publish.track_for_channel(ch) {
                        Some((track, false)) => {
                            publish
                                .adapter
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .on_rtp(track, payload);
                            // Liveness only for a packet the track would
                            // accept, as on the UDP path.
                            let expected_pt = publish.tracks[track].announced.payload_type;
                            if RtpHeader::decode(payload)
                                .is_ok_and(|p| p.header.payload_type == expected_pt)
                            {
                                publish
                                    .last_media_ms
                                    .store(PublishSession::now_ms(), Ordering::Relaxed);
                            }
                        }
                        Some((track, true)) => publish
                            .adapter
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .on_rtcp(track, payload),
                        None => publish.mount.tick(|s| s.malformed_packets += 1),
                    };
                    super::publish::drain_interleaved_head(&mut buf, &mut route)
                }
                None => {
                    // A reader's interleaved frames (RTCP receiver reports on
                    // its RTCP channel, RFC 7826 section 14, which live555, VLC
                    // and GStreamer send over TCP) are consumed and dropped:
                    // nothing ingests them yet. Left at the head they were
                    // framed as a non-UTF-8 request and earned a 413 at the
                    // reader's next keepalive.
                    let mut drop_frame = |_ch: u8, _payload: &[u8]| {};
                    super::publish::drain_interleaved_head(&mut buf, &mut drop_frame)
                }
            };
            if !drained {
                break; // partial frame: read more
            }
            // Before each request, after any interleaved media at the head
            // was routed: a pipelined batch must not carry the connection
            // past a deadline that ran out while it was being answered.
            if let Some(expired) = expired_deadline(admission_deadline, &state, &session) {
                warn_expired(peer, &expired);
                break 'serve;
            }

            match reject_if_over_cap(
                &buf,
                peer,
                &write_half,
                reply_bound(idle_bound, admission_deadline),
                publish_window(&state, &session),
                &state.cancel_token,
                &session_entry.cancel,
            )
            .await
            {
                CapCheck::WithinCaps => {}
                CapCheck::Answered => return Ok(()),
                CapCheck::Stalled => break 'serve,
            }
            let RtspFraming::Complete { total_len } =
                crate::rtsp::message::rtsp_frame_decision(&buf)
            else {
                break; // NeedMore (empty buffer included): read more
            };
            let (req, consumed) = match RtspRequest::parse(&buf[..total_len]) {
                Ok(t) => t,
                Err(e) => {
                    let reply =
                        crate::rtsp::message::classify_unparseable_request(&buf[..total_len]);
                    tracing::warn!(
                        target: "tst_rtp::server",
                        peer = %peer,
                        error = ?e,
                        status = reply.status,
                        "unparseable RTSP request; answering and draining it"
                    );
                    buf.drain(..total_len);
                    rtsp_consumed = true;
                    let bytes = unparseable_reply_bytes(&reply);
                    match write_bounded(
                        &write_half,
                        &bytes,
                        reply_bound(idle_bound, admission_deadline),
                        publish_window(&state, &session),
                        &state.cancel_token,
                        &session_entry.cancel,
                        false,
                    )
                    .await
                    {
                        Some(Ok(())) => {}
                        Some(Err(e)) => {
                            tracing::warn!(target: "tst_rtp::server", peer = %peer, error = %e, "write failed");
                            return Ok(());
                        }
                        None => {
                            match expired_deadline(admission_deadline, &state, &session) {
                                Some(expired) => warn_expired(peer, &expired),
                                None => tracing::warn!(
                                    target: "tst_rtp::server",
                                    peer = %peer,
                                    timeout_secs = idle_bound.as_secs(),
                                    publish_idle_timeout_ms = state.builder.publish_idle_timeout.map(|d| d.as_millis() as u64),
                                    "reply write parked past the idle bound or the publish idle timeout, or cancelled; closing session"
                                ),
                            }
                            break 'serve;
                        }
                    }
                    continue;
                }
            };
            debug_assert_eq!(
                consumed, total_len,
                "parser and framer disagree on the frame length"
            );
            buf.drain(..consumed);
            rtsp_consumed = true;
            session.request_authenticated = false;
            let response = dispatch(&req, &state, &mut session);
            if session.request_authenticated || session.session_id().is_some() {
                admission_deadline = None;
            }
            // Mirror the session's `session_id` + `mount_path` onto the
            // public `ActiveSession` registry entry when a transition
            // changed them (SETUP on its 200 path, TEARDOWN). The registry
            // copies are what `RtspServer::stop` reads to build the Notice
            // 5402 ANNOUNCE on graceful shutdown.
            if session.take_role_change() {
                if let Ok(mut g) = session_entry.session_id.lock() {
                    *g = session.session_id().map(str::to_owned);
                }
                if let Ok(mut g) = session_entry.mount_path.lock() {
                    *g = session.mount_path().map(str::to_owned);
                }
            }
            let bytes = response.encode();
            // Every reply (this one, the unparseable-request reply, the 413
            // and the TEARDOWN close) goes through `write_bounded`, the one
            // bounded writer: it serializes against the fanout task on the
            // writer lock under the same idle bound and cancels as the
            // read. A fanout this very request retired (PAUSE, TEARDOWN, a
            // second PLAY) may be parked mid-frame on a peer that never
            // reads, holding the lock; unbounded, the session (its slot,
            // its registry entry, the retired fanout's subscription) would
            // live as long as the peer kept the TCP open. On the bound we
            // leave the loop: the exit path drops the session, whose Drop
            // aborts the retired fanout and frees the lock, then shuts the
            // write half so the peer sees FIN once it drains. A write cut
            // by the bound mid-`write_all` leaves a partial RTSP response
            // on the wire before that FIN — fine on a terminal path, the
            // same class as the frame the Drop's abort may cut.
            let written = write_bounded(
                &write_half,
                &bytes,
                reply_bound(idle_bound, admission_deadline),
                publish_window(&state, &session),
                &state.cancel_token,
                &session_entry.cancel,
                false,
            )
            .await;
            match written {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    tracing::warn!(target: "tst_rtp::server", peer = %peer, error = %e, "write failed");
                    return Ok(());
                }
                None => {
                    match expired_deadline(admission_deadline, &state, &session) {
                        Some(expired) => warn_expired(peer, &expired),
                        None => tracing::warn!(
                            target: "tst_rtp::server",
                            peer = %peer,
                            timeout_secs = idle_bound.as_secs(),
                            publish_idle_timeout_ms = state.builder.publish_idle_timeout.map(|d| d.as_millis() as u64),
                            "response write parked past the idle bound or the publish idle timeout, or cancelled; closing session"
                        ),
                    }
                    break 'serve;
                }
            }
            if response.status == 401 {
                session.auth_failures = session.auth_failures.saturating_add(1);
                if session.auth_failures >= 3 {
                    tracing::warn!(
                        target: "tst_rtp::server",
                        peer = %peer,
                        "3 auth failures; closing session"
                    );
                    return Ok(());
                }
            } else if (200..=299).contains(&response.status) {
                // Only reset the failure counter on a successful (2xx) response
                // that passed a credential check (`request_authenticated`):
                // an `Open` mount's 2xx proves nothing about the peer. OPTIONS
                // and GET_PARAMETER are never auth-gated (they always return 200), so resetting on
                // them would let an attacker alternate OPTIONS with bad-auth
                // requests to bypass the 3-strike limit indefinitely.
                // 3xx is excluded: no handler emits a redirect on a gated method,
                // and a redirect is not an auth success.
                match req.method {
                    RtspMethod::Describe
                    | RtspMethod::Setup
                    | RtspMethod::Play
                    | RtspMethod::Pause
                    | RtspMethod::Teardown
                    | RtspMethod::Announce
                    | RtspMethod::Record => {
                        if session.request_authenticated {
                            session.auth_failures = 0;
                        }
                    }
                    RtspMethod::Options | RtspMethod::GetParameter => {
                        // Non-auth-gated: do not touch the failure counter.
                    }
                }
            }
            if req.method == RtspMethod::Teardown && response.status == 200 {
                // Clean close after TEARDOWN. `shutdown` lives on the
                // write half; the bounded writer serializes it against any
                // in-flight fanout write (the fanout exits on session
                // cancel; the lock is the safety net) without waiting on a
                // parked one past the bound.
                match write_bounded(
                    &write_half,
                    &[],
                    reply_bound(idle_bound, admission_deadline),
                    publish_window(&state, &session),
                    &state.cancel_token,
                    &session_entry.cancel,
                    true,
                )
                .await
                {
                    Some(_) => return Ok(()),
                    None => break 'serve,
                }
            }
        }
        // Re-arm the message clock for whatever the processing loop left:
        // nothing → no message in progress; a partial message that was
        // already there → keep its start; a new partial behind a request
        // that was consumed → it started now.
        msg_started = if buf.is_empty() {
            None
        } else if buf.len() < len_after_read {
            Some(tokio::time::Instant::now())
        } else {
            msg_started
        };
        if rtsp_consumed || !matches!(session.role, Role::Idle) {
            quiet_since = tokio::time::Instant::now();
        }
    }
    // Stop the fanout BEFORE shutting the write half so no RTP frame STARTS
    // after FIN (the Drop above is what cancels it; `abort` drops the task
    // at its next poll, so a frame already mid-`write_all` may still land
    // partially — acceptable ONLY here because FIN follows and nothing else
    // is written; PLAY/PAUSE/TEARDOWN never abort, see `handle_play`). Idle
    // / EOF / error / cancel exits all land here (TEARDOWN and the cap
    // rejections shut down on their own path).
    //
    // The shutdown is bounded like `stop()`'s write-half pass: over TLS it
    // first flushes close_notify and any pending ciphertext, which never
    // finishes against a peer that stopped reading, and the session keeps
    // its `active_sessions` slot until this future ends. Not
    // `write_bounded`: a cancel already fired on most of these exits and
    // would skip the FIN. Plain TCP's SHUT_WR does not block.
    drop(session);
    let _ = tokio::time::timeout(EXIT_SHUTDOWN_BOUND, async {
        let mut guard = write_half.lock().await;
        let _ = guard.shutdown().await;
    })
    .await;
    tracing::info!(target: "tst_rtp::server", peer = %peer, "session closed");
    Ok(())
}

/// What [`reject_if_over_cap`] found.
enum CapCheck {
    /// The buffer head is within caps (`NeedMore` or `Complete`).
    WithinCaps,
    /// A violation was answered with `413` and the write half shut; the
    /// caller closes the session.
    Answered,
    /// A violation, but the `413` write did not finish within `bound` or a
    /// cancel won; the caller leaves the serve loop.
    Stalled,
}

/// Check the buffer head against the RTSP framing caps (64 KiB headers /
/// 1 MiB body — see [`crate::rtsp::message::rtsp_frame_decision`]) and, on
/// a violation, answer `413 Request Entity Too Large` and shut the write
/// half through [`write_bounded`] under `bound`, the publish idle
/// window and the two cancel tokens.
async fn reject_if_over_cap(
    buf: &[u8],
    peer: SocketAddr,
    write_half: &SessionWriter,
    bound: std::time::Duration,
    publish: Option<(&PublishSession, std::time::Duration)>,
    server_cancel: &CancellationToken,
    session_cancel: &CancellationToken,
) -> CapCheck {
    let cap_rejection = match crate::rtsp::message::rtsp_frame_decision(buf) {
        RtspFraming::HeadersTooLong => Some("headers exceed maximum"),
        RtspFraming::NonUtf8Headers { .. } => Some("non-UTF8 RTSP headers"),
        RtspFraming::BadContentLength(detail) => Some(detail),
        RtspFraming::BodyOffsetOverflow => Some("RTSP body offset overflow"),
        // NeedMore / Complete: not a cap rejection.
        RtspFraming::NeedMore | RtspFraming::Complete { .. } => None,
    };
    let Some(detail) = cap_rejection else {
        return CapCheck::WithinCaps;
    };
    tracing::warn!(
        target: "tst_rtp::server",
        peer = %peer,
        buf_len = buf.len(),
        detail,
        "request exceeded RTSP header/body caps; sending 413 and closing"
    );
    let response_413 = b"RTSP/1.0 413 Request Entity Too Large\r\nContent-Length: 0\r\n\r\n";
    match write_bounded(
        write_half,
        response_413,
        bound,
        publish,
        server_cancel,
        session_cancel,
        true,
    )
    .await
    {
        Some(_) => CapCheck::Answered,
        None => CapCheck::Stalled,
    }
}

/// Wire bytes for an [`crate::rtsp::message::UnparseableRequestReply`]:
/// CSeq echoed when it was clean, `Server:` like every other response,
/// `Content-Length: 0` so the client's framing needs no guess.
fn unparseable_reply_bytes(reply: &crate::rtsp::message::UnparseableRequestReply) -> bytes::Bytes {
    let mut headers = std::collections::HashMap::new();
    if let Some(cseq) = &reply.cseq {
        headers.insert("cseq".to_string(), cseq.clone());
    }
    headers.insert("server".to_string(), handlers::server_header());
    headers.insert("content-length".to_string(), "0".to_string());
    crate::rtsp::message::RtspResponse {
        version: crate::url::RtspVersion::V1_0,
        status: reply.status,
        reason: reply.reason.to_string(),
        headers,
        body: bytes::Bytes::new(),
    }
    .encode()
}

/// Dispatch an RTSP request to the appropriate handler.
fn dispatch(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> crate::rtsp::message::RtspResponse {
    match req.method {
        RtspMethod::Options => handlers::handle_options(req, state),
        RtspMethod::Describe => handlers::handle_describe(req, state, session),
        RtspMethod::Announce => publish::handlers::handle_announce(req, state, session),
        RtspMethod::Setup => handlers::handle_setup(req, state, session),
        RtspMethod::Play => handlers::handle_play(req, state, session),
        RtspMethod::Record => publish::handlers::handle_record(req, state, session),
        RtspMethod::Pause => handlers::handle_pause(req, state, session),
        RtspMethod::Teardown => handlers::handle_teardown(req, state, session),
        RtspMethod::GetParameter => handlers::handle_get_parameter(req, state, session),
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn post_setup_idle_bound_is_never_below_the_advertised_timeout() {
        use std::time::Duration;
        assert_eq!(
            post_setup_idle_bound(Duration::from_secs(60)),
            Duration::from_secs(90)
        );
        assert_eq!(
            post_setup_idle_bound(Duration::from_secs(1)),
            Duration::from_secs(3)
        );
        assert_eq!(
            post_setup_idle_bound(Duration::ZERO),
            Duration::from_secs(2)
        );
    }

    /// Spin up a local TCP listener, accept one connection, run the
    /// session loop. Client sends OPTIONS, we read the 200 OK response.
    #[tokio::test]
    async fn session_responds_to_options() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            // Build a minimal ServerState for the test.
            let builder =
                crate::rtsp::server::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
            let state = std::sync::Arc::new(crate::rtsp::server::ServerState {
                builder,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                sessions_ended: std::sync::Arc::new(tokio::sync::watch::channel(false).0),
                hard_cancel: crate::rtsp::server::cancel::RtspServerCancelHandle::new(),
                mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
                mount_rules: crate::rtsp::server::mount_rules::MountRules::for_tests(),
                active_sessions: std::sync::atomic::AtomicUsize::new(0),
                total_rtp_packets_sent: std::sync::atomic::AtomicU64::new(0),
                total_rtp_bytes_sent: std::sync::atomic::AtomicU64::new(0),
                publish_counters: Default::default(),
                interleaved_writes: Default::default(),
                started: std::sync::atomic::AtomicBool::new(true),
                shutdown: std::sync::atomic::AtomicBool::new(false),
                local_addr: std::sync::Mutex::new(None),
                sessions: std::sync::Mutex::new(Vec::new()),
                notice_cseq: std::sync::atomic::AtomicU64::new(1_000_000),
                #[cfg(feature = "rtsp-server-tls")]
                tls_config: std::sync::Mutex::new(None),
                startup_tx: std::sync::Mutex::new(None),
                publish_queue_tx: std::sync::Mutex::new(None),
            });
            // Mimic the accept loop: reserve a slot, then move the guard
            // into the session task.
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_tcp_connection(state, tcp, peer, slot).await
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        client
            .write_all(b"OPTIONS rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        // Read until we see a complete response.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = client.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("200 OK"), "got: {}", text);
        assert!(
            text.to_lowercase().contains("public:"),
            "expected Public header in: {}",
            text
        );

        // Close client; session should exit cleanly.
        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn session_responds_to_teardown_and_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            let builder =
                crate::rtsp::server::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
            let state = std::sync::Arc::new(crate::rtsp::server::ServerState {
                builder,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                sessions_ended: std::sync::Arc::new(tokio::sync::watch::channel(false).0),
                hard_cancel: crate::rtsp::server::cancel::RtspServerCancelHandle::new(),
                mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
                mount_rules: crate::rtsp::server::mount_rules::MountRules::for_tests(),
                active_sessions: std::sync::atomic::AtomicUsize::new(0),
                total_rtp_packets_sent: std::sync::atomic::AtomicU64::new(0),
                total_rtp_bytes_sent: std::sync::atomic::AtomicU64::new(0),
                publish_counters: Default::default(),
                interleaved_writes: Default::default(),
                started: std::sync::atomic::AtomicBool::new(true),
                shutdown: std::sync::atomic::AtomicBool::new(false),
                local_addr: std::sync::Mutex::new(None),
                sessions: std::sync::Mutex::new(Vec::new()),
                notice_cseq: std::sync::atomic::AtomicU64::new(1_000_000),
                #[cfg(feature = "rtsp-server-tls")]
                tls_config: std::sync::Mutex::new(None),
                startup_tx: std::sync::Mutex::new(None),
                publish_queue_tx: std::sync::Mutex::new(None),
            });
            // Mimic the accept loop: reserve a slot, then move the guard
            // into the session task.
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_tcp_connection(state, tcp, peer, slot).await
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        // The TEARDOWN handler answers 200 OK after auth
        // (no auth configured here), session state cleared, TCP closed
        // by the dispatcher after observing 200 + TEARDOWN method.
        client
            .write_all(b"TEARDOWN rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = client.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("200"), "got: {}", text);

        drop(client);
        let _ = server_handle.await;
    }

    /// Helper: build a minimal `ServerState` with Basic auth configured.
    fn make_state_with_basic_auth() -> Arc<ServerState> {
        let mut builder =
            crate::rtsp::server::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.auth_basic(
            "test",
            "user",
            secrecy::SecretString::from("secret".to_owned()),
        );
        Arc::new(ServerState {
            builder,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            sessions_ended: std::sync::Arc::new(tokio::sync::watch::channel(false).0),
            hard_cancel: crate::rtsp::server::cancel::RtspServerCancelHandle::new(),
            mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
            mount_rules: crate::rtsp::server::mount_rules::MountRules::for_tests(),
            active_sessions: std::sync::atomic::AtomicUsize::new(0),
            total_rtp_packets_sent: std::sync::atomic::AtomicU64::new(0),
            total_rtp_bytes_sent: std::sync::atomic::AtomicU64::new(0),
            publish_counters: Default::default(),
            interleaved_writes: Default::default(),
            started: std::sync::atomic::AtomicBool::new(true),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            local_addr: std::sync::Mutex::new(None),
            sessions: std::sync::Mutex::new(Vec::new()),
            notice_cseq: std::sync::atomic::AtomicU64::new(1_000_000),
            #[cfg(feature = "rtsp-server-tls")]
            tls_config: std::sync::Mutex::new(None),
            startup_tx: std::sync::Mutex::new(None),
            publish_queue_tx: std::sync::Mutex::new(None),
        })
    }

    /// An OPTIONS request (which never requires auth and always returns 200)
    /// must NOT reset the auth-failure counter. Without this guard an attacker
    /// can alternate OPTIONS with bad-auth DESCRIBE requests and never reach
    /// the 3-strike session-close limit.
    ///
    /// Sequence:
    ///   DESCRIBE (no auth) → 401  [failures = 1]
    ///   DESCRIBE (no auth) → 401  [failures = 2]
    ///   OPTIONS            → 200  [must NOT reset to 0; failures stays 2]
    ///   DESCRIBE (no auth) → 401  [failures = 3 → server closes session]
    ///
    /// The test verifies that the server closes the TCP connection after the
    /// third DESCRIBE (reads EOF) rather than continuing to serve.
    #[tokio::test]
    async fn auth_failures_not_reset_by_options() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            let state = make_state_with_basic_auth();
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_tcp_connection(state, tcp, peer, slot).await
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();

        /// Read from `client` until we see a complete RTSP header block
        /// (terminated by `\r\n\r\n`), then return the accumulated bytes.
        async fn read_response(client: &mut tokio::net::TcpStream) -> Vec<u8> {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = client.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break; // EOF
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            buf
        }

        // DESCRIBE #1 — no Authorization header → 401 (failures = 1).
        client
            .write_all(b"DESCRIBE rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        let resp1 = read_response(&mut client).await;
        assert!(
            String::from_utf8_lossy(&resp1).contains("401"),
            "expected 401, got: {}",
            String::from_utf8_lossy(&resp1)
        );

        // DESCRIBE #2 — still no auth → 401 (failures = 2).
        client
            .write_all(b"DESCRIBE rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 2\r\n\r\n")
            .await
            .unwrap();
        let resp2 = read_response(&mut client).await;
        assert!(
            String::from_utf8_lossy(&resp2).contains("401"),
            "expected 401, got: {}",
            String::from_utf8_lossy(&resp2)
        );

        // OPTIONS — always 200, never auth-gated. Must NOT reset the counter.
        client
            .write_all(b"OPTIONS rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 3\r\n\r\n")
            .await
            .unwrap();
        let resp3 = read_response(&mut client).await;
        assert!(
            String::from_utf8_lossy(&resp3).contains("200"),
            "expected 200 from OPTIONS, got: {}",
            String::from_utf8_lossy(&resp3)
        );

        // DESCRIBE #3 — failures reaches 3 → server must close the session.
        client
            .write_all(b"DESCRIBE rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 4\r\n\r\n")
            .await
            .unwrap();
        // Read until EOF. The server closes the connection after the 3rd auth
        // failure, so this read eventually returns 0 (after the 401 response).
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = client.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&buf);
        // The server sends a final 401 then closes.
        assert!(
            text.contains("401"),
            "expected 401 response before EOF, got: {}",
            text
        );

        let _ = server_handle.await;
    }

    /// A successful auth on a gated method (DESCRIBE returning 200) resets the
    /// failure counter, letting a client who had temporary bad credentials
    /// recover without losing their session.
    ///
    /// Sequence:
    ///   DESCRIBE (no auth)      → 401  [failures = 1]
    ///   DESCRIBE (correct auth) → 200  [failures reset to 0]
    ///   DESCRIBE (no auth)      → 401  [failures = 1, not 2]
    ///   DESCRIBE (no auth)      → 401  [failures = 2; below the 3-strike limit]
    ///   OPTIONS                 → 200  [session still alive]
    ///
    /// No-reset trace (hypothetical buggy code): without the reset, failures
    /// would be 1→(no change on 200)→2→3 at the 4th DESCRIBE, closing the
    /// session. The 5th request would receive EOF instead of a 200 OPTIONS
    /// response, causing the distinguishing assertion to fail.
    ///
    /// A real mount is registered so the auth-passing DESCRIBE returns 200
    /// (a 2xx on an auth-gated method) rather than 404.
    #[tokio::test]
    async fn auth_failures_reset_on_successful_auth() {
        use crate::rtsp::server::mount::{MountEntry, MountKind, MountState};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            let state = make_state_with_basic_auth();
            // DESCRIBE reads local_addr to build the SDP; set it before
            // handle_tcp_connection runs.
            *state.local_addr.lock().unwrap() = Some("127.0.0.1:8554".parse().unwrap());
            // Register a real mount at /test so an auth-passing DESCRIBE
            // returns 200 (2xx), triggering the counter reset.
            let mux_cfg = crate::rtsp::server::test_muxer_cfg();
            let mount = MountState::new("/test", MountKind::Unicast, mux_cfg, 16).unwrap();
            state
                .mounts
                .lock()
                .unwrap()
                .insert("/test".into(), MountEntry::Local(mount));
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_tcp_connection(state, tcp, peer, slot).await
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();

        async fn read_response(client: &mut tokio::net::TcpStream) -> Vec<u8> {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = client.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            buf
        }

        // Step 1: DESCRIBE without auth → 401 (failures = 1).
        client
            .write_all(b"DESCRIBE rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        let resp1 = read_response(&mut client).await;
        assert!(
            String::from_utf8_lossy(&resp1).contains("401"),
            "step 1: expected 401, got: {}",
            String::from_utf8_lossy(&resp1)
        );

        // Step 2: DESCRIBE with correct Basic auth → 200.
        // "user:secret" base64 = "dXNlcjpzZWNyZXQ=".
        // DESCRIBE is auth-gated; status 200 (2xx) → failures reset to 0.
        client
            .write_all(
                b"DESCRIBE rtsp://127.0.0.1/test RTSP/1.0\r\n\
                  CSeq: 2\r\n\
                  Authorization: Basic dXNlcjpzZWNyZXQ=\r\n\r\n",
            )
            .await
            .unwrap();
        let resp2 = read_response(&mut client).await;
        assert!(
            String::from_utf8_lossy(&resp2).contains("200"),
            "step 2: expected 200 OK from auth-passing DESCRIBE to real mount, got: {}",
            String::from_utf8_lossy(&resp2)
        );

        // Step 3: DESCRIBE without auth → 401 (failures = 1, not 2 — reset fired).
        client
            .write_all(b"DESCRIBE rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 3\r\n\r\n")
            .await
            .unwrap();
        let resp3 = read_response(&mut client).await;
        assert!(
            String::from_utf8_lossy(&resp3).contains("401"),
            "step 3: expected 401, got: {}",
            String::from_utf8_lossy(&resp3)
        );

        // Step 4: DESCRIBE without auth → 401 (failures = 2; still below the limit).
        // Without the reset, this would be the 3rd failure → server closes.
        client
            .write_all(b"DESCRIBE rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 4\r\n\r\n")
            .await
            .unwrap();
        let resp4 = read_response(&mut client).await;
        assert!(
            String::from_utf8_lossy(&resp4).contains("401"),
            "step 4: expected 401, got: {}",
            String::from_utf8_lossy(&resp4)
        );

        // Step 5 — distinguishing assertion.
        // With reset:    failures = 2; session still open → OPTIONS returns 200.
        // Without reset: failures = 3 after step 4 → server closed after sending
        //   the step-4 401; this read returns EOF, failing the is_empty check.
        client
            .write_all(b"OPTIONS rtsp://127.0.0.1/test RTSP/1.0\r\nCSeq: 5\r\n\r\n")
            .await
            .unwrap();
        let resp5 = read_response(&mut client).await;
        assert!(
            !resp5.is_empty(),
            "step 5: session must still be open after only 2 post-reset failures \
             (got EOF — reset on step 2 did not fire)"
        );
        assert!(
            String::from_utf8_lossy(&resp5).contains("200"),
            "step 5: expected 200 from OPTIONS on live session, got: {}",
            String::from_utf8_lossy(&resp5)
        );

        drop(client);
        let _ = server_handle.await;
    }

    /// A 2xx from an `Open` mount checks no credential, so it must not
    /// reset the 401 counter: bad-auth, bad-auth, open DESCRIBE, bad-auth
    /// is the third strike and closes the connection.
    #[tokio::test]
    async fn auth_failures_not_reset_by_open_mount_success() {
        use crate::rtsp::server::mount::{MountEntry, MountKind, MountState};
        use crate::rtsp::server::mount_rules::{MountPolicy, MountRule};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            let state = make_state_with_basic_auth();
            *state.local_addr.lock().unwrap() = Some("127.0.0.1:8554".parse().unwrap());
            let mux_cfg = crate::rtsp::server::test_muxer_cfg();
            for p in ["/locked", "/open"] {
                let mount = MountState::new(p, MountKind::Unicast, mux_cfg.clone(), 16).unwrap();
                state
                    .mounts
                    .lock()
                    .unwrap()
                    .insert(p.into(), MountEntry::Local(mount));
            }
            state
                .mount_rules
                .set("/open", MountRule::both(MountPolicy::Open))
                .unwrap();
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_tcp_connection(state, tcp, peer, slot).await
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();

        async fn exchange(client: &mut tokio::net::TcpStream, req: &str) -> String {
            client.write_all(req.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = client.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        }

        let r1 = exchange(
            &mut client,
            "DESCRIBE rtsp://127.0.0.1/locked RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        )
        .await;
        assert!(r1.contains("401"), "{r1}");
        let r2 = exchange(
            &mut client,
            "DESCRIBE rtsp://127.0.0.1/locked RTSP/1.0\r\nCSeq: 2\r\n\r\n",
        )
        .await;
        assert!(r2.contains("401"), "{r2}");
        let r3 = exchange(
            &mut client,
            "DESCRIBE rtsp://127.0.0.1/open RTSP/1.0\r\nCSeq: 3\r\n\r\n",
        )
        .await;
        assert!(r3.contains("200"), "open mount DESCRIBE: {r3}");
        // Third strike: the 401 is written, then the connection closes.
        let r4 = exchange(
            &mut client,
            "DESCRIBE rtsp://127.0.0.1/locked RTSP/1.0\r\nCSeq: 4\r\n\r\n",
        )
        .await;
        assert!(r4.contains("401"), "{r4}");
        let r5 = exchange(
            &mut client,
            "OPTIONS rtsp://127.0.0.1/locked RTSP/1.0\r\nCSeq: 5\r\n\r\n",
        )
        .await;
        assert!(
            r5.is_empty(),
            "connection must be closed after the third strike, got: {r5}"
        );

        drop(client);
        let _ = server_handle.await;
    }

    /// PAUSE replaces `peer_cancel` with a fresh token, so
    /// Drop must cancel whichever token is CURRENT and abort the fanout.
    #[tokio::test]
    async fn dropping_a_session_cancels_its_peer_token_and_aborts_the_fanout() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let aborted = Arc::new(AtomicBool::new(false));
        let flag = Flag(Arc::clone(&aborted));
        let mut session = ServerSessionState::new();
        let first = session.peer_cancel.clone();
        session.install_test_fanout(tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        }));
        // What PAUSE does: cancel, then replace.
        session.peer_cancel.cancel();
        session.peer_cancel = tokio_util::sync::CancellationToken::new();
        let current = session.peer_cancel.clone();
        drop(session);
        assert!(first.is_cancelled());
        assert!(
            current.is_cancelled(),
            "Drop must cancel the replaced token"
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !aborted.load(Ordering::Acquire) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fanout task was not aborted"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    // --- Publisher-side idle-bound tests (paused tokio time) ----------
    //
    // These drive `handle_tcp_connection` over a real loopback TCP connection
    // on a paused runtime, which only an in-process test can do. The
    // `$`-frame routing tests, which need no virtual clock, live in
    // `tests/rtsp_server/publish/interleaved_framing.rs` on the raw
    // publisher fixture.

    /// SDP for the one shape the MP2T publish adapter accepts — same text
    /// as `publish::handlers::tests::SDP_MP2T`, duplicated here because
    /// that one is private to its own test module.
    const SDP_MP2T: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=control:streamid=0\r\n";

    /// Bound port of a test listener driving `handle_tcp_connection` on its
    /// one accepted connection, built the same way as
    /// `session_responds_to_options`'s inline setup.
    struct PublisherTestServer {
        port: u16,
    }

    /// `test_state()` + one `/pub` publish mount + a loopback listener
    /// that runs `handle_tcp_connection` on the first connection it accepts
    /// (mirrors `session_responds_to_options`'s inline server task).
    /// Returns the listener's port (for `connect`) and the mount's
    /// `Arc` (for building the application-side transport and reading
    /// stats).
    async fn state_with_publish_mount_and_listener()
    -> (PublisherTestServer, Arc<publish::mount::PublishMountState>) {
        use crate::rtsp::server::mount::MountEntry;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = crate::rtsp::server::test_state();
        let mount = publish::mount::PublishMountState::new("/pub", 8, Default::default());
        state
            .mounts
            .lock()
            .unwrap()
            .insert("/pub".into(), MountEntry::Publish(mount.clone()));

        tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            let _ = handle_tcp_connection(state, tcp, peer, slot).await;
        });

        (PublisherTestServer { port }, mount)
    }

    async fn connect(server: &PublisherTestServer) -> TcpStream {
        tokio::net::TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap()
    }

    /// A loopback listener that runs `handle_tcp_connection` against `state`
    /// on the first connection it accepts (the same shape as
    /// `state_with_publish_mount_and_listener`, for a caller-built state).
    async fn serve_one(state: Arc<ServerState>) -> PublisherTestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            let _ = handle_tcp_connection(state, tcp, peer, slot).await;
        });
        PublisherTestServer { port }
    }

    /// Under paused time, read `c` in 1 ms virtual steps (see
    /// [`read_response_in_paused_steps`] for why) for at most `limit`,
    /// discarding any bytes. Returns how long after the call the server
    /// closed the connection, or `None` if it was still open at `limit`.
    async fn closed_within(
        c: &mut TcpStream,
        limit: std::time::Duration,
    ) -> Option<std::time::Duration> {
        let start = tokio::time::Instant::now();
        let mut chunk = [0u8; 4096];
        while start.elapsed() < limit {
            match tokio::time::timeout(std::time::Duration::from_millis(1), c.read(&mut chunk))
                .await
            {
                Ok(Ok(0)) | Ok(Err(_)) => return Some(start.elapsed()),
                Ok(Ok(_)) | Err(_) => {}
            }
        }
        None
    }

    fn announce_request(path: &str, sdp: &str) -> String {
        format!(
            "ANNOUNCE rtsp://h{path} RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
            sdp.len()
        )
    }

    /// One RTP packet (PT 33, the MP2T adapter's expected PT for
    /// `SDP_MP2T`'s `m=video 0 RTP/AVP 33`) carrying `n` TS packets as
    /// its payload. Same shape as `publish::mount::tests::rtp_mp2t`
    /// (private to that module, so re-declared here).
    fn rtp_mp2t_packet(n: usize) -> Vec<u8> {
        let mut v = vec![0x80u8, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        for _ in 0..n {
            v.push(0x47);
            v.extend(std::iter::repeat_n(0u8, 187));
        }
        v
    }

    /// One byte every 5 s re-arms the per-read idle bound forever; the
    /// per-message deadline must close the connection within the idle
    /// bound (30 s pre-SETUP) of the first byte. Paused tokio time: the
    /// test advances the clock, nothing sleeps for real.
    ///
    /// Each 5 s gap is walked in 1 ms read probes (see
    /// [`read_response_in_paused_steps`] for why a single long wait would
    /// let the clock jump past the server's timers), and closure is
    /// detected by those probes, never by write errors. The fixed server
    /// closes 30 s after the first byte; the window 25..=40 s tolerates a
    /// byte the server reads a whole step late. A server without the
    /// deadline closes only 30 s after the LAST byte (90 s or later) and
    /// fails the "still open" check at 65 s.
    #[tokio::test(start_paused = true)]
    async fn a_request_trickled_past_the_idle_bound_closes_the_session() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let (state, _mount) = state_with_publish_mount_and_listener().await;
        let mut c = connect(&state).await;
        let t0 = tokio::time::Instant::now();
        let mut closed_at = None;
        // Thirteen bytes, 5 s apart: 65 s of "request in progress".
        'trickle: for b in b"OPTIONS rtsp:".iter() {
            if c.write_all(&[*b]).await.is_err() {
                // A write to a peer that already closed: closure observed.
                closed_at = Some(t0.elapsed());
                break;
            }
            for _ in 0..5_000 {
                let mut probe = [0u8; 1];
                match tokio::time::timeout(Duration::from_millis(1), c.read(&mut probe)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => {
                        closed_at = Some(t0.elapsed());
                        break 'trickle;
                    }
                    Ok(Ok(n)) => panic!("the server answered a partial request with {n} bytes"),
                    Err(_) => {} // still open: one more 1 ms step
                }
            }
        }
        let closed_at = closed_at.expect("connection still open 65 s after the first byte");
        assert!(
            closed_at >= Duration::from_secs(25) && closed_at <= Duration::from_secs(40),
            "closure observed at {closed_at:?} after the first byte; the bound is 30 s"
        );
    }

    /// Before SETUP, requests alone do not keep a connection: OPTIONS every
    /// 10 s are each answered, the connection is closed 30 s after it
    /// opened, and its slot is released. A server whose only bound is the
    /// per-read idle timer keeps it open while the OPTIONS keep coming.
    #[tokio::test(start_paused = true)]
    async fn options_alone_do_not_hold_a_connection_past_the_admission_deadline() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let state = crate::rtsp::server::test_state();
        let server = serve_one(state.clone()).await;
        let mut c = connect(&server).await;
        let t0 = tokio::time::Instant::now();
        for cseq in 1..=3u32 {
            if cseq > 1 {
                tokio::time::advance(Duration::from_secs(10)).await;
            }
            let req = format!("OPTIONS rtsp://h/pub RTSP/1.0\r\nCSeq: {cseq}\r\n\r\n");
            c.write_all(req.as_bytes()).await.unwrap();
            let r = read_response_in_paused_steps(&mut c, Duration::from_secs(5)).await;
            assert!(r.starts_with("RTSP/1.0 200"), "request {cseq}: {r}");
        }
        assert!(
            closed_within(&mut c, Duration::from_secs(15))
                .await
                .is_some(),
            "connection still open {:?} after it opened, with no SETUP",
            t0.elapsed()
        );
        let closed_at = t0.elapsed();
        assert!(
            closed_at >= Duration::from_secs(29) && closed_at <= Duration::from_secs(32),
            "closed {closed_at:?} after it opened; the admission deadline is 30 s"
        );
        let mut steps = 0;
        while state
            .active_sessions
            .load(std::sync::atomic::Ordering::Relaxed)
            != 0
        {
            steps += 1;
            assert!(steps <= 1_000, "the session slot was not released");
            let _ =
                tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>()).await;
        }
    }

    /// Interleaved frames on a connection with no session are dropped and are
    /// not activity: one every 5 s does not keep the connection past 30 s.
    #[tokio::test(start_paused = true)]
    async fn interleaved_frames_do_not_hold_a_connection_before_setup() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let state = crate::rtsp::server::test_state();
        let server = serve_one(state).await;
        let mut c = connect(&server).await;
        let t0 = tokio::time::Instant::now();
        let mut closed_at = None;
        for _ in 0..8 {
            if c.write_all(b"$\x00\x00\x04abcd").await.is_err()
                || closed_within(&mut c, Duration::from_secs(5))
                    .await
                    .is_some()
            {
                closed_at = Some(t0.elapsed());
                break;
            }
        }
        let closed_at = closed_at.expect("connection still open 40 s after it opened");
        assert!(
            closed_at >= Duration::from_secs(29) && closed_at <= Duration::from_secs(32),
            "closed {closed_at:?} after it opened; the admission deadline is 30 s"
        );
    }

    /// After a request that passed a credential check the deadline no longer
    /// applies, but interleaved frames on the still-`Idle` connection are not
    /// activity: the per-read idle bound (30 s with no session) runs from the
    /// last request and closes it.
    #[tokio::test(start_paused = true)]
    async fn interleaved_frames_do_not_rearm_the_idle_bound_of_an_authenticated_idle_connection() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let server = serve_one(make_state_with_basic_auth()).await;
        let mut c = connect(&server).await;
        // "user:secret" base64 = "dXNlcjpzZWNyZXQ="; no such mount, so 404
        // after the credential check passed.
        c.write_all(
            b"DESCRIBE rtsp://127.0.0.1/none RTSP/1.0\r\nCSeq: 1\r\nAuthorization: Basic dXNlcjpzZWNyZXQ=\r\n\r\n",
        )
        .await
        .unwrap();
        let r = read_response_in_paused_steps(&mut c, Duration::from_secs(5)).await;
        assert!(r.starts_with("RTSP/1.0 404"), "authenticated DESCRIBE: {r}");
        let t_req = tokio::time::Instant::now();
        let mut closed_at = None;
        for _ in 0..8 {
            if c.write_all(b"$\x00\x00\x04abcd").await.is_err()
                || closed_within(&mut c, Duration::from_secs(5))
                    .await
                    .is_some()
            {
                closed_at = Some(t_req.elapsed());
                break;
            }
        }
        let closed_at = closed_at.expect("connection still open 40 s after its last request");
        assert!(
            closed_at >= Duration::from_secs(28) && closed_at <= Duration::from_secs(32),
            "closed {closed_at:?} after its last request; the idle bound is 30 s"
        );
    }

    /// Controls: a request that passed a credential check lifts the
    /// deadline, so OPTIONS every 10 s keep that connection past 30 s; and a
    /// connection that reaches SETUP well inside the deadline outlives it on
    /// keepalives (the post-SETUP bound is 90 s at the default timeout).
    #[tokio::test(start_paused = true)]
    async fn an_authenticated_or_set_up_connection_outlives_the_admission_deadline() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        async fn exchange(c: &mut TcpStream, req: &str) -> String {
            c.write_all(req.as_bytes()).await.unwrap();
            read_response_in_paused_steps(c, Duration::from_secs(5)).await
        }

        let server = serve_one(make_state_with_basic_auth()).await;
        let mut c = connect(&server).await;
        let r = exchange(
            &mut c,
            "DESCRIBE rtsp://127.0.0.1/none RTSP/1.0\r\nCSeq: 1\r\nAuthorization: Basic dXNlcjpzZWNyZXQ=\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 404"), "authenticated DESCRIBE: {r}");
        for cseq in 2..=5u32 {
            assert!(
                closed_within(&mut c, Duration::from_secs(10))
                    .await
                    .is_none(),
                "an authenticated connection was closed before OPTIONS {cseq}"
            );
            let r = exchange(
                &mut c,
                &format!("OPTIONS rtsp://h/ RTSP/1.0\r\nCSeq: {cseq}\r\n\r\n"),
            )
            .await;
            assert!(r.starts_with("RTSP/1.0 200"), "OPTIONS {cseq}: {r}");
        }

        let (server, _mount) = state_with_publish_mount_and_listener().await;
        let mut c = connect(&server).await;
        let r = exchange(&mut c, &announce_request("/pub", SDP_MP2T)).await;
        assert!(r.starts_with("RTSP/1.0 200"), "ANNOUNCE: {r}");
        let r = exchange(
            &mut c,
            "SETUP rtsp://h/pub/streamid=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "SETUP: {r}");
        let sid = r
            .lines()
            .find_map(|l| l.strip_prefix("Session: "))
            .map(|v| v.split(';').next().unwrap().trim().to_owned())
            .unwrap_or_else(|| panic!("Session header in the SETUP answer: {r}"));
        for cseq in 3..=4u32 {
            assert!(
                closed_within(&mut c, Duration::from_secs(25))
                    .await
                    .is_none(),
                "a set-up connection was closed before keepalive {cseq}"
            );
            let r = exchange(
                &mut c,
                &format!("OPTIONS rtsp://h/pub RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {sid}\r\n\r\n"),
            )
            .await;
            assert!(r.starts_with("RTSP/1.0 200"), "keepalive {cseq}: {r}");
        }
    }

    /// Read one response head under paused time, in 1 ms virtual steps, for
    /// at most `limit` of virtual time.
    ///
    /// A paused runtime that parks advances its clock to the NEXT pending
    /// timer, even while a loopback wakeup for the server task is in flight
    /// (measured on Linux: one `timeout(30 s, read)` let the clock jump
    /// ~29 s before the server read the request, far enough to fire the
    /// server's own idle bound). Polling in 1 ms steps keeps the nearest
    /// timer 1 ms away, so every jump is at most 1 ms and the server's
    /// timers only fire when the test means them to.
    async fn read_response_in_paused_steps(
        c: &mut TcpStream,
        limit: std::time::Duration,
    ) -> String {
        use std::time::Duration;
        let start = tokio::time::Instant::now();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            assert!(
                start.elapsed() < limit,
                "no response within {limit:?} of virtual time"
            );
            match tokio::time::timeout(Duration::from_millis(1), c.read(&mut chunk)).await {
                Ok(Ok(0)) => panic!("connection closed before a response"),
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => panic!("read failed: {e}"),
                Err(_) => {} // nothing yet: one more 1 ms step
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A UDP publisher's media is liveness for an IDLE control connection,
    /// never for a half-sent request: with media flowing (one admitted RTP
    /// datagram stamps `last_media_ms`, a real-time clock the paused test
    /// clock never moves, so the media reads "recent" throughout), a
    /// request trickled onto the control connection is still closed at the
    /// post-SETUP idle bound of its first byte. Without the message
    /// deadline's close, `media_recent` re-arms the sleep and the
    /// connection stays open as long as bytes and media keep coming.
    #[tokio::test(start_paused = true)]
    async fn a_trickled_request_is_closed_even_while_udp_media_keeps_the_publisher_live() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let (server, mount) = state_with_publish_mount_and_listener().await;
        let mut c = connect(&server).await;
        // The listener helper serves `test_state()`, so its bound after
        // SETUP is the default session timeout's.
        let bound =
            post_setup_idle_bound(crate::rtsp::server::test_state().builder.session_timeout);

        async fn exchange(c: &mut TcpStream, req: &str) -> String {
            c.write_all(req.as_bytes()).await.unwrap();
            read_response_in_paused_steps(c, Duration::from_secs(5)).await
        }
        let r = exchange(&mut c, &announce_request("/pub", SDP_MP2T)).await;
        assert!(r.starts_with("RTSP/1.0 200"), "ANNOUNCE: {r}");
        let media = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let p = media.local_addr().unwrap().port();
        let r = exchange(
            &mut c,
            &format!(
                "SETUP rtsp://h/pub/streamid=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP;unicast;client_port={p}-{};mode=record\r\n\r\n",
                p + 1
            ),
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "SETUP: {r}");
        let server_rtp: u16 = r
            .split("server_port=")
            .nth(1)
            .and_then(|t| t.split('-').next())
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(|| panic!("server_port in the SETUP answer: {r}"));
        let session_id = r
            .lines()
            .find_map(|l| l.strip_prefix("Session: "))
            .map(|v| v.split(';').next().unwrap().trim().to_owned())
            .unwrap_or_else(|| panic!("Session header in the SETUP answer: {r}"));
        let r = exchange(
            &mut c,
            &format!("RECORD rtsp://h/pub RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "RECORD: {r}");

        // One admitted datagram: media is now "recent" for the whole test.
        // The ingest stamps `PublishSession::now_ms()`, milliseconds since a
        // lazily created real-time epoch, and 0 means "no media yet". Run
        // alone, this datagram's stamp would be the epoch's first use and
        // land in its first millisecond, reading as "never". So start the
        // epoch and spin, in real time, until it is at least 1 ms old.
        while PublishSession::now_ms() == 0 {
            std::hint::spin_loop();
        }
        media
            .send_to(&rtp_mp2t_packet(1), ("127.0.0.1", server_rtp))
            .await
            .unwrap();
        let mut steps = 0;
        while mount.stats_snapshot().rtp_packets_received == 0 {
            steps += 1;
            assert!(steps <= 5_000, "the UDP ingest never admitted the datagram");
            let _ =
                tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>()).await;
        }

        // Trickle one byte per 5 s for the bound plus 30 s. Unlike the
        // trickle test above, the clock moves by explicit 1 ms `advance`s
        // and closure is probed with `try_read`, so this task never waits
        // on a timer. A server that lets media liveness pre-empt the
        // message deadline does not park: its read bound is spent, so it
        // re-enters `sleep(0)` in a loop, and auto-advance (which only
        // happens on a park) would freeze the clock until the real-time
        // media stamp aged out, hiding the failure. Explicit steps keep
        // the clock moving either way.
        let n_bytes = ((bound + Duration::from_secs(30)).as_secs() / 5) as usize;
        let request: Vec<u8> = b"OPTIONS rtsp://h/"
            .iter()
            .copied()
            .chain(std::iter::repeat(b'a'))
            .take(n_bytes)
            .collect();
        let t0 = tokio::time::Instant::now();
        let mut closed_at = None;
        'trickle: for b in &request {
            if c.write_all(&[*b]).await.is_err() {
                closed_at = Some(t0.elapsed());
                break;
            }
            for _ in 0..5_000 {
                tokio::time::advance(Duration::from_millis(1)).await;
                let mut probe = [0u8; 1];
                match c.try_read(&mut probe) {
                    Ok(0) => {
                        closed_at = Some(t0.elapsed());
                        break 'trickle;
                    }
                    Ok(n) => panic!("the server answered a partial request with {n} bytes"),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {} // still open
                    Err(_) => {
                        closed_at = Some(t0.elapsed()); // reset: closed
                        break 'trickle;
                    }
                }
            }
        }
        let closed_at = closed_at.unwrap_or_else(|| {
            panic!(
                "connection still open {:?} after the first byte (bound {bound:?})",
                t0.elapsed()
            )
        });
        assert!(
            closed_at >= bound - Duration::from_secs(5)
                && closed_at <= bound + Duration::from_secs(10),
            "closure observed at {closed_at:?} after the first byte; the bound is {bound:?}"
        );
    }

    // --- Reply writes against the publish idle timeout (real time) ----
    //
    // `last_media_ms` is a real-time clock, so these run unpaused with a
    // short publish idle timeout. The session runs over an in-memory duplex
    // whose 1 KiB buffer fills once the test stops reading replies, which
    // parks the server's reply write.

    /// `test_state()` with `publish_idle_timeout(timeout)`, one `/pub`
    /// publish mount, and a session task serving one duplex connection.
    async fn publish_idle_session(
        timeout: std::time::Duration,
    ) -> (
        Arc<ServerState>,
        Arc<publish::mount::PublishMountState>,
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<Result<(), RtspServerError>>,
    ) {
        use crate::rtsp::server::mount::MountEntry;
        let mut builder =
            crate::rtsp::server::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.publish_idle_timeout(timeout);
        let state = Arc::new(ServerState {
            builder,
            ..Arc::try_unwrap(crate::rtsp::server::test_state())
                .ok()
                .expect("fresh test state")
        });
        let mount = publish::mount::PublishMountState::new("/pub", 8, Default::default());
        state
            .mounts
            .lock()
            .unwrap()
            .insert("/pub".into(), MountEntry::Publish(mount.clone()));
        let (client, task) = duplex_session(state.clone());
        (state, mount, client, task)
    }

    /// A session task serving one in-memory duplex connection against
    /// `state`, its 1 KiB buffers filling as soon as the test stops reading.
    fn duplex_session(
        state: Arc<ServerState>,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<Result<(), RtspServerError>>,
    ) {
        let (client, server_end) = tokio::io::duplex(1024);
        let (read_half, write_half) = tokio::io::split(server_end);
        state
            .active_sessions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let slot = SessionSlot::new(state.clone());
        let task = tokio::spawn(serve_connection(
            state,
            read_half,
            session_writer(write_half),
            "127.0.0.1:40000".parse().unwrap(),
            slot,
            "test session opened",
        ));
        (client, task)
    }

    /// Write one request on the duplex and read its response head.
    async fn duplex_exchange(c: &mut tokio::io::DuplexStream, req: &str) -> String {
        use tokio::io::AsyncWriteExt;
        c.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = c.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the session closed before a response");
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn session_header(r: &str) -> String {
        r.lines()
            .find_map(|l| l.strip_prefix("Session: "))
            .map(|v| v.split(';').next().unwrap().trim().to_owned())
            .unwrap_or_else(|| panic!("Session header in: {r}"))
    }

    /// Stop reading replies and pipeline GET_PARAMETERs until the server's
    /// reply write parks on the full duplex. Returns the feeder task (abort
    /// it at the end) and the read half the test keeps but never reads.
    fn park_reply_writes(
        client: tokio::io::DuplexStream,
        sid: String,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
    ) {
        use tokio::io::AsyncWriteExt;
        let (unread, mut w) = tokio::io::split(client);
        let feeder = tokio::spawn(async move {
            for cseq in 10..2_000u32 {
                let req = format!(
                    "GET_PARAMETER rtsp://h/pub RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {sid}\r\n\r\n"
                );
                if w.write_all(req.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        (feeder, unread)
    }

    /// A recording publisher that sends no media and stops reading replies
    /// loses its session at the publish idle timeout, not when the parked
    /// reply write reaches the post-SETUP bound (90 s at the default).
    #[tokio::test]
    async fn a_parked_reply_write_does_not_outlast_the_publish_idle_timeout() {
        use std::time::Duration;
        let timeout = Duration::from_millis(500);
        let (state, mount, mut c, task) = publish_idle_session(timeout).await;
        let r = duplex_exchange(&mut c, &announce_request("/pub", SDP_MP2T)).await;
        assert!(r.starts_with("RTSP/1.0 200"), "ANNOUNCE: {r}");
        let r = duplex_exchange(
            &mut c,
            "SETUP rtsp://h/pub/streamid=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "SETUP: {r}");
        let sid = session_header(&r);
        let r = duplex_exchange(
            &mut c,
            &format!("RECORD rtsp://h/pub RTSP/1.0\r\nCSeq: 3\r\nSession: {sid}\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "RECORD: {r}");

        let (feeder, _unread) = park_reply_writes(c, sid);
        let ended = tokio::time::timeout(
            tst_test_helpers::timing::scaled(Duration::from_secs(10)),
            task,
        )
        .await;
        feeder.abort();
        assert!(
            ended.is_ok(),
            "a parked reply write kept a media-less publisher past the publish idle timeout ({timeout:?})"
        );
        assert!(
            mount.publisher.lock().unwrap().info.is_none(),
            "the publisher slot was not released"
        );
        assert_eq!(
            state
                .active_sessions
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    /// Control: with UDP media arriving on its own sockets, a parked reply
    /// write does not end the publisher at the publish idle timeout; it keeps
    /// waiting, and the server cancel still ends it.
    #[tokio::test]
    async fn a_parked_reply_write_does_not_end_a_publisher_whose_media_flows() {
        use std::time::Duration;
        let timeout = Duration::from_millis(500);
        let (state, _mount, mut c, task) = publish_idle_session(timeout).await;
        let r = duplex_exchange(&mut c, &announce_request("/pub", SDP_MP2T)).await;
        assert!(r.starts_with("RTSP/1.0 200"), "ANNOUNCE: {r}");
        let media = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let p = media.local_addr().unwrap().port();
        let r = duplex_exchange(
            &mut c,
            &format!(
                "SETUP rtsp://h/pub/streamid=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP;unicast;client_port={p}-{};mode=record\r\n\r\n",
                p + 1
            ),
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "SETUP: {r}");
        let server_rtp: u16 = r
            .split("server_port=")
            .nth(1)
            .and_then(|t| t.split('-').next())
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(|| panic!("server_port in the SETUP answer: {r}"));
        let sid = session_header(&r);
        let r = duplex_exchange(
            &mut c,
            &format!("RECORD rtsp://h/pub RTSP/1.0\r\nCSeq: 3\r\nSession: {sid}\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "RECORD: {r}");

        // Start the real-time media epoch (see the paused UDP test above).
        while PublishSession::now_ms() == 0 {
            std::hint::spin_loop();
        }
        let sender = {
            let media = media.clone();
            tokio::spawn(async move {
                for seq in 1..=u16::MAX {
                    let mut pkt = rtp_mp2t_packet(1);
                    pkt[2..4].copy_from_slice(&seq.to_be_bytes());
                    let _ = media.send_to(&pkt, ("127.0.0.1", server_rtp)).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
        };

        let (feeder, _unread) = park_reply_writes(c, sid);
        tokio::time::sleep(timeout * 4).await;
        assert!(
            !task.is_finished(),
            "a parked reply write ended a publisher whose media kept flowing"
        );
        state.cancel_token.cancel();
        let ended = tokio::time::timeout(
            tst_test_helpers::timing::scaled(Duration::from_secs(5)),
            task,
        )
        .await;
        sender.abort();
        feeder.abort();
        assert!(ended.is_ok(), "the session ignored the server cancel");
    }

    /// Before SETUP, pipeline OPTIONS without pause on a duplex session and
    /// read the replies back 16 bytes every 100 ms until `stop_reading_at`
    /// (never stop when `None`). Returns how long after it opened the
    /// session ended, or panics if it was still open at 60 s. The next
    /// requests are always buffered when a reply write finishes, so the read
    /// is ready at every turn of the loop.
    async fn pipelined_options_closed_after(
        stop_reading_at: Option<std::time::Duration>,
    ) -> std::time::Duration {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let state = crate::rtsp::server::test_state();
        let (client, task) = duplex_session(state.clone());
        let t0 = tokio::time::Instant::now();
        let (mut r, mut w) = tokio::io::split(client);
        let feeder = tokio::spawn(async move {
            for cseq in 1u32.. {
                let req = format!("OPTIONS rtsp://h/pub RTSP/1.0\r\nCSeq: {cseq}\r\n\r\n");
                if w.write_all(req.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let mut replied = 0usize;
        while !task.is_finished() && t0.elapsed() < Duration::from_secs(60) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if stop_reading_at.is_some_and(|t| t0.elapsed() >= t) {
                continue;
            }
            let mut few = [0u8; 16];
            if let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(1), r.read(&mut few)).await
            {
                replied += n;
            }
        }
        let closed_at = t0.elapsed();
        feeder.abort();
        assert!(replied > 0, "the server never answered");
        assert!(
            task.is_finished(),
            "connection still open {closed_at:?} after it opened, with no SETUP"
        );
        assert_eq!(
            state
                .active_sessions
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the session slot was not released"
        );
        closed_at
    }

    /// A pre-SETUP peer that pipelines OPTIONS and keeps reading the replies
    /// slowly is closed at the admission deadline: the deadline holds
    /// without a timer having to win against a ready read.
    #[tokio::test(start_paused = true)]
    async fn pipelined_options_read_back_slowly_do_not_outlast_the_admission_deadline() {
        use std::time::Duration;
        let closed_at = pipelined_options_closed_after(None).await;
        assert!(
            closed_at >= Duration::from_secs(29) && closed_at <= Duration::from_secs(32),
            "closed {closed_at:?} after it opened; the admission deadline is 30 s"
        );
    }

    /// A pre-SETUP reply write parked when the peer stops reading ends at
    /// the admission deadline, not a full idle bound after it started.
    #[tokio::test(start_paused = true)]
    async fn a_reply_write_parked_before_setup_does_not_outlast_the_admission_deadline() {
        use std::time::Duration;
        let closed_at = pipelined_options_closed_after(Some(Duration::from_secs(25))).await;
        assert!(
            closed_at >= Duration::from_secs(29) && closed_at <= Duration::from_secs(32),
            "closed {closed_at:?} after it opened; the admission deadline is 30 s"
        );
    }

    /// A recording publisher that sends no media but pipelines GET_PARAMETER
    /// keepalives and reads every reply loses its session at the publish
    /// idle timeout: requests never re-arm the window, and the window holds
    /// even when no timer gets to fire. The test keeps the runtime busy, so
    /// the paused clock never auto-advances, and it first moves the clock
    /// half a millisecond off a timer tick, so even a zero-length sleep
    /// (rounded up to the next tick) stays pending.
    #[tokio::test(start_paused = true)]
    async fn pipelined_keepalives_do_not_outlast_the_publish_idle_timeout() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        let timeout = Duration::from_millis(300);
        let (state, mount, mut c, task) = publish_idle_session(timeout).await;
        let r = duplex_exchange(&mut c, &announce_request("/pub", SDP_MP2T)).await;
        assert!(r.starts_with("RTSP/1.0 200"), "ANNOUNCE: {r}");
        let r = duplex_exchange(
            &mut c,
            "SETUP rtsp://h/pub/streamid=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "SETUP: {r}");
        let sid = session_header(&r);
        let r = duplex_exchange(
            &mut c,
            &format!("RECORD rtsp://h/pub RTSP/1.0\r\nCSeq: 3\r\nSession: {sid}\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("RTSP/1.0 200"), "RECORD: {r}");
        tokio::time::advance(Duration::from_micros(500)).await;

        let (mut r, mut w) = tokio::io::split(c);
        let feeder = tokio::spawn(async move {
            for cseq in 10u32.. {
                let req = format!(
                    "GET_PARAMETER rtsp://h/pub RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {sid}\r\n\r\n"
                );
                if w.write_all(req.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let reader = tokio::spawn(async move {
            let mut chunk = [0u8; 1024];
            while matches!(r.read(&mut chunk).await, Ok(n) if n > 0) {}
        });
        // `idle_for` reads a real-time clock: wait in real time, yielding so
        // the paused tokio clock stays where it is.
        let start = std::time::Instant::now();
        let limit = tst_test_helpers::timing::scaled(Duration::from_secs(5));
        while !task.is_finished() && start.elapsed() < limit {
            tokio::task::yield_now().await;
        }
        feeder.abort();
        reader.abort();
        assert!(
            task.is_finished(),
            "pipelined keepalives kept a media-less publisher past the publish idle timeout ({timeout:?})"
        );
        assert!(
            mount.publisher.lock().unwrap().info.is_none(),
            "the publisher slot was not released"
        );
        assert_eq!(
            state
                .active_sessions
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    /// A reference model of the session lifecycle, written from what the
    /// handlers do today, and a property test that drives random request
    /// sequences through `dispatch` and compares every response status and
    /// the session's observable fields against it.
    mod lifecycle_model {
        use super::*;
        use crate::rtsp::server::mount::{MountEntry, MountKind, MountState};
        use crate::rtsp::server::publish::mount::PublishMountState;
        use crate::rtsp::server::{test_muxer_cfg, test_state};
        use proptest::prelude::*;

        const SDP_MP2T: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=control:streamid=0\r\n";

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Mount {
            /// A muxer-backed mount that only readers use.
            Live,
            /// A publish mount: one publisher, and readers of the re-served bytes.
            Pub,
        }

        impl Mount {
            fn path(self) -> &'static str {
                match self {
                    Mount::Live => "/live",
                    Mount::Pub => "/pub",
                }
            }
        }

        #[derive(Clone, Copy, Debug)]
        enum Call {
            Options,
            Describe,
            SetupReader,
            SetupRecord,
            Play,
            Pause,
            Teardown,
            Announce,
            Record,
            GetParameter,
        }

        /// What the request's `Session:` header carries.
        #[derive(Clone, Copy, Debug)]
        enum Sess {
            Absent,
            /// The id this session was given, when it has one.
            Echo,
            Wrong,
        }

        /// The session's lifecycle position. `mount` is the mount the
        /// session's `mount_path` names.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Model {
            Init,
            Described,
            ReaderSetup {
                mount: Mount,
            },
            Playing {
                mount: Mount,
            },
            Paused {
                mount: Mount,
            },
            Announced {
                mount: Mount,
            },
            PublisherSetup {
                mount: Mount,
            },
            Recording {
                mount: Mount,
            },
            /// After a TEARDOWN: the connection closes, so no request follows.
            Closed,
        }

        /// The status `call` draws in `model`, and the position it leaves.
        fn expect(model: Model, call: Call, at: Mount, wrong_sid: bool) -> (u16, Model) {
            use Model::*;
            let publisher = matches!(
                model,
                Announced { .. } | PublisherSetup { .. } | Recording { .. }
            );
            let has_session = matches!(model, ReaderSetup { .. } | Playing { .. } | Paused { .. });
            // A publisher's SETUP naming another mount answers like a
            // missing mount.
            if let (
                Call::SetupReader | Call::SetupRecord,
                Announced { mount: m } | PublisherSetup { mount: m } | Recording { mount: m },
            ) = (call, model)
                && m != at
            {
                return (404, model);
            }
            match call {
                Call::Options | Call::GetParameter => (200, model),
                Call::Describe => (200, if model == Init { Described } else { model }),
                Call::Teardown => (200, Closed),
                Call::Announce => {
                    if publisher || has_session {
                        (455, model)
                    } else if at == Mount::Live {
                        (461, model)
                    } else {
                        (200, Announced { mount: at })
                    }
                }
                Call::SetupReader => {
                    if publisher {
                        (455, model)
                    } else if matches!(model, Playing { .. } | Paused { .. }) {
                        // A SETUP cannot re-point a reader that is playing
                        // or paused: the running fanout would keep serving
                        // the old mount.
                        (455, model)
                    } else {
                        (200, ReaderSetup { mount: at })
                    }
                }
                Call::SetupRecord => {
                    if at == Mount::Live {
                        (461, model)
                    } else {
                        match model {
                            Announced { .. } => (200, PublisherSetup { mount: at }),
                            PublisherSetup { .. } | Recording { .. } => {
                                (if wrong_sid { 454 } else { 455 }, model)
                            }
                            _ => (455, model),
                        }
                    }
                }
                Call::Play => {
                    if publisher {
                        (455, model)
                    } else {
                        match model {
                            ReaderSetup { mount } | Playing { mount } | Paused { mount } => {
                                (200, Playing { mount })
                            }
                            _ => (454, model),
                        }
                    }
                }
                Call::Pause => match model {
                    ReaderSetup { mount } | Playing { mount } | Paused { mount } => {
                        (200, Paused { mount })
                    }
                    PublisherSetup { .. } | Recording { .. } => (200, model),
                    _ => (454, model),
                },
                Call::Record => match model {
                    PublisherSetup { mount } | Recording { mount } => (200, Recording { mount }),
                    _ => (455, model),
                },
            }
        }

        fn request(call: Call, at: Mount, sess: Sess, session: &ServerSessionState) -> RtspRequest {
            use crate::rtsp::message::RtspMethod as M;
            let (method, uri) = match call {
                Call::Options => (M::Options, format!("rtsp://h{}", at.path())),
                Call::Describe => (M::Describe, format!("rtsp://h{}", at.path())),
                Call::SetupReader | Call::SetupRecord => {
                    (M::Setup, format!("rtsp://h{}/streamid=0", at.path()))
                }
                Call::Play => (M::Play, format!("rtsp://h{}", at.path())),
                Call::Pause => (M::Pause, format!("rtsp://h{}", at.path())),
                Call::Teardown => (M::Teardown, format!("rtsp://h{}", at.path())),
                Call::Announce => (M::Announce, format!("rtsp://h{}", at.path())),
                Call::Record => (M::Record, format!("rtsp://h{}", at.path())),
                Call::GetParameter => (M::GetParameter, format!("rtsp://h{}", at.path())),
            };
            let mut r = RtspRequest::new(method, uri, crate::url::RtspVersion::V1_0);
            r.headers.insert("cseq".into(), "1".into());
            match call {
                Call::SetupReader => {
                    r.headers.insert(
                        "transport".into(),
                        "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
                    );
                }
                Call::SetupRecord => {
                    r.headers.insert(
                        "transport".into(),
                        "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record".into(),
                    );
                }
                Call::Announce => {
                    r.headers
                        .insert("content-type".into(), "application/sdp".into());
                    r.body = bytes::Bytes::from(SDP_MP2T);
                }
                _ => {}
            }
            match sess {
                Sess::Absent => {}
                Sess::Echo => {
                    if let Some(id) = session.session_id() {
                        r.headers.insert("session".into(), id.to_owned());
                    }
                }
                Sess::Wrong => {
                    r.headers.insert("session".into(), "not-the-id".into());
                }
            }
            r
        }

        /// The session fields `model` implies.
        fn check_fields(model: Model, s: &ServerSessionState) -> Result<(), TestCaseError> {
            use Model::*;
            let (id, path, fanout, publish, recording) = match model {
                Init | Described | Closed => (false, None, false, false, false),
                ReaderSetup { mount } | Paused { mount } => {
                    (true, Some(mount.path()), false, false, false)
                }
                Playing { mount } => (true, Some(mount.path()), true, false, false),
                Announced { .. } => (false, None, false, true, false),
                PublisherSetup { mount } => (true, Some(mount.path()), false, true, false),
                Recording { mount } => (true, Some(mount.path()), false, true, true),
            };
            prop_assert_eq!(s.session_id().is_some(), id, "session_id in {:?}", model);
            prop_assert_eq!(s.mount_path(), path, "mount_path in {:?}", model);
            prop_assert_eq!(s.is_playing(), fanout, "fanout in {:?}", model);
            prop_assert_eq!(s.publisher().is_some(), publish, "publish in {:?}", model);
            if let Some(p) = s.publisher() {
                prop_assert_eq!(p.recording, recording, "recording in {:?}", model);
            }
            Ok(())
        }

        fn run(calls: &[(Call, Mount, Sess)]) -> Result<(), TestCaseError> {
            let state = test_state();
            {
                let mut mounts = state.mounts.lock().unwrap();
                let live =
                    MountState::new("/live", MountKind::Unicast, test_muxer_cfg(), 8).unwrap();
                mounts.insert("/live".into(), MountEntry::Local(live));
                let publish = PublishMountState::new("/pub", 8, Default::default());
                mounts.insert("/pub".into(), MountEntry::Publish(publish));
            }
            let mut session = ServerSessionState::new();
            session.writer = Some(session_writer(tokio::io::sink()));
            let mut model = Model::Init;
            let mut trail = Vec::new();
            for &(call, at, sess) in calls {
                trail.push((call, at, sess));
                let req = request(call, at, sess, &session);
                let wrong_sid = matches!(sess, Sess::Wrong);
                let (want, next) = expect(model, call, at, wrong_sid);
                let got = dispatch(&req, &state, &mut session).status;
                prop_assert_eq!(
                    got,
                    want,
                    "status of {:?} in {:?}; sequence so far {:?}",
                    (call, at, sess),
                    model,
                    trail
                );
                model = next;
                if model == Model::Closed {
                    // The connection closes after a TEARDOWN: every field is clear.
                    check_fields(model, &session)?;
                    break;
                }
                check_fields(model, &session)?;
            }
            Ok(())
        }

        fn call() -> impl Strategy<Value = Call> {
            prop_oneof![
                2 => Just(Call::Options),
                2 => Just(Call::Describe),
                4 => Just(Call::SetupReader),
                3 => Just(Call::SetupRecord),
                4 => Just(Call::Play),
                3 => Just(Call::Pause),
                1 => Just(Call::Teardown),
                3 => Just(Call::Announce),
                3 => Just(Call::Record),
                2 => Just(Call::GetParameter),
            ]
        }

        fn at() -> impl Strategy<Value = Mount> {
            prop_oneof![Just(Mount::Live), Just(Mount::Pub)]
        }

        fn sess() -> impl Strategy<Value = Sess> {
            prop_oneof![
                3 => Just(Sess::Absent),
                3 => Just(Sess::Echo),
                1 => Just(Sess::Wrong),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2048))]

            #[test]
            fn requests_follow_the_lifecycle_model(
                calls in proptest::collection::vec((call(), at(), sess()), 1..=12),
            ) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async { run(&calls) })?;
            }
        }
    }
}
