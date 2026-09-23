//! JNI surface for `org.tstrans.srt.MuxSender` — the single-call convenience
//! wrapper that owns a `Muxer` + `SrtTransport`.
//!
//! Wraps `tst_pipeline::MuxSender<tst_srt::SrtTransport>`. `nFromUrl` builds the
//! `MuxerConfig` from the parallel-array program description (via the shared
//! [`crate::mpegts::muxer::build_muxer_config_from_arrays`] helper, byte-exact
//! with `Muxer::nOpen`), then parses the SRT caller-mode URL, connects, and
//! hands the transport + config to the pipeline shell. The push family pushes
//! elementary streams; each call ends in an `SrtTransport::send_bytes` flush.
//!
//! Ports tst-py's `bindings/python/src/srt/mux_sender.rs`. The handle is a
//! `jlong` key into a per-type [`crate::handle::HandleRegistry`] over the
//! `MuxSender<SrtTransport>`; per-call methods lease via `REGISTRY.with` (every
//! `MuxSender::send_*`/`stats`/`is_alive` takes `&self`, serialising internally
//! via its own `Mutex<Inner>`), so concurrent pushes from multiple Java threads
//! are sound. `nClose` takes + drops via `REGISTRY.close`, which fires the
//! shell's cancel target BEFORE taking that resource lock (see [`register`]),
//! so a `send*` parked on another thread — libsrt's `srt_sendmsg` blocked on a
//! full send buffer — ends with `SrtException(CLOSED)` instead of holding
//! `close()` hostage.
//! `Socket::nIntoMuxSender` CONSUMES a `Socket` (via `REGISTRY_SOCKET.close`) and
//! returns a fresh handle.
//!
//! Error mapping mirrors tst-py's `mux_sender_error_to_pyerr`: `Mux(...)` →
//! `MuxException`, `Transport(...)` → `SrtException` per `TransportError`
//! variant, forward-compat catch-all → `SrtException(IO)`.

use std::sync::{Arc, LazyLock};

use jni::JNIEnv;
use jni::objects::{JBooleanArray, JByteArray, JClass, JIntArray, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::TransportCancel;
use tst_pipeline::binding::{BindingErrorKind, Owned};
use tst_pipeline::{MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use crate::handle::OwnedRegistry;
use crate::jutil::{build_socket_stats, checked_u8, decode_stream_handle, read_bytes};
use crate::mpegts::build_muxer_stats;
use crate::mpegts::muxer::{build_muxer_config_from_arrays, throw_mux_error};

use super::errors::{srt_error, throw_srt, transport_error};

type Inner = RustMuxSender<SrtTransport>;

/// Per-type `Owned`-backed registry for `org.tstrans.srt.MuxSender`.
static REGISTRY: LazyLock<OwnedRegistry<Inner>> = LazyLock::new(OwnedRegistry::new);

/// Map a `MuxSenderError` (from any `send_*`) to a thrown Java exception.
/// `Mux(...)` → `MuxException`; `Transport(...)` → `SrtException` per
/// `TransportError` variant. Mirrors tst-py's `mux_sender_error_to_pyerr`.
fn throw_mux_sender_error(env: &mut JNIEnv, e: &MuxSenderError) {
    match &e.source {
        MuxSenderErrorSource::Mux(m) => throw_mux_error(env, m),
        MuxSenderErrorSource::Transport(t) => transport_error(env, t),
        // `MuxSenderErrorSource` may gain variants; route any future one to a
        // generic SrtException(IO) with the Display message preserved.
        _ => throw_srt(env, BindingErrorKind::SrtIo, &e.to_string()),
    }
}

/// Register a plain `MuxSender` as an `Owned` entry. `MuxSender<T>` exposes no
/// `transport()` accessor, so the cancel target is taken from the transport
/// BEFORE `RustMuxSender::new` consumes it — the caller passes it in. `Owned`
/// keeps it outside the slot, so `nCancelHandle` answers while a `send*` is
/// parked (libsrt's blocking `srt_sendmsg` on a full send buffer) and
/// `OwnedRegistry::close` cancels first, ending that parked send promptly with
/// `SrtException(CLOSED)` — the cancel closes the socket under it and
/// `SrtTransport` reports the cancel — instead of holding `close()` hostage. The contract the C ABI's
/// `tst_mux_sender_close` and the plain srt receivers already have.
fn register(sender: Inner, cancel: Arc<dyn TransportCancel>) -> jlong {
    REGISTRY.insert(Owned::new(sender, cancel, ())) as jlong
}

/// Build a `MuxSender<SrtTransport>` from a parsed caller-mode URL + a built
/// `MuxerConfig`: parse the URL, reject non-Caller mode, connect, wrap. Returns
/// the boxed handle as `jlong`, or `0` with a pending exception on any failure.
/// Shared by `nFromUrl` (where the socket is created here).
fn build_from_url(
    env: &mut JNIEnv,
    url: &JString,
    cfg: tst_core::mpegts::mux::MuxerConfig,
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

    if parsed.mode != Mode::Caller {
        let msg = format!(
            "MuxSender.fromUrl requires mode=caller (default); got mode={:?}",
            parsed.mode
        );
        throw_srt(env, BindingErrorKind::ConfigInvalid, &msg);
        return 0;
    }

    // One open path (ARCH-01): `SrtUrl::connect_recv` applies the overlay to a
    // default `SocketConfig` and joins host:port (IPv6-bracketing included) —
    // exactly what this site composed via `join_host_port`. `connect_recv`, NOT
    // `connect`: this site has never merged the sender preset, and `connect`
    // would silently add `Role::Sender` + a 5 s linger + a 15 s connect timeout.
    let transport = match parsed.connect_recv() {
        Ok(t) => t,
        Err(e) => {
            srt_error(env, e);
            return 0;
        }
    };
    let cancel = super::srt_cancel(&transport);

    match RustMuxSender::new(transport, cfg) {
        Ok(sender) => register(sender, cancel),
        Err(e) => {
            throw_mux_error(env, &e);
            0
        }
    }
}

/// `MuxSender.nFromUrl(url, ...programConfig...)` — build the muxer config,
/// connect a caller-mode SRT socket, and return a `Box<MuxSender>` handle.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nFromUrl<'local>(
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
        build_from_url(env, &url, cfg)
    })
}

/// Lease the sender and run a push op under the resource lock. A closed/absent
/// handle throws `IllegalStateException`; otherwise any `MuxSenderError` from the
/// op is mapped to the right Java exception. `send_*` take `&self`, so the
/// `&mut Inner` from the registry coerces fine.
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

/// `nSendVideo(handle, nal, pts, keyFrame)` — send one video access unit.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendVideo<'local>(
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

/// `nSendKlv(handle, klv, pts, metadataServiceId)` — send one KLV blob. The
/// muxer auto-wraps the AU-cell header for synchronous-metadata streams; the
/// caller passes raw KLV LS bytes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendKlv<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    klv: JByteArray<'local>,
    pts: jlong,
    metadata_service_id: jint,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        // `metadata_service_id` is `u8`; range-check before narrowing.
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

/// `nSendAudio(handle, frames, pts)` — send one encoded audio frame.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendAudio<'local>(
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

/// `nSendSubtitle(handle, pts, payload)` — send one subtitle access unit.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendSubtitle<'local>(
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendData<'local>(
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendVideoTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, VideoStreamHandle::try_from_raw)
        else {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "invalid stream handle",
            );
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendKlvTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    klv: JByteArray<'local>,
    pts: jlong,
    metadata_service_id: jint,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, KlvStreamHandle::try_from_raw) else {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "invalid stream handle",
            );
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendAudioTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    frames: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, AudioStreamHandle::try_from_raw)
        else {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "invalid stream handle",
            );
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendSubtitleTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    pts: jlong,
    payload: JByteArray<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, SubtitleStreamHandle::try_from_raw)
        else {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "invalid stream handle",
            );
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSendDataTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    data: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, DataStreamHandle::try_from_raw)
        else {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "invalid stream handle",
            );
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

// ── Handle getters ─────────────────────────────────────────────────────────
//
// Return the first configured handle of each kind across all programs (which
// for the single-program ctor is also the only program). `-1` = none, which
// the Java side maps to `Optional.empty()`.

/// `nVideoHandle(handle)` — first configured video stream handle, or `-1`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nVideoHandle(
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nKlvHandle(
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nAudioHandle(
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nSubtitleHandle(
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nDataHandle(
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

/// Build an `org.tstrans.srt.TransportStats` record from a `SocketStats` +
/// `MuxerStats` pair.
pub(crate) fn build_transport_stats<'local>(
    env: &mut JNIEnv<'local>,
    socket_stats: &JObject<'local>,
    muxer_stats: &JObject<'local>,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    env.new_object(
        "org/tstrans/srt/TransportStats",
        "(Lorg/tstrans/srt/SocketStats;Lorg/tstrans/mpegts/MuxerStats;)V",
        &[JValue::Object(socket_stats), JValue::Object(muxer_stats)],
    )
}

/// `nStats(handle)` — return a `TransportStats` projecting the SRT socket
/// counters + the muxer's program/packet totals. Returns null on a JNI builder
/// error (non-fatal; mirrors the stats-builder convention in `transport.rs`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nStats<'local>(
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

// ── Cancellation ───────────────────────────────────────────────────────────

/// `nCancelHandle(handle)` — return a shareable cancel handle that wakes a
/// thread parked in any `nSend*`. Lock-free: the target was captured at
/// registration ([`register`]), so this returns promptly even while another
/// thread is parked in a send on the same handle. Throws
/// `IllegalStateException` on a closed handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY.cancel_view(handle as u64) {
            Some(view) => super::cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "MuxSender");
                0
            }
        }
    })
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

/// `nClose(handle)` — drop the boxed `MuxSender` (best-effort drain + close).
/// No-op on a zero handle so a double `close()` is safe.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nClose(
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
pub extern "system" fn Java_org_tstrans_srt_MuxSender_nIsAlive(
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

// ---------------------------------------------------------------------------
// Socket — nIntoMuxSender (CONSUMES the Box<Socket>)
// ---------------------------------------------------------------------------

/// `Socket.nIntoMuxSender(handle, ...programConfig...)` — consume a
/// `Box<Socket>` and produce a `Box<MuxSender<SrtTransport>>`. The Java caller
/// zeroes its own socket handle unconditionally after this returns (the socket
/// is consumed even on a config/new error → return 0 with the pending
/// exception).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nIntoMuxSender<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
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
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Take the Socket out of its registry (atomic; idempotent).
        let Some(socket) = super::lowlevel::REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };

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
            Err(()) => {
                // Socket already consumed (dropped at scope end) — pending exception.
                drop(socket);
                return 0;
            }
        };

        // Obtain-before-move: the socket's cancel handle is read before
        // `RustMuxSender::new` consumes the transport it is wrapped in.
        let cancel: Arc<dyn TransportCancel> = Arc::new(socket.cancel_handle());
        match RustMuxSender::new(SrtTransport::new(socket), cfg) {
            Ok(sender) => register(sender, cancel),
            Err(e) => {
                throw_mux_error(env, &e);
                0
            }
        }
    })
}
