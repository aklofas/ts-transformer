//! JNI surface for `org.tstrans.rtp.H264Receiver` — the single-call convenience
//! wrapper for the RFC 6184 H.264-over-RTP receive path. Ports tst-py's
//! `bindings/python/src/rtp/h264_receiver.rs`.
//!
//! # Architecture
//!
//! `JniH264Receiver` boxes the Rust `H264Receiver` behind the standard
//! `HandleRegistry<T>`. Unlike `DemuxReceiver` there is NO byte-sink
//! registration and the inner value is NOT wrapped in `Arc<Mutex>` — the
//! single-iterator contract means one thread owns the receiver; any cross-thread
//! stop routes through the cancel handle (held separately so `close()` can fire
//! it without taking the resource lock). This matches tst-py's `PyH264Receiver`.
//!
//! `cancel` is held outside the registry slot (cloned at construction into a
//! `Box<dyn FnOnce>` cancel hook). A cross-thread `nClose` fires the hook BEFORE
//! taking the slot (waking any parked `nRecvAu`), then drops the receiver.
//!
//! # Error mapping (mirrors tst-py)
//!
//! - `TransportError::ExplicitClose`  → `RtpException(CANCELLED)`
//! - `TransportError::TooLarge`       → `RtpException(MALFORMED_PACKET)`
//! - `TransportError::Backpressure`   → `RtpException(TIMEOUT)` (recv deadline
//!   expired; retryable — the transport/session is still alive)
//! - other `TransportError`           → `RtpException(TRANSPORT)`
//! - `ConnectError`                   → `RtpException(TRANSPORT)`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JObjectArray, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_rtp::rtsp::client::RtspClient as RustRtspClient;
use tst_rtp::{H264Au, H264DepayConfig, H264Receiver, ParameterSetInjection};

use crate::handle::HandleRegistry;
use crate::jutil::build_socket_stats;

use super::errors::{connect_error_to_rtp, transport_error_to_rtp};

/// RTSP control plane retained by receivers created via
/// `RtspSession.intoH264Receiver()`. The Java session wrapper is CONSUMED at
/// conversion (NativeHandle contract item 3), so the receiver becomes the sole
/// owner of the control plane: retaining the `RtspClient` keeps the TCP control
/// connection + keepalive thread alive while AUs flow (dropping it would let
/// the server tear the session down mid-stream), and `nClose` performs the
/// best-effort TEARDOWN the consumed session wrapper can no longer issue.
pub(super) struct JniRtspControl {
    pub(super) client: Arc<Mutex<Option<RustRtspClient>>>,
    pub(super) torn_down: Arc<AtomicBool>,
}

/// Native backing for `org.tstrans.rtp.H264Receiver`. A single-iterator
/// wrapper: one thread owns the recv loop. The cancel handle is extracted at
/// construction and wired as the registry's cancel hook so `close()` wakes
/// a parked `nRecvAu` before taking the slot.
struct JniH264Receiver {
    inner: H264Receiver,
    /// `Some` only for RTSP-created receivers (see [`JniRtspControl`]);
    /// `None` for plain `listen()` receivers.
    rtsp: Option<JniRtspControl>,
}

/// Per-type leased-handle registry for `org.tstrans.rtp.H264Receiver`. Registers
/// a cancel hook so a cross-thread `close()` wakes a parked `recv_au`.
static REGISTRY: LazyLock<HandleRegistry<JniH264Receiver>> = LazyLock::new(HandleRegistry::new);

/// Build a registry handle from an already-constructed `H264Receiver`
/// (plain `nListen*` path — no RTSP control plane).
pub(crate) fn h264_receiver_handle_from_receiver(receiver: H264Receiver) -> jlong {
    insert_receiver(receiver, None)
}

/// Build a registry handle from an RTSP-session conversion: the receiver slot
/// additionally owns the session's control plane (see [`JniRtspControl`]).
/// Used by `RtspSession.nIntoH264Receiver`.
pub(super) fn h264_receiver_handle_from_rtsp_session(
    receiver: H264Receiver,
    control: JniRtspControl,
) -> jlong {
    insert_receiver(receiver, Some(control))
}

/// Extract the cancel handle, register it as BOTH the registry cancel hook (fired
/// by `close`) and the lock-free `nCancelHandle` target (readable while `recvAu`
/// holds the resource lock), and return the boxed handle as `jlong`.
fn insert_receiver(receiver: H264Receiver, rtsp: Option<JniRtspControl>) -> jlong {
    // Coerce Arc<RtpCancelHandle> to Arc<dyn TransportCancel + Send + Sync>
    // (RtpCancelHandle implements TransportCancel).
    let cancel: Arc<dyn tst_core::transport::TransportCancel + Send + Sync> =
        receiver.cancel_handle();
    let hook = Arc::clone(&cancel);
    let slot = JniH264Receiver {
        inner: receiver,
        rtsp,
    };
    REGISTRY.insert_full(slot, Some(Box::new(move || hook.cancel())), Some(cancel)) as jlong
}

// ─────────────────────────────────────────────────────────────────────────────
// `H264Receiver.nListen(url)` — default H264DepayConfig
// ─────────────────────────────────────────────────────────────────────────────

/// `H264Receiver.nListen(url)` — parse an `rtp://host:port?pt=N` URL, bind a
/// UDP socket, and return a handle. Mirrors tst-py `PyH264Receiver::listen` with
/// `config=None`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nListen<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };
        match H264Receiver::listen(&url_str) {
            Ok(receiver) => h264_receiver_handle_from_receiver(receiver),
            Err(e) => {
                connect_error_to_rtp(env, &e);
                0
            }
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// `H264Receiver.nListenWithConfig(...)` — explicit H264DepayConfig
// ─────────────────────────────────────────────────────────────────────────────

/// `H264Receiver.nListenWithConfig(url, payloadType, parameterSetInjection,
/// initialParameterSets, maxAuBytes)` — parse the URL, build the config from
/// caller-supplied fields, bind, and return a handle. Mirrors tst-py
/// `PyH264Receiver::listen` with an explicit config.
///
/// `initialParameterSets` is a `byte[][]` (Java `JObjectArray`); may be null
/// (treated as empty). Each element is a raw NALU byte array.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nListenWithConfig<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
    payload_type: jint,
    parameter_set_injection: jint,
    initial_parameter_sets: JObjectArray<'local>,
    max_au_bytes: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        // Build H264DepayConfig from the caller's fields (mirrors Python's kwarg path).
        let mut config = H264DepayConfig::default();
        config.payload_type = payload_type as u8;
        config.parameter_set_injection = match parameter_set_injection {
            1 => ParameterSetInjection::BeforeIdr,
            _ => ParameterSetInjection::None,
        };
        config.max_au_bytes = max_au_bytes as usize;

        // Read each NALU from the byte[][] (may be null / zero-length).
        if !initial_parameter_sets.is_null() {
            let len = match env.get_array_length(&initial_parameter_sets) {
                Ok(n) => n,
                Err(e) => {
                    let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                    return 0;
                }
            };
            for i in 0..len {
                let elem_obj = match env.get_object_array_element(&initial_parameter_sets, i) {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                        return 0;
                    }
                };
                if elem_obj.is_null() {
                    continue;
                }
                let arr = JByteArray::from(elem_obj);
                let bytes = match env.convert_byte_array(&arr) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                        return 0;
                    }
                };
                config.initial_parameter_sets.push(bytes);
            }
        }

        // `listen_with` parses the URL, overrides `config.payload_type` from `?pt=`,
        // and binds. Mirrors tst-py's listen_with path.
        let parsed = match tst_rtp::url::RtpUrl::parse(&url_str) {
            Ok(p) => p,
            Err(e) => {
                super::errors::throw_rtp(env, "TRANSPORT", &e.to_string());
                return 0;
            }
        };
        match H264Receiver::listen_with(&parsed, config) {
            Ok(receiver) => h264_receiver_handle_from_receiver(receiver),
            Err(e) => {
                connect_error_to_rtp(env, &e);
                0
            }
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// `H264Receiver.nRecvAu(handle)` — blocking recv
// ─────────────────────────────────────────────────────────────────────────────

/// `H264Receiver.nRecvAu(handle)` — block until the next H.264 Access Unit is
/// reassembled. Returns an `H264AccessUnit` Java object on success, `null` at EOS,
/// or throws on error. Mirrors tst-py `PyH264Receiver::recv_au`.
///
/// The registry lease holds the slot's resource lock for the duration of the
/// (possibly parked) `recv_au` call. A concurrent `close()` fires the cancel hook
/// (waking the parked recv) BEFORE it takes the resource; so `close()` is a safe
/// cross-thread stop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nRecvAu<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // `with_poisoning` holds the resource lock while recv_au runs (which may park
        // the calling thread). A concurrent `close()` fires the cancel hook first so
        // this call returns promptly then the resource is taken.
        let Some(result) = REGISTRY.with_poisoning(handle as u64, |jdr| jdr.inner.recv_au()) else {
            // Closed/absent — clean EOS: return null (the Java side returns null
            // from recvAu(), which the caller treats as end-of-stream).
            return JObject::null().into_raw();
        };

        match result {
            // EOS (cancel / clean RTSP teardown): return null
            Ok(None) => JObject::null().into_raw(),
            Ok(Some(au)) => build_h264_access_unit(env, &au),
            Err(e) => {
                transport_error_to_rtp(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// `H264Receiver.nRecvAuTimeout(handle, timeoutMs)` — per-call-deadline recv
// ─────────────────────────────────────────────────────────────────────────────

/// `H264Receiver.nRecvAuTimeout(handle, timeoutMs)` — block for at most
/// `timeoutMs` ms for the next H.264 Access Unit. `timeoutMs < 0` blocks
/// indefinitely via the plain `recv_au` path — byte-identical to `nRecvAu`, so
/// any persistent deadline armed by the `?recv_timeout=` URL knob still
/// applies. `timeoutMs >= 0` takes a one-shot `H264Receiver::recv_au_timeout`
/// override for this call only. Mirrors tst-py
/// `PyH264Receiver.recv_au(timeout_ms=...)`.
///
/// Unlike `RtpRecvTransport::recv_timeout`, `recv_au_timeout` reports deadline
/// expiry as `Err(TransportError::Backpressure)`, not `Ok(None)` — so no
/// hand-mapping is needed here: `transport_error_to_rtp` already maps
/// `Backpressure` to `RtpException(TIMEOUT)`, and `Ok(None)` stays EOS-only.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nRecvAuTimeout<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    timeout_ms: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(result) = REGISTRY.with_poisoning(handle as u64, |jdr| {
            if timeout_ms < 0 {
                jdr.inner.recv_au()
            } else {
                jdr.inner
                    .recv_au_timeout(Duration::from_millis(timeout_ms as u64))
            }
        }) else {
            return JObject::null().into_raw();
        };

        match result {
            // EOS (cancel / clean RTSP teardown): return null. NEVER reached on
            // deadline expiry — that's `Err(Backpressure)`, handled below.
            Ok(None) => JObject::null().into_raw(),
            Ok(Some(au)) => build_h264_access_unit(env, &au),
            Err(e) => {
                transport_error_to_rtp(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

/// Build a `org.tstrans.rtp.H264AccessUnit` Java object from a Rust
/// [`H264Au`]. Shared by `nRecvAu` and `nRecvAuTimeout`. Every field is
/// copied: `annexb` is a heap-copied `byte[]` (JDK&lt;22 rule — never a direct
/// `ByteBuffer` over Rust memory), `pts` is `long` (i64 ticks), `keyFrame` is
/// `boolean`, `rtpTimestamp` is `long` (u32 widened to i64). Returns null
/// (after throwing) on a JNI allocation/construction failure.
fn build_h264_access_unit(env: &mut JNIEnv, au: &H264Au) -> jobject {
    let annexb_arr = match env.byte_array_from_slice(&au.annexb) {
        Ok(a) => a,
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            return JObject::null().into_raw();
        }
    };
    let pts = au.pts.as_ticks();
    let key_frame: jboolean = u8::from(au.key_frame);
    let rtp_timestamp = i64::from(au.rtp_timestamp);

    // Construct `new H264AccessUnit(byte[], long, boolean, long)`.
    if let Err(e) = env.ensure_local_capacity(4) {
        let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
        return JObject::null().into_raw();
    }
    match env.new_object(
        "org/tstrans/rtp/H264AccessUnit",
        "([BJZJ)V",
        &[
            JValue::Object(&annexb_arr.into()),
            JValue::Long(pts),
            JValue::Bool(key_frame),
            JValue::Long(rtp_timestamp),
        ],
    ) {
        Ok(o) => o.into_raw(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            JObject::null().into_raw()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stats natives
// ─────────────────────────────────────────────────────────────────────────────

/// `H264Receiver.nDepayStats(handle)` — snapshot the depacketizer counters.
/// Returns an `H264DepayStats` Java record. Throws `IllegalStateException` on
/// a closed handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nDepayStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(s) = REGISTRY.with(handle as u64, |jdr| jdr.inner.depay_stats()) else {
            crate::error::throw_closed(env, "H264Receiver");
            return JObject::null().into_raw();
        };
        // Construct `new H264DepayStats(long x9)`.
        match env.new_object(
            "org/tstrans/rtp/H264DepayStats",
            "(JJJJJJJJJ)V",
            &[
                JValue::Long(s.aus_emitted as i64),
                JValue::Long(s.aus_dropped as i64),
                JValue::Long(s.aus_dropped_oversize as i64),
                JValue::Long(s.packets_discarded as i64),
                JValue::Long(s.nalus_discarded as i64),
                JValue::Long(s.seq_gaps as i64),
                JValue::Long(s.duplicate_packets as i64),
                JValue::Long(s.parameter_set_updates as i64),
                JValue::Long(s.ssrc_changes as i64),
            ],
        ) {
            Ok(o) => o.into_raw(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                JObject::null().into_raw()
            }
        }
    })
}

/// `H264Receiver.nRtpStats(handle)` — RTP protocol-level malformed-packet counter.
/// Returns an `RtpStats` Java record.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nRtpStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(s) = REGISTRY.with(handle as u64, |jdr| jdr.inner.rtp_stats()) else {
            crate::error::throw_closed(env, "H264Receiver");
            return JObject::null().into_raw();
        };
        match env.new_object(
            "org/tstrans/rtp/RtpStats",
            "(J)V",
            &[JValue::Long(s.malformed_packets as i64)],
        ) {
            Ok(o) => o.into_raw(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                JObject::null().into_raw()
            }
        }
    })
}

/// `H264Receiver.nSocketStats(handle)` — wire-level throughput statistics.
/// Returns an `org.tstrans.rtp.SocketStats` record (direct, NOT wrapped in
/// `TransportStats` — mirrors the Rust/Python asymmetry documented in the Java
/// class's Javadoc).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(s) = REGISTRY.with(handle as u64, |jdr| jdr.inner.socket_stats()) else {
            crate::error::throw_closed(env, "H264Receiver");
            return JObject::null().into_raw();
        };
        match build_socket_stats(env, "org/tstrans/rtp/SocketStats", &s) {
            Ok(o) => o.into_raw(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                JObject::null().into_raw()
            }
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Misc natives
// ─────────────────────────────────────────────────────────────────────────────

/// `H264Receiver.nLocalAddr(handle)` — local UDP socket address as
/// `"host:port"` string, or `null` for the TCP-interleaved (RTSP) path.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nLocalAddr<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(addr) = REGISTRY.with(handle as u64, |jdr| jdr.inner.local_addr()) else {
            crate::error::throw_closed(env, "H264Receiver");
            return JObject::null().into_raw();
        };
        match addr {
            None => JObject::null().into_raw(),
            Some(a) => {
                let s = a.to_string();
                match env.new_string(&s) {
                    Ok(js) => JObject::from(js).into_raw(),
                    Err(e) => {
                        let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                        JObject::null().into_raw()
                    }
                }
            }
        }
    })
}

/// `H264Receiver.nCancelHandle(handle)` — clone the cancel handle into a
/// `org.tstrans.rtp.CancelHandle` registry slot and return the slot id as
/// `jlong`. Returns 0 on a closed receiver.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Lock-free: the target was captured at registration (see
        // `insert_receiver`), so this returns while `recvAu` is parked.
        let Some(inner) = REGISTRY.cancel_target(handle as u64) else {
            crate::error::throw_closed(env, "H264Receiver");
            return 0;
        };
        crate::rtp::JniRtpCancel { inner }.into_handle()
    })
}

/// `H264Receiver.nEndReason(handle)` — why the receive session ended, as the
/// wire-pinned ordinal (see `end_reason`'s module doc); `-1` if it hasn't
/// ended yet, or on a closed/absent handle (the closed case never reaches
/// this native — `H264Receiver.endReason()` reads the Java-side snapshot
/// once `peekHandle()` is 0). Unlike `Receiver`/`DemuxReceiver`,
/// `H264Receiver` has no `end_reason_handle()` — this reads the live
/// receiver's own `&self` getter directly.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nEndReason(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    crate::panic::jni_catch(&mut env, -1, |_env| {
        REGISTRY
            .with(handle as u64, |jdr| {
                super::end_reason::end_reason_ordinal(jdr.inner.end_reason().as_ref())
            })
            .unwrap_or(-1)
    })
}

/// `H264Receiver.nEndDetail(handle)` — free-text detail for `nEndReason`;
/// `null` for a detail-less reason, "hasn't ended yet", or a closed/absent
/// handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nEndDetail<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let detail = REGISTRY
            .with(handle as u64, |jdr| {
                jdr.inner
                    .end_reason()
                    .and_then(|r| super::end_reason::end_reason_detail(&r).map(str::to_owned))
            })
            .flatten();
        match detail {
            Some(d) => match env.new_string(&d) {
                Ok(s) => s.into(),
                Err(_) => JObject::null(),
            },
            None => JObject::null(),
        }
    })
}

/// `H264Receiver.nClose(handle)` — cancel-first (wakes a parked recv), then take
/// + close the inner receiver and free the box. Atomic + idempotent.
///
/// For RTSP-created receivers the slot also owns the session control plane
/// (see [`JniRtspControl`]): best-effort TEARDOWN fires first (so the server
/// stops streaming), then the data plane closes — mirroring
/// `RtspSession.nClose`'s teardown contract.
///
/// Returns the close-time `EndReasonSnapshot` (see `end_reason`'s module
/// doc) — read here, right after `slot.inner.close()` records `Cancelled`
/// (first-writer-wins) and before `slot` drops, because the registry entry
/// (and any further `nEndReason`/`nEndDetail` call on this handle) is gone
/// once this function returns. `null` only on a JNI allocation failure
/// building the snapshot (`nativeClose` null-checks before touching it).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_H264Receiver_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // `REGISTRY.close` fires the cancel hook FIRST (waking any parked recv_au
        // WITHOUT taking the resource lock), THEN takes + drops the slot under the
        // lock (blocking briefly until the woken recv releases it). Atomic +
        // idempotent: a double close finds the id gone → no-op.
        let reason = if let Some(mut slot) = REGISTRY.close(handle as u64) {
            if let Some(ctrl) = slot.rtsp.take() {
                if !ctrl.torn_down.load(Ordering::Relaxed) {
                    if let Ok(mut guard) = ctrl.client.lock() {
                        if let Some(c) = guard.as_mut() {
                            let _ = c.teardown(); // best-effort
                        }
                    }
                    ctrl.torn_down.store(true, Ordering::Relaxed);
                }
            }
            slot.inner.close();
            slot.inner.end_reason()
        } else {
            None
        };
        super::end_reason::build_close_snapshot(env, reason)
            .map(|obj| obj.into_raw())
            .unwrap_or(std::ptr::null_mut())
    })
}
