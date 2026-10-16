//! JNI exports for `org.tstrans.shells.Receiver`: any transport in, TS bytes out.
//!
//! Same handle model as `Sender`: an `Owned` entry in an [`OwnedRegistry`],
//! the cancel target outside the slot, the transport's error domain in the
//! snapshot. The transport's end-reason cell is attached to the `Owned`
//! (read lock-free by `nEndReason` / `nEndDetail`), and `nClose` returns the
//! reason it reads after the teardown so the Java object can keep answering
//! once its handle is zeroed.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jstring};
use tst_core::transport::RecvTransport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, Domain, Opened, Owned};
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig};

use super::cancel::cancel_view_handle;
use super::end_reason::{build_close_snapshot, detail, wire_value};
use super::open::{open_recv, read_url};
use super::{domain_or_closed, throw_state};
use crate::error::throw_binding;
use crate::handle::OwnedRegistry;
use crate::jutil::build_socket_stats;
use crate::srt::stats::build_srt_stats;

type Shell = PlReceiver<Box<dyn RecvTransport>>;

static REGISTRY: LazyLock<OwnedRegistry<Shell, Snapshot<()>>> = LazyLock::new(OwnedRegistry::new);

/// `Receiver.nOpen(url)` — for `srt://` the URL must carry `?mode=listener`;
/// the open binds, listens and accepts ONE peer (not cancellable: no handle
/// exists until it returns). Throws `IllegalArgumentException` for an
/// unsupported scheme and the transport's exception otherwise.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nOpen(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let Some(url) = read_url(env, &url) else {
            return 0;
        };
        let Some(opened) = open_recv(env, &url) else {
            return 0;
        };
        let Opened {
            transport,
            cancel,
            domain,
            ..
        } = opened;
        // Taken before the transport moves into the shell.
        let end_reason = transport.end_reason_handle();
        let inner = PlReceiver::new(transport, ReceiverConfig::default());
        let mut owned = Owned::new(
            inner,
            cancel,
            Snapshot {
                domain,
                handles: (),
            },
        );
        if let Some(h) = end_reason {
            owned = owned.with_end_reason(h);
        }
        REGISTRY.insert(owned) as jlong
    })
}

/// One 188-byte TS packet per call. `maxLen` is accepted for API symmetry
/// and ignored. Throws the transport's exception on failure, including
/// `END_OF_STREAM` when the peer closed cleanly.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nRecvBytes(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    _max_len: jint,
) -> jbyteArray {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Receiver") else {
            return std::ptr::null_mut();
        };
        // `next_packet` may park; the closure holds the slot for its duration.
        // `cancelHandle().cancel()` / a concurrent `close()` (which cancels
        // before it takes the slot) wakes a parked recv.
        match REGISTRY.with_mut(handle as u64, |r| r.next_packet()) {
            Ok(Ok(bytes)) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Ok(Err(e)) => {
                throw_binding(env, domain, &BindingError::from(e));
                std::ptr::null_mut()
            }
            Err(state) => {
                throw_state(env, domain, "Receiver", &state);
                std::ptr::null_mut()
            }
        }
    })
}

/// A shareable cancel handle. Lock-free: answers while `recvBytes` is parked.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY.cancel_view(handle as u64) {
            Some(view) => cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "Receiver");
                0
            }
        }
    })
}

/// The scheme-neutral 16-counter `SocketStats` record.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Receiver") else {
            return JObject::null();
        };
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |r| r.socket_stats().unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_state(env, domain, "Receiver", &state);
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
pub extern "system" fn Java_org_tstrans_shells_Receiver_nSrtStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Receiver") else {
            return JObject::null();
        };
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |r| {
                r.transport()
                    .as_any()
                    .and_then(|a| a.downcast_ref::<tst_srt::SrtTransport>())
                    .map(|t| t.stats())
            },
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_state(env, domain, "Receiver", &state);
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

/// Why the receive session ended, as a wire value (`-1` while it has not
/// ended or the transport records none). Lock-free.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nEndReason(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    crate::panic::jni_catch(&mut env, -1, |_env| {
        wire_value(REGISTRY.end_reason(handle as u64).as_ref())
    })
}

/// Free-text detail for `nEndReason`; null for a variant without one.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nEndDetail(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.end_reason(handle as u64).as_ref().and_then(detail) {
            Some(d) => env
                .new_string(d)
                .map_or(std::ptr::null_mut(), |s| s.into_raw()),
            None => std::ptr::null_mut(),
        }
    })
}

/// Close the receiver. Cancel first, so a `recvBytes` parked on another
/// thread ends with the transport's `CLOSED`. Returns the end-reason snapshot
/// (`EndReason$Snapshot`), null when the close lost the race.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nClose<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        match REGISTRY.close_with_end_reason(handle as u64, |mut r| r.close()) {
            Some(reason) => build_close_snapshot(env, reason).unwrap_or_else(|_| JObject::null()),
            None => JObject::null(),
        }
    })
}

/// Whether the receiver owns a live transport. Non-blocking: a parked
/// `recvBytes` holds the slot, and reads as alive.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_shells_Receiver_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        u8::from(REGISTRY.is_alive(handle as u64, |r| r.is_alive()))
    })
}
