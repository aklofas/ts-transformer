//! `TstStreamEndReason` — the shared C stream-end-reason enum, and the one
//! converter from the Rust enum (`tst_core::transport::EndReason`) to it.
//!
//! Defined here, outside any feature-gated module, so cbindgen emits the
//! enum **unconditionally** — same reasoning as the sibling
//! `TstReconnectMode` in `config::builders`: the only feature-specific
//! pieces are the getters that read it (`tst_rtp_receiver_end_reason` /
//! `tst_rtp_demux_receiver_end_reason`, both `TST_HAS_RTP`-gated in
//! `crate::rtp`; `tst_managed_demux_receiver_end_reason`, `TST_HAS_SRT`-gated
//! in `crate::receiver`), not the enum shape itself. Keeping the enum's
//! own definition inside a feature-gated module would make cbindgen wrap
//! every variant in its own `#if defined(...)` — the doubled-guard shape
//! the project deliberately avoids (see
//! `crate::config::builders::TstReconnectMode` for the same pattern).
//!
//! Every getter reads a recorded `EndReason` and converts it with
//! `convert_end_reason`, so the mapping and the last-error contract live
//! in one place.

#[cfg(any(feature = "srt", feature = "rtp"))]
use tst_core::transport::EndReason;

#[cfg(any(feature = "srt", feature = "rtp"))]
use crate::error::{TstError, set_last_error};

/// Why a receive session ended. One shared enum, reused by every
/// transport's own end-reason getter — see the getters' own docs
/// (`tst_rtp_receiver_end_reason` / `tst_rtp_demux_receiver_end_reason` /
/// `tst_managed_demux_receiver_end_reason`) for which reasons each
/// transport can actually produce. `None` (0) — "hasn't ended yet, or
/// ended through an uninstrumented path" — is common to all
/// of them (the case each side's own end-reason handle reports as
/// `Option::None`, e.g. a plain `rtp://` receiver that was never
/// `_cancel`'d or `_close`'d). Some variants are transport-specific: the
/// RTSP-shaped `SessionExpired` / `KeepaliveFailed` / `ProtocolError`
/// only ever surface from the RTP/RTSP side — the SRT managed receiver
/// never produces them.
/// Discriminants 1-6 are cross-surface stable — the Python and JVM
/// bindings use the same numbering.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstStreamEndReason {
    /// The session hasn't ended yet, or ended through an uninstrumented
    /// path.
    None = 0,
    /// The peer closed the connection in an orderly way, with no
    /// protocol or transport error.
    CleanTeardown = 1,
    /// The server no longer honors the session — a keepalive ping was
    /// answered `454 Session Not Found`.
    SessionExpired = 2,
    /// The keepalive background thread failed to encode or send a ping.
    /// Detail message: see the getter doc.
    KeepaliveFailed = 3,
    /// A hard I/O error on the underlying transport. Detail message:
    /// see the getter doc.
    TransportFailed = 4,
    /// The peer violated the wire protocol. Detail message: see the
    /// getter doc.
    ProtocolError = 5,
    /// The caller explicitly cancelled or closed the transport — not a
    /// wire-level failure.
    Cancelled = 6,
}

/// Convert a recorded [`EndReason`] to its C discriminant.
///
/// Only called by the `_end_reason` getters when they already hold a `Some`
/// from `EndReasonHandle::get()` — i.e. every arm here corresponds to an
/// ACTUALLY-RECORDED reason, never the "hasn't ended yet" case (that
/// short-circuits to `TstStreamEndReason::None` at the call site without
/// reaching this function at all — see the getters' doc for why that split
/// matters to the last-error contract).
///
/// Every arm therefore unconditionally writes the thread-local
/// last-error message channel — [`set_last_error`] with
/// [`TstError::Success`] (a recorded end reason is data, not a getter
/// failure, so `tst_get_last_error() == 0` still holds for callers using
/// that as a "did the last call succeed" check). `KeepaliveFailed` /
/// `TransportFailed` / `ProtocolError` write their `msg` detail; every
/// other arm (including the non-exhaustive fallback) writes an
/// EMPTY message — so `tst_get_last_error_str()` always reflects THIS call
/// once a reason has been recorded, never a stale message left over
/// from some earlier, unrelated failure.
///
/// `EndOfStream` maps to `CleanTeardown` and `ReconnectExhausted` to
/// `TransportFailed`. `EndReason` is `#[non_exhaustive]`; a future variant
/// this binding doesn't know how to map yet degrades to `None` with an
/// empty detail rather than panicking.
#[cfg(any(feature = "srt", feature = "rtp"))]
pub(crate) fn convert_end_reason(r: &EndReason) -> TstStreamEndReason {
    let (converted, msg): (TstStreamEndReason, &str) = match r {
        EndReason::EndOfStream => (TstStreamEndReason::CleanTeardown, ""),
        EndReason::ReconnectExhausted => (TstStreamEndReason::TransportFailed, ""),
        EndReason::CleanTeardown => (TstStreamEndReason::CleanTeardown, ""),
        EndReason::SessionExpired => (TstStreamEndReason::SessionExpired, ""),
        EndReason::KeepaliveFailed { msg } => (TstStreamEndReason::KeepaliveFailed, msg.as_str()),
        EndReason::TransportFailed { msg } => (TstStreamEndReason::TransportFailed, msg.as_str()),
        EndReason::ProtocolError { msg } => (TstStreamEndReason::ProtocolError, msg.as_str()),
        EndReason::Cancelled => (TstStreamEndReason::Cancelled, ""),
        _ => (TstStreamEndReason::None, ""),
    };
    set_last_error(TstError::Success, msg);
    converted
}

#[cfg(all(test, any(feature = "srt", feature = "rtp")))]
mod tests {
    use super::*;

    /// Every variant maps to its C discriminant, and a recorded reason
    /// always resets last-error to `(Success, detail)` — pinned against the
    /// converter, independent of live-socket setup.
    #[test]
    fn every_variant_maps_and_sets_the_last_error_detail() {
        for (reason, expected, detail) in [
            (
                EndReason::EndOfStream,
                TstStreamEndReason::CleanTeardown,
                "",
            ),
            (
                EndReason::ReconnectExhausted,
                TstStreamEndReason::TransportFailed,
                "",
            ),
            (EndReason::Cancelled, TstStreamEndReason::Cancelled, ""),
            (
                EndReason::CleanTeardown,
                TstStreamEndReason::CleanTeardown,
                "",
            ),
            (
                EndReason::SessionExpired,
                TstStreamEndReason::SessionExpired,
                "",
            ),
            (
                EndReason::KeepaliveFailed { msg: "ping".into() },
                TstStreamEndReason::KeepaliveFailed,
                "ping",
            ),
            (
                EndReason::TransportFailed {
                    msg: "connection reset".into(),
                },
                TstStreamEndReason::TransportFailed,
                "connection reset",
            ),
            (
                EndReason::ProtocolError {
                    msg: "flood".into(),
                },
                TstStreamEndReason::ProtocolError,
                "flood",
            ),
        ] {
            // Seed a stale message so a missing reset is visible.
            set_last_error(TstError::Internal, "stale");
            let converted = convert_end_reason(&reason);
            // TstStreamEndReason has no Debug impl; compare discriminants.
            assert_eq!(converted as i32, expected as i32, "mapping for {reason:?}");
            assert_eq!(
                crate::error::test_last_error_code(),
                TstError::Success as i32
            );
            assert_eq!(crate::error::test_last_error_msg(), detail);
        }
    }
}
