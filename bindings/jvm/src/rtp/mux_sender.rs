//! JNI surface for `org.tstrans.rtp.MuxSender` — the single-call convenience
//! wrapper that owns a `Muxer` + an RTP `RtpTransport`.
//!
//! Wraps `tst_pipeline::MuxSender<tst_rtp::RtpTransport>`. `nFromUrl` builds the
//! `MuxerConfig` from the parallel-array program description (via the shared
//! `crate::mpegts::muxer::build_muxer_config_from_arrays` helper, byte-exact with
//! `Muxer::nOpen`), then builds an `RtpTransport` from the `rtp://` URL + pkt_size
//! and hands transport + config to the pipeline shell. Ports tst-py's
//! `bindings/python/src/rtp/mux_sender.rs`.
//!
//! The handle is a `Box<MuxSender<RtpTransport>>`; per-call methods reconstitute
//! as a SHARED `&*ptr` borrow (every `MuxSender::send_*`/`stats`/`is_alive` takes
//! `&self`, serialising internally via its own `Mutex<Inner>`), so concurrent
//! pushes from multiple Java threads are sound — no aliased `&mut`. `nClose` drops
//! the box.
//!
//! Error mapping mirrors tst-py's `mux_sender_error_to_pyerr`: `Mux(...)` →
//! `MuxException`, `Transport(...)` → `RtpException` per `TransportError` variant,
//! forward-compat catch-all → `RtpException(TRANSPORT)`.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::{JBooleanArray, JByteArray, JClass, JIntArray, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_pipeline::binding::BindingErrorKind;
use tst_pipeline::binding::Owned;
use tst_pipeline::{MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource};
use tst_rtp::{RtpSocketBuilder, RtpTransport};

use crate::handle::OwnedRegistry;
use crate::jutil::{build_socket_stats, checked_u8, read_bytes};
use crate::mpegts::build_muxer_stats;
use crate::mpegts::muxer::{build_muxer_config_from_arrays, throw_mux_error};

use super::errors::{connect_error, rtp_url_error, throw_rtp, transport_error};

type Inner = RustMuxSender<RtpTransport>;

/// Per-type `Owned`-backed registry for `org.tstrans.rtp.MuxSender`.
///
/// Moved here (not in Task B3.5, which owns the rest of the rtp surface) only
/// because `handle::with_push` / `handle::first_handle` are now typed over
/// [`OwnedRegistry`] and this file is their other user — keeping the tree
/// building. B3.5 replaces the `register` helper below with the shared
/// `rtp_cancel` path and adds the cancel-first close test.
static REGISTRY: LazyLock<OwnedRegistry<Inner>> = LazyLock::new(OwnedRegistry::new);

/// Register a `MuxSender<RtpTransport>` as an `Owned` entry. `RtpTransport`
/// always yields a cancel handle (`crates/tst-rtp/src/transport.rs:370`); the
/// `None` arm is the type's, not a reachable state, and is reported rather than
/// `expect`ed (spec §3.4 retires the `.expect("… always Some")` sites).
fn register(env: &mut JNIEnv, sender: Inner) -> jlong {
    let cancel = match super::rtp_cancel(sender.cancel_handle(), "RtpTransport") {
        Ok(c) => c,
        Err(e) => {
            // `Internal` is deliberately NOT an `RtpException.Kind` member: a
            // transport with no cancel handle is a tst-rtp bug, not a transport
            // outcome, so it surfaces as a plain RuntimeException like every
            // other JVM-level failure in this crate.
            let _ = env.throw_new("java/lang/RuntimeException", e.detail);
            return 0;
        }
    };
    REGISTRY.insert(Owned::new(sender, cancel, ())) as jlong
}

/// Map a `MuxSenderError` (from any `send_*`) to a thrown Java exception.
/// `Mux(...)` → `MuxException`; `Transport(...)` → `RtpException` per
/// `TransportError` variant. Mirrors tst-py's `mux_sender_error_to_pyerr`.
fn throw_mux_sender_error(env: &mut JNIEnv, e: &MuxSenderError) {
    match &e.source {
        MuxSenderErrorSource::Mux(m) => throw_mux_error(env, m),
        MuxSenderErrorSource::Transport(t) => transport_error(env, t),
        // `MuxSenderErrorSource` may gain variants; route any future one to a
        // generic RtpException(TRANSPORT) with the Display message preserved.
        _ => throw_rtp(env, BindingErrorKind::RtpIo, &e.to_string()),
    }
}

/// Build a `MuxSender<RtpTransport>` from an `rtp://` URL + pkt_size + a built
/// `MuxerConfig`. Returns the boxed handle as `jlong`, or `0` with a pending
/// exception on any failure.
fn build_from_url(
    env: &mut JNIEnv,
    url: &JString,
    cfg: tst_core::mpegts::mux::MuxerConfig,
    pkt_size: jint,
) -> jlong {
    let url_str: String = match env.get_string(url) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            return 0;
        }
    };

    let mut builder = match RtpSocketBuilder::from_url(&url_str) {
        Ok(b) => b,
        Err(e) => {
            rtp_url_error(env, &e);
            return 0;
        }
    };
    builder.pkt_size(pkt_size.max(0) as usize);
    let transport = match builder.build() {
        Ok(t) => t,
        Err(e) => {
            connect_error(env, e);
            return 0;
        }
    };

    match RustMuxSender::new(transport, cfg) {
        Ok(sender) => register(env, sender),
        Err(e) => {
            throw_mux_error(env, &e);
            0
        }
    }
}

/// `MuxSender.nFromUrl(url, ...programConfig..., pktSize)` — build the muxer
/// config, build an RTP transport, and return a `Box<MuxSender<RtpTransport>>`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nFromUrl<'local>(
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
    pkt_size: jint,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Build the MuxerConfig FIRST — a pending MuxException is thrown on Err(()).
        let cfg = match build_muxer_config_from_arrays(
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
        build_from_url(env, &url, cfg, pkt_size)
    })
}

/// Lease the sender and run a push op under the resource lock. A closed handle
/// throws `IllegalStateException`; any `MuxSenderError` is mapped.
fn with_push(
    env: &mut JNIEnv,
    handle: jlong,
    op: impl FnOnce(&Inner) -> Result<(), MuxSenderError>,
) {
    crate::handle::with_push(
        env,
        &REGISTRY,
        handle,
        "MuxSender",
        op,
        throw_mux_sender_error,
    );
}

/// Lease the sender and return the first handle-of-kind (`-1` if none). A closed
/// handle throws `IllegalStateException` and returns `-1`.
fn first_handle(
    env: &mut JNIEnv,
    handle: jlong,
    pick: impl FnOnce(&Inner) -> Option<u32>,
) -> jlong {
    crate::handle::first_handle(env, &REGISTRY, handle, "MuxSender", pick)
}

// ── Send family — single-stream variants ───────────────────────────────────

/// `nSendVideo(handle, nal, pts, keyFrame)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendVideo<'local>(
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
        with_push(env, handle, |inner| {
            inner.send_video(&buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

/// `nSendKlv(handle, klv, pts, metadataServiceId)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendKlv<'local>(
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
        with_push(env, handle, |inner| {
            inner.send_klv(&buf, Pts90khz::new(pts), service_id)
        });
    })
}

/// `nSendAudio(handle, frames, pts)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendAudio<'local>(
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
        with_push(env, handle, |inner| {
            inner.send_audio(&buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendSubtitle(handle, pts, payload)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendSubtitle<'local>(
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
        with_push(env, handle, |inner| {
            inner.send_subtitle(&buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendData(handle, data, pts)` — pass-through send onto the lone configured
/// data stream. No AU-cell wrap, no framing; one send = one PES on stream_id
/// `0xBD` (`private_stream_1`). `pts` is written into the PES header only for
/// `carries_pts` streams but always drives PSI/PCR pacing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendData<'local>(
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
        with_push(env, handle, |inner| {
            inner.send_data(&buf, Pts90khz::new(pts))
        });
    })
}

// ── Send family — handle-targeted variants ─────────────────────────────────

/// `nSendVideoTo(handle, streamHandleRaw, nal, pts, keyFrame)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendVideoTo<'local>(
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
            throw_rtp(env, BindingErrorKind::RtpIo, "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_video_to(h, &buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

/// `nSendKlvTo(handle, streamHandleRaw, klv, pts, metadataServiceId)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendKlvTo<'local>(
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
            throw_rtp(env, BindingErrorKind::RtpIo, "invalid stream handle");
            return;
        };
        let Ok(service_id) = checked_u8(env, i64::from(metadata_service_id), "metadataServiceId")
        else {
            return;
        };
        let Some(buf) = read_bytes(env, &klv) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_klv_to(h, &buf, Pts90khz::new(pts), service_id)
        });
    })
}

/// `nSendAudioTo(handle, streamHandleRaw, frames, pts)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendAudioTo<'local>(
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
            throw_rtp(env, BindingErrorKind::RtpIo, "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_audio_to(h, &buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendSubtitleTo(handle, streamHandleRaw, pts, payload)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendSubtitleTo<'local>(
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
            throw_rtp(env, BindingErrorKind::RtpIo, "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_subtitle_to(h, &buf, Pts90khz::new(pts))
        });
    })
}

/// `nSendDataTo(handle, streamHandleRaw, data, pts)`. The raw handle is
/// validated via the strict `u32::try_from` + `DataStreamHandle::try_from_raw`
/// chain (rejecting negative / out-of-u32 values up front rather than
/// truncating, mirroring `Muxer::nPushDataTo`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSendDataTo<'local>(
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
            throw_rtp(env, BindingErrorKind::RtpIo, "invalid stream handle");
            return;
        };
        let Some(buf) = read_bytes(env, &data) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_data_to(h, &buf, Pts90khz::new(pts))
        });
    })
}

// ── Handle getters (first-of-kind; -1 = none) ──────────────────────────────

/// `nVideoHandle(handle)` — first configured video stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nVideoHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        first_handle(env, handle, |inner| {
            inner.video_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nKlvHandle(handle)` — first configured KLV stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nKlvHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        first_handle(env, handle, |inner| {
            inner.klv_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nAudioHandle(handle)` — first configured audio stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nAudioHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        first_handle(env, handle, |inner| {
            inner.audio_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nSubtitleHandle(handle)` — first configured subtitle stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nSubtitleHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        first_handle(env, handle, |inner| {
            inner.subtitle_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

/// `nDataHandle(handle)` — first configured data stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nDataHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        first_handle(env, handle, |inner| {
            inner.data_handles().into_iter().next().map(|h| h.raw())
        })
    })
}

// ── Stats ──────────────────────────────────────────────────────────────────

/// Build an `org.tstrans.rtp.TransportStats` record from an rtp `SocketStats` +
/// `MuxerStats` pair. Shared with `demux_receiver.rs`.
pub(crate) fn build_rtp_transport_stats<'local>(
    env: &mut JNIEnv<'local>,
    socket_stats: &JObject<'local>,
    muxer_stats: &JObject<'local>,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    env.new_object(
        "org/tstrans/rtp/TransportStats",
        "(Lorg/tstrans/rtp/SocketStats;Lorg/tstrans/mpegts/MuxerStats;)V",
        &[JValue::Object(socket_stats), JValue::Object(muxer_stats)],
    )
}

/// `nStats(handle)` — `(SocketStats, MuxerStats)` projection mirroring tst-py's
/// `MuxSender.stats`. Returns null on a JNI builder error (non-fatal).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let (sock, pipe) = match REGISTRY.with_ref(handle as u64, |inner| {
            (inner.socket_stats().unwrap_or_default(), inner.stats())
        }) {
            Ok(v) => v,
            Err(state) => {
                crate::error::throw_handle_state(env, "MuxSender", &state);
                return JObject::null();
            }
        };

        let sock_obj = match build_socket_stats(env, "org/tstrans/rtp/SocketStats", &sock) {
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
        match build_rtp_transport_stats(env, &sock_obj, &mux_obj) {
            Ok(o) => o,
            Err(_) => JObject::null(),
        }
    })
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

/// `nClose(handle)` — drop the boxed `MuxSender`. No-op on a zero handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(inner) = REGISTRY.close(handle as u64) {
            inner.close();
        }
    })
}

/// `nIsAlive(handle)` — whether the sender owns a live transport.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_MuxSender_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY
            .with_ref(handle as u64, |inner| u8::from(inner.is_alive()))
            .unwrap_or(0)
    })
}
