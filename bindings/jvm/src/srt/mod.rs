//! `org.tstrans.srt` — SRT transport JNI surface.

pub(crate) mod errors;
mod listen_rules;
mod lowlevel;
mod managed_basic;
mod managed_convenience;

use std::time::Duration;

use jni::Env;
use tst_pipeline::binding::{BindingError, BindingErrorKind};
use tst_pipeline::{BackoffStrategy, OverflowPolicy, ReconnectMode, ReconnectPolicy};

use crate::error::{Domain, throw_binding};

/// Construction-constant view of a MANAGED sender (`ManagedSender` /
/// `ManagedMuxSender`): the reconnect/gap telemetry observer. It lives outside
/// the slot, so `reconnectStats()` / `reconnectAttempts()` answer while a
/// `send*` is parked in a Blocking reconnect.
pub(crate) struct ManagedSenderSnapshot {
    pub stats: tst_pipeline::ManagedStatsHandle,
}

/// What a managed open reports when its `OpenCancel` was cancelled by the
/// time it finished: `SrtException(CLOSED)` through the usual `SrtError` path.
pub(crate) fn closed_srt() -> tst_srt::SrtError {
    tst_srt::SrtError::Transport(tst_core::transport::TransportError::ExplicitClose)
}

/// Why a managed open failed. The checks it makes after taking its
/// `OpenCancel` slot (the URL, the mode, the policy) fail as `Refused`; the
/// open itself as `Open`. Both reach `finish`, so a cancel that lands during
/// the checks is reported as `CLOSED` like one that lands during the open.
pub(crate) enum ManagedOpenError {
    Refused(BindingError),
    Open(tst_srt::SrtError),
}

impl From<tst_srt::SrtError> for ManagedOpenError {
    fn from(e: tst_srt::SrtError) -> Self {
        Self::Open(e)
    }
}

impl ManagedOpenError {
    /// `CONFIG_INVALID` with `message`.
    pub(crate) fn config(message: String) -> Self {
        Self::Refused(BindingError::new(BindingErrorKind::ConfigInvalid, message))
    }

    /// What `finish` reports for a cancelled object.
    pub(crate) fn closed() -> Self {
        Self::Open(closed_srt())
    }

    /// Throw it as an `SrtException`.
    pub(crate) fn throw(self, env: &mut Env) {
        match self {
            Self::Refused(e) => throw_binding(env, Domain::Srt, &e),
            Self::Open(e) => errors::srt_error(env, e),
        }
    }
}

/// Reconstruct a `tst_pipeline::ReconnectPolicy` from the primitive args the
/// JVM `Managed*.nFromUrl` natives marshal (see `org.tstrans.srt.PolicyArgs`).
/// `backoff_kind`: 0 = Constant, 1 = Exponential — `CONFIG_INVALID` on
/// any other value. `overflow_policy`: 0 = DropOldest, 1 = Reject —
/// `CONFIG_INVALID` on any other value. `mode`: 0 = Blocking, 1 = Background —
/// an out-of-range ordinal defensively degrades to Blocking rather than
/// throwing (mirrors `ReconnectMode`'s non-exhaustive-enum + default-Blocking
/// arm on the Rust side, so a JAR built against a future variant still loads
/// against an older native). `max_attempts_present == false` → retry forever.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_reconnect_policy(
    max_attempts_present: bool,
    max_attempts: i32,
    backoff_kind: i32,
    backoff_base_ms: i64,
    backoff_max_ms: i64,
    gap_buffer_capacity: i32,
    overflow_policy: i32,
    mode: i32,
) -> Result<ReconnectPolicy, ManagedOpenError> {
    let backoff = match backoff_kind {
        0 => BackoffStrategy::Constant(Duration::from_millis(backoff_base_ms.max(0) as u64)),
        1 => BackoffStrategy::Exponential {
            base: Duration::from_millis(backoff_base_ms.max(0) as u64),
            max: Duration::from_millis(backoff_max_ms.max(0) as u64),
        },
        other => {
            return Err(ManagedOpenError::config(format!(
                "unknown BackoffStrategy ordinal {other}"
            )));
        }
    };
    // Order pinned by OrdinalContractTest.
    let overflow = match overflow_policy {
        0 => OverflowPolicy::DropOldest,
        1 => OverflowPolicy::Reject,
        other => {
            return Err(ManagedOpenError::config(format!(
                "unknown OverflowPolicy ordinal {other}"
            )));
        }
    };
    // Defensive default (not a throw, unlike backoff_kind/overflow_policy above):
    // an unrecognized ordinal falls back to Blocking rather than rejecting the
    // call, matching ReconnectMode's own #[default] Blocking arm.
    // Order pinned by OrdinalContractTest.
    let recon_mode = match mode {
        1 => ReconnectMode::Background,
        _ => ReconnectMode::Blocking,
    };
    Ok(ReconnectPolicy::default()
        .with_max_attempts(if max_attempts_present {
            Some(max_attempts.max(0) as u32)
        } else {
            None
        })
        .with_backoff(backoff)
        .with_gap_buffer_capacity(gap_buffer_capacity.max(1) as usize)
        .with_overflow_policy(overflow)
        .with_mode(recon_mode))
}
