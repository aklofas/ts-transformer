//! JNI exports for `org.tstrans.shells.Sender`: TS bytes in, any transport out.
//!
//! The handle is a `jlong` key into an [`OwnedRegistry`] whose entries are
//! `tst_pipeline::binding::Owned<Sender<Box<dyn Transport>>, Snapshot<()>>`,
//! opened from a URL by `nOpen`. Lifecycle:
//! - The cancel target comes from the `Opened` before the transport is
//!   boxed into the shell and lives outside the slot, so `nCancelHandle`
//!   answers while a `send` is parked and `nClose` cancels first.
//! - Per-call methods go through `with_mut` (mutators) /
//!   `with_ref_blocking` (stats); a failed call throws through the domain in
//!   the shell's snapshot, which stays readable after the slot is taken.
//! - `nClose` takes + tears down via `OwnedRegistry::close`.
//!
//! No Rust lock is held across a JNI callback: nothing here calls back into
//! Java while the slot is held. There is no GIL analog in JNI; calls block on
//! the native thread and a cross-thread stop is `cancelHandle().cancel()`.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jlong};
use tst_core::transport::Transport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, Domain};
use tst_pipeline::{Sender as PlSender, SenderConfig};

use super::cancel::cancel_view_handle;
use super::open::{open_send, read_url};
use super::{domain_or_closed, throw_state};
use crate::error::throw_binding;
use crate::handle::OwnedRegistry;
use crate::jutil::build_socket_stats;
use crate::srt::stats::build_srt_stats;

type Shell = PlSender<Box<dyn Transport>>;

static REGISTRY: LazyLock<OwnedRegistry<Shell, Snapshot<()>>> = LazyLock::new(OwnedRegistry::new);

/// `Sender.nOpen(url)` — open the transport named by the URL's scheme and
/// wrap it in a sender. Throws `IllegalArgumentException` for an unsupported
/// scheme and the transport's exception for a bad URL or a connect failure;
/// returns 0 with the exception pending.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nOpen(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let Some(url) = read_url(env, &url) else {
            return 0;
        };
        let Some(opened) = open_send(env, &url) else {
            return 0;
        };
        let tst_pipeline::binding::Opened {
            transport,
            cancel,
            domain,
            ..
        } = opened;
        let inner = PlSender::new(transport, SenderConfig::default());
        REGISTRY.insert(tst_pipeline::binding::Owned::new(
            inner,
            cancel,
            Snapshot {
                domain,
                handles: (),
            },
        )) as jlong
    })
}

/// Send pre-muxed TS bytes. Throws the transport's exception on a
/// transport or framing failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nSendBytes(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    data: JByteArray<'_>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let bytes: Vec<u8> = match env.convert_byte_array(&data) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return;
            }
        };
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Sender") else {
            return;
        };
        match REGISTRY.with_mut(handle as u64, |s| s.send_ts(&bytes)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_binding(env, domain, &BindingError::from(e)),
            Err(state) => throw_state(env, domain, "Sender", &state),
        }
    })
}

/// Flush any buffered partial TS bundle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nFlush(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Sender") else {
            return;
        };
        match REGISTRY.with_mut(handle as u64, |s| s.flush()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_binding(env, domain, &BindingError::from(e)),
            Err(state) => throw_state(env, domain, "Sender", &state),
        }
    })
}

/// A shareable cancel handle. Lock-free: the view is the `Owned` entry
/// itself, so this answers even while a send is parked on the slot.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY.cancel_view(handle as u64) {
            Some(view) => cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "Sender");
                0
            }
        }
    })
}

/// The scheme-neutral 16-counter `SocketStats` record. Null on a JNI builder
/// failure (non-fatal; no throw).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Sender") else {
            return JObject::null();
        };
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |s| s.socket_stats().unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_state(env, domain, "Sender", &state);
                return JObject::null();
            }
        };
        build_socket_stats(env, "org/tstrans/shells/SocketStats", &stats)
            .unwrap_or_else(|_| JObject::null())
    })
}

/// The SRT-rich `SrtStats` record. `UnsupportedOperationException` unless the
/// shell was opened from an `srt://` URL.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nSrtStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Sender") else {
            return JObject::null();
        };
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |s| {
                s.transport()
                    .as_any()
                    .and_then(|a| a.downcast_ref::<tst_srt::SrtTransport>())
                    .map(|t| t.stats())
            },
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_state(env, domain, "Sender", &state);
                return JObject::null();
            }
        };
        match stats {
            Some(Ok(s)) => build_srt_stats(env, &s).unwrap_or_else(|_| JObject::null()),
            Some(Err(e)) => {
                throw_binding(env, Domain::Srt, &BindingError::from(e));
                JObject::null()
            }
            None => {
                let _ = env.throw_new(
                    "java/lang/UnsupportedOperationException",
                    "srtStats() is only available on an srt:// shell",
                );
                JObject::null()
            }
        }
    })
}

/// Close the sender, deallocating the native box. Cancel first, so a send
/// parked on another thread ends promptly.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut inner) = REGISTRY.close(handle as u64) {
            inner.close();
        }
    })
}

/// Whether the sender owns a live transport. Non-blocking: a `sendBytes`
/// parked on another thread holds the slot, and reads as alive.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Sender_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        u8::from(REGISTRY.is_alive(handle as u64, |s| s.is_alive()))
    })
}
