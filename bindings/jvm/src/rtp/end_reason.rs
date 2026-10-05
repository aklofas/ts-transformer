//! `tst_rtp::StreamEndReason` → JVM ordinal-coded conversion, shared by
//! `transport::JniRtpReceiver`, `demux_receiver::JniRtpDemuxReceiver`, and
//! `h264_receiver::JniH264Receiver`.
//!
//! Wire values are cross-surface pinned (matching the C `TstStreamEndReason`
//! discriminants and the Python `StreamEndReason` IntEnum): CLEAN_TEARDOWN=1,
//! SESSION_EXPIRED=2, KEEPALIVE_FAILED=3, TRANSPORT_FAILED=4,
//! PROTOCOL_ERROR=5, CANCELLED=6. `-1` is the "hasn't ended yet, or ended
//! through a path this type doesn't instrument" sentinel `nEndReason` (and the
//! ordinal slot of `nClose`'s snapshot — see below) returns; the Java side's
//! `StreamEndReason.fromWireOrdinal` maps that (and any other unrecognized
//! value) to `null`.
//!
//! ★CROSS-SURFACE RULING (from the Python PR-B `end_reason.rs` this ports):
//! the detail string reads a `KeepaliveFailed`/`TransportFailed`/
//! `ProtocolError` variant's `msg` field DIRECTLY off the Rust enum — NOT
//! through the C ABI's thread-local last-error channel (that's a C-only
//! pattern). A recorded end reason is data, not a failure.
//!
//! # The `nClose` snapshot problem
//!
//! Unlike Python (where the wrapper's PyO3 object — and any
//! `StreamEndReasonHandle`/cancel handle field on it — outlives `inner`
//! being set to `None`) and the C ABI (whose caller-owned struct is simply
//! never freed until the caller calls the `_free` function), a JVM
//! `NativeHandle.close()` atomically zeroes the Java-side handle field
//! BEFORE invoking `nativeClose`, and the leased-handle registry's `close`
//! permanently removes the entry from its table (monotonic ids are never
//! reused — see `crate::handle`). So once `nClose` returns, there is no
//! handle left to pass to a follow-up `nEndReason`/`nEndDetail` call, and no
//! way to read one WITHOUT racing a concurrent producer (e.g. the RTSP
//! keepalive thread) if the read happened before `close()`'s own
//! `Cancelled` write.
//!
//! The fix: `nClose` computes the post-close snapshot itself, from the
//! resource it already exclusively owns (taken out of the registry, no
//! longer shared — see each `nClose` body), and returns it in the SAME JNI
//! call via [`build_close_snapshot`], as a Java `org.tstrans.rtp.
//! EndReasonSnapshot` (package-private carrier, never part of the public
//! surface). `Receiver`/`DemuxReceiver`/`H264Receiver.nativeClose` unpack it
//! into two cached Java fields; `endReason()`/`endDetail()` read those when
//! the handle is 0 (closed). `nativeClose` null-checks the snapshot (a JNI
//! allocation failure building it — extremely unlikely, but `build_close_
//! snapshot` can fail) before touching it, matching `nClose`'s own
//! `unwrap_or(null_mut())` fallback on the Rust side.

use jni::JNIEnv;
use jni::objects::{JObject, JValue};
use jni::sys::jint;

use tst_rtp::StreamEndReason;

/// Convert an `Option<&StreamEndReason>` to the wire-pinned ordinal
/// `nEndReason` / `nClose`'s snapshot returns: `-1` for `None` (not ended)
/// or a future variant this binding doesn't map yet — `StreamEndReason` is
/// non-exhaustive on the tst-rtp side.
pub(crate) fn end_reason_ordinal(r: Option<&StreamEndReason>) -> jint {
    match r {
        Some(StreamEndReason::CleanTeardown) => 1,
        Some(StreamEndReason::SessionExpired) => 2,
        Some(StreamEndReason::KeepaliveFailed { .. }) => 3,
        Some(StreamEndReason::TransportFailed { .. }) => 4,
        Some(StreamEndReason::ProtocolError { .. }) => 5,
        Some(StreamEndReason::Cancelled) => 6,
        Some(_) | None => -1,
    }
}

/// The free-text `msg` carried by `KeepaliveFailed` / `TransportFailed` /
/// `ProtocolError`; `None` for the three detail-less variants and any
/// future non-exhaustive variant.
pub(crate) fn end_reason_detail(r: &StreamEndReason) -> Option<&str> {
    match r {
        StreamEndReason::KeepaliveFailed { msg }
        | StreamEndReason::TransportFailed { msg }
        | StreamEndReason::ProtocolError { msg } => Some(msg.as_str()),
        _ => None,
    }
}

/// Resolve the `org.tstrans.rtp.StreamEndReason` enum constant matching the
/// wire ordinal — `null` for `-1` or any value this binding doesn't
/// recognize. Same by-name `GetStaticField` lookup idiom
/// `mpegts::mod::enum_const` uses for `NonConformantKind` et al. (not reused
/// directly — that helper is hardcoded to the `org.tstrans.mpegts` package).
fn end_reason_java_enum<'local>(
    env: &mut JNIEnv<'local>,
    ord: jint,
) -> jni::errors::Result<JObject<'local>> {
    let name = match ord {
        1 => "CLEAN_TEARDOWN",
        2 => "SESSION_EXPIRED",
        3 => "KEEPALIVE_FAILED",
        4 => "TRANSPORT_FAILED",
        5 => "PROTOCOL_ERROR",
        6 => "CANCELLED",
        _ => return Ok(JObject::null()),
    };
    env.get_static_field(
        "org/tstrans/rtp/StreamEndReason",
        name,
        "Lorg/tstrans/rtp/StreamEndReason;",
    )?
    .l()
}

/// Build the `org.tstrans.rtp.EndReasonSnapshot` `nClose` returns: the
/// resolved `StreamEndReason` enum constant (nullable) plus the nullable
/// detail `String`. See the module doc for why `nClose` — not the getters —
/// is where this is computed.
pub(crate) fn build_close_snapshot<'local>(
    env: &mut JNIEnv<'local>,
    reason: Option<StreamEndReason>,
) -> jni::errors::Result<JObject<'local>> {
    let ord = end_reason_ordinal(reason.as_ref());
    let detail = reason.as_ref().and_then(|r| end_reason_detail(r));

    let reason_obj = end_reason_java_enum(env, ord)?;
    let detail_obj: JObject = match detail {
        Some(d) => env.new_string(d)?.into(),
        None => JObject::null(),
    };
    env.new_object(
        "org/tstrans/rtp/EndReasonSnapshot",
        "(Lorg/tstrans/rtp/StreamEndReason;Ljava/lang/String;)V",
        &[JValue::Object(&reason_obj), JValue::Object(&detail_obj)],
    )
}
