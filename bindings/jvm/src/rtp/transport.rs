//! JNI exports for `org.tstrans.rtp.Transport` and `org.tstrans.rtp.RecvTransport`.
//!
//! Each Java class is handle-backed by an `OwnedRegistry` entry:
//! - `Sender`   → `Owned<SendHalf<tst_rtp::RtpTransport>>`.
//! - `Receiver` → `Owned<JniRtpReceiver, EndReasonHandle>` (the transport
//!   plus a reusable recv scratch buffer, mirroring tst-py's
//!   `PyReceiver.scratch`; the end-reason cell is the lock-free snapshot).
//!
//! Unlike the srt JVM surface (which wraps `tst_pipeline::Sender/Receiver`),
//! the rtp surface wraps the transport DIRECTLY and calls the
//! `Transport`/`RecvTransport` trait methods — exactly as tst-py's
//! `bindings/python/src/rtp/transport.rs` does.

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::sync::LazyLock;
use std::time::Duration;
use tst_pipeline::binding::view::BlockingReason;

use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JByteArray, JObject, JString};
use jni::strings::JNIString;
use jni::sys::{jbyteArray, jint, jlong, jobject};
use tst_core::transport::EndReasonHandle;
use tst_core::transport::{RecvTransport, Transport};
use tst_rtp::{RtpRecvTransport, RtpTransport, RtpUrl};

use tst_pipeline::binding::{BindingErrorKind, Owned, SendHalf};

use super::errors::{connect_error, rtp_url_error, throw_rtp, transport_error};
use crate::error::throw_handle_state;
use crate::generated::stats_shells::build_socket_stats;
use crate::handle::OwnedRegistry;

struct JniRtpReceiver {
    inner: RtpRecvTransport,
    /// Pulled from `inner.end_reason_handle()` at construction. `nClose` reads
    /// it after `inner.close()` (which records `Cancelled` if nothing else
    /// claimed the slot) to build the close-time snapshot — see `end_reason`'s
    /// module doc for why that has to happen inside `nClose` itself. The
    /// LOCK-FREE getters read the entry's `Owned` snapshot, another clone of
    /// this same `Arc<OnceLock>` cell.
    end_reason: EndReasonHandle,
    scratch: Vec<u8>,
}

/// Per-type `Owned`-backed registries. `OwnedRegistry::close` cancels first, so
/// a cross-thread `close()` wakes a parked `send`/`recv` before taking the slot.
/// The sender holds the shared `SendHalf` newtype so all three bindings carry
/// the same entry type for a raw transport.
static REGISTRY_SENDER: LazyLock<OwnedRegistry<SendHalf<RtpTransport>>> =
    LazyLock::new(OwnedRegistry::new);
/// `S = EndReasonHandle`: the construction-time cell `nEndReason` /
/// `nEndDetail` read without the slot a parked `recv` holds.
static REGISTRY_RECEIVER: LazyLock<OwnedRegistry<JniRtpReceiver, EndReasonHandle>> =
    LazyLock::new(OwnedRegistry::new);

/// Unbox a nullable `java.lang.Long` SSRC arg into `Option<u32>`. Returns
/// `Err(())` (after throwing IllegalArgumentException) on out-of-range values.
fn unbox_ssrc(env: &mut Env, obj: &JObject) -> Result<Option<u32>, ()> {
    if obj.is_null() {
        return Ok(None);
    }
    let v = match env.call_method(obj, jni_str!("longValue"), jni_sig!("()J"), &[]) {
        Ok(jv) => jv.j().unwrap_or(-1),
        Err(e) => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                JNIString::from(e.to_string()),
            );
            return Err(());
        }
    };
    if v < 0 || v > i64::from(u32::MAX) {
        let _ = env.throw_new(
            FindClass(jni_str!("java/lang/IllegalArgumentException")),
            JNIString::from(format!("ssrc out of u32 range: {v}")),
        );
        return Err(());
    }
    Ok(Some(v as u32))
}

// ── Sender (org.tstrans.rtp.Transport) ────────────────────────────────────────

jni_native! {
    /// Allocate a `Sender` from an `rtp://host:port` URL. Returns a `jlong` handle;
    /// throws `RtpException` and returns 0 on error. `pktSize` is the UDP datagram
    /// size; `ssrcBoxed` is a nullable `java.lang.Long` SSRC (random when null).
    fn Java_org_tstrans_rtp_Transport_nFromUrl(
        env,
        _class,
        url: JString<'_>,
        pkt_size: jint,
        ssrc_boxed: JObject<'_>,
    ) -> jlong = 0 => {
        let url_str: String = match url.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return 0;
            }
        };
        let ssrc = match unbox_ssrc(env, &ssrc_boxed) {
            Ok(v) => v,
            Err(()) => return 0,
        };

        let mut parsed = match RtpUrl::parse(&url_str) {
            Ok(u) => u,
            Err(e) => {
                rtp_url_error(env, &e);
                return 0;
            }
        };
        parsed.pkt_size = Some(pkt_size.max(0) as usize);
        if let Some(s) = ssrc {
            parsed.ssrc = Some(s);
        }
        let inner = match RtpTransport::connect_with(&parsed) {
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
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.detail));
                return 0;
            }
        };
        REGISTRY_SENDER.insert(Owned::new(SendHalf(inner), cancel, ())) as jlong
    }
}

jni_native! {
    /// Send one MPEG-TS payload chunk over RTP. Throws `RtpException` on failure.
    fn Java_org_tstrans_rtp_Transport_nSend(
        env,
        _class,
        handle: jlong,
        data: JByteArray<'_>,
    ) = () => {
        let bytes: Vec<u8> = match env.convert_byte_array(&data) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return;
            }
        };
        match REGISTRY_SENDER.with_mut(handle as u64, |w| w.0.send_bytes(&bytes)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => transport_error(env, &e),
            Err(state) => throw_handle_state(env, "Sender", &state),
        }
    }
}

jni_native! {
    /// Return a `SocketStats` record. Returns null on JNI builder error (non-fatal).
    fn Java_org_tstrans_rtp_Transport_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        let Ok(stats) =
            // counters live inside the shell and have no value to report while a parked call holds it
            REGISTRY_SENDER.with_ref_blocking(handle as u64, BlockingReason::NoAnswerWithoutSlot, |w| w.0.socket_stats().unwrap_or_default())
        else {
            return JObject::null();
        };
        build_socket_stats(env, &stats)
            .unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// Return a cancel-handle `jlong` (a `CancelView` over the `Owned` entry).
    fn Java_org_tstrans_rtp_Transport_nCancelHandle(_env, _class, handle: jlong) -> jlong = 0 => {
        // Lock-free (see the registration comment); closed handle → 0.
        REGISTRY_SENDER
            .cancel_view(handle as u64)
            .map_or(0, crate::shells::cancel::cancel_view_handle)
    }
}

jni_native! {
    /// Close the Sender, freeing the native box. The cancel hook fires first (waking
    /// a parked `send`) before the resource is taken.
    fn Java_org_tstrans_rtp_Transport_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent: cancel hook wakes a parked send, then take + teardown.
        if let Some(mut w) = REGISTRY_SENDER.close(handle as u64) {
            w.0.close();
        }
    }
}

// ── Receiver (org.tstrans.rtp.RecvTransport) ────────────────────────────────────

jni_native! {
    /// Allocate a `Receiver` bound to an `rtp://host:port` URL. Returns a `jlong`
    /// handle; throws `RtpException` and returns 0 on error.
    fn Java_org_tstrans_rtp_RecvTransport_nFromUrl(env, _class, url: JString<'_>) -> jlong = 0 => {
        let url_str: String = match url.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return 0;
            }
        };
        let parsed = match RtpUrl::parse(&url_str) {
            Ok(u) => u,
            Err(e) => {
                rtp_url_error(env, &e);
                return 0;
            }
        };
        let inner = match RtpRecvTransport::listen_with(&parsed) {
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
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.detail));
                return 0;
            }
        };
        // Pulled BEFORE `inner` is boxed into the registry entry alongside
        // it — same construction-time-capture shape as `cancel` above, so
        // reading it never needs the resource lock. `cancel`
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
    }
}

jni_native! {
    /// Receive one MPEG-TS payload chunk (RTP header already stripped). Returns the
    /// bytes as a `jbyteArray`; throws `RtpException` and returns null on failure.
    fn Java_org_tstrans_rtp_RecvTransport_nRecv(
        env,
        _class,
        handle: jlong,
    ) -> jbyteArray = std::ptr::null_mut() => {
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
                        FindClass(jni_str!("java/lang/RuntimeException")),
                        jni_str!("failed to allocate received packet"),
                    );
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                transport_error(env, &e);
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
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
    fn Java_org_tstrans_rtp_RecvTransport_nRecvTimeout(
        env,
        _class,
        handle: jlong,
        timeout_ms: jlong,
    ) -> jbyteArray = std::ptr::null_mut() => {
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
                        FindClass(jni_str!("java/lang/RuntimeException")),
                        jni_str!("failed to allocate received packet"),
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
    }
}

jni_native! {
    /// Return a `SocketStats` record. Returns null on JNI builder error (non-fatal).
    fn Java_org_tstrans_rtp_RecvTransport_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let Ok(stats) = REGISTRY_RECEIVER.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |w| w.inner.socket_stats().unwrap_or_default(),
        ) else {
            return JObject::null();
        };
        build_socket_stats(env, &stats)
            .unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// Return a cancel-handle `jlong` (a `CancelView` over the `Owned` entry).
    fn Java_org_tstrans_rtp_RecvTransport_nCancelHandle(
        _env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        // Lock-free (see the registration comment); closed handle → 0.
        REGISTRY_RECEIVER
            .cancel_view(handle as u64)
            .map_or(0, crate::shells::cancel::cancel_view_handle)
    }
}

jni_native! {
    /// Why the receive session ended, or `-1` if it hasn't ended yet (or ended
    /// through a path this type doesn't instrument). See `end_reason`'s module
    /// doc for the wire-ordinal convention. Returns `-1` on a closed/absent
    /// handle rather than throwing (matches `endReason()`'s post-close-snapshot
    /// contract — the closed case never reaches this native at all, since
    /// `Receiver.endReason()` reads the Java-side snapshot once `peekHandle()`
    /// is 0).
    fn Java_org_tstrans_rtp_RecvTransport_nEndReason(_env, _class, handle: jlong) -> jint = -1 => {
        // Lock-free: the cell is the entry's `Owned` snapshot, so this answers
        // while a `recv` is parked on the slot — a getter must never block
        // behind a parked call.
        REGISTRY_RECEIVER
            .snapshot(handle as u64, |h| {
                crate::shells::end_reason::wire_value(h.get().as_ref())
            })
            .unwrap_or(-1)
    }
}

jni_native! {
    /// Free-text detail for `nEndReason` — the `msg` carried by
    /// `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`; `null` for
    /// every other reason (including "hasn't ended yet" and a closed/absent
    /// handle).
    fn Java_org_tstrans_rtp_RecvTransport_nEndDetail<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        let detail = REGISTRY_RECEIVER
            .snapshot(handle as u64, |h| {
                h.get()
                    .and_then(|r| crate::shells::end_reason::detail(&r).map(str::to_owned))
            })
            .flatten();
        match detail {
            Some(d) => match env.new_string(&d) {
                Ok(s) => s.into(),
                Err(_) => JObject::null(),
            },
            None => JObject::null(),
        }
    }
}

jni_native! {
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
    fn Java_org_tstrans_rtp_RecvTransport_nClose(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        // Atomic + idempotent: cancel hook wakes a parked recv, then take + teardown.
        let reason = if let Some(mut w) = REGISTRY_RECEIVER.close(handle as u64) {
            w.inner.close();
            w.end_reason.get()
        } else {
            None
        };
        crate::shells::end_reason::build_close_snapshot(env, reason)
            .map(|obj| obj.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }
}
