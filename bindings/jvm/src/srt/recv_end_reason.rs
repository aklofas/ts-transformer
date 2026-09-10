//! `tst_pipeline::RecvEndReason` → JVM ordinal-coded conversion for
//! `org.tstrans.srt.ManagedDemuxReceiver`.
//!
//! # Why a dedicated enum, not the rtp one
//!
//! The C ABI has no `TstRecvEndReason`: it folds these three variants into the
//! RTSP-shaped `TstStreamEndReason` (`EndOfStream` → `CleanTeardown`,
//! `ReconnectExhausted` → `TransportFailed`, `Cancelled` → `Cancelled`), which
//! is lossy — `TransportFailed` cannot tell a caller that the *reconnect budget*
//! is what ran out. The JVM surface therefore mirrors `RecvEndReason` exactly,
//! as its own `org.tstrans.srt.RecvEndReason`, rather than reusing
//! `org.tstrans.rtp.StreamEndReason`. The two enums live in different packages
//! and are never interchanged.
//!
//! # Wire values
//!
//! `0`/`1`/`2` in `tst-pipeline` declaration order (`EndOfStream`,
//! `ReconnectExhausted`, `Cancelled`), with `-1` for "no reason recorded". These
//! are JVM-boundary values between these natives and
//! `RecvEndReason.fromWireOrdinal`, NOT cross-surface-pinned constants like the
//! rtp/C/Python `StreamEndReason` set — nothing else speaks them. Note `0` is a
//! REAL variant here, so `-1` (never `0`) is the "nothing recorded" sentinel.
//! The Java side switches on these values explicitly, so reordering either enum
//! can never silently re-map a variant.
//!
//! # No detail string, and therefore no snapshot carrier
//!
//! Every `RecvEndReason` variant is a unit variant — unlike `tst_rtp::
//! StreamEndReason`, none carries a `msg`. So there is no `endDetail()` twin,
//! and `nClose` needs no `EndReasonSnapshot`-style object: it returns the bare
//! ordinal. That is the same close-time-snapshot mechanism the rtp receivers use
//! (compute it inside `nClose`, from the resource that call exclusively owns,
//! because the registry entry is gone the moment it returns) with a one-value
//! payload instead of two.

use jni::sys::jint;

use tst_pipeline::RecvEndReason;

/// The wire ordinal `nEndReason` / `nClose` return. `-1` for `None` (the stream
/// has not ended, or ended through a path `tst-pipeline` does not instrument)
/// and for any future variant this binding does not map yet — `RecvEndReason` is
/// `#[non_exhaustive]`.
pub(crate) fn recv_end_reason_ordinal(r: Option<RecvEndReason>) -> jint {
    match r {
        Some(RecvEndReason::EndOfStream) => 0,
        Some(RecvEndReason::ReconnectExhausted) => 1,
        Some(RecvEndReason::Cancelled) => 2,
        Some(_) | None => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the ordinals `org.tstrans.srt.RecvEndReason.fromWireOrdinal`
    /// switches on. Changing either side alone silently re-maps a variant.
    #[test]
    fn ordinals_match_tst_pipeline_declaration_order() {
        assert_eq!(recv_end_reason_ordinal(Some(RecvEndReason::EndOfStream)), 0);
        assert_eq!(
            recv_end_reason_ordinal(Some(RecvEndReason::ReconnectExhausted)),
            1
        );
        assert_eq!(recv_end_reason_ordinal(Some(RecvEndReason::Cancelled)), 2);
    }

    /// `-1`, not `0`, is the "nothing recorded" sentinel — `0` is `EndOfStream`.
    #[test]
    fn none_is_minus_one_not_zero() {
        assert_eq!(recv_end_reason_ordinal(None), -1);
    }
}
