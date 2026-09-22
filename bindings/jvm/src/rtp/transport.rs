//! JNI exports for `org.tstrans.rtp.Sender` and `org.tstrans.rtp.Receiver`.
//!
//! Each Java class is handle-backed by an `OwnedRegistry` entry:
//! - `Sender`   → `Owned<SendHalf<tst_rtp::RtpTransport>>`.
//! - `Receiver` → `Owned<JniRtpReceiver, StreamEndReasonHandle>` (the transport
//!   plus a reusable recv scratch buffer, mirroring tst-py's
//!   `PyReceiver.scratch`; the end-reason cell is the lock-free snapshot).
//!
//! Unlike the srt JVM surface (which wraps `tst_pipeline::Sender/Receiver`),
//! the rtp surface wraps the transport DIRECTLY and calls the
//! `Transport`/`RecvTransport` trait methods — exactly as tst-py's
//! `bindings/python/src/rtp/transport.rs` does.

use std::sync::LazyLock;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jbyteArray, jint, jlong, jobject};
use tst_core::transport::{RecvTransport, Transport};
use tst_rtp::builder::RtpRecvSocketBuilder;
use tst_rtp::{RtpRecvTransport, RtpSocketBuilder, RtpTransport, StreamEndReasonHandle};

use tst_pipeline::binding::{BindingErrorKind, Owned, SendHalf};

use super::errors::{connect_error, rtp_url_error, throw_rtp, transport_error};
use crate::error::throw_handle_state;
use crate::handle::OwnedRegistry;
use crate::jutil::build_socket_stats;

struct JniRtpReceiver {
    inner: RtpRecvTransport,
    /// Pulled from `inner.end_reason_handle()` at construction. `nClose` reads
    /// it after `inner.close()` (which records `Cancelled` if nothing else
    /// claimed the slot) to build the close-time snapshot — see `end_reason`'s
    /// module doc for why that has to happen inside `nClose` itself. The
    /// LOCK-FREE getters read the entry's `Owned` snapshot, another clone of
    /// this same `Arc<OnceLock>` cell.
    end_reason: StreamEndReasonHandle,
    scratch: Vec<u8>,
}

/// Per-type `Owned`-backed registries. `OwnedRegistry::close` cancels first, so
/// a cross-thread `close()` wakes a parked `send`/`recv` before taking the slot.
/// The sender holds A1's `SendHalf` newtype so all three bindings carry the same
/// entry type for a raw transport.
static REGISTRY_SENDER: LazyLock<OwnedRegistry<SendHalf<RtpTransport>>> =
    LazyLock::new(OwnedRegistry::new);
/// `S = StreamEndReasonHandle`: the construction-time cell `nEndReason` /
/// `nEndDetail` read without the slot a parked `recv` holds.
static REGISTRY_RECEIVER: LazyLock<OwnedRegistry<JniRtpReceiver, StreamEndReasonHandle>> =
    LazyLock::new(OwnedRegistry::new);

/// Unbox a nullable `java.lang.Long` SSRC arg into `Option<u32>`. Returns
/// `Err(())` (after throwing IllegalArgumentException) on out-of-range values.
fn unbox_ssrc(env: &mut JNIEnv, obj: &JObject) -> Result<Option<u32>, ()> {
    if obj.is_null() {
        return Ok(None);
    }
    let v = match env.call_method(obj, "longValue", "()J", &[]) {
        Ok(jv) => jv.j().unwrap_or(-1),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            return Err(());
        }
    };
    if v < 0 || v > i64::from(u32::MAX) {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("ssrc out of u32 range: {v}"),
        );
        return Err(());
    }
    Ok(Some(v as u32))
}

// ── Sender (org.tstrans.rtp.Sender) ────────────────────────────────────────

/// Allocate a `Sender` from an `rtp://host:port` URL. Returns a `jlong` handle;
/// throws `RtpException` and returns 0 on error. `pktSize` is the UDP datagram
/// size; `ssrcBoxed` is a nullable `java.lang.Long` SSRC (random when null).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Sender_nFromUrl(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
    pkt_size: jint,
    ssrc_boxed: JObject<'_>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };
        let ssrc = match unbox_ssrc(env, &ssrc_boxed) {
            Ok(v) => v,
            Err(()) => return 0,
        };

        let mut builder = match RtpSocketBuilder::from_url(&url_str) {
            Ok(b) => b,
            Err(e) => {
                rtp_url_error(env, &e);
                return 0;
            }
        };
        builder.pkt_size(pkt_size.max(0) as usize);
        if let Some(s) = ssrc {
            builder.ssrc(s);
        }
        let inner = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                connect_error(env, e);
                return 0;
            }
        };
        // One cancel handle, two registry roles: the close hook (wakes a parked
        // `send` on `close()`) and the lock-free target `nCancelHandle` reads
        // while that same `send` holds the resource lock.
        let cancel = match super::rtp_cancel(inner.cancel_handle(), "RtpTransport") {
            Ok(c) => c,
            Err(e) => {
                // `Internal` is deliberately NOT an `RtpException.Kind` member:
                // a transport with no cancel handle is a tst-rtp bug, not a
                // transport outcome. Same shape as the receiver and mux sender.
                let _ = env.throw_new("java/lang/RuntimeException", e.detail);
                return 0;
            }
        };
        REGISTRY_SENDER.insert(Owned::new(SendHalf(inner), cancel, ())) as jlong
    })
}

/// Send one MPEG-TS payload chunk over RTP. Throws `RtpException` on failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Sender_nSend(
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
        match REGISTRY_SENDER.with_mut(handle as u64, |w| w.0.send_bytes(&bytes)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => transport_error(env, &e),
            Err(state) => throw_handle_state(env, "Sender", &state),
        }
    })
}

/// Return a `SocketStats` record. Returns null on JNI builder error (non-fatal).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Sender_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Ok(stats) =
            REGISTRY_SENDER.with_ref(handle as u64, |w| w.0.socket_stats().unwrap_or_default())
        else {
            return JObject::null();
        };
        build_socket_stats(env, "org/tstrans/rtp/SocketStats", &stats)
            .unwrap_or_else(|_| JObject::null())
    })
}

/// Return a cancel-handle `jlong` (a `CancelView` over the `Owned` entry).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Sender_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free (see the registration comment); closed handle → 0.
        REGISTRY_SENDER
            .cancel_view(handle as u64)
            .map_or(0, super::cancel_view_handle)
    })
}

/// Close the Sender, freeing the native box. The cancel hook fires first (waking
/// a parked `send`) before the resource is taken.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Sender_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    // Atomic + idempotent: cancel hook wakes a parked send, then take + teardown.
    crate::panic::jni_catch(&mut env, (), |_env| {
        if let Some(mut w) = REGISTRY_SENDER.close(handle as u64) {
            w.0.close();
        }
    })
}

// ── Receiver (org.tstrans.rtp.Receiver) ────────────────────────────────────

/// Allocate a `Receiver` bound to an `rtp://host:port` URL. Returns a `jlong`
/// handle; throws `RtpException` and returns 0 on error.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nFromUrl(
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
        let builder = match RtpRecvSocketBuilder::from_url(&url_str) {
            Ok(b) => b,
            Err(e) => {
                rtp_url_error(env, &e);
                return 0;
            }
        };
        let inner = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                connect_error(env, e);
                return 0;
            }
        };
        let scratch_len = inner.max_payload();
        let cancel = match super::rtp_cancel(inner.cancel_handle(), "RtpRecvTransport") {
            Ok(c) => c,
            Err(e) => {
                // See `rtp_cancel`: `Internal` is not an `RtpException.Kind`
                // member — a transport with no cancel handle is a tst-rtp bug.
                let _ = env.throw_new("java/lang/RuntimeException", e.detail);
                return 0;
            }
        };
        // Pulled BEFORE `inner` is boxed into the registry entry alongside
        // it — same construction-time-capture shape as `cancel` above (and
        // the D5 `stats_handle` precedent in srt::managed_basic). `cancel`
        // serves as both the close hook and the lock-free `nCancelHandle`
        // target (readable while `recv` holds the resource lock).
        let end_reason = inner.end_reason_handle();
        REGISTRY_RECEIVER.insert(Owned::new(
            JniRtpReceiver {
                inner,
                end_reason: end_reason.clone(),
                scratch: vec![0u8; scratch_len],
            },
            cancel,
            end_reason,
        )) as jlong
    })
}

/// Receive one MPEG-TS payload chunk (RTP header already stripped). Returns the
/// bytes as a `jbyteArray`; throws `RtpException` and returns null on failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nRecv(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jbyteArray {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // `recv_bytes` may park; the closure holds the resource lock for its
        // duration. A concurrent `close()` fires the cancel hook (waking the recv)
        // before taking the lock. We copy the received bytes OUT of `scratch` inside
        // the closure so the Java array is built after the lease releases.
        let res = match REGISTRY_RECEIVER.with_mut(handle as u64, |w| {
            let n = w.inner.recv_bytes(w.scratch.as_mut_slice())?;
            Ok::<Vec<u8>, _>(w.scratch[..n].to_vec())
        }) {
            Ok(res) => res,
            Err(state) => {
                throw_handle_state(env, "Receiver", &state);
                return std::ptr::null_mut();
            }
        };
        match res {
            Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                // Allocating the Java array failed (effectively OOM). Throw rather
                // than return null silently, so `recv()` always yields bytes or an
                // RtpException — matching tst-py's contract (it never returns None).
                Err(_) => {
                    // A JVM allocation failure, not a transport outcome: the
                    // same shape every other JNI failure in this file uses.
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        "failed to allocate received packet",
                    );
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                transport_error(env, &e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Receive one MPEG-TS payload chunk (RTP header already stripped), bounded by
/// a per-call deadline. `timeout_ms < 0` blocks indefinitely via the plain
/// `recv_bytes` path — byte-identical to `nRecv`, so any persistent deadline
/// armed by the `?recv_timeout=` URL knob still applies. `timeout_ms >= 0`
/// takes a one-shot `RtpRecvTransport::recv_timeout` override for this call
/// only. Mirrors tst-py `PyReceiver.recv(timeout_ms=...)`.
///
/// `recv_timeout`'s `Ok(None)` return means the deadline elapsed (the
/// below, since that outcome never reaches `transport_error` (which
/// only sees `Err`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nRecvTimeout(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    timeout_ms: jlong,
) -> jbyteArray {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let res = match REGISTRY_RECEIVER.with_mut(handle as u64, |w| {
            if timeout_ms < 0 {
                let n = w.inner.recv_bytes(w.scratch.as_mut_slice())?;
                Ok(Some(w.scratch[..n].to_vec()))
            } else {
                let dur = Duration::from_millis(timeout_ms as u64);
                match w.inner.recv_timeout(w.scratch.as_mut_slice(), dur)? {
                    Some(n) => Ok(Some(w.scratch[..n].to_vec())),
                    None => Ok(None),
                }
            }
        }) {
            Ok(res) => res,
            Err(state) => {
                throw_handle_state(env, "Receiver", &state);
                return std::ptr::null_mut();
            }
        };
        match res {
            Ok(Some(bytes)) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                // Allocating the Java array failed (effectively OOM). Throw rather
                // than return null silently, matching `nRecv`.
                Err(_) => {
                    // A JVM allocation failure, not a transport outcome: the
                    // same shape every other JNI failure in this file uses.
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        "failed to allocate received packet",
                    );
                    std::ptr::null_mut()
                }
            },
            Ok(None) => {
                // A deadline that elapsed with the transport alive IS backpressure
                // at the Rust level — one kind for one meaning (was TIMEOUT).
                throw_rtp(env, BindingErrorKind::Backpressure, "recv deadline elapsed");
                std::ptr::null_mut()
            }
            Err(e) => {
                transport_error(env, &e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Return a `SocketStats` record. Returns null on JNI builder error (non-fatal).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Ok(stats) = REGISTRY_RECEIVER.with_ref(handle as u64, |w| {
            w.inner.socket_stats().unwrap_or_default()
        }) else {
            return JObject::null();
        };
        build_socket_stats(env, "org/tstrans/rtp/SocketStats", &stats)
            .unwrap_or_else(|_| JObject::null())
    })
}

/// Return a cancel-handle `jlong` (a `CancelView` over the `Owned` entry).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free (see the registration comment); closed handle → 0.
        REGISTRY_RECEIVER
            .cancel_view(handle as u64)
            .map_or(0, super::cancel_view_handle)
    })
}

/// Why the receive session ended, or `-1` if it hasn't ended yet (or ended
/// through a path this arc doesn't instrument). See `end_reason`'s module
/// doc for the wire-ordinal convention. Returns `-1` on a closed/absent
/// handle rather than throwing (matches `endReason()`'s post-close-snapshot
/// contract — the closed case never reaches this native at all, since
/// `Receiver.endReason()` reads the Java-side snapshot once `peekHandle()`
/// is 0).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nEndReason(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    crate::panic::jni_catch(&mut env, -1, |_env| {
        // Lock-free: the cell is the entry's `Owned` snapshot, so this answers
        // while a `recv` is parked on the slot (spec §3.2's getter rule).
        REGISTRY_RECEIVER
            .snapshot(handle as u64, |h| {
                super::end_reason::end_reason_ordinal(h.get().as_ref())
            })
            .unwrap_or(-1)
    })
}

/// Free-text detail for `nEndReason` — the `msg` carried by
/// `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`; `null` for
/// every other reason (including "hasn't ended yet" and a closed/absent
/// handle).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nEndDetail<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let detail = REGISTRY_RECEIVER
            .snapshot(handle as u64, |h| {
                h.get()
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

/// Close the Receiver, freeing the native box. The cancel hook fires first
/// (waking a parked `recv`) before the resource is taken — mirrors tst-py
/// `PyReceiver.close`.
///
/// Returns the close-time `EndReasonSnapshot` (see `end_reason`'s module
/// doc) — computed here, from the resource this call already exclusively
/// owns, because the registry entry (and with it any further
/// `nEndReason`/`nEndDetail` calls on this handle) is gone once this
/// function returns. `null` only on a JNI allocation failure building the
/// snapshot (`nativeClose` null-checks before touching it).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jobject {
    // Atomic + idempotent: cancel hook wakes a parked recv, then take + teardown.
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let reason = if let Some(mut w) = REGISTRY_RECEIVER.close(handle as u64) {
            w.inner.close();
            w.end_reason.get()
        } else {
            None
        };
        super::end_reason::build_close_snapshot(env, reason)
            .map(|obj| obj.into_raw())
            .unwrap_or(std::ptr::null_mut())
    })
}
