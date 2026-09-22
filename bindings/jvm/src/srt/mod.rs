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

use crate::handle::{CancelView, HandleRegistry};

/// `org.tstrans.srt.CancelHandle` boxes a [`CancelView`] over the shell's
/// `Owned` entry: `cancel()` and `isCancelled()` act on the shell's one
/// cancel state, so every handle on a shell — and a handle that outlives
/// the shell's `close()` — agrees. Per-type plain registry: the view has
/// no parked op of its own.
static REGISTRY_CANCEL: LazyLock<HandleRegistry<CancelView>> = LazyLock::new(HandleRegistry::new);

/// Register a cancel view and return its `org.tstrans.srt.CancelHandle` key.
pub(crate) fn cancel_view_handle(view: CancelView) -> jlong {
    REGISTRY_CANCEL.insert(view) as jlong
}

/// The transport's cancel target in the shape `Owned::new` takes. Obtained
/// BEFORE the transport moves into a shell (obtain-before-move). Infallible:
/// A3's inherent accessor is never `Option`.
pub(crate) fn srt_cancel(t: &tst_srt::SrtTransport) -> Arc<dyn TransportCancel> {
    Arc::new(t.srt_cancel_handle())
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

/// Boxed behind a `CancelHandle.handle` for the shells that have NOT yet moved
/// onto `OwnedRegistry` (the managed srt family — Task B3.4 deletes this type
/// together with its last callers). The `flag` is this handle's own
/// observation bit, which is exactly the per-handle semantics B3 replaces;
/// plain shells already read the shell's one state through [`CancelView`].
pub(crate) struct JniCancel {
    pub inner: Arc<dyn TransportCancel + Send + Sync>,
    pub flag: AtomicBool,
}

impl crate::handle::CancelSurface for JniCancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.inner.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

impl JniCancel {
    pub(crate) fn into_handle(self) -> jlong {
        cancel_view_handle(CancelView(Arc::new(self)))
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_CancelHandle_nCancel(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        if REGISTRY_CANCEL
            .with(handle as u64, |c| c.0.cancel())
            .is_none()
        {
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
        match REGISTRY_CANCEL.with(handle as u64, |c| u8::from(c.0.is_cancelled())) {
            Some(v) => v,
            None => {
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
