//! Thread-local last-error storage and the TST_E_* code enum.
//!
//! Mirrors libsrt's idiom: every fallible C function returns 0 on success
//! and a negative TST_E_* code on failure, with a thread-local detail
//! string available via tst_get_last_error_str(). The detail string is
//! stable until the next tst-c call on the same thread.

use alloc::borrow::ToOwned;
use alloc::ffi::CString;
use alloc::string::ToString;
use core::cell::RefCell;

/// Negative codes returned by every fallible tst-c entry point.
///
/// `Success = 0` is the only non-negative variant. Codes are stable
/// across tst-c versions; new codes append at the end.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TstError {
    Success = 0,
    InvalidConfig = -1,
    InvalidNal = -2,
    InvalidTs = -3,
    BufferFull = -4,
    KlvTooLarge = -5,
    TooLarge = -6,
    Closed = -7,
    Transport = -8,
    InvalidUsage = -9,
    Internal = -10,
    /// Internal panic caught at the FFI boundary; the handle is now in
    /// an indeterminate state. Subsequent calls on the same handle will
    /// also fail (returning `Closed`). The caller should free the handle.
    PanicCaught = -11,
    /// Peer disconnected gracefully (received TCP-style FIN / SRT clean close).
    /// Distinguished from `Closed` (caller-side cancel/close) so receive loops
    /// can branch on the shutdown reason. After this code the handle is dead;
    /// subsequent calls return `Closed`.
    EndOfStream = -12,
    /// (-13) Resource temporarily unavailable; retry later.
    ///
    /// Returned by stats/socket_stats accessors on a managed handle while
    /// the underlying transport is reconnecting. The same call may
    /// succeed once reconnect completes — bindings should expose this as
    /// a transient signal that does not require user intervention.
    ///
    /// Returned today by the `tst_*_get_socket_stats` family when the
    /// inner transport's `socket_stats()` returns `None` (mid-reconnect
    /// or after close).
    ///
    /// **Contract:** transient. The next call on the same handle may
    /// succeed.
    ///
    /// See [`TstError::NotFound`] for the persistent counterpart, and
    /// [`TstError::InvalidUsage`] for the wrong-handle-state case.
    NotAvailable = -13,
    /// (-14) Resource not found; the request will not succeed on this handle.
    ///
    /// Returned by per-PID accessors (codec stats, stream info) when the
    /// PID has never been observed on this stream. Distinct from
    /// `NotAvailable` (which is transient — same call may later succeed)
    /// and from `InvalidUsage` (which means the handle is in a
    /// fundamentally wrong state for the call entirely).
    ///
    /// Returned today by the `tst_*_get_stream_codec_stats` family when
    /// the caller asks for a PID that has never been observed on this
    /// handle.
    ///
    /// **Contract:** persistent. The next call on the same handle with
    /// the same key will return the same error. Retry is futile unless
    /// the caller knows the key has since started being observed.
    ///
    /// See [`TstError::NotAvailable`] for the transient counterpart.
    NotFound = -14,
    /// (-15) RTP socket / transport error (bind/connect/send/recv).
    /// Distinct from `Transport` which covers SRT shell errors; RTP has
    /// no concept of a libsrt-flavored shell so it routes directly here.
    RtpTransport = -15,
    /// (-16) Malformed RTSP wire format or unexpected status from peer.
    /// Generic protocol-error bucket.
    RtspProtocol = -16,
    /// (-17) RTSP authentication exhausted (bad credentials, or server
    /// challenged after retry).
    RtspAuthFailed = -17,
    /// (-18) RTSP server requires authentication but client has no
    /// credentials, or the offered auth scheme is unsupported.
    RtspAuthRequired = -18,
    /// (-19) RTSP 404 from server, or no uniquely-identified SDP media
    /// found (MP2T or H.264 — none present, or more than one).
    RtspNotFound = -19,
    /// (-20) RTSP 461 Unsupported Transport — all transport preferences
    /// (UDP + TCP-interleaved) exhausted by server. Also fires when the
    /// H.264 media advertises an unsupported RFC 6184 packetization-mode
    /// (interleaved mode 2).
    RtspUnsupported = -20,
    /// (-21) rustls TLS handshake or certificate validation failure
    /// (only emitted for rtsps:// connections; feature `tls`).
    RtspTls = -21,
    /// (-22) socket I/O failure during an RTSP exchange (TCP close, etc.).
    RtspIo = -22,
    /// (-23) keepalive or request timeout on an RTSP connection.
    RtspTimeout = -23,
    /// (-24) RtspServerError variants (lifecycle, config — bind in use,
    /// duplicate mount path, max sessions reached, etc.).
    RtspServer = -24,
    /// (-25) MountError variants from RtspServer mount surface
    /// (underlying MuxError, backpressure, closed).
    RtspMount = -25,

    // Plan A5a — UDP error codes (-26..=-29).
    /// (-26) UDP transport I/O failure (bind, connect, send, recv).
    /// Maps from `tst_udp::UdpErrorKind::Io`.
    UdpIo = -26,
    /// (-27) UDP URL/config parse failure or invalid host literal.
    /// Maps from `UdpErrorKind::{Url, InvalidConfig}`.
    UdpConfig = -27,
    /// (-28) UDP payload too large for the configured MTU / pkt_size.
    /// Reserved; not currently produced (no `UdpErrorKind` variant maps here).
    UdpPayloadTooLarge = -28,
    /// (-29) UDP multicast interface not supported (e.g., requested
    /// `?iface=eth0` on a platform where `tst-udp` can't apply it).
    /// Reserved; not currently produced (no `UdpErrorKind` variant maps here).
    UdpIfaceUnsupported = -29,

    // Plan A5a — TCP error codes (-30..=-33).
    /// (-30) TCP transport I/O failure (connect, accept, send, recv).
    /// Maps from `tst_tcp::TcpErrorKind::Io`.
    TcpIo = -30,
    /// (-31) TCP URL/config parse failure.
    /// Maps from `TcpErrorKind::{Url, InvalidConfig}`.
    TcpConfig = -31,
    /// (-32) TCP connect timeout (default 10s, override via `?connect_timeout=`).
    /// Maps from `TcpErrorKind::ConnectTimeout`.
    TcpConnectTimeout = -32,
    /// (-33) TCP TLS handshake or certificate validation failure;
    /// or TLS requested but `tst-tcp` built without `tls` feature.
    /// Maps from `TcpErrorKind::{Tls, TlsDisabled}`.
    TcpTls = -33,

    // Plan A5a — HLS error codes (-34..=-37).
    /// (-34) HLS HTTP server bind/listen failure.
    /// Maps from `tst_hls::HlsErrorKind::{BindFailed, Io}`.
    HlsIo = -34,
    /// (-35) HLS configuration invalid (bad output_dir, segment_duration < 1s, etc.).
    /// Maps from `HlsErrorKind::{Url, InvalidConfig, UnalignedPushTs}`.
    HlsConfig = -35,
    /// (-36) HLS publisher already finished (terminal state after
    /// `tst_hls_publisher_finish`); subsequent push/cut calls fail here.
    /// Maps from `HlsErrorKind::Finished`.
    HlsFinished = -36,
    /// (-37) HLS TLS error (cert load, handshake) or TLS requested
    /// but disabled at build time.
    /// Maps from `HlsErrorKind::{Tls, TlsDisabled}`.
    HlsTls = -37,

    // Plan A5a — RIST error codes (-38..=-43).
    /// (-38) RIST librist FFI failure; check the message for the
    /// underlying librist function name + error code.
    /// Maps from `tst_rist::RistErrorKind::{Ffi, ContextCreateFailed, PeerCreateFailed}`.
    RistFfi = -38,
    /// (-39) RIST URL/config parse failure or invalid AES type.
    /// Maps from `RistErrorKind::{Url, InvalidConfig}`.
    RistConfig = -39,
    /// (-40) RIST payload too large for the configured pkt_size
    /// (default 1316 bytes; STANAG-4609-aligned).
    /// Reserved; not currently produced (no `RistErrorKind` variant maps here).
    RistPayloadTooLarge = -40,
    /// (-41) RIST encryption requested but `tst-rist` built without
    /// `mbedtls` feature (uncrypted librist build cannot apply AES).
    /// Maps from `RistErrorKind::EncryptionDisabled`.
    RistEncryptionDisabled = -41,
    /// (-42) RIST receive timeout exceeded the session_timeout.
    /// Reserved; not currently produced (no `RistErrorKind` variant maps here).
    RistRecvTimeout = -42,
    /// (-43) RIST socket I/O failure underlying the librist transport.
    /// Reserved; not currently produced (no `RistErrorKind` variant maps here).
    RistIo = -43,

    /// (-44) AV1 OBU input is not a well-formed elementary OBU stream;
    /// the wrapping push rejected it. Returned when the caller feeds
    /// already-carried (binding-framed) wire bytes to `push_video_to`
    /// instead of raw elementary OBUs.
    /// Maps from `MuxError::InvalidAv1Obu`.
    InvalidAv1Obu = -44,
    /// (-45) A MISP-timestamp push could not build/splice the ST 0604
    /// SEI (nano on H.264, AV1/H.266 stream, or no VCL NAL in the AU).
    /// Maps from `MuxError::MispTime`.
    MispTime = -45,
    /// (-46) `tst_misp_time_extract` matched a MISP SEI identifier but
    /// the payload is malformed (truncated / bad 0xFF guard byte).
    /// Maps from `codec::misp_time::MispTimeExtractError`.
    MispTimeMalformed = -46,
    /// (-47) A `tst_st0601_get_f64` / `tst_st0601_get_u64` accessor was
    /// called for a tag whose corresponding `UasDatalinkLs` field has a
    /// different native Rust type than the accessor requests (e.g.
    /// `tst_st0601_get_f64` on tag 2, whose typed field is `u64`). The
    /// getter refuses to lossily cast; call the correctly-typed accessor
    /// instead.
    WrongType = -47,
    /// (-48) `tst_st0601_decode` could not parse the input bytes as a
    /// MISB ST 0601 UAS Datalink Local Set. Maps from
    /// `tst_core::error::KlvDecodeError`; see the message for the
    /// specific structural failure (truncated buffer, malformed BER
    /// length/tag, checksum mismatch, unexpected universal label, ...).
    KlvDecode = -48,
}

// ---------------------------------------------------------------------------
// Per-thread (std) / per-context (no_std) last-error storage
// ---------------------------------------------------------------------------

#[cfg(feature = "std")]
thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> = RefCell::new((0, CString::new("").unwrap()));
}
#[cfg(feature = "std")]
fn with_last_error<R>(f: impl FnOnce(&mut (i32, CString)) -> R) -> R {
    LAST_ERROR.with(|cell| f(&mut cell.borrow_mut()))
}

// Under no_std (bare-metal, single-core): use critical-section + spin to
// protect a static Option<(i32, CString)>.
// The Option is required because CString::new("") is NOT const-evaluable
// on Rust 1.85, so the static must be initialised to None and lazily
// filled via get_or_insert_with on the first access.
#[cfg(not(feature = "std"))]
static LAST_ERROR: critical_section::Mutex<RefCell<Option<(i32, CString)>>> =
    critical_section::Mutex::new(RefCell::new(None));
#[cfg(not(feature = "std"))]
fn with_last_error<R>(f: impl FnOnce(&mut (i32, CString)) -> R) -> R {
    critical_section::with(|cs| {
        let mut slot = LAST_ERROR.borrow(cs).borrow_mut();
        let inner = slot.get_or_insert_with(|| (0, CString::new("").unwrap()));
        f(inner)
    })
}

/// Set the per-thread last-error code + message. Internal helper used by
/// every fallible entry point on its error path.
pub(crate) fn set_last_error(code: TstError, msg: &str) {
    let cstr = CString::new(msg).unwrap_or_else(|_| CString::new("<message had nul>").unwrap());
    with_last_error(|slot| *slot = (code as i32, cstr));
}

#[cfg(test)]
pub(crate) fn clear_last_error_for_test() {
    with_last_error(|slot| *slot = (0, CString::new("").unwrap()));
}

/// Read the most recent error code on this thread. Returns `0`
/// (`TST_E_SUCCESS`) if no error has been recorded on this thread yet.
/// The value is not cleared by successful calls; it reflects the most
/// recent failure on this thread (or `TST_E_SUCCESS` if there has been
/// none since thread start).
///
/// **Exception — the RTP end-reason getters:** `tst_rtp_receiver_end_reason`
/// and `tst_rtp_demux_receiver_end_reason` reset this to `TST_E_SUCCESS`
/// (with a detail message on [`tst_get_last_error_str`], possibly empty)
/// EVERY time they report an actually-recorded end reason — even though
/// they are not themselves failing; see their doc for the full contract.
/// A pending failure from an earlier call must be read before calling
/// one of those getters, or it is overwritten.
///
/// **Storage:** per-thread (`thread_local!`) under the default `std` build
/// (the desktop cdylib/staticlib — the per-thread wording above is exact).
/// In a `no_std` build the slot is instead a single **process-global**
/// behind a `critical-section` lock, so the value — and the pointer from
/// [`tst_get_last_error_str`] — may be overwritten by a tst-c call from any
/// task/core; a multi-task `no_std` consumer must read it before the next
/// tst-c call from anywhere.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_last_error() -> crate::c_types::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || with_last_error(|slot| slot.0))
}

/// Pointer to the most recent error message on this thread. Valid until
/// the next tst-c call on the same thread. Never NULL — empty string when
/// no error.
///
/// **Exception — the RTP end-reason getters:** see the note on
/// [`tst_get_last_error`] — `tst_rtp_receiver_end_reason` and
/// `tst_rtp_demux_receiver_end_reason` overwrite this message (to the
/// recorded reason's detail, or an empty string when the reason carries
/// none) on every call that reports an actually-recorded reason.
///
/// **`no_std` builds:** the backing slot is process-global rather than
/// per-thread (see [`tst_get_last_error`]), so "the next tst-c call"
/// means the next call from **any** task — copy the message out before
/// another task can make a tst-c call if it must be retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_last_error_str() -> *const crate::c_types::c_char {
    // The thread-local CString stays alive for the thread's lifetime, but
    // if `borrow()` panicked (reentrant Drop double-borrow), the happy-path
    // pointer is unreachable. Fall back to a static empty C string so the
    // never-NULL contract above is preserved.
    static EMPTY: &[u8] = b"\0";
    let fallback = EMPTY.as_ptr() as *const crate::c_types::c_char;
    crate::panic::ffi_catch(fallback, || with_last_error(|slot| slot.1.as_ptr()))
}

/// Clears the thread-local last-error slot, resetting it to
/// `(TST_E_SUCCESS, "")`.
///
/// Most callers should NOT need this — every fallible `tst_*` function
/// returns its result code directly (0 on success, negative on failure),
/// so checking the return value is the idiomatic pattern. The
/// thread-local last-error slot is a side-channel for the **message
/// string** corresponding to the most recent failure, useful for
/// logging and diagnostics.
///
/// Use this function when:
///
/// 1. Chaining checks through code that doesn't propagate return values
///    (e.g., a series of `tst_mux_config_add_*_stream` calls in a
///    higher-level helper that returns a single combined status).
/// 2. Discriminating "the most recent call succeeded" from "the most
///    recent call failed and set an error" using `tst_get_last_error()
///    == 0` as the post-call check.
///
/// **Thread-locality:** clears only the calling thread's slot. Other
/// threads' last-error values are unaffected. Matches the libsrt
/// `srt_clearlasterror()` semantic.
///
/// # Safety
///
/// Sound under any caller invocation — no pointer arguments. Under `std`
/// the per-thread slot is mutated without locks; under `no_std` a single
/// process-global slot is mutated inside a brief `critical-section` (which
/// disables interrupts on single-core targets). The `unsafe extern "C"`
/// annotation matches the convention of every other `tst_*` entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_clear_last_error() {
    crate::panic::ffi_catch((), || {
        set_last_error(TstError::Success, "");
    });
}

use tst_core::error::{DemuxError, KlvDecodeError, MuxError};
#[cfg(test)]
use tst_core::mpegts::mux::StreamKind;
use tst_pipeline::{ShellError, ShellErrorKind, TransportError};

/// Map a [`ShellErrorKind`] to its corresponding [`TstError`] code.
///
/// This is the single point of truth for the kind-to-code projection.
/// CI ratchet `scripts/check/rust/shell-error-kind-coverage.sh` enforces
/// every `ShellErrorKind` variant is matched explicitly here.
pub(crate) fn tst_error_from_kind(kind: ShellErrorKind) -> TstError {
    match kind {
        ShellErrorKind::ConfigInvalid => TstError::InvalidConfig,
        ShellErrorKind::InputMalformed => TstError::InvalidTs,
        ShellErrorKind::Backpressure => TstError::BufferFull,
        ShellErrorKind::TransportBroken => TstError::Transport,
        ShellErrorKind::Closed => TstError::Closed,
        ShellErrorKind::EndOfStream => TstError::EndOfStream,
        // Required by #[non_exhaustive]. CI ratchet
        // scripts/check/rust/shell-error-kind-coverage.sh enforces every
        // ShellErrorKind variant is matched above before this arm.
        _ => TstError::Internal,
    }
}

/// Record a shell error to the per-thread last-error slot. Used by
/// every C ABI entry point's error path. Replaces the per-variant
/// `record_sender_error` / `record_ts_sender_error` functions from
/// pre-Wave-4 code. The standalone-muxer path still uses
/// `record_mux_error` (for raw `MuxError` values not wrapped in a shell),
/// and the connect/listen helper paths still use `record_transport_error`
/// (for raw `TransportError` from pre-shell-layer code).
///
/// Returns the negative TST_E_* code suitable for direct return from
/// the C entry point.
pub(crate) fn record_shell_error<E: ShellError>(e: &E) -> i32 {
    let code = tst_error_from_kind(e.kind());
    set_last_error(code, &e.to_string());
    code as i32
}

/// Map a `MuxError` to a code + message via the inner-tier
/// `MuxErrorKind` category.
///
/// The code projection routes through `MuxError::kind()` for the
/// `ConfigInvalid` / `InvalidUsage` / `Backpressure` / `Internal`
/// categories (each has a stable per-kind `TST_E_*` code). The
/// `InputMalformed` category has 6 variants mapping to 4 different
/// `TstError` codes, so 3 variants get explicit overrides. The
/// diagnostic message uses `MuxError`'s `Display` impl preserving
/// spec-rich diagnostics from the `#[error("...")]` attributes.
///
/// **CI invariants:**
///
/// 1. `scripts/check/c/raw-mapper-coverage.sh` — every `MuxError`
///    variant must be mentioned in the per-variant routing table
///    inside this function before the wildcard arm.
/// 2. The in-file unit test `every_known_mux_error_variant_maps_to_expected_code`
///    verifies all 37 variants produce the expected `TstError` code.
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn record_mux_error(e: &MuxError) {
    use tst_core::error::MuxErrorKind;

    // Per-variant code routing (covered by kind() projection below
    // unless explicitly overridden). The ratchet
    // scripts/check/c/raw-mapper-coverage.sh greps this block for
    // every MuxError::VariantName before the wildcard arm.
    //
    //   MuxError::InvalidNal              -> TstError::InvalidNal     [override]
    //   MuxError::InvalidAv1Obu          -> TstError::InvalidAv1Obu  [override]
    //   MuxError::MispTime(_)            -> TstError::MispTime        [override]
    //   MuxError::KlvTooLarge             -> TstError::KlvTooLarge    [override]
    //   MuxError::AudioTooLarge           -> TstError::InvalidUsage   (InputMalformed kind default)
    //   MuxError::SubtitleTooLarge        -> TstError::InvalidUsage   (InputMalformed kind default)
    //   MuxError::DataTooLarge            -> TstError::InvalidUsage   (InputMalformed kind default)
    //   MuxError::BufferFull              -> TstError::BufferFull     (Backpressure kind default)
    //   MuxError::InvalidConfig           -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::ConfigInvalid           -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::InvalidLanguageCode     -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::InvalidTeletextField    -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::TooManyVideoStreams     -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::TooManyKlvStreams       -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::TooManyAudioStreams     -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::TooManySubtitleStreams  -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::TooManyDataStreams      -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::TooManyPrograms         -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::EmptyProgram            -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::DuplicateProgramNumber  -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::DuplicatePmtPid         -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::DuplicatePidAcrossPrograms -> TstError::InvalidConfig (ConfigInvalid kind default)
    //   MuxError::PmtPidConflictsWithStream  -> TstError::InvalidConfig (ConfigInvalid kind default)
    //   MuxError::SubtitlePidUsedAsPcrPid -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::KlvPidUsedAsPcrPid      -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::DataPidUsedAsPcrPid     -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::NoPcrEligibleStream     -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::MalformedDescriptor     -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::PmtTooLarge             -> TstError::InvalidConfig  (ConfigInvalid kind default)
    //   MuxError::InvalidStreamHandle     -> TstError::InvalidUsage   (InvalidUsage kind default)
    //   MuxError::AmbiguousTarget         -> TstError::InvalidUsage   (InvalidUsage kind default)
    //   MuxError::NoKlvStreamsConfigured  -> TstError::InvalidUsage   (InvalidUsage kind default)
    //   MuxError::NoAudioStreamsConfigured -> TstError::InvalidUsage  (InvalidUsage kind default)
    //   MuxError::NoSubtitleStreamsConfigured -> TstError::InvalidUsage (InvalidUsage kind default)
    //   MuxError::NoDataStreamsConfigured -> TstError::InvalidUsage   (InvalidUsage kind default)
    //   MuxError::ProgramNotFound         -> TstError::InvalidUsage   (InvalidUsage kind default)
    //   MuxError::DescriptorIndexOutOfRange -> TstError::InvalidUsage (InvalidUsage kind default)
    //   MuxError::AbsIndexOutOfRange      -> TstError::InvalidUsage   (InvalidUsage kind default)
    let code = match e {
        // InputMalformed bucket — variant-specific code overrides.
        // The kind-default for InputMalformed maps to InvalidUsage;
        // these variants project to more specific codes for
        // diagnostic precision.
        MuxError::InvalidNal => TstError::InvalidNal,
        MuxError::InvalidAv1Obu => TstError::InvalidAv1Obu,
        MuxError::MispTime(_) => TstError::MispTime,
        MuxError::KlvTooLarge { .. } => TstError::KlvTooLarge,

        // All other variants route via the kind() projection.
        _ => match e.kind() {
            MuxErrorKind::ConfigInvalid => TstError::InvalidConfig,
            MuxErrorKind::InvalidUsage => TstError::InvalidUsage,
            MuxErrorKind::Backpressure => TstError::BufferFull,
            // AudioTooLarge + SubtitleTooLarge + DataTooLarge fall through
            // here (the 3 InputMalformed variants not covered by overrides
            // above). All three project to InvalidUsage per the
            // pre-Wave-6.D behavior.
            MuxErrorKind::InputMalformed => TstError::InvalidUsage,
            MuxErrorKind::Internal => TstError::Internal,
            // Required by #[non_exhaustive]. CI ratchet
            // scripts/check/rust/mux-error-kind-coverage.sh enforces every
            // MuxErrorKind variant is matched above before this arm.
            // Matches the wildcard-default-to-Internal pattern from Wave
            // 4.A (record_shell_error) and Wave 6.D (MuxError::kind() at
            // tst-core/src/error.rs:631): an unknown future coarse kind
            // is more truthful as a library/internal failure than as
            // caller InvalidConfig.
            _ => TstError::Internal,
        },
    };
    // Use the existing Display impl on MuxError — each variant's
    // #[error("...")] attribute already produces a spec-rich diagnostic
    // string.
    set_last_error(code, &e.to_string());
}

/// Map a [`DemuxError`] to a code + message and record it to the per-thread
/// last-error slot.
///
/// Used by the standalone offline demuxer path (`tst_demuxer_feed`). The
/// transport-coupled `tst_demux_receiver_*` surface uses `record_shell_error`
/// for `DemuxError`-rooted failures because those arrive wrapped in a
/// `ShellError`. This mapper handles the raw demuxer path where no shell wraps
/// the error.
///
/// **Variant coverage (DemuxError is `#[non_exhaustive]`):** all four known
/// variants have explicit arms; the wildcard arm maps future additions to
/// `TST_E_INVALID_TS` (the demux-parse error bucket) and surfaces the
/// `Display` string so the message is still informative.
pub(crate) fn record_demux_error(e: &DemuxError) -> i32 {
    // Per-variant code routing. All four known DemuxError variants are listed
    // explicitly before the wildcard. The scripts/check/c/raw-mapper-coverage.sh
    // ratchet intentionally does NOT scan DemuxError (it is `#[non_exhaustive]`
    // from tst-core; the mux/transport raw mappers it covers predate this
    // function). The explicit arms below give the same coverage guarantee
    // without an automated ratchet row.
    //
    //   DemuxError::StrictRejection(_)   → TstError::InvalidTs (-3)
    //   DemuxError::Unrecoverable{..}    → TstError::InvalidTs (-3)
    //   DemuxError::MalformedPsi{..}     → TstError::InvalidTs (-3)
    //   DemuxError::MalformedPes{..}     → TstError::InvalidTs (-3)
    //   DemuxError::SyncBufExhausted{..} → TstError::TooLarge  (-6)
    let code = match e {
        DemuxError::StrictRejection(_) => TstError::InvalidTs,
        DemuxError::Unrecoverable { .. } => TstError::InvalidTs,
        DemuxError::MalformedPsi { .. } => TstError::InvalidTs,
        DemuxError::MalformedPes { .. } => TstError::InvalidTs,
        // Fired when the caller feeds a pathologically large byte stream with
        // no 0x47 sync bytes — the sync buffer hit its 4 MiB cap.
        DemuxError::SyncBufExhausted { .. } => TstError::TooLarge,
        // Required by #[non_exhaustive]. Future variants map to InvalidTs
        // (the most generic demux-parse error bucket) until explicitly added.
        _ => TstError::InvalidTs,
    };
    set_last_error(code, &e.to_string());
    code as i32
}

#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn record_transport_error(e: &TransportError) {
    // D5 follow-up: helper to render the optional errno suffix. `SrtErrno::Bad.raw_code() == 0`,
    // which would read as "(errno 0)" — i.e., "no error" in libsrt's idiom.
    // That's a footgun for C consumers; suppress the suffix when the code
    // is 0 (the Bad sentinel) so it doesn't masquerade as a real errno.
    // Extracted into a fn (rather than a nested match) so the outer match
    // body doesn't contain an inner `_ =>` arm — that would confuse the
    // scripts/check/c/raw-mapper-coverage.sh ratchet (its awk extractor stops at
    // the first `_ =>` line, treating it as the outer wildcard).
    fn errno_suffix(errno_code: &Option<i32>) -> alloc::string::String {
        match errno_code {
            Some(c) if *c != 0 => alloc::format!(" (errno {c})"),
            Some(_) | None => alloc::string::String::new(),
        }
    }
    let (code, msg) = match e {
        // The struct variants now carry an optional `errno_code` (libsrt
        // MJ_* major when the underlying transport is SRT). Append it to
        // the message when present so C consumers can see the wire-level
        // cause without reaching past the C ABI; Rust callers that need
        // structured access still get the typed field via the
        // TransportError struct variant directly.
        TransportError::Backpressure { msg: s, errno_code } => (
            TstError::Transport,
            alloc::format!("backpressure: {s}{}", errno_suffix(errno_code)),
        ),
        TransportError::Broken {
            msg: s, errno_code, ..
        } => (
            TstError::Transport,
            alloc::format!("broken: {s}{}", errno_suffix(errno_code)),
        ),
        TransportError::Closed => (
            TstError::Closed,
            alloc::string::String::from("transport closed"),
        ),
        TransportError::TooLarge { len, max } => (
            TstError::TooLarge,
            alloc::format!("message {len} bytes exceeds payload cap {max}"),
        ),
        _ => {
            // Required by #[non_exhaustive]. See scripts/check/c/raw-mapper-coverage.sh
            // for the CI ratchet that prevents this arm from firing.
            (
                TstError::Transport,
                alloc::format!("unhandled TransportError variant: {e:?}"),
            )
        }
    };
    set_last_error(code, &msg);
}

/// Helper for entry points that catch panics or Mutex poison.
pub(crate) fn record_internal(detail: &str) {
    set_last_error(
        TstError::Internal,
        &alloc::format!("internal error: {detail}"),
    );
}

/// Helper for the `catch_unwind` arm of `Handle::with_inner_*`. Records
/// a `PanicCaught` last-error with a useful detail message extracted
/// from the panic payload.
pub(crate) fn record_panic_caught(detail: &str) {
    set_last_error(
        TstError::PanicCaught,
        &alloc::format!("panic caught at FFI boundary: {detail}"),
    );
}

/// Record an end-of-stream condition. Used by receivers when the transport
/// reports a graceful peer close and the call was not caller-initiated.
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn record_eos() {
    set_last_error(TstError::EndOfStream, "end of stream (peer disconnected)");
}

/// Record `NotAvailable` (-13) with a per-call message and return the
/// negative code. Use this from C ABI entry points that hit a transient
/// "unavailable" condition (typically `socket_stats() -> None` mid-reconnect
/// or after close).
///
/// Replaces the direct `TstError::NotAvailable as i32` pattern that leaves
/// stale last-error state visible to `tst_get_last_error()` (per Codex
/// re-review finding 1, plan #93).
pub(crate) fn record_not_available(msg: &str) -> i32 {
    set_last_error(TstError::NotAvailable, msg);
    TstError::NotAvailable as i32
}

/// Record `NotFound` (-14) with a per-call message and return the negative
/// code. Use this from C ABI per-PID / per-key accessors when the requested
/// key has never been observed on this handle.
///
/// Replaces the direct `TstError::NotFound as i32` pattern that leaves
/// stale last-error state visible to `tst_get_last_error()`.
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn record_not_found(msg: &str) -> i32 {
    set_last_error(TstError::NotFound, msg);
    TstError::NotFound as i32
}

/// Record `WrongType` (-47) with a per-call message and return the
/// negative code. Use this from C ABI accessors that refuse a lossy
/// cast between the caller's requested native type and the mapped
/// field's actual Rust type (see [`TstError::WrongType`]).
pub(crate) fn record_wrong_type(msg: &str) -> i32 {
    set_last_error(TstError::WrongType, msg);
    TstError::WrongType as i32
}

/// Map a [`KlvDecodeError`] to `TstError::KlvDecode` (-48) and record it
/// to the per-thread last-error slot. Every variant collapses to the
/// same code — `tst_st0601_decode` is a single structural-parse entry
/// point with no per-variant C-side branching, unlike
/// [`record_mux_error`]'s per-variant overrides. The `Display` impl on
/// `KlvDecodeError` still carries the spec-rich diagnostic (offset,
/// expected/found bytes, ...) into the last-error message.
pub(crate) fn record_klv_decode_error(e: &KlvDecodeError) -> i32 {
    set_last_error(TstError::KlvDecode, &e.to_string());
    TstError::KlvDecode as i32
}

// ---------------------------------------------------------------------------
// Phase 4 — RTP/RTSP error-to-code converters (gated on "rtp" feature).
//
// All three target enums are `#[non_exhaustive]` in tst-rtp, so Rust requires
// a wildcard arm on matches from outside that crate. The explicit arms below
// cover every variant known at Phase 4 ship time; the wildcard is a safe
// fallback for future additions. CI ratchet
// `scripts/check/rust/rtsp-error-mapping-coverage.sh` catches any gap at pre-push
// time (not compile time) — it greps the explicit arm list here against the
// enum definition in tst-rtp.
// ---------------------------------------------------------------------------

/// Map a [`tst_rtp::RtspError`] to the appropriate `TstError` variant.
///
/// Explicit arms cover every `RtspError` variant. The wildcard fallback is
/// required by `#[non_exhaustive]` and maps future variants to
/// `TstError::RtspProtocol` (the most generic RTSP failure bucket).
///
/// CI ratchet `scripts/check/rust/rtsp-error-mapping-coverage.sh` verifies every
/// known variant has an explicit arm.
#[cfg(feature = "rtp")]
#[allow(dead_code)] // RTSP-feature-gated callers; unused in minimal builds
pub(crate) fn rtsp_error_to_code(e: &tst_rtp::RtspError) -> TstError {
    use tst_rtp::RtspError::*;
    match e {
        Io(_) => TstError::RtspIo,
        Tls(_) => TstError::RtspTls,
        Protocol { .. } => TstError::RtspProtocol,
        AuthFailed => TstError::RtspAuthFailed,
        AuthUnsupported { .. } => TstError::RtspAuthRequired,
        BadResponse { .. } => TstError::RtspProtocol,
        BadSdp { .. } => TstError::RtspProtocol,
        UnsupportedTransport => TstError::RtspUnsupported,
        SessionExpired => TstError::RtspProtocol,
        Timeout => TstError::RtspTimeout,
        LocalCancel => TstError::RtspProtocol,
        NoMp2tMedia => TstError::RtspNotFound,
        MultipleMp2tMedia { .. } => TstError::RtspNotFound,
        NoH264Media => TstError::RtspNotFound,
        MultipleH264Media { .. } => TstError::RtspNotFound,
        UnsupportedPacketizationMode(_) => TstError::RtspUnsupported,
        Url(_) => TstError::RtspProtocol,
        // A header name/value (or the request-line URI) carried a CR/LF/NUL/
        // control byte and was rejected before reaching the wire — a caller-side
        // protocol violation (mirrors the JVM/Python InvalidHeader → protocol
        // mapping).
        InvalidHeader { .. } => TstError::RtspProtocol,
        // Required by #[non_exhaustive] — future variants fall through to the
        // generic protocol-error bucket. CI ratchet catches any new variant
        // that was not explicitly mapped above.
        _ => TstError::RtspProtocol,
    }
}

/// Map a [`tst_rtp::MountError`] to the appropriate `TstError` variant.
/// Every variant collapses to `TstError::RtspMount`.
#[cfg(feature = "rtp")]
#[allow(dead_code)] // RTSP-feature-gated callers; unused in minimal builds
pub(crate) fn mount_error_to_code(e: &tst_rtp::MountError) -> TstError {
    use tst_rtp::MountError::*;
    match e {
        Mux(_) => TstError::RtspMount,
        Closed => TstError::RtspMount,
        // Required by #[non_exhaustive].
        _ => TstError::RtspMount,
    }
}

/// Map a [`tst_rtp::RtspServerError`] to the appropriate `TstError` variant.
/// Every variant collapses to `TstError::RtspServer`.
#[cfg(feature = "rtp")]
#[allow(dead_code)] // RTSP-feature-gated callers; unused in minimal builds
pub(crate) fn rtsp_server_error_to_code(e: &tst_rtp::RtspServerError) -> TstError {
    use tst_rtp::RtspServerError::*;
    match e {
        Io(_) => TstError::RtspServer,
        Tls(_) => TstError::RtspServer,
        UrlParse(_) => TstError::RtspServer,
        BindAddrInUse => TstError::RtspServer,
        InvalidMountPath { .. } => TstError::RtspServer,
        InvalidMulticastGroup { .. } => TstError::RtspServer,
        DuplicateMount { .. } => TstError::RtspServer,
        InvalidConfig { .. } => TstError::RtspServer,
        AlreadyStarted => TstError::RtspServer,
        NotStarted => TstError::RtspServer,
        Shutdown => TstError::RtspServer,
        // Required by #[non_exhaustive].
        _ => TstError::RtspServer,
    }
}

// ─────────────────────────────────────────────────────────────────────
// Plan A5a — per-protocol error converters. Each is an exhaustive
// `match e.kind() { ... }`; coverage is enforced by the TSV-driven rail
// at scripts/check/rust/rust-error-mapping-coverage.sh (data in
// scripts/ratchets/error-mapping.tsv, driver at
// scripts/ratchets/run-rust-coverage.sh).
// ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "udp")]
pub(crate) fn udp_error_to_code(e: &tst_udp::UdpError) -> TstError {
    use tst_udp::UdpErrorKind;
    // Exhaustive match — every UdpErrorKind variant maps to a single
    // TstError code. CI ratchet scripts/check/rust/rust-error-mapping-coverage.sh
    // enforces this completeness.
    match e.kind() {
        UdpErrorKind::Url => TstError::UdpConfig,
        UdpErrorKind::Io => TstError::UdpIo,
        UdpErrorKind::InvalidConfig => TstError::UdpConfig,
        // Required by #[non_exhaustive]. CI ratchet allows this arm only
        // when UdpErrorKind is non_exhaustive; verifies all 3 named
        // variants above are still explicit.
        _ => TstError::UdpIo,
    }
}

#[cfg(feature = "tcp")]
pub(crate) fn tcp_error_to_code(e: &tst_tcp::error::TcpError) -> TstError {
    use tst_tcp::error::TcpErrorKind;
    // Exhaustive match — every TcpErrorKind variant maps to a single
    // TstError code. CI ratchet scripts/check/rust/rust-error-mapping-coverage.sh
    // enforces this completeness.
    match e.kind() {
        TcpErrorKind::Url => TstError::TcpConfig,
        TcpErrorKind::Io => TstError::TcpIo,
        TcpErrorKind::Closed => TstError::Closed, // reuse global Closed = -7
        TcpErrorKind::ConnectTimeout => TstError::TcpConnectTimeout,
        TcpErrorKind::InvalidConfig => TstError::TcpConfig,
        TcpErrorKind::Tls => TstError::TcpTls,
        TcpErrorKind::TlsDisabled => TstError::TcpTls,
        // Required by #[non_exhaustive]. CI ratchet allows this arm only
        // when TcpErrorKind is non_exhaustive; verifies all 7 named
        // variants above are still explicit.
        _ => TstError::TcpIo,
    }
}

#[cfg(feature = "hls")]
pub(crate) fn hls_error_to_code(e: &tst_hls::HlsError) -> TstError {
    use tst_hls::HlsErrorKind;
    // Exhaustive match — every HlsErrorKind variant maps to a single TstError
    // code. CI ratchet scripts/check/rust/rust-error-mapping-coverage.sh enforces
    // this completeness.
    match e.kind() {
        HlsErrorKind::Url => TstError::HlsConfig,
        HlsErrorKind::Io => TstError::HlsIo,
        HlsErrorKind::BindFailed => TstError::HlsIo,
        HlsErrorKind::InvalidConfig => TstError::HlsConfig,
        HlsErrorKind::UnalignedPushTs => TstError::HlsConfig,
        HlsErrorKind::Finished => TstError::HlsFinished,
        HlsErrorKind::TlsDisabled => TstError::HlsTls,
        HlsErrorKind::Tls => TstError::HlsTls,
        HlsErrorKind::Internal => TstError::Internal, // reuse global Internal = -10
        // Required by #[non_exhaustive]. CI ratchet allows this arm only
        // when HlsErrorKind is non_exhaustive; verifies all 9 named
        // variants above are still explicit.
        _ => TstError::HlsIo,
    }
}

#[cfg(feature = "rist")]
pub(crate) fn rist_error_to_code(e: &tst_rist::RistError) -> TstError {
    use tst_rist::RistErrorKind;
    // Exhaustive match — every RistErrorKind variant maps to a single
    // TstError code. CI ratchet scripts/check/rust/rust-error-mapping-coverage.sh
    // enforces this completeness.
    match e.kind() {
        RistErrorKind::Url => TstError::RistConfig,
        RistErrorKind::Ffi => TstError::RistFfi,
        RistErrorKind::InvalidConfig => TstError::RistConfig,
        RistErrorKind::EncryptionDisabled => TstError::RistEncryptionDisabled,
        RistErrorKind::ContextCreateFailed => TstError::RistFfi,
        RistErrorKind::PeerCreateFailed => TstError::RistFfi,
        // Required by #[non_exhaustive]. CI ratchet allows this arm only
        // when RistErrorKind is non_exhaustive; verifies all 6 named
        // variants above are still explicit.
        _ => TstError::RistFfi,
    }
}

/// Expose `record_shell_error` to integration tests that cannot access
/// `pub(crate)` items. Integration tests in `bindings/c/tests/` are
/// separate crates that can only reach `pub` items on the rlib.
///
/// These functions are NOT `extern "C"` and therefore do NOT appear in the
/// cbindgen-generated C header (`tstrans.h`). They are only reachable from
/// Rust tests that link the rlib. Named with a `test_` prefix so call sites
/// are self-documenting about their test-only status.
pub fn test_record_shell_error<E: ShellError>(e: &E) -> i32 {
    record_shell_error(e)
}

/// Read the thread-local last-error code for test assertions. Equivalent to
/// `tst_get_last_error()` but callable without `unsafe`. Not `extern "C"`;
/// does not appear in the C header.
pub fn test_last_error_code() -> i32 {
    with_last_error(|slot| slot.0)
}

/// Read the thread-local last-error message string for test assertions. Not
/// `extern "C"`; does not appear in the C header.
pub fn test_last_error_msg() -> alloc::string::String {
    with_last_error(|slot| slot.1.to_str().unwrap_or("<invalid utf8>").to_owned())
}

/// Clear the thread-local last-error for test isolation. Not `extern "C"`;
/// does not appear in the C header.
pub fn test_clear_last_error() {
    with_last_error(|slot| *slot = (0, CString::new("").unwrap()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    use tst_pipeline::BrokenCause;

    #[test]
    fn set_then_get_roundtrips() {
        set_last_error(TstError::InvalidConfig, "bad pid");
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::InvalidConfig as i32
        );
        let s_ptr = unsafe { tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(s.to_str().unwrap(), "bad pid");
    }

    #[test]
    fn default_is_success_with_empty_string() {
        clear_last_error_for_test();
        assert_eq!(unsafe { tst_get_last_error() }, 0);
        let s_ptr = unsafe { tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(s.to_str().unwrap(), "");
    }

    #[test]
    fn tst_clear_last_error_resets_to_success_state() {
        // Defensive baseline: ensure we start from success state regardless
        // of any test ordering. set_last_error in Step 1 then primes the
        // specific non-success state we're testing the clear of.
        clear_last_error_for_test();

        // Step 1: prime the thread-local with a non-success error.
        set_last_error(TstError::InvalidConfig, "stale failure");
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::InvalidConfig as i32,
            "precondition: error should be set before clear"
        );
        let s_ptr = unsafe { tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(
            s.to_str().unwrap(),
            "stale failure",
            "precondition: message should be 'stale failure' before clear"
        );

        // Step 2: call the new public C entry under test.
        unsafe { tst_clear_last_error() };

        // Step 3: assert both code and message are reset.
        assert_eq!(
            unsafe { tst_get_last_error() },
            0,
            "after tst_clear_last_error(), code should be TST_E_SUCCESS (0)"
        );
        let s_ptr = unsafe { tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(
            s.to_str().unwrap(),
            "",
            "after tst_clear_last_error(), message should be empty"
        );
    }

    #[test]
    fn tst_clear_last_error_idempotent_when_already_clear() {
        // Reset baseline, then clear twice — must remain in success state.
        clear_last_error_for_test();
        assert_eq!(
            unsafe { tst_get_last_error() },
            0,
            "baseline: expected TST_E_SUCCESS (0) after clear_last_error_for_test()"
        );

        unsafe { tst_clear_last_error() };
        assert_eq!(unsafe { tst_get_last_error() }, 0);
        unsafe { tst_clear_last_error() };
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[test]
    fn ambiguous_target_message_points_to_to_siblings() {
        let e = MuxError::AmbiguousTarget {
            kind: StreamKind::Video,
            count: 2,
        };
        record_mux_error(&e);
        let s_ptr = unsafe { tst_get_last_error_str() };
        let msg = unsafe { core::ffi::CStr::from_ptr(s_ptr) }
            .to_str()
            .unwrap();
        // The message is the MuxError Display impl output which says
        // "call push_video_to(handle, ...) instead" — the key is that
        // it points to a disambiguation API, not the deferred path.
        assert!(msg.contains("push_video_to"), "got: {msg}");
        assert!(!msg.contains("deferred"), "got: {msg}");
    }

    #[test]
    fn end_of_stream_code_is_negative_twelve() {
        assert_eq!(TstError::EndOfStream as i32, -12);
    }

    #[test]
    fn not_available_code_is_negative_thirteen() {
        assert_eq!(TstError::NotAvailable as i32, -13);
    }

    #[test]
    fn end_of_stream_records_distinct_from_closed() {
        clear_last_error_for_test();
        super::record_eos();
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::EndOfStream as i32
        );
        let s_ptr = unsafe { tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert!(s.to_str().unwrap().contains("end of stream"));
    }

    /// Helper: read the thread-local last-error string and assert it does
    /// NOT begin with `"unhandled "`. That prefix is uniquely produced by
    /// the Debug-format wildcard arms in `record_*_error`; its presence
    /// means a known variant fell through to the wildcard. Belt-and-
    /// suspenders with the per-variant exact-code assertion.
    fn assert_not_unhandled_wildcard() {
        let s_ptr = unsafe { tst_get_last_error_str() };
        let msg = unsafe { core::ffi::CStr::from_ptr(s_ptr) }
            .to_str()
            .unwrap();
        assert!(
            !msg.starts_with("unhandled "),
            "wildcard arm fired for a known variant: {msg}"
        );
    }

    #[test]
    fn every_known_mux_error_variant_maps_to_expected_code() {
        use tst_core::mpegts::mux::{StreamKind, TeletextField};

        // (variant, expected TstError code). Cover every variant of MuxError.
        // Expected codes come from reading record_mux_error's explicit match
        // arms above.
        let cases: Vec<(MuxError, TstError)> = vec![
            (MuxError::InvalidConfig("test"), TstError::InvalidConfig),
            (
                MuxError::ConfigInvalid {
                    reason: "test".into(),
                },
                TstError::InvalidConfig,
            ),
            (MuxError::InvalidNal, TstError::InvalidNal),
            (MuxError::InvalidAv1Obu, TstError::InvalidAv1Obu),
            (
                MuxError::MispTime(
                    tst_core::codec::misp_time::MispTimeError::UnsupportedCodec {
                        codec: tst_core::mpegts::mux::VideoCodec::Av1,
                    },
                ),
                TstError::MispTime,
            ),
            (
                MuxError::BufferFull {
                    capacity_packets: 1,
                },
                TstError::BufferFull,
            ),
            (
                MuxError::KlvTooLarge { size: 100, max: 50 },
                TstError::KlvTooLarge,
            ),
            (
                MuxError::InvalidStreamHandle {
                    kind: StreamKind::Video,
                    index: 0,
                },
                TstError::InvalidUsage,
            ),
            (
                MuxError::AmbiguousTarget {
                    kind: StreamKind::Video,
                    count: 2,
                },
                TstError::InvalidUsage,
            ),
            (MuxError::NoKlvStreamsConfigured, TstError::InvalidUsage),
            (MuxError::NoAudioStreamsConfigured, TstError::InvalidUsage),
            (
                MuxError::NoSubtitleStreamsConfigured,
                TstError::InvalidUsage,
            ),
            (
                MuxError::TooManyVideoStreams { count: 17, cap: 16 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::TooManyKlvStreams { count: 17, cap: 16 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::TooManyAudioStreams { count: 17, cap: 16 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::PmtTooLarge {
                    used_bytes: 200,
                    max_bytes: 183,
                },
                TstError::InvalidConfig,
            ),
            (
                MuxError::MalformedDescriptor {
                    stream_index: 0,
                    descriptor_index: 0,
                    reason: "test",
                },
                TstError::InvalidConfig,
            ),
            (
                MuxError::TooManyPrograms { count: 17, cap: 16 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::EmptyProgram { program_number: 1 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::DuplicateProgramNumber { program_number: 1 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::DuplicatePmtPid {
                    pid: 0x100,
                    programs: [1, 2],
                },
                TstError::InvalidConfig,
            ),
            (
                MuxError::DuplicatePidAcrossPrograms {
                    pid: 0x100,
                    programs: [1, 2],
                },
                TstError::InvalidConfig,
            ),
            (
                MuxError::ProgramNotFound { program_number: 1 },
                TstError::InvalidUsage,
            ),
            (
                MuxError::PmtPidConflictsWithStream {
                    pmt_pid: 0x100,
                    program_number: 1,
                },
                TstError::InvalidConfig,
            ),
            (
                MuxError::AudioTooLarge { size: 100, max: 50 },
                TstError::InvalidUsage,
            ),
            (
                MuxError::TooManySubtitleStreams { count: 17, cap: 16 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::SubtitleTooLarge { size: 100, max: 50 },
                TstError::InvalidUsage,
            ),
            (
                MuxError::SubtitlePidUsedAsPcrPid { pid: 0x100 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::KlvPidUsedAsPcrPid { pid: 0x100 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::InvalidLanguageCode {
                    code: [b'X', b'X', b'X'],
                },
                TstError::InvalidConfig,
            ),
            (
                MuxError::InvalidTeletextField {
                    field: TeletextField::MagazineNumber,
                    value: 99,
                    max: 7,
                },
                TstError::InvalidConfig,
            ),
            (
                MuxError::NoPcrEligibleStream { program_number: 1 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::DescriptorIndexOutOfRange {
                    kind: StreamKind::Video,
                    index: 5,
                    program_number: 1,
                },
                TstError::InvalidUsage,
            ),
            (
                MuxError::AbsIndexOutOfRange {
                    abs_idx: 99,
                    len: 3,
                    program_number: 1,
                },
                TstError::InvalidUsage,
            ),
            (MuxError::NoDataStreamsConfigured, TstError::InvalidUsage),
            (
                MuxError::TooManyDataStreams { count: 17, cap: 16 },
                TstError::InvalidConfig,
            ),
            (
                MuxError::DataTooLarge { size: 100, max: 50 },
                TstError::InvalidUsage,
            ),
            (
                MuxError::DataPidUsedAsPcrPid { pid: 0x100 },
                TstError::InvalidConfig,
            ),
        ];

        for (case, expected) in cases {
            clear_last_error_for_test();
            record_mux_error(&case);
            let code = unsafe { tst_get_last_error() };
            assert_eq!(
                code, expected as i32,
                "MuxError variant mapped to wrong code: {case:?} -> got {code}, expected {}",
                expected as i32
            );
            assert_not_unhandled_wildcard();
        }
    }

    #[test]
    fn every_known_transport_error_variant_maps_to_expected_code() {
        let cases: Vec<(TransportError, TstError)> = vec![
            (
                TransportError::Backpressure {
                    msg: "test".into(),
                    errno_code: None,
                },
                TstError::Transport,
            ),
            (
                TransportError::Broken {
                    msg: "test".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                },
                TstError::Transport,
            ),
            (TransportError::Closed, TstError::Closed),
            (
                TransportError::TooLarge { len: 100, max: 50 },
                TstError::TooLarge,
            ),
        ];

        for (case, expected) in cases {
            clear_last_error_for_test();
            record_transport_error(&case);
            let code = unsafe { tst_get_last_error() };
            assert_eq!(
                code, expected as i32,
                "TransportError variant mapped to wrong code: {case:?} -> got {code}, expected {}",
                expected as i32
            );
            assert_not_unhandled_wildcard();
        }
    }

    #[test]
    fn record_not_available_sets_last_error_code() {
        test_clear_last_error();
        let rc = record_not_available("socket stats unavailable (reconnecting)");
        assert_eq!(rc, TstError::NotAvailable as i32);
        assert_eq!(test_last_error_code(), TstError::NotAvailable as i32);
    }

    #[test]
    fn record_not_available_overwrites_prior_error() {
        test_clear_last_error();
        // Seed a stale unrelated error (simulating a prior failing call).
        set_last_error(TstError::InvalidConfig, "stale config error");
        assert_eq!(test_last_error_code(), TstError::InvalidConfig as i32);

        // record_not_available must overwrite both code AND message.
        let _ = record_not_available("socket stats unavailable");
        assert_eq!(test_last_error_code(), TstError::NotAvailable as i32);
        assert!(
            test_last_error_msg().contains("socket stats unavailable"),
            "last-error message did not overwrite; got: {:?}",
            test_last_error_msg()
        );
    }

    #[test]
    fn record_not_found_sets_last_error_code() {
        test_clear_last_error();
        let rc = record_not_found("pid 0x100 not observed on this handle");
        assert_eq!(rc, TstError::NotFound as i32);
        assert_eq!(test_last_error_code(), TstError::NotFound as i32);
    }

    /// D5 follow-up: `(errno N)` suffix only appended for non-zero
    /// codes. `errno_code: Some(0)` (the `SrtErrno::Bad` sentinel) is
    /// suppressed because "(errno 0)" reads as "no error" — a footgun
    /// for C consumers parsing the message string.
    #[test]
    fn record_transport_error_suppresses_errno_zero_suffix() {
        clear_last_error_for_test();
        record_transport_error(&TransportError::Broken {
            msg: "synthetic".into(),
            errno_code: Some(0),
            cause: BrokenCause::Unspecified,
        });
        let s_ptr = unsafe { tst_get_last_error_str() };
        let msg = unsafe { core::ffi::CStr::from_ptr(s_ptr) }
            .to_str()
            .unwrap();
        assert!(
            !msg.contains("(errno"),
            "errno 0 should not surface a (errno N) suffix: {msg}"
        );

        // Non-zero codes still surface.
        clear_last_error_for_test();
        record_transport_error(&TransportError::Broken {
            msg: "synthetic".into(),
            errno_code: Some(2),
            cause: BrokenCause::Unspecified,
        });
        let s_ptr = unsafe { tst_get_last_error_str() };
        let msg = unsafe { core::ffi::CStr::from_ptr(s_ptr) }
            .to_str()
            .unwrap();
        assert!(msg.contains("(errno 2)"), "got: {msg}");
    }

    #[test]
    fn record_not_found_overwrites_prior_error() {
        test_clear_last_error();
        set_last_error(TstError::InvalidUsage, "stale usage error");
        assert_eq!(test_last_error_code(), TstError::InvalidUsage as i32);

        let _ = record_not_found("pid not observed");
        assert_eq!(test_last_error_code(), TstError::NotFound as i32);
        assert!(
            test_last_error_msg().contains("pid not observed"),
            "last-error message did not overwrite; got: {:?}",
            test_last_error_msg()
        );
    }
}
