//! JNI exports for `org.tstrans.srt.ManagedSender` and
//! `org.tstrans.srt.ManagedReceiver` — the auto-reconnect basic-bytes wrappers.
//!
//! Ported from tst-py's `bindings/python/src/srt/managed_basic.rs`. The send
//! side wraps `tst_pipeline::Sender<ManagedTransport<SrtTransport>>`; the recv
//! side wraps `tst_pipeline::Receiver<ManagedRecvTransport<SrtTransport>>`. On
//! any Broken/Closed event the captured URL is rerun through a reconnect
//! factory under the configured `ReconnectPolicy`.
//!
//! Handle lifecycle mirrors `transport.rs`:
//! - `nFromUrl` opens through `tst_srt::shells::managed_{sender,receiver}_from_url`
//!   — the ONE open path (ARCH-01): the initial dial, the reconnect factory,
//!   the attempt/success counters and the cancel handle are composed in
//!   tst-srt, once, for every binding. The returned `ManagedHandles` (plus the
//!   sender's `ManagedStatsHandle`) become the entry's `Owned` SNAPSHOT, so
//!   every counter/stats/cancel read is lock-free.
//! - Per-call methods go through `with_mut` / `with_ref`; a `HandleState`
//!   becomes the one Java mapping in `error::throw_handle_state`.
//! - `nClose` cancels first, then takes + tears down, via `OwnedRegistry::close`:
//!   a parked `send`/`recv` ends with `SrtException(CLOSED)` instead of holding
//!   `close()` for the reconnect budget.
//!
//! ## Stats drift (intentional — mirrors tst-py)
//!
//! `nSrtStats` ALWAYS throws `SrtException(IO)` on both wrappers:
//! `ManagedTransport` / `ManagedRecvTransport` do not expose the SRT-rich
//! 17-field shape (no accessor in tst-pipeline). Callers use `nSocketStats`.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong};
use tst_pipeline::binding::{BindingErrorKind, ManagedHandles, Owned};
use tst_pipeline::{
    ManagedRecvTransport, ManagedTransport, Receiver as PlReceiver, Sender as PlSender,
    SenderConfig,
};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use super::ManagedSenderSnapshot;
use super::errors::{throw_receiver_error, throw_sender_error, throw_srt};
use super::stats::build_managed_transport_stats;
use crate::error::throw_handle_state;
use crate::handle::OwnedRegistry;
use crate::jutil::build_socket_stats;

// -----------------------------------------------------------------------
// ManagedSender  (org.tstrans.srt.ManagedSender)
//
// handle = Box<PlSender<ManagedTransport<SrtTransport>>>
// -----------------------------------------------------------------------

/// Backing state for `ManagedSender`: just the shell. The reconnect/gap
/// telemetry, the cancel target and the attempt counters live in the entry's
/// `Owned` snapshot ([`ManagedSenderSnapshot`]), outside the slot.
struct JniManagedSender {
    inner: PlSender<ManagedTransport<SrtTransport>>,
}

/// Per-type `Owned`-backed registry for `org.tstrans.srt.ManagedSender`.
static REGISTRY_SENDER: LazyLock<OwnedRegistry<JniManagedSender, ManagedSenderSnapshot>> =
    LazyLock::new(OwnedRegistry::new);

/// Allocate a `ManagedSender` from an SRT caller-mode URL + the 8 flattened
/// reconnect-policy args. Returns a `jlong` handle on success; throws
/// `SrtException` and returns 0 on any error.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nFromUrl(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
    max_attempts_present: jboolean,
    max_attempts: jint,
    backoff_kind: jint,
    backoff_base_ms: jlong,
    backoff_max_ms: jlong,
    gap_buffer_capacity: jint,
    overflow_policy: jint,
    mode: jint,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        // Validate URL up-front so a malformed URL / wrong mode raises
        // CONFIG_INVALID before we materialize the factory closure (otherwise the
        // same failure would surface as a Broken from the factory, the wrong kind
        // for a caller-misconfigured URL).
        let parsed = match SrtUrl::parse(&url_str) {
            Ok(p) => p,
            Err(e) => {
                super::errors::url_error(env, &e);
                return 0;
            }
        };
        if parsed.mode != Mode::Caller {
            let msg = format!(
                "ManagedSender.fromUrl requires mode=caller (default); got mode={:?}",
                parsed.mode
            );
            throw_srt(env, BindingErrorKind::ConfigInvalid, &msg);
            return 0;
        }

        let Some(policy) = super::build_reconnect_policy(
            env,
            max_attempts_present != 0,
            max_attempts,
            backoff_kind,
            backoff_base_ms,
            backoff_max_ms,
            gap_buffer_capacity,
            overflow_policy,
            mode,
        ) else {
            return 0;
        };

        // One open path (ARCH-01 / ARCH-08): the initial dial, the reconnect
        // factory (`SrtUrl::connect` per attempt), the attempt counter and the
        // cancel/stats/reconnect handles are composed in tst-srt, once.
        let (inner, handles, stats) = match tst_srt::shells::managed_sender_from_url(
            &parsed,
            policy,
            SenderConfig::default(),
        ) {
            Ok(triple) => triple,
            Err(e) => {
                // Initial connect failure: CONNECT_FAILED (was BROKEN — the
                // old factory relabelled it; now the same kind the other
                // three managed shells already reported).
                super::errors::srt_error(env, e);
                return 0;
            }
        };
        let cancel = Arc::clone(&handles.cancel);
        REGISTRY_SENDER.insert(Owned::new(
            JniManagedSender { inner },
            cancel,
            ManagedSenderSnapshot { handles, stats },
        )) as jlong
    })
}

/// Send pre-muxed TS bytes through the managed sender. Throws `SrtException` on
/// transport/framing failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nSendBytes(
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

        match REGISTRY_SENDER.with_mut(handle as u64, |jstruct| jstruct.inner.send_ts(&bytes)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_sender_error(env, &e),
            Err(state) => throw_handle_state(env, "ManagedSender", &state),
        }
    })
}

/// Flush any buffered partial TS bundle. Throws `SrtException` on failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nFlush(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        match REGISTRY_SENDER.with_mut(handle as u64, |jstruct| jstruct.inner.flush()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_sender_error(env, &e),
            Err(state) => throw_handle_state(env, "ManagedSender", &state),
        }
    })
}

/// Obtain a cancel handle for this managed sender. Returns a `jlong` handle on
/// success; throws `IllegalStateException` and returns 0 if absent.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free: the view is the `Owned` entry itself, so this returns
        // even while `send` is parked on the slot. Closed handle → 0
        // (no throw, matching the original contract).
        REGISTRY_SENDER
            .cancel_view(handle as u64)
            .map_or(0, super::cancel_view_handle)
    })
}

/// Return a `SocketStats` record from the current inner transport. Returns null
/// on JNI builder error (non-fatal; no throw). Uses `unwrap_or_default` so a
/// mid-reconnect sender yields a zeroed snapshot.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Ok(stats) = REGISTRY_SENDER.with_ref(handle as u64, |jstruct| {
            jstruct.inner.socket_stats().unwrap_or_default()
        }) else {
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/srt/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// SRT-rich stats are NOT available on a managed sender — this ALWAYS throws
/// `SrtException(IO)`, mirroring tst-py's `PyManagedSender::srt_stats`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nSrtStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    _handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        throw_srt(
            env,
            BindingErrorKind::SrtIo,
            "srt_stats not available on ManagedSender (use socketStats); a future \
             tst-pipeline accessor will expose the SRT-rich shape",
        );
        JObject::null()
    })
}

/// Reconnect/gap telemetry: attempts, successes, current gap-buffer depth, and
/// drop counters. Throws `SrtException(IO)` if the internal gap-buffer lock is
/// poisoned — a read-only telemetry path must not panic. Throws
/// `IllegalStateException` (via `throw_closed`) on a closed handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nReconnectStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        // Lock-free: `ManagedStatsHandle` lives in the snapshot, not the slot,
        // so this answers while a `sendBytes()` is parked in a reconnect.
        let Some(maybe_stats) = REGISTRY_SENDER.snapshot(handle as u64, |s| s.stats.stats()) else {
            crate::error::throw_closed(env, "ManagedSender");
            return JObject::null();
        };
        let Some(stats) = maybe_stats else {
            throw_srt(
                env,
                BindingErrorKind::SrtIo,
                "reconnect stats unavailable: gap lock poisoned",
            );
            return JObject::null();
        };
        match build_managed_transport_stats(env, &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// Close the managed sender, deallocating the native box. `close()` latches the
/// cancel flag (so any in-flight reconnect loop exits) and tears down the inner.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut jstruct) = REGISTRY_SENDER.close(handle as u64) {
            jstruct.inner.close();
        }
    })
}

/// Return whether the managed sender holds a live transport.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedSender_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_SENDER
            .with_ref(handle as u64, |jstruct| u8::from(jstruct.inner.is_alive()))
            .unwrap_or(0)
    })
}

// -----------------------------------------------------------------------
// ManagedReceiver  (org.tstrans.srt.ManagedReceiver)
//
// handle = Box<JniManagedReceiver>
// -----------------------------------------------------------------------

/// Backing state for `ManagedReceiver`: just the shell. The reconnect counters,
/// the cancel target and the end-reason cell live in the entry's `Owned`
/// snapshot ([`ManagedHandles`]), outside the slot, so they answer while a
/// `recvBytes()` is parked — including a listener-mode re-accept.
struct JniManagedReceiver {
    inner: PlReceiver<ManagedRecvTransport<SrtTransport>>,
}

/// Per-type `Owned`-backed registry for `org.tstrans.srt.ManagedReceiver`.
static REGISTRY_RECEIVER: LazyLock<OwnedRegistry<JniManagedReceiver, ManagedHandles>> =
    LazyLock::new(OwnedRegistry::new);

/// Allocate a `ManagedReceiver` from an SRT listener-mode URL + the 8 flattened
/// reconnect-policy args. Returns a `jlong` handle on success; throws
/// `SrtException` and returns 0 on any error.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nFromUrl(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
    max_attempts_present: jboolean,
    max_attempts: jint,
    backoff_kind: jint,
    backoff_base_ms: jlong,
    backoff_max_ms: jlong,
    gap_buffer_capacity: jint,
    overflow_policy: jint,
    mode: jint,
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
                super::errors::url_error(env, &e);
                return 0;
            }
        };
        if parsed.mode != Mode::Listener {
            let msg = format!(
                "ManagedReceiver.fromUrl requires mode=listener; got mode={:?}",
                parsed.mode
            );
            throw_srt(env, BindingErrorKind::ConfigInvalid, &msg);
            return 0;
        }

        let Some(policy) = super::build_reconnect_policy(
            env,
            max_attempts_present != 0,
            max_attempts,
            backoff_kind,
            backoff_base_ms,
            backoff_max_ms,
            gap_buffer_capacity,
            overflow_policy,
            mode,
        ) else {
            return 0;
        };

        // One open path (ARCH-01 / ARCH-08): the initial bind+accept, the
        // reconnect factory (whose re-accept is reachable through the managed
        // `FactoryCancel` slot), the attempt/success counters and the cancel
        // handle are composed in tst-srt, once.
        let (inner, handles) = match tst_srt::shells::managed_receiver_from_url(&parsed, policy) {
            Ok(pair) => pair,
            Err(e) => {
                super::errors::srt_error(env, e);
                return 0;
            }
        };
        // Cancel-on-close: `OwnedRegistry::close` fires the cancel before taking
        // the slot, so a `recvBytes()` parked on another thread ends with CLOSED
        // instead of holding `close()` hostage (tst-py / C ABI contract).
        // No `with_end_reason`: `ManagedReceiver` exposes no `endReason()` native,
        // and A3 documents that the plain `Receiver` never RECORDS one either
        // (`ManagedHandles::end_reason` is a fresh, never-set handle for every
        // shell but `ManagedDemuxReceiver`). Attaching it would advertise a
        // capability that does not exist; the cell stays reachable through the
        // `ManagedHandles` snapshot if a rider ever adds the native.
        let cancel = Arc::clone(&handles.cancel);
        REGISTRY_RECEIVER.insert(Owned::new(JniManagedReceiver { inner }, cancel, handles)) as jlong
    })
}

/// Receive one TS packet (188 bytes). Returns the packet as a `jbyteArray` on
/// success; throws `SrtException` and returns null on failure. `maxLen` is
/// accepted for API symmetry but a single `next_packet` quantum is returned.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nRecvBytes(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    _max_len: jint,
) -> jbyteArray {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let res = match REGISTRY_RECEIVER
            .with_mut(handle as u64, |jstruct| jstruct.inner.next_packet())
        {
            Ok(res) => res,
            Err(state) => {
                throw_handle_state(env, "ManagedReceiver", &state);
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

/// Total factory invocations since construction (ARCH-08 — was the success
/// counter). Lock-free: read off the `Owned` snapshot, so it answers while
/// `recvBytes()` is parked in a re-accept that has not completed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nReconnectAttempts(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_RECEIVER
            .snapshot(handle as u64, |h| {
                h.attempts.load(Ordering::Acquire) as jlong
            })
            .unwrap_or(0)
    })
}

/// Obtain a cancel handle for this managed receiver.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free: the view is the `Owned` entry itself, so this returns
        // even while `recvBytes` is parked on the slot. Closed handle → 0
        // (no throw, matching the original contract).
        REGISTRY_RECEIVER
            .cancel_view(handle as u64)
            .map_or(0, super::cancel_view_handle)
    })
}

/// Return a `SocketStats` record from the current inner transport.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Ok(stats) = REGISTRY_RECEIVER.with_ref(handle as u64, |jstruct| {
            jstruct.inner.socket_stats().unwrap_or_default()
        }) else {
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/srt/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// SRT-rich stats are NOT available on a managed receiver — this ALWAYS throws
/// `SrtException(IO)`, mirroring tst-py's `PyManagedReceiver::srt_stats`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nSrtStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    _handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        throw_srt(
            env,
            BindingErrorKind::SrtIo,
            "srt_stats not available on ManagedReceiver (use socketStats); a future \
             tst-pipeline accessor will expose the SRT-rich shape",
        );
        JObject::null()
    })
}

/// Close the managed receiver, deallocating the native box.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut jstruct) = REGISTRY_RECEIVER.close(handle as u64) {
            jstruct.inner.close();
        }
    })
}

/// Return whether the managed receiver holds a live shell.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_RECEIVER
            .with_ref(handle as u64, |jstruct| u8::from(jstruct.inner.is_alive()))
            .unwrap_or(0)
    })
}
