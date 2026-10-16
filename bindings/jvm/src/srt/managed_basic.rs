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
//!   — the ONE open path: the initial dial, the reconnect factory,
//!   the attempt/success counters and the cancel handle are composed in
//!   tst-srt, once, for every binding. The returned `ManagedHandles` (plus the
//!   sender's `ManagedStatsHandle`) become the entry's `Owned` SNAPSHOT, so
//!   every counter/stats/cancel read is lock-free.
//! - Per-call methods go through `with_mut` / `with_ref_blocking`; a `HandleState`
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

use crate::panic::jni_native;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
use tst_pipeline::binding::view::BlockingReason;

use jni::objects::{JByteArray, JObject, JString};
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

jni_native! {
    /// Allocate a `ManagedSender` from an SRT caller-mode URL + the 8 flattened
    /// reconnect-policy args. Returns a `jlong` handle on success; throws
    /// `SrtException` and returns 0 on any error.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_srt_ManagedSender_nFromUrl(
        env,
        _class,
        url: JString<'_>,
        max_attempts_present: jboolean,
        max_attempts: jint,
        backoff_kind: jint,
        backoff_base_ms: jlong,
        backoff_max_ms: jlong,
        gap_buffer_capacity: jint,
        overflow_policy: jint,
        mode: jint,
    ) -> jlong = 0 => {
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

        // One open path: the initial dial, the reconnect
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
    }
}

jni_native! {
    /// Send pre-muxed TS bytes through the managed sender. Throws `SrtException` on
    /// transport/framing failure.
    fn Java_org_tstrans_srt_ManagedSender_nSendBytes(
        env,
        _class,
        handle: jlong,
        data: JByteArray<'_>,
    ) = () => {
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
    }
}

jni_native! {
    /// Flush any buffered partial TS bundle. Throws `SrtException` on failure.
    fn Java_org_tstrans_srt_ManagedSender_nFlush(env, _class, handle: jlong) = () => {
        match REGISTRY_SENDER.with_mut(handle as u64, |jstruct| jstruct.inner.flush()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_sender_error(env, &e),
            Err(state) => throw_handle_state(env, "ManagedSender", &state),
        }
    }
}

jni_native! {
    /// Obtain a cancel handle for this managed sender. Returns a `jlong` handle on
    /// success; throws `IllegalStateException` and returns 0 if absent.
    fn Java_org_tstrans_srt_ManagedSender_nCancelHandle(
        _env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        // Lock-free: the view is the `Owned` entry itself, so this returns
        // even while `send` is parked on the slot. Closed handle → 0
        // (no throw, matching the original contract).
        REGISTRY_SENDER
            .cancel_view(handle as u64)
            .map_or(0, crate::shells::cancel::cancel_view_handle)
    }
}

jni_native! {
    /// Return a `SocketStats` record from the current inner transport. Returns null
    /// on JNI builder error (non-fatal; no throw). Uses `unwrap_or_default` so a
    /// mid-reconnect sender yields a zeroed snapshot.
    fn Java_org_tstrans_srt_ManagedSender_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let Ok(stats) = REGISTRY_SENDER.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jstruct| jstruct.inner.socket_stats().unwrap_or_default(),
        ) else {
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/shells/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    }
}

jni_native! {
    /// SRT-rich stats are NOT available on a managed sender — this ALWAYS throws
    /// `SrtException(IO)`, mirroring tst-py's `PyManagedSender::srt_stats`.
    fn Java_org_tstrans_srt_ManagedSender_nSrtStats<'local>(
        env,
        _class,
        _handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        throw_srt(
            env,
            BindingErrorKind::SrtIo,
            "srt_stats not available on ManagedSender (use socketStats); a future \
             tst-pipeline accessor will expose the SRT-rich shape",
        );
        JObject::null()
    }
}

jni_native! {
    /// Reconnect/gap telemetry: attempts, successes, current gap-buffer depth, and
    /// drop counters. Throws `SrtException(IO)` if the internal gap-buffer lock is
    /// poisoned — a read-only telemetry path must not panic. Throws
    /// `IllegalStateException` (via `throw_closed`) on a closed handle.
    fn Java_org_tstrans_srt_ManagedSender_nReconnectStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
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
    }
}

jni_native! {
    /// Close the managed sender, deallocating the native box. `close()` latches the
    /// cancel flag (so any in-flight reconnect loop exits) and tears down the inner.
    fn Java_org_tstrans_srt_ManagedSender_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut jstruct) = REGISTRY_SENDER.close(handle as u64) {
            jstruct.inner.close();
        }
    }
}

jni_native! {
    /// Return whether the managed sender holds a live transport. Non-blocking: a
    /// `sendBytes` parked on another thread holds the slot, and reads as alive.
    fn Java_org_tstrans_srt_ManagedSender_nIsAlive(_env, _class, handle: jlong) -> jboolean = 0 => {
        u8::from(REGISTRY_SENDER.is_alive(handle as u64, |jstruct| jstruct.inner.is_alive()))
    }
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

jni_native! {
    /// Allocate a `ManagedReceiver` from an SRT listener-mode URL + the 8 flattened
    /// reconnect-policy args. Returns a `jlong` handle on success; throws
    /// `SrtException` and returns 0 on any error.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_srt_ManagedReceiver_nFromUrl(
        env,
        _class,
        url: JString<'_>,
        max_attempts_present: jboolean,
        max_attempts: jint,
        backoff_kind: jint,
        backoff_base_ms: jlong,
        backoff_max_ms: jlong,
        gap_buffer_capacity: jint,
        overflow_policy: jint,
        mode: jint,
    ) -> jlong = 0 => {
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

        // One open path: the initial bind+accept, the
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
        // and the plain `Receiver` never RECORDS one either
        // (`ManagedHandles::end_reason` is a fresh, never-set handle for every
        // shell but `ManagedDemuxReceiver`). Attaching it would advertise a
        // capability that does not exist.
        let cancel = Arc::clone(&handles.cancel);
        REGISTRY_RECEIVER.insert(Owned::new(JniManagedReceiver { inner }, cancel, handles)) as jlong
    }
}

jni_native! {
    /// Receive one TS packet (188 bytes). Returns the packet as a `jbyteArray` on
    /// success; throws `SrtException` and returns null on failure. `maxLen` is
    /// accepted for API symmetry but a single `next_packet` quantum is returned.
    fn Java_org_tstrans_srt_ManagedReceiver_nRecvBytes(
        env,
        _class,
        handle: jlong,
        _max_len: jint,
    ) -> jbyteArray = std::ptr::null_mut() => {
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
    }
}

jni_native! {
    /// Total factory invocations since construction (was the success
    /// counter). Lock-free: read off the `Owned` snapshot, so it answers while
    /// `recvBytes()` is parked in a re-accept that has not completed.
    fn Java_org_tstrans_srt_ManagedReceiver_nReconnectAttempts(
        _env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        REGISTRY_RECEIVER
            .snapshot(handle as u64, |h| {
                h.attempts.load(Ordering::Acquire) as jlong
            })
            .unwrap_or(0)
    }
}

jni_native! {
    /// Obtain a cancel handle for this managed receiver.
    fn Java_org_tstrans_srt_ManagedReceiver_nCancelHandle(
        _env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        // Lock-free: the view is the `Owned` entry itself, so this returns
        // even while `recvBytes` is parked on the slot. Closed handle → 0
        // (no throw, matching the original contract).
        REGISTRY_RECEIVER
            .cancel_view(handle as u64)
            .map_or(0, crate::shells::cancel::cancel_view_handle)
    }
}

jni_native! {
    /// Return a `SocketStats` record from the current inner transport.
    fn Java_org_tstrans_srt_ManagedReceiver_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let Ok(stats) = REGISTRY_RECEIVER.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jstruct| jstruct.inner.socket_stats().unwrap_or_default(),
        ) else {
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/shells/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    }
}

jni_native! {
    /// SRT-rich stats are NOT available on a managed receiver — this ALWAYS throws
    /// `SrtException(IO)`, mirroring tst-py's `PyManagedReceiver::srt_stats`.
    fn Java_org_tstrans_srt_ManagedReceiver_nSrtStats<'local>(
        env,
        _class,
        _handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        throw_srt(
            env,
            BindingErrorKind::SrtIo,
            "srt_stats not available on ManagedReceiver (use socketStats); a future \
             tst-pipeline accessor will expose the SRT-rich shape",
        );
        JObject::null()
    }
}

jni_native! {
    /// Close the managed receiver, deallocating the native box.
    fn Java_org_tstrans_srt_ManagedReceiver_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut jstruct) = REGISTRY_RECEIVER.close(handle as u64) {
            jstruct.inner.close();
        }
    }
}

jni_native! {
    /// Return whether the managed receiver holds a live shell. Non-blocking: a
    /// `recvBytes` parked on another thread holds the slot, and reads as alive.
    fn Java_org_tstrans_srt_ManagedReceiver_nIsAlive(
        _env,
        _class,
        handle: jlong,
    ) -> jboolean = 0 => {
        u8::from(REGISTRY_RECEIVER.is_alive(handle as u64, |jstruct| jstruct.inner.is_alive()))
    }
}
