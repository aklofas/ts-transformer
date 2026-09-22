//! `org.tstrans.rtp` — RTP transport JNI surface.

pub(crate) mod errors;
mod client;
mod demux_receiver;
pub(crate) mod end_reason;
pub(crate) mod h264_receiver;
mod mux_sender;
mod server;
mod transport;

use std::sync::Arc;
use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::jlong;
use tst_core::transport::TransportCancel;

use tst_pipeline::binding::{BindingError, BindingErrorKind};

use crate::handle::{CancelView, HandleRegistry};

/// Per-type leased-handle registry for `org.tstrans.rtp.CancelHandle`, which
/// boxes a [`CancelView`] over the shell's `Owned` entry. A cancel view has no
/// parked op of its own, so it registers plain (cancel = None).
///
/// `org.tstrans.rtp.CancelHandle` exposes only `cancel()` — no
/// `isCancelled()`, unlike the srt twin. The view supports one if a later rider
/// adds the method.
static REGISTRY_CANCEL: LazyLock<HandleRegistry<CancelView>> = LazyLock::new(HandleRegistry::new);

/// Register a cancel view and return its `org.tstrans.rtp.CancelHandle` key.
pub(crate) fn cancel_view_handle(view: CancelView) -> jlong {
    REGISTRY_CANCEL.insert(view) as jlong
}

/// The rtp transports expose their cancel only through the trait's `Option`
/// (no inherent non-`Option` accessor exists on `RtpTransport` /
/// `RtpRecvTransport`, unlike `SrtTransport::srt_cancel_handle`). A `None`
/// would be a tst-rtp bug, reported as `Internal` at open — never an `expect`.
pub(crate) fn rtp_cancel(
    c: Option<Arc<dyn TransportCancel + Send + Sync>>,
    what: &str,
) -> Result<Arc<dyn TransportCancel>, BindingError> {
    match c {
        Some(c) => Ok(c),
        None => Err(BindingError::new(
            BindingErrorKind::Internal,
            format!("{what} exposes no cancel handle"),
        )),
    }
}

/// Signal cancellation. Wakes a thread parked in `Sender.send` / `Receiver.recv`
/// at the next ~100 ms cancel-poll tick.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_CancelHandle_nCancel(
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

/// Free the boxed cancel handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_CancelHandle_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    // Atomic + idempotent drop.
    crate::panic::jni_catch(&mut env, (), |_env| {
        let _ = REGISTRY_CANCEL.close(handle as u64);
    })
}
