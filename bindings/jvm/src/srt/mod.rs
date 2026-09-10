//! `org.tstrans.srt` — SRT transport JNI surface.

mod demux_receiver;
pub(crate) mod errors;
mod lowlevel;
mod managed_basic;
mod managed_convenience;
mod mux_sender;
mod recv_end_reason;
mod stats;
mod transport;

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jlong};
use tst_core::transport::TransportCancel;
use tst_pipeline::{BackoffStrategy, OverflowPolicy, ReconnectMode, ReconnectPolicy};

use crate::handle::{HandleRegistry, TryWith};

/// Per-type leased-handle registry for `org.tstrans.srt.CancelHandle`. The
/// cancel-handle types are themselves cancel *targets* (no parked op to wake),
/// so they register with `insert` (cancel = None); the registry kills their own
/// UAF/double-free on `close`.
static REGISTRY_CANCEL: LazyLock<HandleRegistry<JniCancel>> = LazyLock::new(HandleRegistry::new);

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
    env: &mut JNIEnv,
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
                "CONFIG_INVALID",
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
                "CONFIG_INVALID",
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

/// Boxed behind a `CancelHandle.handle`. Mirrors tst-py's `PyCancelHandle`:
/// a shared trait-erased cancel target + a per-handle observation flag.
pub(crate) struct JniCancel {
    pub inner: Arc<dyn TransportCancel + Send + Sync>,
    pub flag: AtomicBool,
}

impl JniCancel {
    pub(crate) fn into_handle(self) -> jlong {
        REGISTRY_CANCEL.insert(self) as jlong
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_CancelHandle_nCancel(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let ran = REGISTRY_CANCEL.with(handle as u64, |c| {
            c.flag.store(true, Ordering::Release);
            c.inner.cancel();
        });
        if ran.is_none() {
            crate::error::throw_closed(env, "CancelHandle");
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_CancelHandle_nIsCancelled(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // A cancel target is never "parked", so `try_with` never reports `Locked`
        // here; treat `Locked`/`Taken` as closed.
        match REGISTRY_CANCEL.try_with(handle as u64, |c| u8::from(c.flag.load(Ordering::Acquire)))
        {
            TryWith::Ran(v) => v,
            _ => {
                crate::error::throw_closed(env, "CancelHandle");
                0
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_CancelHandle_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent drop.
        let _ = REGISTRY_CANCEL.close(handle as u64);
    })
}
