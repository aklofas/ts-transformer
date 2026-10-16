//! `org.tstrans.shells.CancelHandle` natives and the registry behind them.
//!
//! A handle boxes a [`CancelView`] over the shell's `Owned` entry:
//! `cancel()` and `isCancelled()` act on the shell's one cancel state, so
//! every handle on a shell, and a handle that outlives the shell's
//! `close()`, agree. Per-type plain registry: the view has no parked op of
//! its own.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jlong};

use crate::handle::{CancelView, HandleRegistry};

static REGISTRY_CANCEL: LazyLock<HandleRegistry<CancelView>> = LazyLock::new(HandleRegistry::new);

/// Register a cancel view and return its `org.tstrans.shells.CancelHandle` key.
pub(super) fn cancel_view_handle(view: CancelView) -> jlong {
    REGISTRY_CANCEL.insert(view) as jlong
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_CancelHandle_nCancel(
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
pub extern "system" fn Java_org_tstrans_shells_CancelHandle_nIsCancelled(
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
pub extern "system" fn Java_org_tstrans_shells_CancelHandle_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent drop.
        let _ = REGISTRY_CANCEL.close(handle as u64);
    })
}
