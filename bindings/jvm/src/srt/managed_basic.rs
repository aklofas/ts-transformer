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
//! - `nFromUrl` registers via `REGISTRY.insert` (a `JniManagedSender` on the
//!   send side; a `JniManagedReceiver` on the recv side).
//! - Per-call methods lease via `REGISTRY.with` (non-consuming).
//! - `nClose` takes + tears down via `REGISTRY.close`.
//!
//! ## Stats drift (intentional — mirrors tst-py)
//!
//! `nSrtStats` ALWAYS throws `SrtException(IO)` on both wrappers:
//! `ManagedTransport` / `ManagedRecvTransport` do not expose the SRT-rich
//! 17-field shape (no accessor in tst-pipeline). Callers use `nSocketStats`.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong};
use tst_core::transport::TransportError;
use tst_pipeline::receiver::ReceiverErrorSource;
use tst_pipeline::sender::SenderErrorSource;
use tst_pipeline::{
    ManagedRecvTransport, ManagedTransport, Receiver as PlReceiver, ReceiverConfig,
    Sender as PlSender, SenderConfig,
};
use tst_srt::{Listener, ListenerConfig, Socket, SocketConfig, SrtTransport, SrtUrl, url::Mode};

use super::JniCancel;
use super::stats::build_managed_transport_stats;
use crate::handle::HandleRegistry;
use crate::jutil::{build_socket_stats, join_host_port};

// -----------------------------------------------------------------------
// Factory helpers — rebuild a fresh SrtTransport from a URL string.
// Ported verbatim from tst-py's managed_basic.rs: every failure maps to
// TransportError::Broken so the reconnect loop treats it as recoverable.
// -----------------------------------------------------------------------

/// Build a fresh caller-mode `SrtTransport` from a URL string. Used by the
/// reconnect factory closure: every Broken/Closed event reruns this.
fn build_sender_transport(url: &str) -> Result<SrtTransport, TransportError> {
    let parsed = SrtUrl::parse(url).map_err(|e| TransportError::Broken {
        msg: format!("managed sender factory: URL parse failed: {e}"),
        errno_code: None,
    })?;
    if parsed.mode != Mode::Caller {
        return Err(TransportError::Broken {
            msg: format!(
                "managed sender factory: URL mode={:?} but caller required",
                parsed.mode
            ),
            errno_code: None,
        });
    }
    let mut cfg = SocketConfig::default();
    parsed.overlay.apply_to_socket(&mut cfg);
    let addr = join_host_port(&parsed.host, parsed.port);
    let socket = Socket::connect_with(&cfg, addr.as_str()).map_err(|e| TransportError::Broken {
        msg: format!("managed sender factory: connect failed: {e}"),
        errno_code: None,
    })?;
    Ok(SrtTransport::new(socket))
}

/// Parse a listener-mode URL into its bind address + `ListenerConfig`
/// (shared by the initial open and the reconnect factory below).
fn listener_bind_target(url: &str) -> Result<(String, ListenerConfig), TransportError> {
    let parsed = SrtUrl::parse(url).map_err(|e| TransportError::Broken {
        msg: format!("managed receiver factory: URL parse failed: {e}"),
        errno_code: None,
    })?;
    if parsed.mode != Mode::Listener {
        return Err(TransportError::Broken {
            msg: format!(
                "managed receiver factory: URL mode={:?} but listener required",
                parsed.mode
            ),
            errno_code: None,
        });
    }
    let mut cfg = ListenerConfig::default();
    parsed.overlay.apply_to_listener(&mut cfg);
    let addr = if parsed.host.is_empty() {
        format!("0.0.0.0:{}", parsed.port)
    } else {
        join_host_port(&parsed.host, parsed.port)
    };
    Ok((addr, cfg))
}

fn build_receiver_transport(url: &str) -> Result<SrtTransport, TransportError> {
    let (addr, cfg) = listener_bind_target(url)?;
    let mut listener =
        Listener::bind_with(&cfg, addr.as_str()).map_err(|e| TransportError::Broken {
            msg: format!("managed receiver factory: bind failed: {e}"),
            errno_code: None,
        })?;
    let (socket, _peer) = listener.accept().map_err(|e| TransportError::Broken {
        msg: format!("managed receiver factory: accept failed: {e}"),
        errno_code: None,
    })?;
    Ok(SrtTransport::new(socket))
}

/// [`build_receiver_transport`] for the RECONNECT factory: the listener's
/// cancel handle is published into the shared `FactoryCancel` slot around
/// the accept, so `cancel()` on the managed receiver can wake a re-accept
/// parked with no peer in sight (the initial open above stays plain because
/// no handle exists yet to cancel it with). That lifecycle lives in
/// [`Listener::accept_one_cancellable`] (shared with the C and Python
/// managed listener factories); here it is wrapped only to keep the
/// `managed receiver factory:` prefix every error out of this module wears.
fn build_receiver_transport_cancellable(
    url: &str,
    cancel: &tst_pipeline::FactoryCancel,
) -> Result<SrtTransport, TransportError> {
    let (addr, cfg) = listener_bind_target(url)?;
    Listener::accept_one_cancellable(&cfg, addr.as_str(), cancel).map_err(|e| match e {
        TransportError::Broken { msg, errno_code } => TransportError::Broken {
            msg: format!("managed receiver factory: {msg}"),
            errno_code,
        },
        other => other,
    })
}

// -----------------------------------------------------------------------
// ManagedSender  (org.tstrans.srt.ManagedSender)
//
// handle = Box<PlSender<ManagedTransport<SrtTransport>>>
// -----------------------------------------------------------------------

/// Backing state for `ManagedSender`. Holds the pipeline shell plus a
/// reconnect/gap telemetry observer snapshotted at construction (mirrors
/// `JniManagedMuxSender` in managed_convenience.rs and tst-py's
/// `PyManagedSender`).
struct JniManagedSender {
    inner: PlSender<ManagedTransport<SrtTransport>>,
    stats_handle: tst_pipeline::ManagedStatsHandle,
}

/// Per-type leased-handle registry for `org.tstrans.srt.ManagedSender`. No cancel
/// hook (single-threaded; the public cancel handle drives reconnect-loop exit).
static REGISTRY_SENDER: LazyLock<HandleRegistry<JniManagedSender>> =
    LazyLock::new(HandleRegistry::new);

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
            super::errors::throw_srt(env, "CONFIG_INVALID", &msg);
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

        // Initial connect — the FIRST inner ManagedTransport::new wraps.
        let initial = match build_sender_transport(&url_str) {
            Ok(t) => t,
            Err(e) => {
                super::errors::transport_error(env, &e);
                return 0;
            }
        };

        // Factory for subsequent reconnects. `Fn + Send + Sync + 'static` per
        // ManagedTransport::new's bound; `move` captures the URL string.
        let factory = {
            let url_for_factory = url_str.clone();
            move || -> Result<SrtTransport, TransportError> {
                build_sender_transport(&url_for_factory)
            }
        };

        let managed = ManagedTransport::new(initial, factory, policy);
        // Snapshot the stats handle and the cancel target BEFORE moving `managed`
        // into the shell (same pattern as the convenience wrappers). The target
        // lives outside the registry's resource lock so `nCancelHandle` returns
        // while `send` is parked; `ManagedTransport::cancel_handle` is always Some.
        let stats_handle = managed.stats_handle();
        let target = tst_core::transport::Transport::cancel_handle(&managed)
            .expect("ManagedTransport::cancel_handle is always Some");
        let inner = PlSender::new(managed, SenderConfig::default());
        REGISTRY_SENDER.insert_with_target(
            JniManagedSender {
                inner,
                stats_handle,
            },
            target,
        ) as jlong
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

        match REGISTRY_SENDER.with_poisoning(handle as u64, |jstruct| jstruct.inner.send_ts(&bytes))
        {
            Some(Ok(())) => {}
            Some(Err(e)) => match e.source {
                SenderErrorSource::Transport(t) => super::errors::transport_error(env, &t),
                SenderErrorSource::Framing(f) => {
                    super::errors::throw_srt(env, "CONFIG_INVALID", &f.to_string())
                }
                _ => super::errors::throw_srt(env, "IO", &e.to_string()),
            },
            None => {
                crate::error::throw_closed(env, "ManagedSender");
            }
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
        match REGISTRY_SENDER.with_poisoning(handle as u64, |jstruct| jstruct.inner.flush()) {
            Some(Ok(())) => {}
            Some(Err(e)) => match e.source {
                SenderErrorSource::Transport(t) => super::errors::transport_error(env, &t),
                SenderErrorSource::Framing(f) => {
                    super::errors::throw_srt(env, "CONFIG_INVALID", &f.to_string())
                }
                _ => super::errors::throw_srt(env, "IO", &e.to_string()),
            },
            None => {
                crate::error::throw_closed(env, "ManagedSender");
            }
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
        let Some(stats) = REGISTRY_SENDER.with(handle as u64, |jstruct| {
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
        super::errors::throw_srt(
            env,
            "IO",
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
        let Some(maybe_stats) =
            REGISTRY_SENDER.with(handle as u64, |jstruct| jstruct.stats_handle.stats())
        else {
            crate::error::throw_closed(env, "ManagedSender");
            return JObject::null();
        };
        let Some(stats) = maybe_stats else {
            super::errors::throw_srt(env, "IO", "reconnect stats unavailable: gap lock poisoned");
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
            .with(handle as u64, |jstruct| u8::from(jstruct.inner.is_alive()))
            .unwrap_or(0)
    })
}

// -----------------------------------------------------------------------
// ManagedReceiver  (org.tstrans.srt.ManagedReceiver)
//
// handle = Box<JniManagedReceiver>
// -----------------------------------------------------------------------

/// Backing state for `ManagedReceiver`. Holds the pipeline shell plus a shared
/// handle to the `ManagedRecvTransport`'s reconnect counter so callers can read
/// it even mid-reconnect.
struct JniManagedReceiver {
    inner: PlReceiver<ManagedRecvTransport<SrtTransport>>,
    reconnects: Arc<AtomicU64>,
}

/// Per-type leased-handle registry for `org.tstrans.srt.ManagedReceiver`. No
/// cancel hook (single-threaded; the public cancel handle wakes a parked recv).
static REGISTRY_RECEIVER: LazyLock<HandleRegistry<JniManagedReceiver>> =
    LazyLock::new(HandleRegistry::new);

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
            super::errors::throw_srt(env, "CONFIG_INVALID", &msg);
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

        // Initial bind+accept — the FIRST inner ManagedRecvTransport::new wraps.
        let initial = match build_receiver_transport(&url_str) {
            Ok(t) => t,
            Err(e) => {
                super::errors::transport_error(env, &e);
                return 0;
            }
        };

        // FnMut factory for the recv-side (no `Sync` required — it lives entirely
        // behind `&mut self` on the recv path).
        // The factory's re-accept is reachable by `cancel()` through this
        // slot (see `build_receiver_transport_cancellable`).
        let factory_cancel = Arc::new(tst_pipeline::FactoryCancel::new());
        let factory: Box<dyn FnMut() -> Result<SrtTransport, TransportError> + Send> = {
            let url_for_factory = url_str.clone();
            let fc = Arc::clone(&factory_cancel);
            Box::new(move || build_receiver_transport_cancellable(&url_for_factory, &fc))
        };

        let managed =
            ManagedRecvTransport::new_with_factory_cancel(initial, factory, policy, factory_cancel);
        // Snapshot the reconnect counter and the cancel target BEFORE moving
        // `managed` into the shell. The target lives outside the registry's
        // resource lock so `nCancelHandle` returns while `recvBytes` is parked
        // (including a listener-mode re-accept); the managed handle follows
        // reconnects, so one capture at open is enough.
        let reconnects = managed.reconnects_handle();
        let target = tst_core::transport::RecvTransport::cancel_handle(&managed)
            .expect("ManagedRecvTransport::cancel_handle is always Some");
        let inner = PlReceiver::new(managed, ReceiverConfig::default());
        REGISTRY_RECEIVER.insert_with_target(JniManagedReceiver { inner, reconnects }, target)
            as jlong
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
        let Some(res) =
            REGISTRY_RECEIVER.with_poisoning(handle as u64, |jstruct| jstruct.inner.next_packet())
        else {
            crate::error::throw_closed(env, "ManagedReceiver");
            return std::ptr::null_mut();
        };
        match res {
            Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Err(e) => {
                match e.source {
                    ReceiverErrorSource::Transport(t) => super::errors::transport_error(env, &t),
                    _ => super::errors::throw_srt(env, "IO", &e.to_string()),
                }
                std::ptr::null_mut()
            }
        }
    })
}

/// Return the total number of successful reconnect rebuilds.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nReconnectAttempts(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_RECEIVER
            .with(handle as u64, |jstruct| {
                jstruct.reconnects.load(Ordering::Acquire) as jlong
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

/// Return a `SocketStats` record from the current inner transport.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedReceiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY_RECEIVER.with(handle as u64, |jstruct| {
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
        super::errors::throw_srt(
            env,
            "IO",
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
            .with(handle as u64, |jstruct| u8::from(jstruct.inner.is_alive()))
            .unwrap_or(0)
    })
}
