//! JNI exports for `org.tstrans.shells.Receiver`: any transport in, TS bytes out.
//!
//! Same handle model as `Sender`: an `Owned` entry in an [`OwnedRegistry`],
//! the cancel target outside the slot, the transport's error domain in the
//! snapshot. The transport's end-reason cell is attached to the `Owned`
//! (read lock-free by `nEndReason` / `nEndDetail`), and `nClose` returns the
//! reason it reads after the teardown so the Java object can keep answering
//! once its handle is zeroed.

use crate::panic::jni_native;
use std::sync::LazyLock;

use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jstring};
use tst_core::transport::RecvTransport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, Domain, Opened, Owned};
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig};

use super::cancel::cancel_view_handle;
use super::end_reason::{build_close_snapshot, detail, wire_value};
use super::open::{open_recv, read_url};
use super::{domain_or_closed, throw_state};
use crate::error::{throw_binding, throw_handle_state};
use crate::handle::OwnedRegistry;
use crate::jutil::build_socket_stats;
use crate::srt::stats::build_srt_stats;

type Shell = PlReceiver<Box<dyn RecvTransport>>;

static REGISTRY: LazyLock<OwnedRegistry<Shell, Snapshot<()>>> = LazyLock::new(OwnedRegistry::new);

jni_native! {
    /// `Receiver.nOpen(url)` — for `srt://` the URL must carry `?mode=listener`;
    /// the open binds, listens and accepts ONE peer (not cancellable: no handle
    /// exists until it returns). Throws `IllegalArgumentException` for an
    /// unsupported scheme and the transport's exception otherwise.
    fn Java_org_tstrans_shells_Receiver_nOpen(env, _class, url: JString<'_>) -> jlong = 0 => {
        let Some(url) = read_url(env, &url) else {
            return 0;
        };
        let Some(opened) = open_recv(env, &url) else {
            return 0;
        };
        register(opened)
    }
}

/// Wrap an opened receive transport in a receiver and register it. Shared by
/// `nOpen` and by `srt.Socket.intoReceiver`.
pub(crate) fn register(opened: Opened<Box<dyn RecvTransport>>) -> jlong {
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
}

jni_native! {
    /// One 188-byte TS packet per call. `maxLen` is accepted for API symmetry
    /// and ignored. Throws the transport's exception on failure, including
    /// `END_OF_STREAM` when the peer closed cleanly (an `srt://` shell reads
    /// `BROKEN` first: libsrt reports a broken link, never a clean EOF).
    fn Java_org_tstrans_shells_Receiver_nRecvBytes(
        env,
        _class,
        handle: jlong,
        _max_len: jint,
    ) -> jbyteArray = std::ptr::null_mut() => {
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
    }
}

jni_native! {
    /// A shareable cancel handle. Lock-free: answers while `recvBytes` is parked.
    fn Java_org_tstrans_shells_Receiver_nCancelHandle(env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY.cancel_view(handle as u64) {
            Some(view) => cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "Receiver");
                0
            }
        }
    }
}

jni_native! {
    /// The scheme-neutral 16-counter `SocketStats` record.
    fn Java_org_tstrans_shells_Receiver_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |r| r.socket_stats().unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "Receiver", &state);
                return JObject::null();
            }
        };
        build_socket_stats(env, "org/tstrans/shells/SocketStats", &stats)
            .unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// The SRT-rich `SrtStats` record. `UnsupportedOperationException` unless the
    /// shell was opened from an `srt://` URL.
    fn Java_org_tstrans_shells_Receiver_nSrtStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
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
                throw_handle_state(env, "Receiver", &state);
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
    }
}

jni_native! {
    /// Why the receive session ended, as a wire value (`-1` while it has not
    /// ended or the transport records none). Lock-free.
    fn Java_org_tstrans_shells_Receiver_nEndReason(_env, _class, handle: jlong) -> jint = -1 => {
        wire_value(REGISTRY.end_reason(handle as u64).as_ref())
    }
}

jni_native! {
    /// Free-text detail for `nEndReason`; null for a variant without one.
    fn Java_org_tstrans_shells_Receiver_nEndDetail(
        env,
        _class,
        handle: jlong,
    ) -> jstring = std::ptr::null_mut() => {
        match REGISTRY.end_reason(handle as u64).as_ref().and_then(detail) {
            Some(d) => env
                .new_string(d)
                .map_or(std::ptr::null_mut(), |s| s.into_raw()),
            None => std::ptr::null_mut(),
        }
    }
}

jni_native! {
    /// Close the receiver. Cancel first, so a `recvBytes` parked on another
    /// thread ends with the transport's `CLOSED`. Returns the end-reason snapshot
    /// (`EndReason$Snapshot`), null when the close lost the race.
    fn Java_org_tstrans_shells_Receiver_nClose<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        match REGISTRY.close_with_end_reason(handle as u64, |mut r| r.close()) {
            Some(reason) => build_close_snapshot(env, reason).unwrap_or_else(|_| JObject::null()),
            None => JObject::null(),
        }
    }
}

jni_native! {
    /// Whether the receiver owns a live transport. Non-blocking: a parked
    /// `recvBytes` holds the slot, and reads as alive.
    fn Java_org_tstrans_shells_Receiver_nIsAlive(_env, _class, handle: jlong) -> jboolean = 0 => {
        u8::from(REGISTRY.is_alive(handle as u64, |r| r.is_alive()))
    }
}
