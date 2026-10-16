//! `tst_core::transport::EndReason` → the wire values
//! `org.tstrans.shells.EndReason.fromWireValue` switches on.
//!
//! The values are cross-surface pinned (the C `TstStreamEndReason`
//! discriminants and Python's `EndReason` `IntEnum`): CLEAN_TEARDOWN=1,
//! SESSION_EXPIRED=2, KEEPALIVE_FAILED=3, TRANSPORT_FAILED=4,
//! PROTOCOL_ERROR=5, CANCELLED=6, END_OF_STREAM=7, RECONNECT_EXHAUSTED=8.
//! `-1` is "has not ended, or ended through a path this type does not
//! instrument", which Java maps to `null`.
//!
//! A recorded end reason is data, not a failure: the free-text detail is read
//! off the `msg` the `KeepaliveFailed` / `TransportFailed` / `ProtocolError`
//! variants carry, never through an exception.
//!
//! # Why `nClose` returns the reason
//!
//! A JVM handle is zeroed before `nClose` runs and the registry key is gone
//! once it returns, so there is nothing left to pass to a later
//! `nEndReason`. `nClose` therefore reads the reason itself, after the shell's
//! teardown recorded `Cancelled`, and returns it as an `EndReason$Snapshot`
//! the Java class caches.

use jni::JNIEnv;
use jni::objects::{JObject, JValue};
use jni::sys::jint;
use tst_core::transport::EndReason;

/// The wire value for `r`; `-1` for `None` and for a future `#[non_exhaustive]`
/// variant this binding does not map yet.
pub(crate) fn wire_value(r: Option<&EndReason>) -> jint {
    match r {
        Some(EndReason::CleanTeardown) => 1,
        Some(EndReason::SessionExpired) => 2,
        Some(EndReason::KeepaliveFailed { .. }) => 3,
        Some(EndReason::TransportFailed { .. }) => 4,
        Some(EndReason::ProtocolError { .. }) => 5,
        Some(EndReason::Cancelled) => 6,
        Some(EndReason::EndOfStream) => 7,
        Some(EndReason::ReconnectExhausted) => 8,
        Some(_) | None => -1,
    }
}

/// The free-text `msg` of the three variants that carry one.
pub(crate) fn detail(r: &EndReason) -> Option<&str> {
    match r {
        EndReason::KeepaliveFailed { msg }
        | EndReason::TransportFailed { msg }
        | EndReason::ProtocolError { msg } => Some(msg.as_str()),
        _ => None,
    }
}

/// Build the package-private `EndReason$Snapshot` `nClose` returns.
pub(crate) fn build_close_snapshot<'local>(
    env: &mut JNIEnv<'local>,
    reason: Option<EndReason>,
) -> jni::errors::Result<JObject<'local>> {
    let wire = wire_value(reason.as_ref());
    let detail_obj: JObject = match reason.as_ref().and_then(detail) {
        Some(d) => env.new_string(d)?.into(),
        None => JObject::null(),
    };
    env.new_object(
        "org/tstrans/shells/EndReason$Snapshot",
        "(ILjava/lang/String;)V",
        &[JValue::Int(wire), JValue::Object(&detail_obj)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the numbers `EndReason.fromWireValue` switches on; reordering
    /// either side alone silently re-maps a variant.
    #[test]
    fn wire_values_match_the_cross_surface_numbering() {
        assert_eq!(wire_value(Some(&EndReason::CleanTeardown)), 1);
        assert_eq!(wire_value(Some(&EndReason::SessionExpired)), 2);
        assert_eq!(
            wire_value(Some(&EndReason::KeepaliveFailed { msg: String::new() })),
            3
        );
        assert_eq!(
            wire_value(Some(&EndReason::TransportFailed { msg: String::new() })),
            4
        );
        assert_eq!(
            wire_value(Some(&EndReason::ProtocolError { msg: String::new() })),
            5
        );
        assert_eq!(wire_value(Some(&EndReason::Cancelled)), 6);
        assert_eq!(wire_value(Some(&EndReason::EndOfStream)), 7);
        assert_eq!(wire_value(Some(&EndReason::ReconnectExhausted)), 8);
        assert_eq!(wire_value(None), -1);
    }

    #[test]
    fn only_three_variants_carry_a_detail() {
        assert_eq!(
            detail(&EndReason::TransportFailed { msg: "x".into() }),
            Some("x")
        );
        assert_eq!(detail(&EndReason::Cancelled), None);
    }
}
