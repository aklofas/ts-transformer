//! RTSP-control-plane errors.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! RTSP failures do not fit the [`tst_core::transport::TransportError`]
//! semantics — RTSP is a separate state machine before the
//! [`crate::RtpRecvTransport`] is constructed. This type carries all
//! RTSP-side failures up to the caller; post-SETUP failures (TCP RST
//! mid-PLAY, UDP recv errors) bubble through the transport's normal
//! `TransportError::Broken` path.
//!
//! Total variants: 18 (Phase 2 master-spec 12 + Url + NoMp2tMedia +
//! MultipleMp2tMedia + NoH264Media + MultipleH264Media +
//! UnsupportedPacketizationMode).

use std::io;

use crate::url::UrlError;

/// Failure shape for the RTSP client state machine.
///
/// Constructed at every point where the client may fail before the
/// pipeline is wired up. Once [`crate::RtpRecvTransport`] is in hand,
/// subsequent failures bubble through `TransportError` instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RtspError {
    /// Socket-level I/O on the control channel (connect refused,
    /// connection reset, etc.). Mirrors Phase 1's
    /// [`crate::ConnectError::Io`] but scoped to the RTSP TCP connection.
    #[error("RTSP I/O error: {0:?}")]
    Io(io::ErrorKind),

    /// rustls handshake / certificate-verification failure (only emitted
    /// when the `tls` feature is enabled).
    #[error("RTSP TLS error: {0}")]
    Tls(String),

    /// 4xx or 5xx server response that doesn't match a more specific
    /// variant. `code` is the numeric status (e.g., 404 Not Found),
    /// `reason` is the human-readable reason phrase ("Stream Not
    /// Found").
    #[error("RTSP server returned {code} {reason}")]
    Protocol { code: u16, reason: String },

    /// 401 Unauthorized after credential retry. Either credentials were
    /// missing/wrong, or the server returned a fresh nonce that didn't
    /// help on the second attempt.
    #[error("RTSP authentication failed")]
    AuthFailed,

    /// Server demanded an auth scheme we don't implement (e.g., NTLM,
    /// Bearer). `scheme` is the lowercase scheme name from the
    /// `WWW-Authenticate` header.
    #[error("RTSP auth scheme not supported: {scheme}")]
    AuthUnsupported { scheme: String },

    /// Server response couldn't be parsed. `detail` is a short reason
    /// ("missing CSeq header", "truncated body").
    #[error("malformed RTSP response: {detail}")]
    BadResponse { detail: &'static str },

    /// SDP from `DESCRIBE` couldn't be parsed by `sdp-types`. `detail`
    /// is the parse-error rendering.
    #[error("malformed SDP from server: {detail}")]
    BadSdp { detail: String },

    /// Server returned 461 Unsupported Transport on both UDP and TCP
    /// attempts, or `?transport=` URL query forced a transport the
    /// server refused.
    #[error("RTSP server does not support a transport we accept")]
    UnsupportedTransport,

    /// Server closed our session (sent RTSP/1.0 454 Session Not Found
    /// on a keepalive ping, or the underlying TCP went RST). After
    /// this, no further requests succeed; caller must construct a fresh
    /// `RtspClient`.
    #[error("RTSP session expired or was closed by server")]
    SessionExpired,

    /// A request's response deadline elapsed
    /// (`RtspClientBuilder::request_timeout`, default 10 s; also the 500 ms
    /// bound `Drop` puts on its best-effort TEARDOWN). The request was
    /// written; whether the server acted on it is unknown, and its late
    /// response may still be in the socket — treat the control connection
    /// as indeterminate and build a fresh client. Distinct from
    /// [`RtspError::SessionExpired`], which is the server's explicit 454.
    #[error("RTSP request timed out waiting for the server's response")]
    Timeout,

    /// Caller invoked `RtspCancelHandle::cancel` (lands Wave B) mid-request.
    /// The TCP write/read returned early; no server state was
    /// necessarily mutated, so caller should treat the session as
    /// indeterminate.
    #[error("RTSP request canceled by caller")]
    LocalCancel,

    /// `DESCRIBE` returned an SDP that contains no `m=` line with PT=33
    /// (MP2T, RFC 3551 §6). Only emitted by
    /// `RtspClient::setup_mp2t_auto` (lands Wave B); explicit
    /// `setup(&media)` does not consult MP2T-ness.
    #[error("no MPEG-TS m-line in SDP (no payload type 33)")]
    NoMp2tMedia,

    /// `DESCRIBE` returned an SDP with multiple `m=` lines containing
    /// PT=33. Caller should fall back to explicit
    /// `RtspClient::setup` (lands Wave B) with a chosen media line.
    #[error("multiple MPEG-TS m-lines in SDP ({count} found)")]
    MultipleMp2tMedia { count: usize },

    /// `DESCRIBE` returned an SDP with no H.264 (RFC 6184) media line —
    /// no `m=` section contains an `a=rtpmap` naming `H264/90000`. Only
    /// emitted by [`crate::rtsp::client::RtspClient::setup_h264_auto`];
    /// explicit `setup(&media)` does not consult H.264-ness.
    #[error("SDP has no H.264 (rtpmap H264/90000) media")]
    NoH264Media,

    /// `DESCRIBE` returned an SDP with multiple H.264 media lines. Caller
    /// should fall back to explicit `RtspClient::setup` with a chosen
    /// media line.
    #[error("SDP has {count} H.264 media lines; explicit selection required")]
    MultipleH264Media { count: usize },

    /// The H.264 media line advertises packetization-mode 2 (interleaved
    /// mode). Only modes 0 and 1 are implemented. Emitted by
    /// [`crate::rtsp::client::RtspClient::setup_h264_auto`].
    #[error(
        "H.264 media requires packetization-mode {0}; only modes 0/1 are supported (interleaved mode is not implemented)"
    )]
    UnsupportedPacketizationMode(u8),

    /// A header name or value destined for an outgoing RTSP request
    /// contained a forbidden byte — CR, LF, NUL, or another ASCII control
    /// character. Per RFC 7826 a header field-value is a single line of
    /// visible ASCII (plus SP/HT); a CR/LF would let a caller-supplied
    /// value (User-Agent, Authorization built from credentials, a custom
    /// header) inject a second header or smuggle a whole request. Rejected
    /// at encode time so no injected bytes ever reach the wire. `detail`
    /// names the offending field.
    #[error("invalid RTSP header: {detail}")]
    InvalidHeader { detail: &'static str },

    /// URL parsing failed before any RTSP exchange. Wraps the
    /// underlying [`crate::RtpUrlError`] from Phase 1.
    #[error("RTSP URL parse error: {0}")]
    Url(#[from] UrlError),
}

impl From<io::Error> for RtspError {
    fn from(e: io::Error) -> Self {
        RtspError::Io(e.kind())
    }
}

/// Failure shape for `RtspServer` (introduced in Phase 3 Task 7)
/// lifecycle and configuration. Per-session errors (one client
/// misbehaving) do NOT surface here — they are logged via
/// `tracing::warn!` and the session closes; the server keeps running.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RtspServerError {
    /// Socket-level I/O on the listener.
    #[error("RTSP server I/O error: {0:?}")]
    Io(io::ErrorKind),

    /// rustls server-side handshake / cert-loading failure (the
    /// acceptor lives behind the `rtsp-server-tls` feature; an
    /// `rtsps://` bind without it also fails with this variant).
    #[error("RTSP server TLS error: {0}")]
    Tls(String),

    /// Bind URL parsing failed before any server lifecycle.
    #[error("RTSP server bind URL parse error: {0}")]
    UrlParse(#[from] UrlError),

    /// The bind URL's host:port pair could not be claimed (another process
    /// holds it, or insufficient privileges for the port).
    #[error("bind address in use")]
    BindAddrInUse,

    /// `add_mount("/path", ...)` rejected the path — empty, doesn't start
    /// with `/`, contains URL-reserved characters, etc.
    #[error("invalid mount path: {detail}")]
    InvalidMountPath { detail: String },

    /// `add_multicast_mount(...)` rejected the group address — not in the
    /// 224.0.0.0/4 or ff00::/8 ranges, or the URL is malformed.
    #[error("invalid multicast group '{addr}': {detail}")]
    InvalidMulticastGroup { addr: String, detail: String },

    /// `add_mount("/path", ...)` called twice with the same path.
    #[error("duplicate mount path '{path}'")]
    DuplicateMount { path: String },

    /// `MuxerConfig` failed validation (no programs declared, etc.) or
    /// some other configuration-time invariant was violated.
    #[error("invalid mount config: {detail}")]
    InvalidConfig { detail: String },

    /// `start()` called twice without an intervening `stop()`.
    #[error("RTSP server already started")]
    AlreadyStarted,

    /// `stop()`, `add_mount()`, or similar called before `start()`.
    #[error("RTSP server not started")]
    NotStarted,

    /// Public method invoked after `stop()` completed (or after `cancel()`).
    #[error("RTSP server has been shut down")]
    Shutdown,
}

/// Failure shape for `MountHandle` (introduced in Phase 3 Wave C) push methods.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MountError {
    /// The inner muxer surfaced an error during push or drain.
    #[error("muxer error: {0}")]
    Mux(#[from] tst_core::error::MuxError),

    /// The mount's parent server has been shut down (or the mount was
    /// explicitly removed in a future API).
    #[error("mount closed")]
    Closed,
}
