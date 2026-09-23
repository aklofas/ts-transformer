//! JNI exports for `org.tstrans.srt.Sender` and `org.tstrans.srt.Receiver`.
//!
//! Each export backs one static-native method on the Java class. The handle is
//! a `jlong` key into a per-type [`OwnedRegistry`] whose entries are
//! `tst_pipeline::binding::Owned<Sender<SrtTransport>>` /
//! `Owned<Receiver<SrtTransport>>`. Handle lifecycle:
//! - `nFromUrl` registers via [`register_sender`] / [`register_receiver`]
//!   (the cancel target is read off the transport before the shell is boxed and
//!   kept outside the slot, so `OwnedRegistry::close` fires it before taking
//!   the slot — cancel-first close).
//! - Per-call methods go through `with_mut` (mutators) / `with_ref` (readers);
//!   a `HandleState` becomes the one Java mapping in `error::throw_handle_state`.
//! - `nClose` takes + tears down via `OwnedRegistry::close`.
//!
//! The Java side guards all per-call methods with `ensureOpen()` and always
//! passes a non-zero handle to Rust, but zero-handle checks are retained here
//! as a safety net.
//!
//! There is no GIL analog in JNI — calls simply block on the native thread.
//! Callers that need cancellation call `nCancelHandle` and invoke `.cancel()`
//! from another thread; that wakes the libsrt socket within ~3-10 ms.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jlong};
use tst_pipeline::binding::{BindingErrorKind, Owned};
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig, Sender as PlSender, SenderConfig};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use super::errors::{srt_error, throw_receiver_error, throw_sender_error, throw_srt, url_error};
use super::stats::build_srt_stats;
use crate::error::throw_handle_state;
use crate::handle::OwnedRegistry;
use crate::jutil::build_socket_stats;

/// Per-type `Owned`-backed registries for `org.tstrans.srt.Sender` / `Receiver`.
/// `pub(crate)` so `srt::lowlevel::nIntoSender`/`nIntoReceiver` can register the
/// shells they build from a consumed `Socket`.
pub(crate) static REGISTRY_SENDER: LazyLock<OwnedRegistry<PlSender<SrtTransport>>> =
    LazyLock::new(OwnedRegistry::new);
pub(crate) static REGISTRY_RECEIVER: LazyLock<OwnedRegistry<PlReceiver<SrtTransport>>> =
    LazyLock::new(OwnedRegistry::new);

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
            throw_srt(env, BindingErrorKind::ConfigInvalid, &msg);
            return 0;
        }

        // One open path (ARCH-01): `SrtUrl::connect_recv` applies the overlay to
        // a default `SocketConfig` and brackets IPv6 itself — byte-for-byte what
        // this file composed by hand, minus the private `[{host}]:{port}` copy.
        // `connect_recv`, NOT `connect`: `connect` also merges the sender preset
        // (15 s connect timeout, 5 s linger, `Role::Sender`), which this site has
        // never applied; routing through it would be a released-behaviour change.
        let transport = match parsed.connect_recv() {
            Ok(t) => t,
            Err(e) => {
                srt_error(env, e);
                return 0;
            }
        };

        register_sender(PlSender::new(transport, SenderConfig::default()))
    })
}

/// Register a plain `Sender` as an `Owned` entry. The cancel target is read off
/// the transport BEFORE the shell is boxed; `Owned` keeps it outside the slot,
/// so `nCancelHandle` answers while a `send` parked in libsrt's blocking
/// `srt_sendmsg` holds the slot, and `nClose` cancels first — a `sendBytes()`
/// parked on another thread ends promptly (with `SrtException(CLOSED)`: the
/// cancel closes the socket under the parked send and `SrtTransport` reports
/// the cancel) instead of holding `close()` hostage. That is the
/// cancel-on-close contract of PR #207.
pub(super) fn register_sender(inner: PlSender<SrtTransport>) -> jlong {
    let cancel = super::srt_cancel(inner.transport());
    REGISTRY_SENDER.insert(Owned::new(inner, cancel, ())) as jlong
}

/// `Receiver` twin of [`register_sender`], same contract: the cancel target is
/// read lock-free while `recvBytes()` is parked, and `nClose` cancels first.
pub(super) fn register_receiver(inner: PlReceiver<SrtTransport>) -> jlong {
    let cancel = super::srt_cancel(inner.transport());
    REGISTRY_RECEIVER.insert(Owned::new(inner, cancel, ())) as jlong
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

        match REGISTRY_SENDER.with_mut(handle as u64, |inner| inner.send_ts(&bytes)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_sender_error(env, &e),
            Err(state) => throw_handle_state(env, "Sender", &state),
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
        match REGISTRY_SENDER.with_mut(handle as u64, |inner| inner.flush()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_sender_error(env, &e),
            Err(state) => throw_handle_state(env, "Sender", &state),
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
        // Lock-free: the view is the `Owned` entry itself, so this returns even
        // while `send` is parked on the slot. Closed handle → 0 (no throw,
        // matching the original contract).
        REGISTRY_SENDER
            .cancel_view(handle as u64)
            .map_or(0, super::cancel_view_handle)
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
        let Ok(stats) = REGISTRY_SENDER.with_ref(handle as u64, |inner| {
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
        let Ok(stats) = REGISTRY_SENDER.with_ref(handle as u64, |inner| inner.transport().stats())
        else {
            return JObject::null();
        };
        match stats {
            Ok(s) => match build_srt_stats(env, &s) {
                Ok(obj) => obj,
                Err(_) => JObject::null(),
            },
            Err(e) => {
                srt_error(env, e);
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
            .with_ref(handle as u64, |inner| u8::from(inner.is_alive()))
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
            throw_srt(env, BindingErrorKind::ConfigInvalid, &msg);
            return 0;
        }

        // The one-shot accept of a plain receiver has no cancel handle yet (the
        // object does not exist): a fresh, never-fired slot. DEBT-16 ruling
        // (Arc 2): the FIRST accept inside a blocking constructor stays
        // uncancellable; the `Receiver.fromUrl` javadoc line stands.
        // `accept_one` renders the empty-host → `0.0.0.0` bind and the IPv6
        // bracketing this file used to compose by hand.
        let slot = tst_core::cancel::CancelSlot::new();
        let transport = match parsed.accept_one(&slot) {
            Ok(t) => t,
            Err(e) => {
                srt_error(env, e);
                return 0;
            }
        };

        register_receiver(PlReceiver::new(transport, ReceiverConfig::default()))
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
        let res = match REGISTRY_RECEIVER.with_mut(handle as u64, |inner| inner.next_packet()) {
            Ok(res) => res,
            Err(state) => {
                throw_handle_state(env, "Receiver", &state);
                return std::ptr::null_mut();
            }
        };
        match res {
            Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Err(e) => {
                throw_receiver_error(env, &e);
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
        // Lock-free: the view is the `Owned` entry itself, so this returns even
        // while `recvBytes` is parked on the slot. Closed handle → 0 (no throw,
        // matching the original contract).
        REGISTRY_RECEIVER
            .cancel_view(handle as u64)
            .map_or(0, super::cancel_view_handle)
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
        let Ok(stats) = REGISTRY_RECEIVER.with_ref(handle as u64, |inner| {
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
        let Ok(stats) =
            REGISTRY_RECEIVER.with_ref(handle as u64, |inner| inner.transport().stats())
        else {
            return JObject::null();
        };
        match stats {
            Ok(s) => match build_srt_stats(env, &s) {
                Ok(obj) => obj,
                Err(_) => JObject::null(),
            },
            Err(e) => {
                srt_error(env, e);
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
            .with_ref(handle as u64, |inner| u8::from(inner.is_alive()))
            .unwrap_or(0)
    })
}
