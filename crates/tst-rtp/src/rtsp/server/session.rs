//! Per-connection async task — runs the RTSP request/response loop
//! against one client. Reads requests via tokio AsyncRead, dispatches
//! to handlers, writes responses back.
//!
//! State machine: Idle → Described (after DESCRIBE) → SettingUp (after
//! SETUP) → Playing (after PLAY) → TornDown (after TEARDOWN or
//! disconnect). State lives in [`ServerSessionState`] with fan-out
//! subscription handle and transport choice (Udp/TcpInterleaved).
//!
//! A TLS variant `handle_connection_tls` is gated on `feature = "rtsp-server-tls"`.
//! Without `tokio-rustls`, the TLS path is not implemented.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::RtspServerError;
use crate::rtsp::message::{RtspFraming, RtspMethod, RtspRequest};
use crate::rtsp::server::ServerState;
use crate::rtsp::server::auth::generate_nonce;
use crate::rtsp::server::fanout::PeerDropCounter;
use crate::rtsp::server::handlers;

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
/// path, where the old in-`handle_connection` `fetch_sub` was never
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

/// Per-connection idle read timeout. If no bytes arrive within this window
/// the session closes. Bounds slow-loris attacks that drip bytes slowly
/// toward a huge declared Content-Length, keeping the connection alive
/// (and memory growing) indefinitely.
const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Per-session state. Lives for the duration of one client's TCP
/// connection. Tracks transport choice and allocated UDP sockets /
/// interleaved channels.
pub struct ServerSessionState {
    /// RTSP session ID — None pre-SETUP, Some after SETUP returns 200.
    pub session_id: Option<String>,
    /// Mount path the client SETUP'd against, e.g. "/live". None
    /// pre-SETUP.
    pub mount_path: Option<String>,
    /// Nonce for Digest auth — generated per connection and emitted in
    /// every WWW-Authenticate; rotated by `challenge_response` on a
    /// stale re-challenge (which also resets `auth_nc_hwm`).
    pub auth_nonce: String,
    /// Digest nc (nonce-count) high-water mark for replay detection
    /// (DA-RTP-4b). Tracks the highest nc observed/consumed under the
    /// current nonce — it advances even when the digest-response check
    /// subsequently fails (see `verify_digest`), so a captured header
    /// can never be replayed. Reset to 0 whenever the nonce is rotated.
    pub(crate) auth_nc_hwm: u32,
    /// Count of consecutive 401-bounced auth requests on this connection
    /// since the last successful authentication. Increments on every 401;
    /// resets to 0 on a successful (2xx) response from an auth-gated method
    /// (DESCRIBE/SETUP/PLAY/PAUSE/TEARDOWN). Non-auth-gated methods
    /// (OPTIONS, GET_PARAMETER) never reset it, so an attacker cannot bypass
    /// the 3-strike limit by interleaving OPTIONS between bad-auth requests.
    /// After 3 consecutive failures the session closes.
    pub auth_failures: u8,
    /// Transport negotiation result from SETUP. None pre-SETUP.
    pub transport: Option<crate::rtsp::client::transport_negotiation::TransportResponse>,
    /// Server-allocated UDP RTP+RTCP pair (for UDP-transport sessions).
    /// `None` for TCP-interleaved sessions, multicast SETUPs, or
    /// pre-SETUP. T17's PLAY handler hands these to the per-peer
    /// fan-out task.
    pub udp_sockets: Option<(
        std::sync::Arc<tokio::net::UdpSocket>,
        std::sync::Arc<tokio::net::UdpSocket>,
    )>,
    /// Interleaved RTP+RTCP channel pair (for TCP-interleaved sessions).
    /// `None` for UDP sessions or pre-SETUP.
    pub interleaved_channels: Option<(u8, u8)>,
    /// JoinHandle for the per-peer fanout subscriber task (`spawn_peer_fanout`).
    /// `Some` after PLAY, `None` after PAUSE or TEARDOWN.
    /// PAUSE may re-spawn on a subsequent PLAY.
    pub fanout_handle: Option<tokio::task::JoinHandle<()>>,
    /// CancellationToken for the per-peer task. PAUSE cancels + replaces
    /// with a fresh token so a subsequent PLAY can re-spawn; TEARDOWN
    /// cancels + drops the handle without replacement.
    pub peer_cancel: tokio_util::sync::CancellationToken,
    /// Drop counter observed by `MountStats::frames_dropped_total`. Held
    /// here so the session can keep the `Arc` alive for the duration of
    /// the fanout task even after PAUSE drops the JoinHandle. Field is
    /// `pub(crate)` because `PeerDropCounter` itself is crate-private.
    pub(crate) peer_drop_counter: Option<std::sync::Arc<PeerDropCounter>>,
    /// Async-locked write half of the per-session TCP. Populated at
    /// session start by `handle_connection_inner` (where the `TcpStream`
    /// is split). The PLAY handler clones this `Arc` into
    /// `PeerTransport::Interleaved` so the per-peer fanout task can
    /// share the control TCP for RFC 7826 §14 interleaved RTP frames.
    /// `None` only in unit-test constructions that don't drive a real
    /// connection (the session loop always populates it). Field is
    /// `pub(crate)` because `OwnedWriteHalf` is a tokio internal type
    /// we don't expose across the crate boundary.
    pub(crate) tcp_write: Option<Arc<AsyncMutex<OwnedWriteHalf>>>,
    /// The TCP peer address for this session. Set by `handle_connection_inner`
    /// from the accepted socket's peer address. Used by `handle_play` to
    /// direct UDP RTP packets to the client's actual IP, not loopback.
    /// Defaults to `0.0.0.0:0` for unit-test sessions that don't go
    /// through `handle_connection_inner`.
    pub(crate) peer_addr: std::net::SocketAddr,
}

impl ServerSessionState {
    pub(crate) fn new() -> Self {
        Self {
            session_id: None,
            mount_path: None,
            auth_nonce: generate_nonce(),
            auth_nc_hwm: 0,
            auth_failures: 0,
            transport: None,
            udp_sockets: None,
            interleaved_channels: None,
            fanout_handle: None,
            peer_cancel: tokio_util::sync::CancellationToken::new(),
            peer_drop_counter: None,
            tcp_write: None,
            peer_addr: std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        }
    }
}

/// Handle one client TCP connection until disconnect or cancel.
///
/// `_slot` is the [`SessionSlot`] guard for the `active_sessions` slot the
/// accept loop reserved before spawning this task; holding it here (and
/// dropping it when this future ends) releases the slot on every exit
/// path. The counter is therefore touched only by the accept loop (+reserve)
/// and this guard's `Drop` (-release) — never here directly.
pub(crate) async fn handle_connection(
    state: Arc<ServerState>,
    tcp: TcpStream,
    peer: SocketAddr,
    _slot: SessionSlot,
) -> Result<(), RtspServerError> {
    let entry = crate::rtsp::server::register_session(&state, peer);
    let res = handle_connection_inner(state.clone(), tcp, peer, entry.clone()).await;
    crate::rtsp::server::unregister_session(&state, &entry);
    res
}

async fn handle_connection_inner(
    state: Arc<ServerState>,
    tcp: TcpStream,
    peer: SocketAddr,
    session_entry: Arc<crate::rtsp::server::ActiveSession>,
) -> Result<(), RtspServerError> {
    tracing::info!(target: "tst_rtp::server", peer = %peer, "session opened");

    // Split the TCP up front. The write half lives behind an
    // `Arc<AsyncMutex>` so the PLAY handler can hand a clone to the
    // per-peer fanout task; that task interleaves RFC 7826 §14 binary
    // RTP frames on the same TCP as the RTSP control responses. The
    // mutex serializes the two writers so an RTSP response is never
    // intermixed mid-bytes with a `$<channel><len><payload>` frame.
    let (read_half, write_half) = tcp.into_split();
    let write_half = Arc::new(AsyncMutex::new(write_half));

    let mut session = ServerSessionState::new();
    // Populate the session's write-half handle so PLAY can clone it
    // into `PeerTransport::Interleaved` without re-plumbing through the
    // handler signature.
    session.tcp_write = Some(write_half.clone());
    // Record the peer IP so `handle_play`'s UDP branch can direct RTP
    // packets to the client's real address rather than loopback.
    session.peer_addr = peer;

    // Stash the same write-half `Arc` on the public session registry
    // so `RtspServer::stop` can write the RFC 7826 §13.5.1 Notice 5402
    // ANNOUNCE before cancelling. Using `if let Ok` for the std::Mutex
    // matches the rest of the file's poison-tolerant pattern.
    if let Ok(mut g) = session_entry.tcp_write.lock() {
        *g = Some(write_half.clone());
    }

    serve_requests(state, peer, session_entry, session, read_half, write_half).await
}

/// The RTSP request/response loop, generic over the underlying byte
/// stream's split halves so the plain `rtsp://` (TCP) and `rtsps://`
/// (TLS) sessions share one implementation — the "shared generic over
/// `AsyncRead + AsyncWrite`" shape the plain/TLS handlers were always
/// meant to converge on.
///
/// Reads requests, dispatches each to its handler, writes the response,
/// and enforces the auth-failure + TEARDOWN close semantics. The caller
/// owns whether `session.tcp_write` is populated (plain TCP stashes its
/// `OwnedWriteHalf` for §14 interleaved fanout; TLS leaves it `None`).
async fn serve_requests<R, W>(
    state: Arc<ServerState>,
    peer: SocketAddr,
    session_entry: Arc<crate::rtsp::server::ActiveSession>,
    mut session: ServerSessionState,
    mut read_half: R,
    write_half: Arc<AsyncMutex<W>>,
) -> Result<(), RtspServerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Bounded read buffer — RTSP requests are typically << 4 KiB.
    let mut buf: Vec<u8> = Vec::with_capacity(8192);

    loop {
        // Cancellation guard — hard cancel exits the loop immediately,
        // graceful cancel observed via the tokio::select! below.
        if state.hard_cancel.is_cancelled() {
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
        // READ_IDLE_TIMEOUT per-read bounds that window.
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
            _ = tokio::time::sleep(READ_IDLE_TIMEOUT) => {
                tracing::warn!(
                    target: "tst_rtp::server",
                    peer = %peer,
                    timeout_secs = READ_IDLE_TIMEOUT.as_secs(),
                    "idle read timeout; closing session"
                );
                break;
            },
            _ = state.cancel_token.cancelled() => {
                tracing::info!(target: "tst_rtp::server", peer = %peer, "graceful cancel observed");
                break;
            }
            _ = session_entry.cancel.cancelled() => {
                tracing::info!(target: "tst_rtp::server", peer = %peer, "per-session cancel observed");
                break;
            }
        };
        buf.extend_from_slice(&chunk[..n]);

        // Body-aware cap, coherent with the client `send_and_read` loop and
        // both interleaved pumps (all four share `rtsp_frame_decision` + the
        // same MAX_RTSP_MESSAGE_BYTES / MAX_RTSP_BODY_BYTES constants). The
        // earlier blanket "buf.len() > 64 KiB → 413" cap wrongly rejected a
        // valid request whose body legitimately ran up to MAX_RTSP_BODY_BYTES
        // (1 MiB) — the exact incoherence the B1/B2 reviewers flagged. Now:
        //
        //   Phase 1 (no CRLFCRLF yet): 413 only once the *headers* exceed
        //     MAX_RTSP_MESSAGE_BYTES (64 KiB) — preserves the unterminated-
        //     header DoS bound.
        //   Phase 2 (CRLFCRLF seen): parse the declared Content-Length up
        //     front. An over-cap (> 1 MiB) / malformed / duplicate value is a
        //     413 NOW (don't read toward EOF). A legitimate body up to 1 MiB is
        //     awaited in full; the exact header + 4 + content_length ceiling
        //     bounds a peer dribbling past its declared body.
        //
        // The decision is applied to the buffer head (`rtsp_frame_decision`
        // assumes a buffer beginning with an RTSP message); pipelined requests
        // are drained one at a time by the inner parse loop below, so the head
        // is always either an in-progress request or empty.
        //
        // NeedMore / Complete fall through to parse complete request(s) below:
        // a complete request is parsed + drained; NeedMore loops back to read
        // more (bounded: header ≤ 64 KiB, body ≤ 1 MiB).
        if reject_if_over_cap(&buf, peer, &write_half).await {
            return Ok(());
        }

        // Parse complete request(s). Framing is decided FIRST (CORR-25): a
        // frame that is complete but does not parse is answered — 501 for a
        // method we don't implement, 400 otherwise — and DRAINED, so a
        // `SET_PARAMETER` no longer wedges every request queued behind it
        // until the idle timeout. An incomplete frame loops back to read.
        // `while let`, not `loop { match … }`: every non-`Complete` framing
        // outcome means "stop draining"; `NeedMore` then loops back to read
        // more, while `HeadersTooLong` / `BadContentLength` on a pipelined
        // head is caught by the `reject_if_over_cap` call right after this
        // loop, so an already-buffered oversized second request gets its
        // 413 immediately rather than waiting on another socket read (or
        // the idle timeout) that may never come.
        while let RtspFraming::Complete { total_len } =
            crate::rtsp::message::rtsp_frame_decision(&buf)
        {
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
                    let bytes = unparseable_reply_bytes(&reply);
                    let mut guard = write_half.lock().await;
                    if let Err(e) = guard.write_all(&bytes).await {
                        tracing::warn!(target: "tst_rtp::server", peer = %peer, error = %e, "write failed");
                        return Ok(());
                    }
                    continue;
                }
            };
            debug_assert_eq!(
                consumed, total_len,
                "parser and framer disagree on the frame length"
            );
            buf.drain(..consumed);
            let response = dispatch(&req, &state, &mut session);
            // Mirror the per-session state's `session_id` + `mount_path`
            // back onto the public `ActiveSession` registry entry. SETUP
            // populates them on the 200 path; TEARDOWN clears them. The
            // registry copies are what `RtspServer::stop` reads to build
            // the Notice 5402 ANNOUNCE on graceful shutdown.
            if let Ok(mut g) = session_entry.session_id.lock() {
                g.clone_from(&session.session_id);
            }
            if let Ok(mut g) = session_entry.mount_path.lock() {
                g.clone_from(&session.mount_path);
            }
            let bytes = response.encode();
            // Lock the write half to serialize against the fanout task
            // (which holds it for RFC 7826 §14 interleaved frames).
            {
                let mut guard = write_half.lock().await;
                if let Err(e) = guard.write_all(&bytes).await {
                    tracing::warn!(target: "tst_rtp::server", peer = %peer, error = %e, "write failed");
                    return Ok(());
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
                // from an auth-gated method. OPTIONS and GET_PARAMETER are
                // never auth-gated (they always return 200), so resetting on
                // them would let an attacker alternate OPTIONS with bad-auth
                // requests to bypass the 3-strike limit indefinitely.
                // 3xx is excluded: no handler emits a redirect on a gated method,
                // and a redirect is not an auth success.
                match req.method {
                    RtspMethod::Describe
                    | RtspMethod::Setup
                    | RtspMethod::Play
                    | RtspMethod::Pause
                    | RtspMethod::Teardown => {
                        session.auth_failures = 0;
                    }
                    RtspMethod::Options | RtspMethod::GetParameter => {
                        // Non-auth-gated: do not touch the failure counter.
                    }
                }
            }
            if req.method == RtspMethod::Teardown && response.status == 200 {
                // Clean close after TEARDOWN. `shutdown` lives on the
                // write half — take the lock to serialize against any
                // in-flight fanout writes (fanout exits on session
                // cancel; the lock is the safety net).
                let mut guard = write_half.lock().await;
                let _ = guard.shutdown().await;
                return Ok(());
            }
        }

        // The drain loop above stops at the first non-`Complete` framing
        // outcome. If that's a cap violation on the new buffer head (a
        // pipelined request whose entirety already arrived in this read,
        // e.g. reqA + an oversized reqB in one packet), answer 413 now
        // instead of waiting for another socket read — which, for a
        // client that has moved on to awaiting reqA's response, may not
        // come until the idle timeout.
        if reject_if_over_cap(&buf, peer, &write_half).await {
            return Ok(());
        }
    }
    tracing::info!(target: "tst_rtp::server", peer = %peer, "session closed");
    Ok(())
}

/// Check the buffer head against the RTSP framing caps (64 KiB headers /
/// 1 MiB body — see [`crate::rtsp::message::rtsp_frame_decision`]) and, on
/// a violation, answer `413 Request Entity Too Large` and shut the write
/// half. Returns `true` when the caller must close the session (a
/// violation was answered), `false` when the buffer head is within caps
/// (`NeedMore` or `Complete` — nothing to do here either way).
async fn reject_if_over_cap<W: AsyncWrite + Unpin>(
    buf: &[u8],
    peer: SocketAddr,
    write_half: &AsyncMutex<W>,
) -> bool {
    let cap_rejection = match crate::rtsp::message::rtsp_frame_decision(buf) {
        RtspFraming::HeadersTooLong => Some("headers exceed maximum"),
        RtspFraming::BadContentLength(detail) => Some(detail),
        // NeedMore / Complete: not a cap rejection.
        _ => None,
    };
    let Some(detail) = cap_rejection else {
        return false;
    };
    tracing::warn!(
        target: "tst_rtp::server",
        peer = %peer,
        buf_len = buf.len(),
        detail,
        "request exceeded RTSP header/body caps; sending 413 and closing"
    );
    let response_413 = b"RTSP/1.0 413 Request Entity Too Large\r\nContent-Length: 0\r\n\r\n";
    let mut guard = write_half.lock().await;
    let _ = guard.write_all(response_413).await;
    let _ = guard.shutdown().await;
    true
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
        RtspMethod::Setup => handlers::handle_setup(req, state, session),
        RtspMethod::Play => handlers::handle_play(req, state, session),
        RtspMethod::Pause => handlers::handle_pause(req, state, session),
        RtspMethod::Teardown => handlers::handle_teardown(req, state, session),
        RtspMethod::GetParameter => handlers::handle_get_parameter(req, state, session),
    }
}

/// TLS-variant of [`handle_connection`]. Takes a tokio-rustls
/// `TlsStream<TcpStream>` instead of plain `TcpStream` and drives the
/// same [`serve_requests`] loop after splitting it via
/// [`tokio::io::split`].
///
/// `rtsps://` covers the full control plane (OPTIONS / DESCRIBE / SETUP /
/// PLAY / PAUSE / TEARDOWN / GET_PARAMETER) plus UDP-transport PLAY (the
/// RTP fanout goes over independent UDP sockets). It does NOT stash a
/// control-stream write half, so RFC 7826 §14 TCP-interleaved PLAY over a
/// TLS session is unsupported and returns 461 (see `handle_play`): the
/// interleaved fanout writer is typed to the plain TCP `OwnedWriteHalf`.
///
/// Listener (Task 8) calls this when the bind URL scheme is `rtsps://`.
///
/// The accept loop reserves the `active_sessions` slot *before* the TLS
/// handshake and moves the [`SessionSlot`] guard into the spawning task
/// (so the slot is released even when the handshake fails before this
/// function runs). By the time control reaches here the slot is already
/// accounted for; this function does not touch the counter.
#[cfg(feature = "rtsp-server-tls")]
pub(crate) async fn handle_connection_tls(
    state: Arc<ServerState>,
    tls: crate::rtsp::server::tls::TokioTlsServerStream,
    peer: SocketAddr,
) -> Result<(), RtspServerError> {
    let entry = crate::rtsp::server::register_session(&state, peer);
    let res = handle_connection_tls_inner(state.clone(), tls, peer, entry.clone()).await;
    crate::rtsp::server::unregister_session(&state, &entry);
    res
}

#[cfg(feature = "rtsp-server-tls")]
async fn handle_connection_tls_inner(
    state: Arc<ServerState>,
    tls: crate::rtsp::server::tls::TokioTlsServerStream,
    peer: SocketAddr,
    session_entry: Arc<crate::rtsp::server::ActiveSession>,
) -> Result<(), RtspServerError> {
    tracing::info!(target: "tst_rtp::server", peer = %peer, "TLS session opened");
    let (read_half, write_half) = tokio::io::split(tls);
    let write_half = Arc::new(AsyncMutex::new(write_half));
    // NOTE: `session.tcp_write` is intentionally left `None` for TLS — the
    // §14 interleaved fanout writer is typed to the plain TCP
    // `OwnedWriteHalf`. `RtspServer::stop`'s Notice 5402 ANNOUNCE path
    // skips sessions whose `tcp_write` is `None` (let-else in mod.rs).
    let mut session = ServerSessionState::new();
    // Record the peer IP so `handle_play`'s UDP branch directs RTP to the
    // client's real address. UDP-transport PLAY over a TLS control channel
    // is supported (only §14 TCP-interleaved over TLS is not), so the TLS
    // path must thread the peer the same way the plain-TCP path does.
    session.peer_addr = peer;
    serve_requests(state, peer, session_entry, session, read_half, write_half).await
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Spin up a local TCP listener, accept one connection, run the
    /// session loop. Client sends OPTIONS, we read the 200 OK response.
    #[tokio::test]
    async fn session_responds_to_options() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            // Build a minimal ServerState for the test.
            let builder = crate::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
            let state = std::sync::Arc::new(crate::rtsp::server::ServerState {
                builder,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                hard_cancel: crate::cancel::RtspServerCancelHandle::new(),
                mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
                active_sessions: std::sync::atomic::AtomicUsize::new(0),
                total_rtp_packets_sent: std::sync::atomic::AtomicU64::new(0),
                total_rtp_bytes_sent: std::sync::atomic::AtomicU64::new(0),
                started: std::sync::atomic::AtomicBool::new(true),
                shutdown: std::sync::atomic::AtomicBool::new(false),
                local_addr: std::sync::Mutex::new(None),
                sessions: std::sync::Mutex::new(Vec::new()),
                notice_cseq: std::sync::atomic::AtomicU64::new(1_000_000),
                #[cfg(feature = "rtsp-server-tls")]
                tls_config: std::sync::Mutex::new(None),
                startup_tx: std::sync::Mutex::new(None),
            });
            // Mimic the accept loop: reserve a slot, then move the guard
            // into the session task.
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_connection(state, tcp, peer, slot).await
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
            let builder = crate::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
            let state = std::sync::Arc::new(crate::rtsp::server::ServerState {
                builder,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                hard_cancel: crate::cancel::RtspServerCancelHandle::new(),
                mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
                active_sessions: std::sync::atomic::AtomicUsize::new(0),
                total_rtp_packets_sent: std::sync::atomic::AtomicU64::new(0),
                total_rtp_bytes_sent: std::sync::atomic::AtomicU64::new(0),
                started: std::sync::atomic::AtomicBool::new(true),
                shutdown: std::sync::atomic::AtomicBool::new(false),
                local_addr: std::sync::Mutex::new(None),
                sessions: std::sync::Mutex::new(Vec::new()),
                notice_cseq: std::sync::atomic::AtomicU64::new(1_000_000),
                #[cfg(feature = "rtsp-server-tls")]
                tls_config: std::sync::Mutex::new(None),
                startup_tx: std::sync::Mutex::new(None),
            });
            // Mimic the accept loop: reserve a slot, then move the guard
            // into the session task.
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_connection(state, tcp, peer, slot).await
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        // T17 implemented the real TEARDOWN handler: 200 OK after auth
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
        let mut builder = crate::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.auth_basic(
            "test",
            "user",
            secrecy::SecretString::from("secret".to_owned()),
        );
        Arc::new(ServerState {
            builder,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            hard_cancel: crate::cancel::RtspServerCancelHandle::new(),
            mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_sessions: std::sync::atomic::AtomicUsize::new(0),
            total_rtp_packets_sent: std::sync::atomic::AtomicU64::new(0),
            total_rtp_bytes_sent: std::sync::atomic::AtomicU64::new(0),
            started: std::sync::atomic::AtomicBool::new(true),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            local_addr: std::sync::Mutex::new(None),
            sessions: std::sync::Mutex::new(Vec::new()),
            notice_cseq: std::sync::atomic::AtomicU64::new(1_000_000),
            #[cfg(feature = "rtsp-server-tls")]
            tls_config: std::sync::Mutex::new(None),
            startup_tx: std::sync::Mutex::new(None),
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
            handle_connection(state, tcp, peer, slot).await
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
        use crate::rtsp::server::mount::{MountKind, MountState};
        use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_handle = tokio::spawn(async move {
            let (tcp, peer) = listener.accept().await.unwrap();
            let state = make_state_with_basic_auth();
            // DESCRIBE reads local_addr to build the SDP; set it before
            // handle_connection runs.
            *state.local_addr.lock().unwrap() = Some("127.0.0.1:8554".parse().unwrap());
            // Register a real mount at /test so an auth-passing DESCRIBE
            // returns 200 (2xx), triggering the counter reset.
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            let mux_cfg = b.build().unwrap();
            let mount = MountState::new("/test", MountKind::Unicast, mux_cfg, 16).unwrap();
            state.mounts.lock().unwrap().insert("/test".into(), mount);
            state
                .active_sessions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let slot = SessionSlot::new(state.clone());
            handle_connection(state, tcp, peer, slot).await
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
}
