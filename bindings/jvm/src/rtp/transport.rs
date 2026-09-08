//! JNI exports for `org.tstrans.rtp.Sender` and `org.tstrans.rtp.Receiver`.
//!
//! Each Java class is handle-backed by a `Box`:
//! - `Sender`   → `Box<JniRtpSender>`   (wraps `tst_rtp::RtpTransport`).
//! - `Receiver` → `Box<JniRtpReceiver>` (wraps `tst_rtp::RtpRecvTransport` + a
//!   reusable recv scratch buffer, mirroring tst-py's `PyReceiver.scratch`).
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

use super::JniRtpCancel;
use super::errors::{connect_error_to_rtp, throw_rtp, transport_error_to_rtp};
use crate::handle::HandleRegistry;
use crate::jutil::build_socket_stats;

struct JniRtpSender {
    inner: RtpTransport,
}

struct JniRtpReceiver {
    inner: RtpRecvTransport,
    /// Pulled from `inner.end_reason_handle()` at construction — cheap to
    /// clone, independent of `inner`'s lifetime within this struct. Read by
    /// `nEndReason`/`nEndDetail` while the registry entry is live; `nClose`
    /// reads it once more (after `inner.close()` records `Cancelled` if
    /// nothing else already claimed the slot) to build the close-time
    /// snapshot — see `end_reason`'s module doc for why that has to happen
    /// inside `nClose` itself.
    end_reason: StreamEndReasonHandle,
    scratch: Vec<u8>,
}

/// Per-type leased-handle registries. Both register a cancel hook so a
/// cross-thread `close()` wakes a parked `send`/`recv` before taking the
/// resource lock (mirrors the round-1 cancel-first-then-free discipline).
static REGISTRY_SENDER: LazyLock<HandleRegistry<JniRtpSender>> = LazyLock::new(HandleRegistry::new);
static REGISTRY_RECEIVER: LazyLock<HandleRegistry<JniRtpReceiver>> =
    LazyLock::new(HandleRegistry::new);

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
                throw_rtp(env, "TRANSPORT", &e.to_string());
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
                connect_error_to_rtp(env, &e);
                return 0;
            }
        };
        // One cancel handle, two registry roles: the close hook (wakes a parked
        // `send` on `close()`) and the lock-free target `nCancelHandle` reads
        // while that same `send` holds the resource lock.
        let cancel = inner
            .cancel_handle()
            .expect("RtpTransport always returns Some(cancel_handle)");
        let cancel_for_hook = cancel.clone();
        REGISTRY_SENDER.insert_full(
            JniRtpSender { inner },
            Some(Box::new(move || cancel_for_hook.cancel())),
            Some(cancel),
        ) as jlong
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
        match REGISTRY_SENDER.with_poisoning(handle as u64, |w| w.inner.send_bytes(&bytes)) {
            Some(Ok(())) => {}
            Some(Err(e)) => transport_error_to_rtp(env, &e),
            None => {
                crate::error::throw_closed(env, "Sender");
            }
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
        let Some(stats) = REGISTRY_SENDER.with(handle as u64, |w| {
            w.inner.socket_stats().unwrap_or_default()
        }) else {
            return JObject::null();
        };
        build_socket_stats(env, "org/tstrans/rtp/SocketStats", &stats)
            .unwrap_or_else(|_| JObject::null())
    })
}

/// Return a cancel-handle `jlong` (Box<JniRtpCancel>).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Sender_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free (see the registration comment); closed handle → 0.
        REGISTRY_SENDER
            .cancel_target(handle as u64)
            .map_or(0, |inner| JniRtpCancel { inner }.into_handle())
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
            w.inner.close();
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
                throw_rtp(env, "TRANSPORT", &e.to_string());
                return 0;
            }
        };
        let inner = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                connect_error_to_rtp(env, &e);
                return 0;
            }
        };
        let scratch_len = inner.max_payload();
        let cancel = inner
            .cancel_handle()
            .expect("RtpRecvTransport always returns Some(cancel_handle)");
        let cancel_for_hook = cancel.clone();
        // Pulled BEFORE `inner` is boxed into the registry entry alongside
        // it — same construction-time-capture shape as `cancel` above (and
        // the D5 `stats_handle` precedent in srt::managed_basic). `cancel`
        // serves as both the close hook and the lock-free `nCancelHandle`
        // target (readable while `recv` holds the resource lock).
        let end_reason = inner.end_reason_handle();
        REGISTRY_RECEIVER.insert_full(
            JniRtpReceiver {
                inner,
                end_reason,
                scratch: vec![0u8; scratch_len],
            },
            Some(Box::new(move || cancel_for_hook.cancel())),
            Some(cancel),
        ) as jlong
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
        let Some(res) = REGISTRY_RECEIVER.with_poisoning(handle as u64, |w| {
            let n = w.inner.recv_bytes(w.scratch.as_mut_slice())?;
            Ok::<Vec<u8>, _>(w.scratch[..n].to_vec())
        }) else {
            crate::error::throw_closed(env, "Receiver");
            return std::ptr::null_mut();
        };
        match res {
            Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                // Allocating the Java array failed (effectively OOM). Throw rather
                // than return null silently, so `recv()` always yields bytes or an
                // RtpException — matching tst-py's contract (it never returns None).
                Err(_) => {
                    throw_rtp(env, "TRANSPORT", "failed to allocate received packet");
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                transport_error_to_rtp(env, &e);
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
/// transport/session stays alive) — hand-mapped to `RtpException(TIMEOUT)`
/// below, since that outcome never reaches `transport_error_to_rtp` (which
/// only sees `Err`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nRecvTimeout(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    timeout_ms: jlong,
) -> jbyteArray {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(res) = REGISTRY_RECEIVER.with_poisoning(handle as u64, |w| {
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
        }) else {
            crate::error::throw_closed(env, "Receiver");
            return std::ptr::null_mut();
        };
        match res {
            Ok(Some(bytes)) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                // Allocating the Java array failed (effectively OOM). Throw rather
                // than return null silently, matching `nRecv`.
                Err(_) => {
                    throw_rtp(env, "TRANSPORT", "failed to allocate received packet");
                    std::ptr::null_mut()
                }
            },
            Ok(None) => {
                throw_rtp(env, "TIMEOUT", "recv deadline elapsed");
                std::ptr::null_mut()
            }
            Err(e) => {
                transport_error_to_rtp(env, &e);
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
        let Some(stats) = REGISTRY_RECEIVER.with(handle as u64, |w| {
            w.inner.socket_stats().unwrap_or_default()
        }) else {
            return JObject::null();
        };
        build_socket_stats(env, "org/tstrans/rtp/SocketStats", &stats)
            .unwrap_or_else(|_| JObject::null())
    })
}

/// Return a cancel-handle `jlong` (Box<JniRtpCancel>).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_Receiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lock-free (see the registration comment); closed handle → 0.
        REGISTRY_RECEIVER
            .cancel_target(handle as u64)
            .map_or(0, |inner| JniRtpCancel { inner }.into_handle())
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
        REGISTRY_RECEIVER
            .with(handle as u64, |w| {
                super::end_reason::end_reason_ordinal(w.end_reason.get().as_ref())
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
            .with(handle as u64, |w| {
                w.end_reason
                    .get()
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
