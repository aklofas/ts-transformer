//! `org.tstrans.srt` — SRT transport JNI surface.

pub(crate) mod errors;
mod lowlevel;
mod managed_basic;
mod managed_convenience;

use std::time::Duration;

use jni::Env;
use tst_pipeline::binding::ManagedHandles;
use tst_pipeline::{BackoffStrategy, OverflowPolicy, ReconnectMode, ReconnectPolicy};

/// Construction-constant view of a MANAGED sender (`ManagedSender` /
/// `ManagedMuxSender`): the handles `tst_srt::shells` composed at open plus the
/// reconnect/gap telemetry observer. Both live outside the slot, so
/// `reconnectStats()` / `reconnectAttempts()` answer while a `send*` is parked
/// in a Blocking reconnect.
pub(crate) struct ManagedSenderSnapshot {
    /// The open-time handles. `cancel` is consumed by `Owned::new` at
    /// registration; the counters are kept because the send side reports
    /// attempts from [`Self::stats`] (`ManagedTransportStats::reconnect_attempts`)
    /// rather than from `ManagedHandles::attempts`, which the recv side
    /// maintains separately. Held so a binding-side `reconnecting()` getter
    /// has the same snapshot to read as the receivers do.
    #[expect(
        dead_code,
        reason = "send side reads attempts from `stats`; kept for the binding-side `reconnecting()` getter and cancel-handle use"
    )]
    pub handles: ManagedHandles,
    pub stats: tst_pipeline::ManagedStatsHandle,
}

/// Reconstruct a `tst_pipeline::ReconnectPolicy` from the primitive args the
/// JVM `Managed*.nFromUrl` natives marshal (see `org.tstrans.srt.PolicyArgs`).
/// `backoff_kind`: 0 = Constant, 1 = Exponential — throws `CONFIG_INVALID` on
/// any other value. `overflow_policy`: 0 = DropOldest, 1 = Reject — throws
/// `CONFIG_INVALID` on any other value. `mode`: 0 = Blocking, 1 = Background —
/// an out-of-range ordinal defensively degrades to Blocking rather than
/// throwing (mirrors `ReconnectMode`'s non-exhaustive-enum + default-Blocking
/// arm on the Rust side, so a JAR built against a future variant still loads
/// against an older native). `max_attempts_present == false` → retry forever.
///
/// Returns `None` (with a pending `SrtException`) on an invalid ordinal;
/// callers must propagate the `None` as a `return 0` early-exit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_reconnect_policy(
    env: &mut Env,
    max_attempts_present: bool,
    max_attempts: i32,
    backoff_kind: i32,
    backoff_base_ms: i64,
    backoff_max_ms: i64,
    gap_buffer_capacity: i32,
    overflow_policy: i32,
    mode: i32,
) -> Option<ReconnectPolicy> {
    let backoff = match backoff_kind {
        0 => BackoffStrategy::Constant(Duration::from_millis(backoff_base_ms.max(0) as u64)),
        1 => BackoffStrategy::Exponential {
            base: Duration::from_millis(backoff_base_ms.max(0) as u64),
            max: Duration::from_millis(backoff_max_ms.max(0) as u64),
        },
        other => {
            errors::throw_srt(
                env,
                tst_pipeline::binding::BindingErrorKind::ConfigInvalid,
                &format!("unknown BackoffStrategy ordinal {other}"),
            );
            return None;
        }
    };
    let overflow = match overflow_policy {
        0 => OverflowPolicy::DropOldest,
        1 => OverflowPolicy::Reject,
        other => {
            errors::throw_srt(
                env,
                tst_pipeline::binding::BindingErrorKind::ConfigInvalid,
                &format!("unknown OverflowPolicy ordinal {other}"),
            );
            return None;
        }
    };
    // Defensive default (not a throw, unlike backoff_kind/overflow_policy above):
    // an unrecognized ordinal falls back to Blocking rather than rejecting the
    // call, matching ReconnectMode's own #[default] Blocking arm.
    let recon_mode = match mode {
        1 => ReconnectMode::Background,
        _ => ReconnectMode::Blocking,
    };
    Some(ReconnectPolicy {
        max_attempts: if max_attempts_present {
            Some(max_attempts.max(0) as u32)
        } else {
            None
        },
        backoff,
        gap_buffer_capacity: gap_buffer_capacity.max(1) as usize,
        overflow_policy: overflow,
        mode: recon_mode,
    })
}
