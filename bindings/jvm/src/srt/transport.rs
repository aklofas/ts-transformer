//! JNI exports for `org.tstrans.srt.Sender` and `org.tstrans.srt.Receiver`.
//!
//! Each export backs one static-native method on the Java class. The handle is
//! a `jlong` key into a per-type [`HandleRegistry`] over a
//! `tst_pipeline::Sender<SrtTransport>` / `Receiver<SrtTransport>`. Handle
//! lifecycle:
//! - `nFromUrl` registers via `REGISTRY.insert`.
//! - Per-call methods lease via `REGISTRY.with` (non-consuming).
//! - `nClose` takes + tears down via `REGISTRY.close`.
//!
//! The Java side guards all per-call methods with `ensureOpen()` and always
//! passes a non-zero handle to Rust, but zero-handle checks are retained here
//! as a safety net.
//!
//! There is no GIL analog in JNI — calls simply block on the native thread.
//! Callers that need cancellation call `nCancelHandle` and invoke `.cancel()`
//! from another thread; that wakes the libsrt socket within ~3-10 ms.

use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jlong};
use tst_pipeline::receiver::ReceiverErrorSource;
use tst_pipeline::sender::SenderErrorSource;
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig, Sender as PlSender, SenderConfig};
use tst_srt::{Listener, ListenerConfig, Socket, SocketConfig, SrtTransport, SrtUrl, url::Mode};

use super::JniCancel;
use super::errors::{
    accept_error, bind_error, connect_error, io_error, transport_error, url_error,
};
use super::stats::build_srt_stats;
use crate::handle::HandleRegistry;
use crate::jutil::build_socket_stats;

/// Per-type leased-handle registries for `org.tstrans.srt.Sender` / `Receiver`.
/// `pub(crate)` so `srt::lowlevel::nIntoSender`/`nIntoReceiver` can register the
/// shells they build from a consumed `Socket`.
pub(crate) static REGISTRY_SENDER: LazyLock<HandleRegistry<PlSender<SrtTransport>>> =
    LazyLock::new(HandleRegistry::new);
pub(crate) static REGISTRY_RECEIVER: LazyLock<HandleRegistry<PlReceiver<SrtTransport>>> =
    LazyLock::new(HandleRegistry::new);

// -----------------------------------------------------------------------
// Sender  (org.tstrans.srt.Sender)
// -----------------------------------------------------------------------

/// Allocate a `Sender` from an SRT caller-mode URL. Returns a `jlong` handle
/// on success; throws `SrtException` and returns 0 on any error.
///
/// The URL must use `mode=caller` (the default when omitted). The host is
/// bracketed for bare IPv6 literals, matching tst-py's IPv6 bracketing logic.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nFromUrl(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        let parsed = match SrtUrl::parse(&url_str) {
            Ok(p) => p,
            Err(e) => {
                url_error(env, &e);
                return 0;
            }
        };

        if parsed.mode != Mode::Caller {
            let msg = format!(
                "Sender.fromUrl requires mode=caller (default); got mode={:?}",
                parsed.mode
            );
            super::errors::throw_srt(env, "CONFIG_INVALID", &msg);
            return 0;
        }

        let mut cfg = SocketConfig::default();
        parsed.overlay.apply_to_socket(&mut cfg);

        let addr = if parsed.host.contains(':') && !parsed.host.starts_with('[') {
            format!("[{}]:{}", parsed.host, parsed.port)
        } else {
            format!("{}:{}", parsed.host, parsed.port)
        };

        let socket = match Socket::connect_with(&cfg, addr.as_str()) {
            Ok(s) => s,
            Err(e) => {
                connect_error(env, &e);
                return 0;
            }
        };

        let transport = SrtTransport::new(socket);
        let inner = PlSender::new(transport, SenderConfig::default());
        register_sender(inner)
    })
}

/// Register a plain `Sender` shell, capturing its cancel target BEFORE the shell
/// is boxed so `nCancelHandle` never needs the resource lock (a `send` parked on
/// backpressure holds it). A fresh `SrtTransport` always has a cancel handle.
pub(super) fn register_sender(inner: PlSender<SrtTransport>) -> jlong {
    let target = inner
        .cancel_handle()
        .expect("a fresh SrtTransport always returns Some(cancel_handle)");
    REGISTRY_SENDER.insert_with_target(inner, target) as jlong
}

/// `Receiver` twin of [`register_sender`]: the target is read lock-free while
/// `recvBytes()` is parked. Unlike the sender, the receiver registers
/// cancel-on-close: `nClose` fires `target` before taking the resource lock, so
/// a `recvBytes()` parked on another thread ends promptly (with
/// `SrtException(BROKEN)` — the plain cancel closes the socket under the parked
/// recv) instead of holding `close()` hostage. The contract tst-py's plain
/// `Receiver.close()` already states.
pub(super) fn register_receiver(inner: PlReceiver<SrtTransport>) -> jlong {
    let target = inner
        .cancel_handle()
        .expect("a fresh SrtTransport always returns Some(cancel_handle)");
    REGISTRY_RECEIVER.insert_cancel_on_close(inner, target, None) as jlong
}

/// Send pre-muxed TS bytes. Throws `SrtException` on transport/framing failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nSendBytes(
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

        match REGISTRY_SENDER.with_poisoning(handle as u64, |inner| inner.send_ts(&bytes)) {
            Some(Ok(())) => {}
            Some(Err(e)) => match e.source {
                SenderErrorSource::Transport(t) => transport_error(env, &t),
                SenderErrorSource::Framing(f) => {
                    super::errors::throw_srt(env, "CONFIG_INVALID", &f.to_string())
                }
                _ => super::errors::throw_srt(env, "IO", &e.to_string()),
            },
            None => {
                crate::error::throw_closed(env, "Sender");
            }
        }
    })
}

/// Flush any buffered partial TS bundle. Throws `SrtException` on failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nFlush(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        match REGISTRY_SENDER.with_poisoning(handle as u64, |inner| inner.flush()) {
            Some(Ok(())) => {}
            Some(Err(e)) => match e.source {
                SenderErrorSource::Transport(t) => transport_error(env, &t),
                SenderErrorSource::Framing(f) => {
                    super::errors::throw_srt(env, "CONFIG_INVALID", &f.to_string())
                }
                _ => super::errors::throw_srt(env, "IO", &e.to_string()),
            },
            None => {
                crate::error::throw_closed(env, "Sender");
            }
        }
    })
}

/// Obtain a cancel handle for this Sender. Returns a `jlong` handle on
/// success; returns 0 if the transport doesn't expose a cancel handle (should
/// not happen for a live SrtTransport).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free: the target was captured at registration, so this returns
        // even while `send` is parked on the resource lock. Closed handle → 0
        // (no throw, matching the original contract).
        match REGISTRY_SENDER.cancel_target(handle as u64) {
            Some(inner) => JniCancel {
                inner,
                flag: AtomicBool::new(false),
            }
            .into_handle(),
            None => 0,
        }
    })
}

/// Return a `SocketStats` record for this Sender. Returns null on JNI error
/// (the underlying stats call uses `unwrap_or_default` so the Java side always
/// gets a valid snapshot or null on a builder failure — the latter is
/// considered non-fatal, hence no throw).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY_SENDER.with(handle as u64, |inner| {
            inner.socket_stats().unwrap_or_default()
        }) else {
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/srt/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// Return an `SrtStats` record for this Sender. Throws `SrtException` (via
/// `io_error`) if the underlying `SrtTransport::stats()` call fails; returns
/// null if the JNI record-builder fails (non-fatal).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nSrtStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY_SENDER.with(handle as u64, |inner| inner.transport().stats())
        else {
            return JObject::null();
        };
        match stats {
            Ok(s) => match build_srt_stats(env, &s) {
                Ok(obj) => obj,
                Err(_) => JObject::null(),
            },
            Err(e) => {
                io_error(env, &e);
                JObject::null()
            }
        }
    })
}

/// Close the Sender, deallocating the native box.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut inner) = REGISTRY_SENDER.close(handle as u64) {
            inner.close();
        }
    })
}

/// Return whether the Sender transport is still live.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Sender_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_SENDER
            .with(handle as u64, |inner| u8::from(inner.is_alive()))
            .unwrap_or(0)
    })
}

// -----------------------------------------------------------------------
// Receiver  (org.tstrans.srt.Receiver)
// -----------------------------------------------------------------------

/// Allocate a `Receiver` from an SRT listener-mode URL. Returns a `jlong`
/// handle on success; throws `SrtException` and returns 0 on any error.
///
/// The URL must use `mode=listener`. Binds the socket, then blocks on one
/// incoming SRT handshake (one-shot accept). An empty host
/// (`srt://:7000?mode=listener`) binds to `0.0.0.0`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Receiver_nFromUrl(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        let parsed = match SrtUrl::parse(&url_str) {
            Ok(p) => p,
            Err(e) => {
                url_error(env, &e);
                return 0;
            }
        };

        if parsed.mode != Mode::Listener {
            let msg = format!(
                "Receiver.fromUrl requires mode=listener; got mode={:?}",
                parsed.mode
            );
            super::errors::throw_srt(env, "CONFIG_INVALID", &msg);
            return 0;
        }

        let mut cfg = ListenerConfig::default();
        parsed.overlay.apply_to_listener(&mut cfg);

        let addr = if parsed.host.is_empty() {
            format!("0.0.0.0:{}", parsed.port)
        } else if parsed.host.contains(':') && !parsed.host.starts_with('[') {
            format!("[{}]:{}", parsed.host, parsed.port)
        } else {
            format!("{}:{}", parsed.host, parsed.port)
        };

        let mut listener = match Listener::bind_with(&cfg, addr.as_str()) {
            Ok(l) => l,
            Err(e) => {
                bind_error(env, &e);
                return 0;
            }
        };

        let (socket, _peer) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) => {
                accept_error(env, &e);
                return 0;
            }
        };

        let transport = SrtTransport::new(socket);
        let inner = PlReceiver::new(transport, ReceiverConfig::default());
        register_receiver(inner)
    })
}

/// Receive one TS packet (188 bytes) from the underlying transport. Returns
/// the packet as a `jbyteArray` on success; throws `SrtException` and returns
/// null on transport/error failure. `maxLen` is accepted for API symmetry with
/// tst-py but a single `next_packet` quantum is always returned.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Receiver_nRecvBytes(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    _max_len: jni::sys::jint,
) -> jbyteArray {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // `next_packet` may park; the closure holds the resource lock for its
        // duration. `cancelHandle().cancel()` / a concurrent `close()` (which fires
        // the cancel hook before taking the lock) wakes a parked recv.
        let Some(res) =
            REGISTRY_RECEIVER.with_poisoning(handle as u64, |inner| inner.next_packet())
        else {
            crate::error::throw_closed(env, "Receiver");
            return std::ptr::null_mut();
        };
        match res {
            Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Err(e) => {
                match e.source {
                    ReceiverErrorSource::Transport(t) => transport_error(env, &t),
                    _ => super::errors::throw_srt(env, "IO", &e.to_string()),
                }
                std::ptr::null_mut()
            }
        }
    })
}

/// Obtain a cancel handle for this Receiver. Returns a `jlong` handle on
/// success; returns 0 if no cancel handle is available.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Receiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free: the target was captured at registration, so this returns
        // even while `recvBytes` is parked on the resource lock. Closed handle →
        // 0 (no throw, matching the original contract).
        match REGISTRY_RECEIVER.cancel_target(handle as u64) {
            Some(inner) => JniCancel {
                inner,
                flag: AtomicBool::new(false),
            }
            .into_handle(),
            None => 0,
        }
    })
}

/// Return a `SocketStats` record for this Receiver. Returns null on JNI
/// builder error (non-fatal; no throw).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Receiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY_RECEIVER.with(handle as u64, |inner| {
            inner.socket_stats().unwrap_or_default()
        }) else {
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/srt/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// Return an `SrtStats` record for this Receiver. Throws `SrtException` (via
/// `io_error`) if the underlying `SrtTransport::stats()` call fails.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Receiver_nSrtStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY_RECEIVER.with(handle as u64, |inner| inner.transport().stats())
        else {
            return JObject::null();
        };
        match stats {
            Ok(s) => match build_srt_stats(env, &s) {
                Ok(obj) => obj,
                Err(_) => JObject::null(),
            },
            Err(e) => {
                io_error(env, &e);
                JObject::null()
            }
        }
    })
}

/// Close the Receiver, deallocating the native box.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Receiver_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent. NOTE: no cancel hook (srt model — the public cancel
        // handle wakes a parked recv); a `close()` racing a parked recv blocks on the
        // resource lock until the recv is cancelled, never UAFs (the single-iterator
        // contract still applies for wake-up).
        if let Some(mut inner) = REGISTRY_RECEIVER.close(handle as u64) {
            inner.close();
        }
    })
}

/// Return whether the Receiver transport is still live.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Receiver_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_RECEIVER
            .with(handle as u64, |inner| u8::from(inner.is_alive()))
            .unwrap_or(0)
    })
}
