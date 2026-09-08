//! JNI surface for `org.tstrans.srt.ManagedMuxSender` and
//! `org.tstrans.srt.ManagedDemuxReceiver` — the two convenience auto-reconnect
//! SRT wrappers (sub-wave C, Task 2).
//!
//! Ports tst-py's `bindings/python/src/srt/managed_convenience.rs`. The sender
//! wraps `MuxSender<ManagedTransport<SrtTransport>>`; the receiver wraps
//! `ManagedDemuxReceiver<SrtTransport>` (which owns a `ManagedRecvTransport`).
//! URL + socket config are captured at construction and replayed by the
//! reconnect factory on each Broken/Closed event from the inner SRT socket.
//!
//! ## Reconnect-attempt counter (SOURCE-WINS divergence #1)
//!
//! Both wrappers expose `reconnectAttempts()` as a FACTORY-INSTRUMENTED ATTEMPT
//! counter — NOT `reconnects_count()` (a SUCCESS counter). Mirroring tst-py,
//! the factory closure bumps a captured `Arc<AtomicU64>` on every invocation
//! BEFORE calling `connect_srt`/`listen_srt`; `nReconnectAttempts` loads it.
//!
//! ## Mode (SOURCE-WINS divergence #2)
//!
//! `ManagedMuxSender` REQUIRES `?mode=caller` (CONFIG_INVALID otherwise).
//! `ManagedDemuxReceiver` accepts BOTH `?mode=listener` (default) AND
//! `?mode=caller`, building `listen_srt`/`connect_srt` accordingly in both the
//! initial connect and the factory.
//!
//! ## Threading model
//!
//! Single-threaded boxes (NO inner `Arc<Mutex>`) — the JVM follows the shipped
//! single-thread `MuxSender`/`DemuxReceiver` model. `MuxSender` push methods
//! reconstitute as a SHARED `&*ptr` (every `send_*` takes `&self` + internal
//! mutex; concurrent pushes are sound). `ManagedDemuxReceiver::nNext` uses
//! `&mut *ptr` (`recv_event` is `&mut self`). The receiver has NO byte sink.
//!
//! ## Stats drift (SOURCE-WINS divergence #4)
//!
//! `ManagedMuxSender` exposes a combined `TransportStats stats()` +
//! `reconnectAttempts()` — NO `srtStats()`. `ManagedDemuxReceiver` exposes
//! `SocketStats socketStats()` AND `SocketStats srtStats()` (the latter returns
//! the SAME value as `socketStats`, return type `SocketStats`, no throw) +
//! `reconnectAttempts()` — NO combined `stats()`.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use jni::JNIEnv;
use jni::objects::{JBooleanArray, JByteArray, JClass, JIntArray, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::TransportError;
use tst_pipeline::{
    ManagedDemuxReceiver as RustManagedDemuxReceiver, ManagedDemuxReceiverConfig,
    ManagedRecvTransport, ManagedTransport, MuxSender as RustMuxSender, MuxSenderError,
    MuxSenderErrorSource,
};
use tst_srt::{Listener, ListenerConfig, Socket, SocketConfig, SrtTransport, SrtUrl, url::Mode};

use crate::handle::HandleRegistry;
use crate::jutil::{build_socket_stats, checked_u8, join_host_port, read_bytes};
use crate::mpegts::muxer::{build_muxer_config_from_arrays, throw_mux_error};
use crate::mpegts::{build_demux_config_from_args, build_muxer_stats, convert_event};

use super::JniCancel;
use super::errors::{throw_srt, transport_error};
use super::mux_sender::build_transport_stats;
use super::stats::build_managed_transport_stats;

// ---------------------------------------------------------------------------
// Port helpers — rebuild a fresh SrtTransport. Copied verbatim from tst-py's
// managed_convenience.rs (every failure maps to TransportError::Broken so the
// reconnect loop treats it as recoverable; the initial-connect path re-maps it
// to CONNECT_FAILED at the JNI boundary, divergence #6).
// ---------------------------------------------------------------------------

/// Build a fresh `SrtTransport` connected as a caller. `cfg.merge_sender_defaults()`
/// applies the sender-side socket defaults before connecting.
fn connect_srt(host: &str, port: u16, cfg: &SocketConfig) -> Result<SrtTransport, TransportError> {
    let mut cfg = cfg.clone();
    cfg.merge_sender_defaults();
    let addr = join_host_port(host, port);
    let socket = Socket::connect_with(&cfg, addr.as_str()).map_err(|e| TransportError::Broken {
        msg: format!("connect: {e}"),
        errno_code: None,
    })?;
    Ok(SrtTransport::new(socket))
}

/// Bind a listener + accept one peer; return the accepted `SrtTransport`.
fn listen_srt(host: &str, port: u16, cfg: &ListenerConfig) -> Result<SrtTransport, TransportError> {
    let bind_host = if host.is_empty() { "0.0.0.0" } else { host };
    let addr = if host.contains(':') && !host.starts_with('[') {
        format!("[{bind_host}]:{port}")
    } else {
        format!("{bind_host}:{port}")
    };
    let mut listener =
        Listener::bind_with(cfg, addr.as_str()).map_err(|e| TransportError::Broken {
            msg: format!("bind: {e}"),
            errno_code: None,
        })?;
    let (socket, _peer) = listener.accept().map_err(|e| TransportError::Broken {
        msg: format!("accept: {e}"),
        errno_code: None,
    })?;
    Ok(SrtTransport::new(socket))
}

/// [`listen_srt`] for the reconnect factory: the listener's cancel handle
/// is published into the shared `FactoryCancel` slot around the accept so
/// `cancel()` can reach a re-accept parked with no peer in sight. Mirror of
/// `tst-c`'s `listen_srt_cancellable`.
fn listen_srt_cancellable(
    host: &str,
    port: u16,
    cfg: &ListenerConfig,
    cancel: &tst_pipeline::FactoryCancel,
) -> Result<SrtTransport, TransportError> {
    if cancel.is_cancelled() {
        return Err(TransportError::ExplicitClose);
    }
    let bind_host = if host.is_empty() { "0.0.0.0" } else { host };
    let addr = if host.contains(':') && !host.starts_with('[') {
        format!("[{bind_host}]:{port}")
    } else {
        format!("{bind_host}:{port}")
    };
    let mut listener =
        Listener::bind_with(cfg, addr.as_str()).map_err(|e| TransportError::Broken {
            msg: format!("bind: {e}"),
            errno_code: None,
        })?;
    cancel.install(Arc::new(listener.cancel_handle()));
    let accepted = listener.accept();
    cancel.clear();
    match accepted {
        Ok((socket, _peer)) => Ok(SrtTransport::new(socket)),
        Err(_) if cancel.is_cancelled() => Err(TransportError::ExplicitClose),
        Err(e) => Err(TransportError::Broken {
            msg: format!("accept: {e}"),
            errno_code: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// JniManagedMuxSender — wraps MuxSender<ManagedTransport<SrtTransport>>.
// ---------------------------------------------------------------------------

/// Native backing for `org.tstrans.srt.ManagedMuxSender`. Single-threaded box
/// (NO inner mutex; the Rust shell serialises `send_*` through its own
/// `Mutex<Inner>`). `factory_attempts` is bumped from inside the reconnect
/// factory closure on every invocation.
struct JniManagedMuxSender {
    inner: RustMuxSender<ManagedTransport<SrtTransport>>,
    factory_attempts: Arc<AtomicU64>,
    /// Reconnect/gap telemetry observer, snapshotted from the
    /// `ManagedTransport` BEFORE it moves into `RustMuxSender::new` (same
    /// pattern as `ManagedSender`'s `cancel_handle` capture in managed_basic.rs).
    stats_handle: tst_pipeline::ManagedStatsHandle,
}

/// Per-type leased-handle registry for `org.tstrans.srt.ManagedMuxSender`.
static REGISTRY_MUX: LazyLock<HandleRegistry<JniManagedMuxSender>> =
    LazyLock::new(HandleRegistry::new);

/// Map a `MuxSenderError` (from any `send_*`) to a thrown Java exception.
/// `Mux(...)` → `MuxException`; `Transport(...)` → `SrtException` per
/// `TransportError` variant. Mirrors tst-py's `mux_sender_error_to_pyerr`.
fn throw_managed_mux_sender_error(env: &mut JNIEnv, e: &MuxSenderError) {
    match &e.source {
        MuxSenderErrorSource::Mux(m) => throw_mux_error(env, m),
        MuxSenderErrorSource::Transport(t) => transport_error(env, t),
        // `MuxSenderErrorSource` may gain variants; route any future one to a
        // generic SrtException(IO) with the Display message preserved.
        _ => throw_srt(env, "IO", &e.to_string()),
    }
}

/// Lease the managed sender and run a push op under the resource lock. A closed
/// handle throws `IllegalStateException`; any `MuxSenderError` is mapped.
fn with_mux_push(
    env: &mut JNIEnv,
    handle: jlong,
    op: impl FnOnce(&RustMuxSender<ManagedTransport<SrtTransport>>) -> Result<(), MuxSenderError>,
) {
    match REGISTRY_MUX.with_poisoning(handle as u64, |jstruct| op(&jstruct.inner)) {
        Some(Ok(())) => {}
        Some(Err(e)) => throw_managed_mux_sender_error(env, &e),
        None => {
            crate::error::throw_closed(env, "ManagedMuxSender");
        }
    }
}

/// Lease the managed sender and return the first handle-of-kind (`-1` if none).
/// A closed handle throws `IllegalStateException` and returns `-1`.
fn mux_first_handle(
    env: &mut JNIEnv,
    handle: jlong,
    pick: impl FnOnce(&RustMuxSender<ManagedTransport<SrtTransport>>) -> Option<u32>,
) -> jlong {
    match REGISTRY_MUX.with(handle as u64, |jstruct| pick(&jstruct.inner)) {
        Some(Some(raw)) => i64::from(raw),
        Some(None) => -1,
        None => {
            crate::error::throw_closed(env, "ManagedMuxSender");
            -1
        }
    }
}

/// `ManagedMuxSender.nFromUrl(url, ...programConfig..., ...policyArgs...)` —
/// build the muxer config, parse the caller-mode URL, do the initial connect,
/// wrap it in a `ManagedTransport`, and hand it to `MuxSender`. Returns the
/// boxed handle as `jlong`, or `0` with a pending exception on any failure.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nFromUrl<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
    program_number: jint,
    pmt_pid: jint,
    pcr_pid: jint,
    pcr_interval_ms: jint,
    psi_interval_ms: jint,
    buffer_packets: jint,
    av1_carriage: jint,
    stream_pids: JIntArray<'local>,
    stream_kinds: JIntArray<'local>,
    stream_codecs: JIntArray<'local>,
    stream_type_codes: JIntArray<'local>,
    stream_carries_pts: JBooleanArray<'local>,
    data_desc_bytes: JByteArray<'local>,
    data_desc_lens: JIntArray<'local>,
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
        // Build the MuxerConfig FIRST — a pending MuxException is thrown on Err(()).
        let muxer_cfg = match build_muxer_config_from_arrays(
            env,
            program_number,
            pmt_pid,
            pcr_pid,
            pcr_interval_ms,
            psi_interval_ms,
            buffer_packets,
            av1_carriage,
            &stream_pids,
            &stream_kinds,
            &stream_codecs,
            &stream_type_codes,
            &stream_carries_pts,
            &data_desc_bytes,
            &data_desc_lens,
        ) {
            Ok(c) => c,
            Err(()) => return 0,
        };

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
        if parsed.mode != Mode::Caller {
            let msg = format!(
                "ManagedMuxSender.fromUrl requires mode=caller (default); got mode={:?}",
                parsed.mode
            );
            throw_srt(env, "CONFIG_INVALID", &msg);
            return 0;
        }

        let mut sock_cfg = SocketConfig::default();
        parsed.overlay.apply_to_socket(&mut sock_cfg);

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

        // Reconnect factory: bump the attempt counter on every call, then dial.
        // `ManagedTransport::new` requires `Fn + Send + Sync`.
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_factory = attempts.clone();
        let host_for_factory = parsed.host.clone();
        let port_for_factory = parsed.port;
        let cfg_for_factory = sock_cfg.clone();
        let factory = move || -> Result<SrtTransport, TransportError> {
            attempts_for_factory.fetch_add(1, Ordering::Release);
            connect_srt(&host_for_factory, port_for_factory, &cfg_for_factory)
        };

        // Initial connect — failure maps to CONNECT_FAILED (divergence #6), not
        // BROKEN, so callers can distinguish it from runtime reconnect failures.
        let initial = match connect_srt(&parsed.host, parsed.port, &sock_cfg) {
            Ok(t) => t,
            Err(e) => {
                let msg = match &e {
                    TransportError::Broken { msg, .. } => msg.clone(),
                    _ => format!("{e:?}"),
                };
                throw_srt(env, "CONNECT_FAILED", &msg);
                return 0;
            }
        };

        let managed = ManagedTransport::new(initial, factory, policy);
        // Snapshot the stats handle BEFORE moving `managed` into `RustMuxSender::new`
        // (same pattern as `ManagedSender`'s stats_handle capture).
        let stats_handle = managed.stats_handle();
        match RustMuxSender::new(managed, muxer_cfg) {
            Ok(sender) => REGISTRY_MUX.insert(JniManagedMuxSender {
                inner: sender,
                factory_attempts: attempts,
                stats_handle,
            }) as jlong,
            Err(e) => {
                throw_mux_error(env, &e);
                0
            }
        }
    })
}

// ── Send family — single-stream variants ───────────────────────────────────

/// `nSendVideo(handle, nal, pts, keyFrame)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendVideo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_video(&buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

/// `nSendKlv(handle, klv, pts, metadataServiceId)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendKlv<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    klv: JByteArray<'local>,
    pts: jlong,
    metadata_service_id: jint,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Ok(service_id) = checked_u8(env, i64::from(metadata_service_id), "metadataServiceId")
        else {
            return; // IllegalArgumentException pending
        };
        let Some(buf) = read_bytes(env, &klv) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_klv(&buf, Pts90khz::new(pts), service_id)
        });
    })
}

/// `nSendAudio(handle, frames, pts)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendAudio<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    frames: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_audio(&buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendSubtitle(handle, pts, payload)` — note the swapped arg order.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendSubtitle<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    pts: jlong,
    payload: JByteArray<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_subtitle(&buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendData(handle, data, pts)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendData<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    data: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &data) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_data(&buf, Pts90khz::new(pts))
        });
    })
}

// ── Send family — handle-targeted variants ─────────────────────────────────

/// `nSendVideoTo(handle, streamHandleRaw, nal, pts, keyFrame)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendVideoTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| VideoStreamHandle::try_from_raw(r).ok())
        else {
            throw_srt(env, "CONFIG_INVALID", "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_video_to(h, &buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

/// `nSendKlvTo(handle, streamHandleRaw, klv, pts, metadataServiceId)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendKlvTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    klv: JByteArray<'local>,
    pts: jlong,
    metadata_service_id: jint,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| KlvStreamHandle::try_from_raw(r).ok())
        else {
            throw_srt(env, "CONFIG_INVALID", "invalid stream handle");
            return;
        };
        let Ok(service_id) = checked_u8(env, i64::from(metadata_service_id), "metadataServiceId")
        else {
            return;
        };
        let Some(buf) = read_bytes(env, &klv) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_klv_to(h, &buf, Pts90khz::new(pts), service_id)
        });
    })
}

/// `nSendAudioTo(handle, streamHandleRaw, frames, pts)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendAudioTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    frames: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| AudioStreamHandle::try_from_raw(r).ok())
        else {
            throw_srt(env, "CONFIG_INVALID", "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_audio_to(h, &buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendSubtitleTo(handle, streamHandleRaw, pts, payload)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendSubtitleTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    pts: jlong,
    payload: JByteArray<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| SubtitleStreamHandle::try_from_raw(r).ok())
        else {
            throw_srt(env, "CONFIG_INVALID", "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_subtitle_to(h, &buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendDataTo(handle, streamHandleRaw, data, pts)`. The raw handle is
/// validated via the strict `u32::try_from` + `DataStreamHandle::try_from_raw`
/// chain (rejecting negative / out-of-u32 values up front rather than
/// truncating, mirroring `MuxSender::nSendDataTo`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSendDataTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    data: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| DataStreamHandle::try_from_raw(r).ok())
        else {
            throw_srt(env, "CONFIG_INVALID", "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &data) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_data_to(h, &buf, Pts90khz::new(pts))
        });
    })
}

// ── Handle getters ─────────────────────────────────────────────────────────

/// `nVideoHandle(handle)` — first configured video stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nVideoHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        mux_first_handle(env, handle, |inner| {
            inner.video_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nKlvHandle(handle)` — first configured KLV stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nKlvHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        mux_first_handle(env, handle, |inner| {
            inner.klv_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nAudioHandle(handle)` — first configured audio stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nAudioHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        mux_first_handle(env, handle, |inner| {
            inner.audio_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nSubtitleHandle(handle)` — first configured subtitle stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nSubtitleHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        mux_first_handle(env, handle, |inner| {
            inner.subtitle_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nDataHandle(handle)` — first configured data stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nDataHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        mux_first_handle(env, handle, |inner| {
            inner.data_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

// ── Stats + lifecycle ──────────────────────────────────────────────────────

/// `nStats(handle)` — `TransportStats` projecting the SRT socket counters + the
/// muxer's program/packet totals. Identical to `mux_sender.rs::nStats`. Returns
/// null on a JNI builder error (non-fatal).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some((sock, pipe)) = REGISTRY_MUX.with(handle as u64, |jstruct| {
            (
                jstruct.inner.socket_stats().unwrap_or_default(),
                jstruct.inner.stats(),
            )
        }) else {
            crate::error::throw_closed(env, "ManagedMuxSender");
            return JObject::null();
        };

        let sock_obj = match build_socket_stats(env, "org/tstrans/srt/SocketStats", &sock) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        let mux_obj = match build_muxer_stats(
            env,
            pipe.packets_sent as i64,
            pipe.bytes_sent as i64,
            i64::from(pipe.programs_configured),
        ) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        match build_transport_stats(env, &sock_obj, &mux_obj) {
            Ok(o) => o,
            Err(_) => JObject::null(),
        }
    })
}

/// `nReconnectAttempts(handle)` — total factory invocations since construction.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nReconnectAttempts(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        REGISTRY_MUX
            .with(handle as u64, |jstruct| {
                jstruct.factory_attempts.load(Ordering::Acquire) as jlong
            })
            .unwrap_or_else(|| {
                crate::error::throw_closed(env, "ManagedMuxSender");
                0
            })
    })
}

/// `nReconnectStats(handle)` — reconnect/gap telemetry: attempts, successes,
/// current gap-buffer depth, and drop counters. Throws `SrtException(IO)` if the
/// internal gap-buffer lock is poisoned — a read-only telemetry path must not
/// panic.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nReconnectStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(maybe_stats) =
            REGISTRY_MUX.with(handle as u64, |jstruct| jstruct.stats_handle.stats())
        else {
            crate::error::throw_closed(env, "ManagedMuxSender");
            return JObject::null();
        };
        let Some(stats) = maybe_stats else {
            throw_srt(env, "IO", "reconnect stats unavailable: gap lock poisoned");
            return JObject::null();
        };
        match build_managed_transport_stats(env, &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// `nClose(handle)` — drop the boxed sender (best-effort drain + close). No-op on
/// a zero handle so a double `close()` is safe.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(jstruct) = REGISTRY_MUX.close(handle as u64) {
            jstruct.inner.close();
        }
    })
}

/// `nIsAlive(handle)` — whether the sender owns a live transport.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedMuxSender_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_MUX
            .with(handle as u64, |jstruct| u8::from(jstruct.inner.is_alive()))
            .unwrap_or(0)
    })
}

// ---------------------------------------------------------------------------
// JniManagedDemuxReceiver — wraps ManagedDemuxReceiver<SrtTransport>.
// ---------------------------------------------------------------------------

/// Native backing for `org.tstrans.srt.ManagedDemuxReceiver`. Single-threaded
/// box (NO inner mutex, NO byte sink — divergence #5). `factory_attempts` is
/// bumped from inside the reconnect factory closure on every invocation.
struct JniManagedDemuxReceiver {
    inner: RustManagedDemuxReceiver<SrtTransport>,
    factory_attempts: Arc<AtomicU64>,
}

/// Per-type leased-handle registry for `org.tstrans.srt.ManagedDemuxReceiver`. No
/// cancel hook (single-threaded; the public cancel handle wakes a parked recv).
static REGISTRY_DEMUX: LazyLock<HandleRegistry<JniManagedDemuxReceiver>> =
    LazyLock::new(HandleRegistry::new);

/// Shared construction body for `nFromUrl` / `nFromUrlWithConfig`: parse the URL
/// (accepting BOTH listener and caller mode — divergence #2), do the initial
/// listen/connect, wrap in a `ManagedRecvTransport` + `ManagedDemuxReceiver`.
/// Returns the boxed handle as `jlong`, or `0` with a pending exception.
#[allow(clippy::too_many_arguments)]
fn build_demux_from_url(
    env: &mut JNIEnv,
    url: &JString,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    max_attempts_present: jboolean,
    max_attempts: jint,
    backoff_kind: jint,
    backoff_base_ms: jlong,
    backoff_max_ms: jlong,
    gap_buffer_capacity: jint,
    overflow_policy: jint,
    mode: jint,
) -> jlong {
    let url_str: String = match env.get_string(url) {
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
    // Divergence #2: accept BOTH modes — listener (default) AND caller.
    let is_listener = parsed.mode == Mode::Listener;
    let mut listener_cfg = ListenerConfig::default();
    let mut sock_cfg = SocketConfig::default();
    if is_listener {
        parsed.overlay.apply_to_listener(&mut listener_cfg);
    } else {
        parsed.overlay.apply_to_socket(&mut sock_cfg);
    }
    let host = parsed.host.clone();
    let port = parsed.port;

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

    // Reconnect factory: bump the attempt counter on every call, then re-bind /
    // re-dial per the captured mode. `ManagedRecvTransport::new` takes
    // `Box<dyn FnMut() -> Result<R, TransportError> + Send>` (no Sync bound).
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_factory = attempts.clone();
    let host_for_factory = host.clone();
    let listener_cfg_for_factory = listener_cfg.clone();
    let sock_cfg_for_factory = sock_cfg.clone();
    // Listener mode: the re-accept is reachable by `cancel()` through this
    // slot (see `listen_srt_cancellable`).
    let factory_cancel = Arc::new(tst_pipeline::FactoryCancel::new());
    let fc = Arc::clone(&factory_cancel);
    let factory: Box<dyn FnMut() -> Result<SrtTransport, TransportError> + Send> =
        Box::new(move || {
            attempts_for_factory.fetch_add(1, Ordering::Release);
            if is_listener {
                listen_srt_cancellable(&host_for_factory, port, &listener_cfg_for_factory, &fc)
            } else {
                connect_srt(&host_for_factory, port, &sock_cfg_for_factory)
            }
        });

    // Initial transport (listen or connect) — failure maps to CONNECT_FAILED
    // (divergence #6), not BROKEN.
    let initial = if is_listener {
        listen_srt(&host, port, &listener_cfg)
    } else {
        connect_srt(&host, port, &sock_cfg)
    };
    let initial = match initial {
        Ok(t) => t,
        Err(e) => {
            let msg = match &e {
                TransportError::Broken { msg, .. } => msg.clone(),
                _ => format!("{e:?}"),
            };
            throw_srt(env, "CONNECT_FAILED", &msg);
            return 0;
        }
    };

    let managed =
        ManagedRecvTransport::new_with_factory_cancel(initial, factory, policy, factory_cancel);
    let receiver = match opts {
        None => RustManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default()),
        Some(opts) => RustManagedDemuxReceiver::with_demux_options(
            managed,
            opts,
            ManagedDemuxReceiverConfig::default(),
        ),
    };
    // Capture the cancel target BEFORE the receiver is boxed: it lives outside
    // the registry's resource lock, so `nCancelHandle` returns while `nNext` is
    // parked (including a listener-mode re-accept). The managed handle follows
    // reconnects, so one capture at open is enough. Mirrors tst-py, which fails
    // construction rather than exposing a receiver with no cancel path.
    let Some(target) = receiver.cancel_handle() else {
        let _ = env.throw_new(
            "java/lang/IllegalStateException",
            "ManagedDemuxReceiver constructed without a live cancel handle",
        );
        return 0;
    };
    REGISTRY_DEMUX.insert_with_target(
        JniManagedDemuxReceiver {
            inner: receiver,
            factory_attempts: attempts,
        },
        target,
    ) as jlong
}

/// `ManagedDemuxReceiver.nFromUrl(url, ...policyArgs...)` — default demux options.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nFromUrl<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
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
        build_demux_from_url(
            env,
            &url,
            None,
            max_attempts_present,
            max_attempts,
            backoff_kind,
            backoff_base_ms,
            backoff_max_ms,
            gap_buffer_capacity,
            overflow_policy,
            mode,
        )
    })
}

/// `ManagedDemuxReceiver.nFromUrlWithConfig(url, ...policyArgs..., ...demuxArgs...)`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nFromUrlWithConfig<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
    max_attempts_present: jboolean,
    max_attempts: jint,
    backoff_kind: jint,
    backoff_base_ms: jlong,
    backoff_max_ms: jlong,
    gap_buffer_capacity: jint,
    overflow_policy: jint,
    mode: jint,
    strict: jint,
    pes_cap_per_pid: jlong,
    pes_cap_total: jlong,
    cfi: jboolean,
    av1: jint,
    au_cell_cap: jlong,
    lenient_psi: jboolean,
    sync_buf_cap: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let Some(opts) = build_demux_config_from_args(
            env,
            strict,
            pes_cap_per_pid,
            pes_cap_total,
            cfi,
            av1,
            au_cell_cap,
            lenient_psi,
            sync_buf_cap,
        ) else {
            return 0;
        };
        build_demux_from_url(
            env,
            &url,
            Some(opts),
            max_attempts_present,
            max_attempts,
            backoff_kind,
            backoff_base_ms,
            backoff_max_ms,
            gap_buffer_capacity,
            overflow_policy,
            mode,
        )
    })
}

/// `nNext(handle)` — block until the next `DemuxEvent`, returning it as a Java
/// object; Java `null` on clean EOF. Throws `SrtException` / `DemuxException` on
/// a recv-side error. Emits `DemuxEvent.ReconnectDiscontinuity` once after each
/// transport reconnect. No byte sink (divergence #5), so no captured-exception
/// drain.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nNext<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // recv_event runs INSIDE the registry lease (under the resource lock,
        // single-threaded per the Receiver model).
        let Some(res) =
            REGISTRY_DEMUX.with_poisoning(handle as u64, |jstruct| jstruct.inner.recv_event())
        else {
            crate::error::throw_closed(env, "ManagedDemuxReceiver");
            return JObject::null().into_raw();
        };
        match res {
            Ok(None) => JObject::null().into_raw(),
            Ok(Some(ev)) => match convert_event(env, &ev) {
                Ok(Some(obj)) => obj.into_raw(),
                // All current `DemuxEvent` variants map to a record; retained as a
                // forward-compat guard (mirrors demux_receiver::nNext).
                Ok(None) => JObject::null().into_raw(),
                Err(()) => {
                    // Event-conversion JNI failure. `throw_demux` guards against
                    // clobbering a pending exception; the INTERNAL literal stays
                    // ratchet-visible.
                    crate::error::throw_demux(env, "INTERNAL", "event conversion failed");
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                super::demux_receiver::throw_demux_recv_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

/// `nCancelHandle(handle)` — return a shareable cancel handle that wakes a thread
/// parked in `nNext`. Lock-free: the target was captured at open, so this returns
/// promptly even while another thread is parked in `nNext` (a blocked receive or
/// a listener-mode re-accept) and regardless of reconnect state. Throws
/// `IllegalStateException` on a closed handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY_DEMUX.cancel_target(handle as u64) {
            Some(inner) => JniCancel {
                inner,
                flag: AtomicBool::new(false),
            }
            .into_handle(),
            None => {
                crate::error::throw_closed(env, "ManagedDemuxReceiver");
                0
            }
        }
    })
}

/// `nSocketStats(handle)` — scheme-neutral 16-field wire stats. Uses
/// `unwrap_or_default` so a mid-reconnect receiver yields a zeroed snapshot.
/// Returns null on a JNI builder error (non-fatal).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY_DEMUX.with(handle as u64, |jstruct| {
            jstruct.inner.socket_stats().unwrap_or_default()
        }) else {
            crate::error::throw_closed(env, "ManagedDemuxReceiver");
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/srt/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// `nSrtStats(handle)` — stats drift (divergence #4): returns the SAME
/// `SocketStats` view as `nSocketStats` and does NOT throw. Mirrors tst-py's
/// `PyManagedDemuxReceiver::srt_stats`, which delegates to `socket_stats`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nSrtStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY_DEMUX.with(handle as u64, |jstruct| {
            jstruct.inner.socket_stats().unwrap_or_default()
        }) else {
            crate::error::throw_closed(env, "ManagedDemuxReceiver");
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/srt/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// `nLastSeenMicros(handle, pid)` — Unix-epoch microsecond timestamp the
/// stream identified by `pid` last carried a demuxed item through this
/// receiver (last emitted event); `-1` if `pid` was never seen — including
/// an unrecognized PID (no range check beyond the native `u16` truncating
/// cast, same as `pmtPid`/`pcrPid` elsewhere in this binding) — or a
/// timestamp predating the Unix epoch. Boxed to `Long` (`null` for `-1`) at
/// the Java layer. Same registry-lock discipline as `nSocketStats`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nLastSeenMicros(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    pid: jint,
) -> jlong {
    crate::panic::jni_catch(&mut env, -1, |env| {
        let Some(last_seen) = REGISTRY_DEMUX.with(handle as u64, |jstruct| {
            jstruct
                .inner
                .stats()
                .per_stream
                .get(&(pid as u16))
                .and_then(|s| s.last_seen)
        }) else {
            crate::error::throw_closed(env, "ManagedDemuxReceiver");
            return -1;
        };
        last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as i64)
            .unwrap_or(-1)
    })
}

/// `nReconnectAttempts(handle)` — total factory invocations since construction.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nReconnectAttempts(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        REGISTRY_DEMUX
            .with(handle as u64, |jstruct| {
                jstruct.factory_attempts.load(Ordering::Acquire) as jlong
            })
            .unwrap_or_else(|| {
                crate::error::throw_closed(env, "ManagedDemuxReceiver");
                0
            })
    })
}

/// `nClose(handle)` — close the underlying transport and drop the box. No-op on a
/// zero handle so a double `close()` is safe.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(mut jstruct) = REGISTRY_DEMUX.close(handle as u64) {
            jstruct.inner.close();
        }
    })
}

/// `nIsAlive(handle)` — whether the receiver owns a live transport.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_ManagedDemuxReceiver_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY_DEMUX
            .with(handle as u64, |jstruct| u8::from(jstruct.inner.is_alive()))
            .unwrap_or(0)
    })
}
