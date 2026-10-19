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

use crate::panic::jni_native;
use std::sync::LazyLock;

use jni::jni_str;
use jni::objects::{JByteArray, JObject, JString};
use jni::strings::JNIString;
use jni::sys::{jboolean, jlong};
use tst_core::transport::Transport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, Domain, Opened, Owned};
use tst_pipeline::{Sender as PlSender, SenderConfig};

use super::cancel::cancel_view_handle;
use super::open::{open_send, read_url};
use super::{domain_or_closed, throw_state};
use crate::error::{throw_binding, throw_handle_state};
use crate::generated::stats_shells::build_socket_stats;
use crate::generated::stats_srt::build_srt_stats;
use crate::handle::OwnedRegistry;

type Shell = PlSender<Box<dyn Transport>>;

static REGISTRY: LazyLock<OwnedRegistry<Shell, Snapshot<()>>> = LazyLock::new(OwnedRegistry::new);

jni_native! {
    /// `Sender.nOpen(url)` — open the transport named by the URL's scheme and
    /// wrap it in a sender. Throws `IllegalArgumentException` for an unsupported
    /// scheme and the transport's exception for a bad URL or a connect failure;
    /// returns 0 with the exception pending.
    fn Java_org_tstrans_shells_Sender_nOpen(env, _class, url: JString<'_>) -> jlong = 0 => {
        let Some(url) = read_url(env, &url) else {
            return 0;
        };
        let Some(opened) = open_send(env, &url) else {
            return 0;
        };
        register(opened)
    }
}

/// Wrap an opened send transport in a sender and register it. Shared by
/// `nOpen` and by `srt.Socket.intoSender`, which opens its transport from an
/// already-connected socket.
pub(crate) fn register(opened: Opened<Box<dyn Transport>>) -> jlong {
    let Opened {
        transport,
        cancel,
        domain,
        ..
    } = opened;
    let inner = PlSender::new(transport, SenderConfig::default());
    REGISTRY.insert(Owned::new(
        inner,
        cancel,
        Snapshot {
            domain,
            handles: (),
        },
    )) as jlong
}

jni_native! {
    /// Send pre-muxed TS bytes. Throws the transport's exception on a
    /// transport or framing failure.
    fn Java_org_tstrans_shells_Sender_nSendBytes(
        env,
        _class,
        handle: jlong,
        data: JByteArray<'_>,
    ) = () => {
        let bytes: Vec<u8> = match env.convert_byte_array(&data) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(jni_str!("java/lang/RuntimeException"), JNIString::from(e.to_string()));
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
    }
}

jni_native! {
    /// Flush any buffered partial TS bundle.
    fn Java_org_tstrans_shells_Sender_nFlush(env, _class, handle: jlong) = () => {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "Sender") else {
            return;
        };
        match REGISTRY.with_mut(handle as u64, |s| s.flush()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_binding(env, domain, &BindingError::from(e)),
            Err(state) => throw_state(env, domain, "Sender", &state),
        }
    }
}

jni_native! {
    /// A shareable cancel handle. Lock-free: the view is the `Owned` entry
    /// itself, so this answers even while a send is parked on the slot.
    fn Java_org_tstrans_shells_Sender_nCancelHandle(env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY.cancel_view(handle as u64) {
            Some(view) => cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "Sender");
                0
            }
        }
    }
}

jni_native! {
    /// The scheme-neutral 16-counter `SocketStats` record. Null on a JNI builder
    /// failure (non-fatal; no throw).
    fn Java_org_tstrans_shells_Sender_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |s| s.socket_stats().unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "Sender", &state);
                return JObject::null();
            }
        };
        build_socket_stats(env, &stats)
            .unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// The SRT-rich `SrtStats` record. `UnsupportedOperationException` unless the
    /// shell was opened from an `srt://` URL.
    fn Java_org_tstrans_shells_Sender_nSrtStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
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
                throw_handle_state(env, "Sender", &state);
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
                    jni_str!("java/lang/UnsupportedOperationException"),
                    jni_str!("srtStats() is only available on an srt:// shell"),
                );
                JObject::null()
            }
        }
    }
}

jni_native! {
    /// Close the sender, deallocating the native box. Cancel first, so a send
    /// parked on another thread ends promptly.
    fn Java_org_tstrans_shells_Sender_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut inner) = REGISTRY.close(handle as u64) {
            inner.close();
        }
    }
}

jni_native! {
    /// Whether the sender owns a live transport. Non-blocking: a `sendBytes`
    /// parked on another thread holds the slot, and reads as alive.
    fn Java_org_tstrans_shells_Sender_nIsAlive(_env, _class, handle: jlong) -> jboolean = false => {
        REGISTRY.is_alive(handle as u64, |s| s.is_alive())
    }
}
