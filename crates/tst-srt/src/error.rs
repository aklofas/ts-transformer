//! Error types for tst-srt.
//!
//! Per-call-category enums (`ConnectError`, `BindError`, etc.) are the
//! Rust-idiomatic surface — exhaustive `match` is meaningful at every call
//! site. The umbrella `SrtError` exists for callers who want one type to
//! propagate across categories.

use std::io;
use thiserror::Error;

use tst_core::error::{DemuxError, KlvDecodeError, KlvEncodeError, KlvFieldError, MuxError};
#[cfg(test)]
use tst_core::mpegts::mux::StreamKind;

// ============================================================================
// Validation errors (newtype constructors)
// ============================================================================

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PassphraseError {
    #[error("passphrase length must be 10-79 chars (got {0})")]
    InvalidLength(usize),
    #[error("passphrase contains non-printable ASCII")]
    InvalidCharset,
    #[error("environment variable '{0}' is unset or empty")]
    EnvUnset(String),
    #[error("error reading passphrase file: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StreamIdError {
    #[error("stream ID exceeds 512 chars (got {0})")]
    TooLong(usize),
    #[error("stream ID contains non-ASCII characters")]
    NonAscii,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PacketFilterError {
    #[error("filter spec exceeds 512 chars")]
    TooLong,
    #[error("filter spec contains invalid characters")]
    InvalidCharset,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AddrError {
    #[error("could not resolve address: {0}")]
    Resolve(String),
    #[error("IO error reading address: {0}")]
    Io(#[from] io::Error),
}

// ============================================================================
// Supporting public types
// ============================================================================

/// libsrt's MJ_* major error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SrtErrno {
    Setup,
    Connection,
    SystemRes,
    FileSystem,
    Notsup,
    Async,
    PeerError,
    Bad,
    Unknown(i32),
}

/// Reject codes carried in SRT's handshake or set by a remote service.
///
/// libsrt partitions the code space into three categories
/// (`srt.h:565-571`):
///
/// | Range | Category | Source |
/// |---|---|---|
/// | `0..=999` | `SRT_REJC_INTERNAL` | libsrt-internal enum `SRT_REJECT_REASON` (`srt.h:535-558`); emitted by the SRT library itself during handshake. |
/// | `1000..=1999` | `SRT_REJC_PREDEFINED` | Application-layer codes from `access_control.h`; set by a remote service via `srt_setrejectreason()`. Mostly HTTP-derived. |
/// | `2000..` | `SRT_REJC_USERDEFINED` | Free-form; modeled as `Other(raw)`. |
///
/// Per-variant rustdoc records the category and ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectReason {
    // -- Internal (SRT_REJC_INTERNAL, 0..=999) --
    /// `SRT_REJ_UNKNOWN` (0) — initial state, set while a handshake is in progress.
    Unknown,
    /// `SRT_REJ_SYSTEM` (1) — broken due to system function error.
    System,
    /// `SRT_REJ_PEER` (2) — connection was rejected by peer.
    Peer,
    /// `SRT_REJ_RESOURCE` (3) — internal problem with resource allocation.
    Resource,
    /// `SRT_REJ_ROGUE` (4) — incorrect data in handshake messages.
    Rogue,
    /// `SRT_REJ_BACKLOG` (5) — listener's backlog exceeded.
    Backlog,
    /// `SRT_REJ_IPE` (6) — internal program error.
    Ipe,
    /// `SRT_REJ_CLOSE` (7) — socket is closing.
    Close,
    /// `SRT_REJ_VERSION` (8) — peer is older version than agent's minimum set.
    Version,
    /// `SRT_REJ_RDVCOOKIE` (9) — rendezvous cookie collision.
    RdvCookie,
    /// `SRT_REJ_BADSECRET` (10) — wrong password.
    BadSecret,
    /// `SRT_REJ_UNSECURE` (11) — password required or unexpected.
    Unsecure,
    /// `SRT_REJ_MESSAGEAPI` (12) — streamapi/messageapi collision.
    MessageApi,
    /// `SRT_REJ_CONGESTION` (13) — incompatible congestion-controller type.
    Congestion,
    /// `SRT_REJ_FILTER` (14) — incompatible packet filter.
    Filter,
    /// `SRT_REJ_GROUP` (15) — incompatible group.
    Group,
    /// `SRT_REJ_TIMEOUT` (16) — connection timeout.
    Timeout,
    /// `SRT_REJ_CRYPTO` (17) — conflicting cryptographic configurations
    /// (behind `ENABLE_AEAD_API_PREVIEW` upstream; included unconditionally
    /// so downstream consumers can match it without feature-gating).
    Crypto,

    // -- Extension (SRT_REJC_PREDEFINED, 1000..=1999) --
    /// `SRT_REJX_FALLBACK` (1000) — generic predefined fallback.
    Fallback,
    /// `SRT_REJX_KEY_NOTSUP` (1001) — StreamID key not supported by service.
    KeyNotSupported,
    /// `SRT_REJX_FILEPATH` (1002) — bad file path syntax / not found.
    Filepath,
    /// `SRT_REJX_HOSTNOTFOUND` (1003) — host specification not recognized.
    HostNotFound,
    /// `SRT_REJX_BAD_REQUEST` (1400) — general SocketID syntax error.
    BadRequest,
    /// `SRT_REJX_UNAUTHORIZED` (1401) — authentication failed.
    Unauthorized,
    /// `SRT_REJX_OVERLOAD` (1402) — server overloaded or credits exceeded.
    Overload,
    /// `SRT_REJX_FORBIDDEN` (1403) — access denied.
    Forbidden,
    /// `SRT_REJX_NOTFOUND` (1404) — resource not found.
    NotFound,
    /// `SRT_REJX_BAD_MODE` (1405) — mode in `m` key not supported.
    BadMode,
    /// `SRT_REJX_UNACCEPTABLE` (1406) — requested parameters cannot be satisfied.
    Unacceptable,
    /// `SRT_REJX_CONFLICT` (1409) — resource locked for modification.
    Conflict,
    /// `SRT_REJX_NOTSUP_MEDIA` (1415) — media type not supported.
    NotSupportedMedia,
    /// `SRT_REJX_LOCKED` (1423) — resource locked for any access.
    Locked,
    /// `SRT_REJX_FAILED_DEPEND` (1424) — dependent session ID disconnected.
    FailedDependency,
    /// `SRT_REJX_ISE` (1500) — unexpected internal server error.
    InternalServerError,
    /// `SRT_REJX_UNIMPLEMENTED` (1501) — current service version doesn't support request.
    Unimplemented,
    /// `SRT_REJX_GW` (1502) — gateway target rejected the connection.
    Gateway,
    /// `SRT_REJX_DOWN` (1503) — service temporarily down.
    Down,
    /// `SRT_REJX_VERSION` (1505) — SRT version not supported by service.
    VersionUnsupported,
    /// `SRT_REJX_NOROOM` (1507) — out of storage to archive stream.
    NoRoom,

    // -- Catch-all (SRT_REJC_USERDEFINED, 2000+, plus unknown codes in any range) --
    /// Raw code outside the typed set. Includes:
    /// - `SRT_REJC_USERDEFINED` (2000+) — application free-form codes.
    /// - Unknown codes within the `0..=999` or `1000..=1999` ranges
    ///   (drift from a newer libsrt header).
    /// - Negative values (defensive — libsrt should not emit these).
    Other(i32),
}

// ============================================================================
// Cross-cutting error enums
// ============================================================================

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OptionError {
    #[error("option not settable in current socket state")]
    InvalidState,
    #[error("option value out of range: {0}")]
    OutOfRange(String),
    #[error("invalid passphrase: {0}")]
    InvalidPassphrase(#[from] PassphraseError),
    #[error("invalid stream id: {0}")]
    InvalidStreamId(#[from] StreamIdError),
    #[error("invalid packet filter: {0}")]
    InvalidPacketFilter(#[from] PacketFilterError),
    #[error("libsrt error: {kind:?} - {message}")]
    Other { kind: SrtErrno, message: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IoError {
    #[error("socket has been closed")]
    SocketClosed,
    #[error("system error: {0}")]
    System(#[from] io::Error),
    #[error("libsrt error: {kind:?} - {message}")]
    Other { kind: SrtErrno, message: String },
}

// ============================================================================
// Per-call-site error enums
// ============================================================================

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConnectError {
    #[error("invalid address: {0}")]
    InvalidAddress(#[from] AddrError),
    #[error("encryption configuration rejected: {detail}")]
    BadEncryption { detail: String },
    #[error("peer rejected connection: {reason:?} - {detail}")]
    Rejected {
        reason: RejectReason,
        detail: String,
    },
    #[error("connection timed out")]
    TimedOut,
    #[error("connection refused")]
    Refused,
    #[error("config validation failed: {0}")]
    InvalidOption(#[from] OptionError),
    #[error("system error: {0}")]
    System(#[from] io::Error),
    #[error("libsrt error: {kind:?} - {message}")]
    Other { kind: SrtErrno, message: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BindError {
    #[error("invalid address: {0}")]
    InvalidAddress(#[from] AddrError),
    #[error("address already in use")]
    AddressInUse,
    #[error("permission denied")]
    PermissionDenied,
    #[error("config validation failed: {0}")]
    InvalidOption(#[from] OptionError),
    #[error("system error: {0}")]
    System(#[from] io::Error),
    #[error("libsrt error: {kind:?} - {message}")]
    Other { kind: SrtErrno, message: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AcceptError {
    #[error("accept timed out")]
    TimedOut,
    #[error("listener was closed")]
    ListenerClosed,
    #[error("peer rejected during handshake: {reason:?} - {detail}")]
    PeerRejected {
        reason: RejectReason,
        detail: String,
    },
    #[error("system error: {0}")]
    System(#[from] io::Error),
    #[error("libsrt error: {kind:?} - {message}")]
    Other { kind: SrtErrno, message: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SendError {
    #[error("send timed out")]
    TimedOut,
    #[error("connection broken")]
    ConnectionBroken,
    #[error("payload exceeds payload_size limit ({actual} > {limit})")]
    PayloadTooLarge { actual: usize, limit: usize },
    #[error("send queue full")]
    QueueFull,
    #[error("system error: {0}")]
    System(#[from] io::Error),
    #[error("libsrt error: {kind:?} - {message}")]
    Other { kind: SrtErrno, message: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RecvError {
    #[error("recv timed out")]
    TimedOut,
    #[error("connection broken")]
    ConnectionBroken,
    #[error("buffer too small for incoming message ({buf_len} < {message_len})")]
    BufferTooSmall { buf_len: usize, message_len: usize },
    #[error("system error: {0}")]
    System(#[from] io::Error),
    #[error("libsrt error: {kind:?} - {message}")]
    Other { kind: SrtErrno, message: String },
}

// ============================================================================
// Umbrella error type
//
// SrtError exists for callers who want one type to propagate across
// categories (per-call-category enums above are the Rust-idiomatic surface).
// ============================================================================

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SrtError {
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Bind(#[from] BindError),
    #[error(transparent)]
    Accept(#[from] AcceptError),
    #[error(transparent)]
    Send(#[from] SendError),
    #[error(transparent)]
    Recv(#[from] RecvError),
    #[error(transparent)]
    Option(#[from] OptionError),
    #[error(transparent)]
    Io(#[from] IoError),
    #[error(transparent)]
    KlvDecode(#[from] KlvDecodeError),
    #[error(transparent)]
    KlvEncode(#[from] KlvEncodeError),
    #[error(transparent)]
    KlvField(#[from] KlvFieldError),
    #[error(transparent)]
    Mux(#[from] MuxError),
    #[error(transparent)]
    Demux(#[from] DemuxError),
    #[error(transparent)]
    Transport(#[from] tst_core::transport::TransportError),
}

// ============================================================================
// libsrt-error → typed-enum mapping
// ============================================================================

#[derive(Debug, Clone)]
pub(crate) struct RawError {
    pub kind: SrtErrno,
    /// Full libsrt errno exactly as `srt_getlasterror` returned it
    /// (`major * 1000 + minor`, e.g. `SRT_ETIMEOUT` = 6003). `kind`
    /// keeps only the major category; classifiers that need to
    /// distinguish siblings within a category (timeout vs would-block,
    /// both major 6) match on this instead of the error message text,
    /// which libsrt does not treat as a stable API.
    pub code: i32,
    pub message: String,
}

/// Read libsrt's last-error state. Call immediately after a libsrt FFI call
/// returned an error indicator.
pub(crate) fn last_error() -> RawError {
    let code = unsafe { srt_sys::srt_getlasterror(std::ptr::null_mut()) };
    let msg_ptr = unsafe { srt_sys::srt_getlasterror_str() };
    let message = if msg_ptr.is_null() {
        "<no error string>".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(msg_ptr) }
            .to_string_lossy()
            .into_owned()
    };
    RawError {
        kind: SrtErrno::from_raw(code),
        code,
        message,
    }
}

/// Read the typed reject code from a live socket handle (only meaningful after
/// a connection-rejected error, and only BEFORE the socket is closed).
///
/// Returns `RejectReason::Unknown` (raw 0 / `SRT_REJ_UNKNOWN`) when libsrt
/// has not set a reject code (e.g. a connection error that wasn't a
/// handshake reject). Callers that want to suppress this case should check
/// `reason != RejectReason::Unknown`.
///
/// # Correctness
/// `srt_getrejectreason` must be called on the **live** (not yet closed)
/// socket. Once `srt_close` is called, `locateSocket` inside libsrt returns
/// null and the reason is lost. Callers in the connect path must therefore
/// call this before `srt_close`.
pub(crate) fn last_reject(sock: srt_sys::SRTSOCKET) -> RejectReason {
    let raw = unsafe { srt_sys::srt_getrejectreason(sock) };
    RejectReason::from_raw(raw)
}

/// Classify a failed connect attempt using a pre-captured reject reason.
///
/// The `reason` must be obtained via [`last_reject`] from the live socket
/// **before** `srt_close` is called. This function is the single point where
/// `RawError` is translated into `ConnectError`; the `From<RawError>` impl
/// delegates here (for call sites that have already read the reject reason).
///
/// # Category note
/// SRT handshake rejects produce `SRT_ECONNREJ = 1002` → major 1 →
/// `SrtErrno::Setup`. Plain connection refusals produce major-2 codes. Both
/// categories must be checked for a typed reject reason.
pub(crate) fn classify_connect_error(raw: RawError, reason: RejectReason) -> ConnectError {
    if matches!(raw.kind, SrtErrno::Connection | SrtErrno::Setup) {
        // SRT_REJ_TIMEOUT (16): the peer set RejectReason::Timeout on a
        // connect that exhausted SRTO_CONNTIMEO (raises SRT_ENOSERVER /
        // SrtErrno::Setup). Must map to TimedOut BEFORE the generic
        // `reason != Unknown` branch, which would otherwise produce a
        // misleading Rejected{Timeout} ("peer rejected" — no peer answered).
        if reason == RejectReason::Timeout {
            return ConnectError::TimedOut;
        }
        // `reason != Unknown` means libsrt recorded a typed handshake reject.
        if reason != RejectReason::Unknown {
            return ConnectError::Rejected {
                reason,
                detail: raw.message,
            };
        }
    }
    // The "refused" string heuristic is scoped to the Connection category only.
    // A plain refusal (peer not listening) surfaces as major-2 (Connection); the
    // Setup category (e.g. SRT_ECONNREJ handshake rejects) must NOT be reclassified
    // as Refused on a message-substring match — it falls through to Other instead.
    if matches!(raw.kind, SrtErrno::Connection)
        && (raw.message.contains("refused") || raw.message.contains("Refused"))
    {
        return ConnectError::Refused;
    }
    if is_timeout(&raw) {
        return ConnectError::TimedOut;
    }
    ConnectError::Other {
        kind: raw.kind,
        message: raw.message,
    }
}

impl SrtErrno {
    /// Map the raw `srt_getlasterror` int to a typed variant.
    /// libsrt encodes `MJ_*` in upper bits; we drop sub-codes here.
    pub(crate) fn from_raw(raw: i32) -> Self {
        // libsrt's MJ_* values (see vendor/srt/srtcore/srt.h SRT_ERRNO enum).
        // Major category is encoded as (major * 1000 + minor) in the SRT_ERRNO enum.
        let major = raw / 1000;
        match major {
            1 => SrtErrno::Setup,
            2 => SrtErrno::Connection,
            3 => SrtErrno::SystemRes,
            4 => SrtErrno::FileSystem,
            5 => SrtErrno::Notsup,
            6 => SrtErrno::Async,
            7 => SrtErrno::PeerError,
            _ => SrtErrno::Unknown(raw),
        }
    }

    /// Map back to an integer code for transport-layer error propagation.
    ///
    /// Always returns the libsrt MJ_* major category (`1..=7`) for known
    /// variants — `Setup`→1, `Connection`→2, `SystemRes`→3, `FileSystem`→4,
    /// `Notsup`→5, `Async`→6, `PeerError`→7, `Bad`→0. For
    /// [`Self::Unknown`] the preserved raw libsrt errno is folded back to
    /// its major category via integer division (libsrt encodes errnos as
    /// `major * 1000 + minor`, so `raw / 1000` recovers the major).
    /// Callers that need the full raw errno can pattern-match on
    /// `SrtErrno::Unknown(raw)` directly.
    ///
    /// Used by [`crate::transport::SrtTransport`] to populate the
    /// `errno_code` field on
    /// [`tst_core::transport::TransportError::Backpressure`] /
    /// [`tst_core::transport::TransportError::Broken`], giving JNI /
    /// UniFFI bindings a typed-source signal in a small, stable range.
    /// Bindings can match on `code <= 7` without worrying about an
    /// `Unknown(6002)` slipping through as the full encoded errno.
    ///
    /// Note: `Bad` isn't produced by the internal `from_raw` constructor
    /// today (no major-category mapping); it's listed for future-proofing
    /// if the typed enum is ever populated through another path.
    #[must_use]
    pub fn raw_code(&self) -> i32 {
        match self {
            SrtErrno::Setup => 1,
            SrtErrno::Connection => 2,
            SrtErrno::SystemRes => 3,
            SrtErrno::FileSystem => 4,
            SrtErrno::Notsup => 5,
            SrtErrno::Async => 6,
            SrtErrno::PeerError => 7,
            SrtErrno::Bad => 0,
            // libsrt errnos are `major * 1000 + minor`; fold back to the
            // major category so the return value range matches the typed
            // variants. Bindings that want the full raw errno can match
            // `SrtErrno::Unknown(raw)` directly.
            SrtErrno::Unknown(raw) => raw / 1000,
        }
    }
}

impl RejectReason {
    /// Map a raw reject code (from `srt_getrejectreason`) to a typed variant.
    ///
    /// See [`RejectReason`] for the category split. Codes outside the typed
    /// set land in `Other(raw)`.
    pub(crate) fn from_raw(raw: i32) -> Self {
        match raw {
            // SRT_REJC_INTERNAL — SRT_REJECT_REASON enum (srt.h:535-558).
            0 => RejectReason::Unknown,
            1 => RejectReason::System,
            2 => RejectReason::Peer,
            3 => RejectReason::Resource,
            4 => RejectReason::Rogue,
            5 => RejectReason::Backlog,
            6 => RejectReason::Ipe,
            7 => RejectReason::Close,
            8 => RejectReason::Version,
            9 => RejectReason::RdvCookie,
            10 => RejectReason::BadSecret,
            11 => RejectReason::Unsecure,
            12 => RejectReason::MessageApi,
            13 => RejectReason::Congestion,
            14 => RejectReason::Filter,
            15 => RejectReason::Group,
            16 => RejectReason::Timeout,
            17 => RejectReason::Crypto,

            // SRT_REJC_PREDEFINED — access_control.h SRT_REJX_* macros.
            1000 => RejectReason::Fallback,
            1001 => RejectReason::KeyNotSupported,
            1002 => RejectReason::Filepath,
            1003 => RejectReason::HostNotFound,
            1400 => RejectReason::BadRequest,
            1401 => RejectReason::Unauthorized,
            1402 => RejectReason::Overload,
            1403 => RejectReason::Forbidden,
            1404 => RejectReason::NotFound,
            1405 => RejectReason::BadMode,
            1406 => RejectReason::Unacceptable,
            1409 => RejectReason::Conflict,
            1415 => RejectReason::NotSupportedMedia,
            1423 => RejectReason::Locked,
            1424 => RejectReason::FailedDependency,
            1500 => RejectReason::InternalServerError,
            1501 => RejectReason::Unimplemented,
            1502 => RejectReason::Gateway,
            1503 => RejectReason::Down,
            1505 => RejectReason::VersionUnsupported,
            1507 => RejectReason::NoRoom,

            // SRT_REJC_USERDEFINED (2000+), and unknown codes inside any range.
            other => RejectReason::Other(other),
        }
    }
}

/// Decide whether a `RawError` indicates a timeout.
///
/// Matches the exact libsrt errno (`SRT_ETIMEOUT`, 6003 — what a
/// blocking call raises when `SRTO_RCVTIMEO`/`SRTO_SNDTIMEO`/
/// `SRTO_CONNTIMEO` expires) rather than the error message text.
/// Substring-matching "timeout"/"timed out" is not an option: libsrt does
/// not treat its message text as a stable API, so any upstream rewording
/// would silently turn timeouts into generic errors.
pub(crate) fn is_timeout(raw: &RawError) -> bool {
    raw.code == srt_sys::SRT_ERRNO_SRT_ETIMEOUT
}

/// Decide whether a `RawError` indicates the connection has been broken.
pub(crate) fn is_broken(raw: &RawError) -> bool {
    matches!(raw.kind, SrtErrno::Connection)
        && (raw.message.contains("broken") || raw.message.contains("Broken"))
}

// ============================================================================
// From<RawError> impls — translate raw libsrt errors into typed variants.
// Each per-call enum has an `Other { kind, message }` catch-all populated here.
// Specific cases (timeout, broken, refused, rejected, address-in-use,
// permission-denied) are detected by classifier helpers — errno-matched
// where libsrt gives a dedicated code (timeout = SRT_ETIMEOUT), string-
// matched where it doesn't (broken/refused/in-use ride generic majors).
// ============================================================================

impl From<RawError> for ConnectError {
    /// Convert a raw libsrt error to `ConnectError` without a pre-captured
    /// reject reason. The connect path in `socket.rs` uses
    /// [`classify_connect_error`] directly (which accepts a reject reason
    /// captured from the live socket before `srt_close`); this `From` impl
    /// is the fallback for callers that don't have a live socket.
    fn from(raw: RawError) -> Self {
        classify_connect_error(raw, RejectReason::Unknown)
    }
}

impl From<RawError> for BindError {
    fn from(raw: RawError) -> Self {
        if raw.message.contains("in use") || raw.message.contains("already") {
            return BindError::AddressInUse;
        }
        if raw.message.contains("permission") || raw.message.contains("Permission") {
            return BindError::PermissionDenied;
        }
        BindError::Other {
            kind: raw.kind,
            message: raw.message,
        }
    }
}

impl From<RawError> for AcceptError {
    fn from(raw: RawError) -> Self {
        if is_timeout(&raw) {
            return AcceptError::TimedOut;
        }
        if matches!(raw.kind, SrtErrno::Setup) || raw.message.contains("closed") {
            return AcceptError::ListenerClosed;
        }
        AcceptError::Other {
            kind: raw.kind,
            message: raw.message,
        }
    }
}

impl From<RawError> for SendError {
    fn from(raw: RawError) -> Self {
        if is_timeout(&raw) {
            return SendError::TimedOut;
        }
        if is_broken(&raw) {
            return SendError::ConnectionBroken;
        }
        SendError::Other {
            kind: raw.kind,
            message: raw.message,
        }
    }
}

impl From<RawError> for RecvError {
    fn from(raw: RawError) -> Self {
        if is_timeout(&raw) {
            return RecvError::TimedOut;
        }
        if is_broken(&raw) {
            return RecvError::ConnectionBroken;
        }
        RecvError::Other {
            kind: raw.kind,
            message: raw.message,
        }
    }
}

impl From<RawError> for OptionError {
    fn from(raw: RawError) -> Self {
        OptionError::Other {
            kind: raw.kind,
            message: raw.message,
        }
    }
}

impl From<RawError> for IoError {
    fn from(raw: RawError) -> Self {
        IoError::Other {
            kind: raw.kind,
            message: raw.message,
        }
    }
}

#[cfg(test)]
// bindgen emits `SRT_REJECT_REASON_*` and `SRT_REJX_*` constants as
// `u32` on most targets (since they're #defined as positive ordinals
// in srt.h / access_control.h) but as `i32` on `*-pc-windows-msvc`
// where the platform's int width-rule lands them differently. The
// `as i32` casts in the reject-reason tests are necessary on Linux
// (where they convert u32→i32) but redundant on Windows (where the
// source is already i32). Allow `unnecessary_cast` here rather than
// add per-platform cfg gates around 25+ test assertions.
#[allow(clippy::unnecessary_cast)]
mod tests {
    use super::*;

    #[test]
    fn validation_error_displays() {
        let e = PassphraseError::InvalidLength(5);
        assert_eq!(
            e.to_string(),
            "passphrase length must be 10-79 chars (got 5)"
        );
    }

    #[test]
    fn umbrella_from_per_call() {
        let e: SrtError = ConnectError::TimedOut.into();
        match e {
            SrtError::Connect(ConnectError::TimedOut) => {}
            _ => panic!("expected Connect(TimedOut)"),
        }
    }

    #[test]
    fn option_error_from_validation() {
        let e: OptionError = StreamIdError::TooLong(600).into();
        match e {
            OptionError::InvalidStreamId(StreamIdError::TooLong(600)) => {}
            _ => panic!("expected InvalidStreamId(TooLong(600))"),
        }
    }

    #[test]
    fn srt_errno_mapping() {
        assert_eq!(SrtErrno::from_raw(1000), SrtErrno::Setup);
        assert_eq!(SrtErrno::from_raw(1003), SrtErrno::Setup); // sub-codes folded into major
        assert_eq!(SrtErrno::from_raw(2000), SrtErrno::Connection);
        assert_eq!(SrtErrno::from_raw(3001), SrtErrno::SystemRes);
        assert_eq!(SrtErrno::from_raw(4000), SrtErrno::FileSystem);
        assert_eq!(SrtErrno::from_raw(5000), SrtErrno::Notsup);
        assert_eq!(SrtErrno::from_raw(6000), SrtErrno::Async);
        assert_eq!(SrtErrno::from_raw(7000), SrtErrno::PeerError);
        assert_eq!(SrtErrno::from_raw(99999), SrtErrno::Unknown(99999));
    }

    #[test]
    fn reject_reason_internal_ordinals_match_srt_h() {
        // SRT_REJECT_REASON enum (srt.h:535-558). These constants are
        // generated by bindgen as `SRT_REJECT_REASON_SRT_REJ_*` and are the
        // source of truth — if libsrt's header changes ordinals the build
        // fails here, not at runtime. SRT_REJ_CRYPTO (=17) is gated by
        // ENABLE_AEAD_API_PREVIEW upstream and not in our bindings; the
        // explicit-ordinal test below pins ordinal 17 → Crypto.
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_UNKNOWN as i32),
            RejectReason::Unknown
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_SYSTEM as i32),
            RejectReason::System
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_PEER as i32),
            RejectReason::Peer
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_RESOURCE as i32),
            RejectReason::Resource
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_ROGUE as i32),
            RejectReason::Rogue
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_BACKLOG as i32),
            RejectReason::Backlog
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_IPE as i32),
            RejectReason::Ipe
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_CLOSE as i32),
            RejectReason::Close
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_VERSION as i32),
            RejectReason::Version
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_RDVCOOKIE as i32),
            RejectReason::RdvCookie
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_BADSECRET as i32),
            RejectReason::BadSecret
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_UNSECURE as i32),
            RejectReason::Unsecure
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_MESSAGEAPI as i32),
            RejectReason::MessageApi
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_CONGESTION as i32),
            RejectReason::Congestion
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_FILTER as i32),
            RejectReason::Filter
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_GROUP as i32),
            RejectReason::Group
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJECT_REASON_SRT_REJ_TIMEOUT as i32),
            RejectReason::Timeout
        );
    }

    #[test]
    fn reject_reason_internal_explicit_ordinals() {
        // Belt-and-braces: pin the explicit ordinals so a libsrt header
        // drift (e.g. SRT_REJ_CRYPTO moving from 17 → some other value) is
        // caught. Mirrors the table in srt.h:535-558 verbatim.
        assert_eq!(RejectReason::from_raw(0), RejectReason::Unknown);
        assert_eq!(RejectReason::from_raw(1), RejectReason::System);
        assert_eq!(RejectReason::from_raw(2), RejectReason::Peer);
        assert_eq!(RejectReason::from_raw(3), RejectReason::Resource);
        assert_eq!(RejectReason::from_raw(4), RejectReason::Rogue);
        assert_eq!(RejectReason::from_raw(5), RejectReason::Backlog);
        assert_eq!(RejectReason::from_raw(6), RejectReason::Ipe);
        assert_eq!(RejectReason::from_raw(7), RejectReason::Close);
        assert_eq!(RejectReason::from_raw(8), RejectReason::Version);
        assert_eq!(RejectReason::from_raw(9), RejectReason::RdvCookie);
        assert_eq!(RejectReason::from_raw(10), RejectReason::BadSecret);
        assert_eq!(RejectReason::from_raw(11), RejectReason::Unsecure);
        assert_eq!(RejectReason::from_raw(12), RejectReason::MessageApi);
        assert_eq!(RejectReason::from_raw(13), RejectReason::Congestion);
        assert_eq!(RejectReason::from_raw(14), RejectReason::Filter);
        assert_eq!(RejectReason::from_raw(15), RejectReason::Group);
        assert_eq!(RejectReason::from_raw(16), RejectReason::Timeout);
        assert_eq!(RejectReason::from_raw(17), RejectReason::Crypto);
    }

    #[test]
    fn reject_reason_extension_codes_match_access_control_h() {
        // SRT_REJX_* extension codes from access_control.h (1000..=1999
        // range). Set by remote services via srt_setrejectreason; carried
        // verbatim on the wire.
        assert_eq!(RejectReason::from_raw(1000), RejectReason::Fallback);
        assert_eq!(RejectReason::from_raw(1001), RejectReason::KeyNotSupported);
        assert_eq!(RejectReason::from_raw(1002), RejectReason::Filepath);
        assert_eq!(RejectReason::from_raw(1003), RejectReason::HostNotFound);
        assert_eq!(RejectReason::from_raw(1400), RejectReason::BadRequest);
        assert_eq!(RejectReason::from_raw(1401), RejectReason::Unauthorized);
        assert_eq!(RejectReason::from_raw(1402), RejectReason::Overload);
        assert_eq!(RejectReason::from_raw(1403), RejectReason::Forbidden);
        assert_eq!(RejectReason::from_raw(1404), RejectReason::NotFound);
        assert_eq!(RejectReason::from_raw(1405), RejectReason::BadMode);
        assert_eq!(RejectReason::from_raw(1406), RejectReason::Unacceptable);
        assert_eq!(RejectReason::from_raw(1409), RejectReason::Conflict);
        assert_eq!(
            RejectReason::from_raw(1415),
            RejectReason::NotSupportedMedia
        );
        assert_eq!(RejectReason::from_raw(1423), RejectReason::Locked);
        assert_eq!(RejectReason::from_raw(1424), RejectReason::FailedDependency);
        assert_eq!(
            RejectReason::from_raw(1500),
            RejectReason::InternalServerError
        );
        assert_eq!(RejectReason::from_raw(1501), RejectReason::Unimplemented);
        assert_eq!(RejectReason::from_raw(1502), RejectReason::Gateway);
        assert_eq!(RejectReason::from_raw(1503), RejectReason::Down);
        assert_eq!(
            RejectReason::from_raw(1505),
            RejectReason::VersionUnsupported
        );
        assert_eq!(RejectReason::from_raw(1507), RejectReason::NoRoom);
    }

    #[test]
    fn reject_reason_extension_named_constants() {
        // access_control.h SRT_REJX_* constants are #define macros; bindgen
        // exposes them as `pub const SRT_REJX_*: u32`. If upstream renumbers
        // any of these, this test fails and we know to update from_raw.
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_FALLBACK as i32),
            RejectReason::Fallback
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_KEY_NOTSUP as i32),
            RejectReason::KeyNotSupported
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_FILEPATH as i32),
            RejectReason::Filepath
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_HOSTNOTFOUND as i32),
            RejectReason::HostNotFound
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_BAD_REQUEST as i32),
            RejectReason::BadRequest
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_UNAUTHORIZED as i32),
            RejectReason::Unauthorized
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_OVERLOAD as i32),
            RejectReason::Overload
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_FORBIDDEN as i32),
            RejectReason::Forbidden
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_NOTFOUND as i32),
            RejectReason::NotFound
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_BAD_MODE as i32),
            RejectReason::BadMode
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_UNACCEPTABLE as i32),
            RejectReason::Unacceptable
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_CONFLICT as i32),
            RejectReason::Conflict
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_NOTSUP_MEDIA as i32),
            RejectReason::NotSupportedMedia
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_LOCKED as i32),
            RejectReason::Locked
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_FAILED_DEPEND as i32),
            RejectReason::FailedDependency
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_ISE as i32),
            RejectReason::InternalServerError
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_UNIMPLEMENTED as i32),
            RejectReason::Unimplemented
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_GW as i32),
            RejectReason::Gateway
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_DOWN as i32),
            RejectReason::Down
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_VERSION as i32),
            RejectReason::VersionUnsupported
        );
        assert_eq!(
            RejectReason::from_raw(srt_sys::SRT_REJX_NOROOM as i32),
            RejectReason::NoRoom
        );
    }

    #[test]
    fn reject_reason_extension_unknown_in_predefined_range_falls_to_other() {
        // Codes within SRT_REJC_PREDEFINED (1000..=1999) that aren't named
        // in access_control.h MUST fall to Other(raw) rather than silently
        // snap to a near neighbor.
        assert_eq!(RejectReason::from_raw(1099), RejectReason::Other(1099));
        // HTTP 408 is "not used" per access_control.h header comments.
        assert_eq!(RejectReason::from_raw(1408), RejectReason::Other(1408));
    }

    #[test]
    fn reject_reason_user_defined_falls_to_other() {
        // SRT_REJC_USERDEFINED (2000+) is application-defined; we never type
        // these. Negative is defensive — libsrt should not emit it.
        assert_eq!(RejectReason::from_raw(2000), RejectReason::Other(2000));
        assert_eq!(RejectReason::from_raw(9999), RejectReason::Other(9999));
        assert_eq!(RejectReason::from_raw(-1), RejectReason::Other(-1));
    }

    #[test]
    fn timeout_classifier() {
        // The real thing: SRT_ETIMEOUT (6003), regardless of message text.
        let r = RawError {
            kind: SrtErrno::Async,
            code: srt_sys::SRT_ERRNO_SRT_ETIMEOUT,
            message: "whatever text libsrt uses this release".into(),
        };
        assert!(is_timeout(&r));
        // A sibling major-6 errno (EASYNCRCV, 6002) is NOT a timeout even
        // when its message happens to contain the word — pins the
        // errno-over-text contract.
        let r = RawError {
            kind: SrtErrno::Async,
            code: srt_sys::SRT_ERRNO_SRT_EASYNCRCV,
            message: "Operation timed out".into(),
        };
        assert!(!is_timeout(&r));
    }

    #[test]
    fn broken_classifier() {
        let r = RawError {
            kind: SrtErrno::Connection,
            code: 2001, // SRT_ECONNLOST; is_broken doesn't consult code
            message: "Connection broken".into(),
        };
        assert!(is_broken(&r));
    }

    #[test]
    fn klv_decode_displays() {
        let e = KlvDecodeError::ChecksumMismatch {
            expected: 0xDEAD,
            found: 0xBEEF,
        };
        assert_eq!(
            e.to_string(),
            "checksum mismatch: declared 0xdead, computed 0xbeef"
        );
    }

    #[test]
    fn klv_encode_displays() {
        let e = KlvEncodeError::BufferTooSmall {
            needed: 256,
            got: 100,
        };
        assert_eq!(
            e.to_string(),
            "output buffer too small: needed 256 bytes, got 100"
        );
    }

    #[test]
    fn klv_field_clone_and_eq() {
        let a = KlvFieldError::InvalidUtf8 { tag: 50 };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn umbrella_from_klv_decode() {
        let e: SrtError = KlvDecodeError::Truncated {
            offset: 42,
            needed: 8,
            have: 3,
        }
        .into();
        match e {
            SrtError::KlvDecode(KlvDecodeError::Truncated { offset: 42, .. }) => {}
            _ => panic!("expected KlvDecode(Truncated{{offset:42, ...}})"),
        }
    }

    #[test]
    fn umbrella_from_klv_field() {
        let e: SrtError = KlvFieldError::InvalidUtf8 { tag: 50 }.into();
        matches!(
            e,
            SrtError::KlvField(KlvFieldError::InvalidUtf8 { tag: 50 })
        );
    }

    #[test]
    fn mux_error_displays() {
        let e = MuxError::BufferFull {
            capacity_packets: 10000,
        };
        assert_eq!(
            e.to_string(),
            "muxer packet buffer is full (10000 packets); drain via pull and retry"
        );
    }

    #[test]
    fn mux_invalid_config_displays() {
        let e = MuxError::InvalidConfig("video_pid must differ from klv_pid");
        assert_eq!(
            e.to_string(),
            "muxer configuration is invalid: video_pid must differ from klv_pid"
        );
    }

    #[test]
    fn umbrella_from_mux() {
        let e: SrtError = MuxError::InvalidNal.into();
        match e {
            SrtError::Mux(MuxError::InvalidNal) => {}
            _ => panic!("expected Mux(InvalidNal)"),
        }
    }

    #[test]
    fn mux_error_invalid_stream_handle_displays_kind_and_index() {
        let e = MuxError::InvalidStreamHandle {
            kind: StreamKind::Video,
            index: 7,
        };
        assert_eq!(
            e.to_string(),
            "invalid video stream handle (index 7) — not a configured stream",
        );
    }

    #[test]
    fn mux_error_ambiguous_target_displays_kind_and_count() {
        let e = MuxError::AmbiguousTarget {
            kind: StreamKind::Klv,
            count: 3,
        };
        assert_eq!(
            e.to_string(),
            "ambiguous push: 3 klv streams configured — call push_klv_to(handle, ...) instead",
        );
    }

    #[test]
    fn mux_error_too_many_video_streams_reports_cap() {
        let e = MuxError::TooManyVideoStreams { count: 17, cap: 16 };
        assert_eq!(
            e.to_string(),
            "too many video streams: 17 configured, cap is 16",
        );
    }

    #[test]
    fn mux_error_too_many_klv_streams_reports_cap() {
        let e = MuxError::TooManyKlvStreams { count: 20, cap: 16 };
        assert_eq!(
            e.to_string(),
            "too many klv streams: 20 configured, cap is 16",
        );
    }

    #[test]
    fn mux_error_too_many_audio_streams_reports_cap() {
        let e = MuxError::TooManyAudioStreams { count: 18, cap: 16 };
        assert_eq!(
            e.to_string(),
            "too many audio streams: 18 configured, cap is 16",
        );
    }

    #[test]
    fn mux_error_pmt_too_large_displays() {
        let e = MuxError::PmtTooLarge {
            used_bytes: 200,
            max_bytes: 166,
        };
        let s = format!("{e}");
        assert!(s.contains("200"));
        assert!(s.contains("166"));
        assert!(s.contains("PMT"));
    }

    #[test]
    fn mux_error_malformed_descriptor_displays() {
        let e = MuxError::MalformedDescriptor {
            stream_index: 2,
            descriptor_index: 1,
            reason: "length byte exceeds slice",
        };
        let s = format!("{e}");
        assert!(s.contains("stream 2"));
        assert!(s.contains("descriptor 1"));
        assert!(s.contains("length byte exceeds slice"));
    }

    #[test]
    fn mux_error_audio_too_large_reports_max() {
        let e = MuxError::AudioTooLarge {
            size: 70_000,
            max: 65527,
        };
        assert_eq!(
            e.to_string(),
            "audio frames too large: 70000 bytes, max 65527",
        );
    }

    #[test]
    fn mux_error_subtitle_variants_construct() {
        let _ = MuxError::TooManySubtitleStreams { count: 17, cap: 16 };
        let _ = MuxError::SubtitleTooLarge {
            size: 70_000,
            max: 65527,
        };
        let _ = MuxError::SubtitlePidUsedAsPcrPid { pid: 0x400 };
    }

    // SRT_ECONNREJ = 1002 → major 1 → SrtErrno::Setup. A gate that only
    // checks SrtErrno::Connection makes classify_connect_error with a
    // Setup-category raw + BadSecret reason fall through to Other.
    #[test]
    fn setup_category_reject_maps_to_rejected_badsecret() {
        let raw = RawError {
            kind: SrtErrno::Setup,
            code: 1002, // SRT_ECONNREJ; classifier reads kind + reject reason
            message: "Connection rejected".into(),
        };
        let reason = RejectReason::BadSecret;
        let err = classify_connect_error(raw, reason);
        match err {
            ConnectError::Rejected {
                reason: RejectReason::BadSecret,
                ..
            } => {}
            other => panic!("expected Rejected(BadSecret), got {other:?}"),
        }
    }

    // Connection-category with Unknown reason must NOT produce Rejected.
    #[test]
    fn connection_category_unknown_reason_maps_to_other() {
        let raw = RawError {
            kind: SrtErrno::Connection,
            code: 2001, // SRT_ECONNLOST; classifier reads kind + message
            message: "some other connection error".into(),
        };
        let reason = RejectReason::Unknown;
        let err = classify_connect_error(raw, reason);
        match err {
            ConnectError::Other { .. } => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    // Connection-category refused message with Unknown reason maps to Refused.
    #[test]
    fn connection_category_refused_message_maps_to_refused() {
        let raw = RawError {
            kind: SrtErrno::Connection,
            code: 2001, // SRT_ECONNLOST; classifier reads kind + message
            message: "Connection refused".into(),
        };
        let reason = RejectReason::Unknown;
        let err = classify_connect_error(raw, reason);
        match err {
            ConnectError::Refused => {}
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    // T3-SRT-REFUSED: the "refused" substring heuristic must NOT follow the Setup
    // category. A Setup-category error with an Unknown reason and a message that
    // happens to contain "refused" must map to Other, not Refused — the heuristic
    // is scoped to Connection (major-2 plain refusals) only.
    #[test]
    fn setup_category_refused_message_maps_to_other_not_refused() {
        let raw = RawError {
            kind: SrtErrno::Setup,
            code: 1002, // SRT_ECONNREJ; classifier reads kind + reject reason
            message: "Connection setup failure: connection refused".into(),
        };
        let reason = RejectReason::Unknown;
        let err = classify_connect_error(raw, reason);
        match err {
            ConnectError::Other { .. } => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    // A blocking connect that exhausts SRTO_CONNTIMEO raises
    // SRT_ENOSERVER (major Setup) and sets RejectReason::Timeout.  The
    // documented ConnectError::TimedOut must be returned, not Rejected.
    #[test]
    fn setup_category_reject_timeout_maps_to_timed_out() {
        let raw = RawError {
            kind: SrtErrno::Setup,
            code: 1002, // SRT_ECONNREJ; classifier reads kind + reject reason
            message: "Connection setup failure: connection timeout".into(),
        };
        let reason = RejectReason::Timeout;
        let err = classify_connect_error(raw, reason);
        match err {
            ConnectError::TimedOut => {}
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    // A typed reject that is NOT a timeout must still produce Rejected, even
    // when the category is Setup (e.g. SRT_ECONNREJ with a BadSecret reason).
    #[test]
    fn setup_category_reject_badsecret_still_maps_to_rejected() {
        let raw = RawError {
            kind: SrtErrno::Setup,
            code: 1002, // SRT_ECONNREJ; classifier reads kind + reject reason
            message: "Connection rejected".into(),
        };
        let reason = RejectReason::BadSecret;
        let err = classify_connect_error(raw, reason);
        match err {
            ConnectError::Rejected {
                reason: RejectReason::BadSecret,
                ..
            } => {}
            other => panic!("expected Rejected(BadSecret), got {other:?}"),
        }
    }

    // The SRT_ETIMEOUT path must remain TimedOut (classification now keys
    // on the errno, not the message text — the text here is deliberately
    // unhelpful to pin that).
    #[test]
    fn async_timeout_errno_maps_to_timed_out() {
        let raw = RawError {
            kind: SrtErrno::Async,
            code: srt_sys::SRT_ERRNO_SRT_ETIMEOUT,
            message: "some future libsrt wording".into(),
        };
        let reason = RejectReason::Unknown;
        let err = classify_connect_error(raw, reason);
        match err {
            ConnectError::TimedOut => {}
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }
}
