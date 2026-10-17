//! JNI surface for `org.tstrans.srt.ManagedMuxSender` and
//! `org.tstrans.srt.ManagedDemuxReceiver` — the two convenience auto-reconnect
//! SRT wrappers.
//!
//! Ports tst-py's `bindings/python/src/srt/managed_convenience.rs`. The sender
//! wraps `MuxSender<ManagedTransport<SrtTransport>>`; the receiver wraps
//! `ManagedDemuxReceiver<SrtTransport>` (which owns a `ManagedRecvTransport`).
//! The URL is opened — initially and on every reconnect — by
//! `tst_srt::shells::managed_{mux_sender,demux_receiver}_from_url`, the one
//! open path all three bindings share.
//!
//! ## Reconnect-attempt counter
//!
//! `nReconnectAttempts` reads `ManagedHandles.attempts` (factory invocations) —
//! the same counter on all four managed shells since 0.7.0, composed once in
//! `tst_srt::shells` instead of by a per-binding counting closure.
//! It is an ATTEMPT counter, not `reconnects` (the SUCCESS counter): a factory
//! call parked in a re-accept has incremented `attempts` and not `reconnects`.
//!
//! ## Mode
//!
//! `ManagedMuxSender` REQUIRES `?mode=caller` (CONFIG_INVALID otherwise).
//! `ManagedDemuxReceiver` accepts BOTH `?mode=listener` (default) AND
//! `?mode=caller`; `tst_srt::shells::managed_demux_receiver_from_url` dispatches
//! on the mode for the initial open and every reconnect alike.
//!
//! ## Threading model
//!
//! Single-threaded boxes (NO inner `Arc<Mutex>`) — the JVM follows the shipped
//! single-thread `MuxSender`/`DemuxReceiver` model. `MuxSender` push methods
//! reconstitute as a SHARED `&*ptr` (every `send_*` takes `&self` + internal
//! mutex; concurrent pushes are sound). `ManagedDemuxReceiver::nNext` uses
//! `&mut *ptr` (`recv_event` is `&mut self`). The receiver has NO byte sink.
//!
//! ## Stats drift
//!
//! `ManagedMuxSender` exposes a combined `TransportStats stats()` +
//! `reconnectAttempts()` — NO `srtStats()`. `ManagedDemuxReceiver` exposes
//! `SocketStats socketStats()` AND `SocketStats srtStats()` (the latter returns
//! the SAME value as `socketStats`, return type `SocketStats`, no throw) +
//! `reconnectAttempts()` — NO combined `stats()`.

use crate::panic::jni_native;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
use tst_pipeline::binding::view::BlockingReason;

use jni::JNIEnv;
use jni::objects::{JBooleanArray, JByteArray, JIntArray, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::EndReasonHandle;
use tst_pipeline::binding::{BindingErrorKind, HandleState, ManagedHandles, Owned};
use tst_pipeline::{
    DemuxReceiverError, DemuxReceiverErrorSource, ManagedDemuxReceiver as RustManagedDemuxReceiver,
    ManagedTransport, MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource,
};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use crate::error::throw_handle_state;
use crate::generated::stats_mpegts::build_muxer_stats;
use crate::generated::stats_shells::build_socket_stats;
use crate::handle::OwnedRegistry;
use crate::jutil::{checked_u8, read_bytes};
use crate::mpegts::muxer::{build_muxer_config_from_arrays, throw_mux_error};
use crate::mpegts::{build_demux_config_from_args, convert_event, shell_muxer_stats};
use tst_pipeline::binding::view::StreamHandles;

use super::ManagedSenderSnapshot;
use super::errors::{srt_error, throw_srt, transport_error};
use crate::generated::stats_srt::build_managed_transport_stats;
use crate::shells::mux_sender::build_transport_stats;

/// Map a `DemuxReceiverError` raised by `recv_event` onto a thrown Java
/// exception: transport-side errors become `SrtException` (via
/// `transport_error`), demux-side errors `DemuxException`. No end-of-stream
/// arm: `recv_event` swallows a clean end of stream, returning `Ok(None)`.
fn throw_demux_recv_error(env: &mut JNIEnv, e: &DemuxReceiverError) {
    match &e.source {
        DemuxReceiverErrorSource::Transport(t) => transport_error(env, t),
        DemuxReceiverErrorSource::Demux(d) => crate::mpegts::throw_demux_error(env, d),
        // `DemuxReceiverErrorSource` is non-exhaustive; route any future variant
        // through `SrtException(IO)` with the Display message preserved.
        _ => throw_srt(env, BindingErrorKind::SrtIo, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// JniManagedMuxSender — wraps MuxSender<ManagedTransport<SrtTransport>>.
// ---------------------------------------------------------------------------

/// Native backing for `org.tstrans.srt.ManagedMuxSender`: just the shell. The
/// attempt counters, the cancel target and the reconnect/gap telemetry observer
/// live in the entry's `Owned` snapshot ([`ManagedSenderSnapshot`]), outside the
/// slot, so every getter answers while a `send*` is parked in a reconnect.
struct JniManagedMuxSender {
    inner: RustMuxSender<ManagedTransport<SrtTransport>>,
}

/// Per-type `Owned`-backed registry for `org.tstrans.srt.ManagedMuxSender`.
/// `OwnedRegistry::close` cancels first, so a `send*` parked in the Blocking
/// reconnect (a backoff wait or a re-dial) ends with `SrtException(CLOSED)`
/// promptly — the contract the C ABI's `tst_managed_mux_sender_close` and
/// `ManagedDemuxReceiver` already have.
static REGISTRY_MUX: LazyLock<OwnedRegistry<JniManagedMuxSender, ManagedMuxSenderSnapshot>> =
    LazyLock::new(OwnedRegistry::new);

/// Construction-constant view of a `ManagedMuxSender`: the managed-sender
/// handles plus the stream handles, which are fixed by the muxer config.
struct ManagedMuxSenderSnapshot {
    managed: ManagedSenderSnapshot,
    streams: StreamHandles,
}

/// Map a `MuxSenderError` (from any `send_*`) to a thrown Java exception.
/// `Mux(...)` → `MuxException`; `Transport(...)` → `SrtException` per
/// `TransportError` variant. Mirrors tst-py's `mux_sender_error_to_pyerr`.
fn throw_managed_mux_sender_error(env: &mut JNIEnv, e: &MuxSenderError) {
    match &e.source {
        MuxSenderErrorSource::Mux(m) => throw_mux_error(env, m),
        MuxSenderErrorSource::Transport(t) => transport_error(env, t),
        // `MuxSenderErrorSource` may gain variants; route any future one to a
        // generic SrtException(IO) with the Display message preserved.
        _ => throw_srt(env, BindingErrorKind::SrtIo, &e.to_string()),
    }
}

/// Lease the managed sender and run a push op under the resource lock. A closed
/// handle throws `IllegalStateException`; any `MuxSenderError` is mapped.
fn with_mux_push(
    env: &mut JNIEnv,
    handle: jlong,
    op: impl FnOnce(&RustMuxSender<ManagedTransport<SrtTransport>>) -> Result<(), MuxSenderError>,
) {
    match REGISTRY_MUX.with_mut(handle as u64, |jstruct| op(&jstruct.inner)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => throw_managed_mux_sender_error(env, &e),
        Err(state) => throw_handle_state(env, "ManagedMuxSender", &state),
    }
}

/// Return the first handle-of-kind from the construction-time snapshot (`-1`
/// if none). A closed handle throws `IllegalStateException` and returns `-1`.
fn mux_first_handle(
    env: &mut JNIEnv,
    handle: jlong,
    pick: impl FnOnce(&StreamHandles) -> Option<u32>,
) -> jlong {
    crate::handle::first_handle(env, &REGISTRY_MUX, handle, "ManagedMuxSender", |s| {
        pick(&s.streams)
    })
}

jni_native! {
    /// `ManagedMuxSender.nFromUrl(url, ...programConfig..., ...policyArgs...)` —
    /// build the muxer config, parse the caller-mode URL, do the initial connect,
    /// wrap it in a `ManagedTransport`, and hand it to `MuxSender`. Returns the
    /// boxed handle as `jlong`, or `0` with a pending exception on any failure.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_srt_ManagedMuxSender_nFromUrl<'local>(
        env,
        _class,
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
    ) -> jlong = 0 => {
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
        // factory, the attempt counter and the cancel/stats handles are composed
        // in tst-srt, once. A bad MuxerConfig stays a MuxException, as before.
        let (inner, handles, stats) =
            match tst_srt::shells::managed_mux_sender_from_url(&parsed, policy, muxer_cfg) {
                Ok(triple) => triple,
                Err(tst_srt::SrtError::Mux(m)) => {
                    throw_mux_error(env, &m);
                    return 0;
                }
                Err(e) => {
                    // Initial connect failure → CONNECT_FAILED, through the
                    // shared `From<SrtError>` mapping.
                    srt_error(env, e);
                    return 0;
                }
            };
        let cancel = Arc::clone(&handles.cancel);
        let streams = StreamHandles::of(&inner);
        REGISTRY_MUX.insert(Owned::new(
            JniManagedMuxSender { inner },
            cancel,
            ManagedMuxSenderSnapshot {
                managed: ManagedSenderSnapshot { handles, stats },
                streams,
            },
        )) as jlong
    }
}

// ── Send family — single-stream variants ───────────────────────────────────

jni_native! {
    /// `nSendVideo(handle, nal, pts, keyFrame)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendVideo<'local>(
        env,
        _class,
        handle: jlong,
        nal: JByteArray<'local>,
        pts: jlong,
        key_frame: jboolean,
    ) = () => {
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_video(&buf, Pts90khz::new(pts), key_frame != 0)
        });
    }
}

jni_native! {
    /// `nSendKlv(handle, klv, pts, metadataServiceId)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendKlv<'local>(
        env,
        _class,
        handle: jlong,
        klv: JByteArray<'local>,
        pts: jlong,
        metadata_service_id: jint,
    ) = () => {
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
    }
}

jni_native! {
    /// `nSendAudio(handle, frames, pts)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendAudio<'local>(
        env,
        _class,
        handle: jlong,
        frames: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_audio(&buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendSubtitle(handle, pts, payload)` — note the swapped arg order.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendSubtitle<'local>(
        env,
        _class,
        handle: jlong,
        pts: jlong,
        payload: JByteArray<'local>,
    ) = () => {
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_subtitle(&buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendData(handle, data, pts)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendData<'local>(
        env,
        _class,
        handle: jlong,
        data: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &data) else {
            return;
        };
        with_mux_push(env, handle, |inner| {
            inner.send_data(&buf, Pts90khz::new(pts))
        });
    }
}

// ── Send family — handle-targeted variants ─────────────────────────────────

jni_native! {
    /// `nSendVideoTo(handle, streamHandleRaw, nal, pts, keyFrame)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendVideoTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        nal: JByteArray<'local>,
        pts: jlong,
        key_frame: jboolean,
    ) = () => {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| VideoStreamHandle::try_from_raw(r).ok())
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
        with_mux_push(env, handle, |inner| {
            inner.send_video_to(h, &buf, Pts90khz::new(pts), key_frame != 0)
        });
    }
}

jni_native! {
    /// `nSendKlvTo(handle, streamHandleRaw, klv, pts, metadataServiceId)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendKlvTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        klv: JByteArray<'local>,
        pts: jlong,
        metadata_service_id: jint,
    ) = () => {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| KlvStreamHandle::try_from_raw(r).ok())
        else {
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
        with_mux_push(env, handle, |inner| {
            inner.send_klv_to(h, &buf, Pts90khz::new(pts), service_id)
        });
    }
}

jni_native! {
    /// `nSendAudioTo(handle, streamHandleRaw, frames, pts)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendAudioTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        frames: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| AudioStreamHandle::try_from_raw(r).ok())
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
        with_mux_push(env, handle, |inner| {
            inner.send_audio_to(h, &buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendSubtitleTo(handle, streamHandleRaw, pts, payload)`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendSubtitleTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        pts: jlong,
        payload: JByteArray<'local>,
    ) = () => {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| SubtitleStreamHandle::try_from_raw(r).ok())
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
        with_mux_push(env, handle, |inner| {
            inner.send_subtitle_to(h, &buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendDataTo(handle, streamHandleRaw, data, pts)`. The raw handle is
    /// validated via the strict `u32::try_from` + `DataStreamHandle::try_from_raw`
    /// chain (rejecting negative / out-of-u32 values up front rather than
    /// truncating, mirroring `MuxSender::nSendDataTo`).
    fn Java_org_tstrans_srt_ManagedMuxSender_nSendDataTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        data: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(h) = u32::try_from(stream_handle_raw)
            .ok()
            .and_then(|r| DataStreamHandle::try_from_raw(r).ok())
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
        with_mux_push(env, handle, |inner| {
            inner.send_data_to(h, &buf, Pts90khz::new(pts))
        });
    }
}

// ── Handle getters ─────────────────────────────────────────────────────────

jni_native! {
    /// `nVideoHandle(handle)` — first configured video stream handle, or `-1`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nVideoHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        mux_first_handle(env, handle, |s| s.video.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nKlvHandle(handle)` — first configured KLV stream handle, or `-1`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nKlvHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        mux_first_handle(env, handle, |s| s.klv.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nAudioHandle(handle)` — first configured audio stream handle, or `-1`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nAudioHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        mux_first_handle(env, handle, |s| s.audio.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nSubtitleHandle(handle)` — first configured subtitle stream handle, or `-1`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nSubtitleHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        mux_first_handle(env, handle, |s| s.subtitle.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nDataHandle(handle)` — first configured data stream handle, or `-1`.
    fn Java_org_tstrans_srt_ManagedMuxSender_nDataHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        mux_first_handle(env, handle, |s| s.data.map(|h| h.raw()))
    }
}

// ── Stats + lifecycle ──────────────────────────────────────────────────────

jni_native! {
    /// `nStats(handle)` — `TransportStats` projecting the SRT socket counters + the
    /// muxer's program/packet totals. Identical to `mux_sender.rs::nStats`. Returns
    /// null on a JNI builder error (non-fatal).
    fn Java_org_tstrans_srt_ManagedMuxSender_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let (sock, pipe) = match REGISTRY_MUX.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jstruct| {
                (
                    jstruct.inner.socket_stats().unwrap_or_default(),
                    jstruct.inner.stats(),
                )
            },
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "ManagedMuxSender", &state);
                return JObject::null();
            }
        };

        let sock_obj = match build_socket_stats(env, &sock) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        let mux_obj = match build_muxer_stats(
            env,
            &shell_muxer_stats(
                pipe.packets_sent,
                pipe.bytes_sent,
                u64::from(pipe.programs_configured),
            ),
        ) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        match build_transport_stats(env, &sock_obj, &mux_obj) {
            Ok(o) => o,
            Err(_) => JObject::null(),
        }
    }
}

jni_native! {
    /// `nReconnectAttempts(handle)` — total factory invocations since construction.
    fn Java_org_tstrans_srt_ManagedMuxSender_nReconnectAttempts(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        // The send side counts attempts in the reconnect stats (the recv side
        // keeps its own `ManagedHandles.attempts`); both live in the
        // snapshot, so this is lock-free. A poisoned gap lock reads as 0 here —
        // `nReconnectStats` is where that condition throws `IO`.
        REGISTRY_MUX
            .snapshot(handle as u64, |s| {
                s.managed
                    .stats
                    .stats()
                    .map_or(0, |st| st.reconnect_attempts as jlong)
            })
            .unwrap_or_else(|| {
                crate::error::throw_closed(env, "ManagedMuxSender");
                0
            })
    }
}

jni_native! {
    /// `nReconnectStats(handle)` — reconnect/gap telemetry: attempts, successes,
    /// current gap-buffer depth, and drop counters. Throws `SrtException(IO)` if the
    /// internal gap-buffer lock is poisoned — a read-only telemetry path must not
    /// panic.
    fn Java_org_tstrans_srt_ManagedMuxSender_nReconnectStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // Lock-free: the stats handle lives in the snapshot, not the slot.
        let Some(maybe_stats) = REGISTRY_MUX.snapshot(handle as u64, |s| s.managed.stats.stats())
        else {
            crate::error::throw_closed(env, "ManagedMuxSender");
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
    /// `nCancelHandle(handle)` — return a shareable cancel handle that wakes a
    /// thread parked in any `nSend*`: a live send, the Blocking reconnect's
    /// backoff wait, or a re-dial. Lock-free: the target was captured at open, so
    /// this returns promptly even while another thread is parked in a send on the
    /// same handle and regardless of reconnect state (the `ManagedCancel` follows
    /// reconnects). Throws `IllegalStateException` on a closed handle.
    fn Java_org_tstrans_srt_ManagedMuxSender_nCancelHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        match REGISTRY_MUX.cancel_view(handle as u64) {
            Some(view) => crate::shells::cancel::cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "ManagedMuxSender");
                0
            }
        }
    }
}

jni_native! {
    /// `nClose(handle)` — drop the boxed sender (best-effort drain + close). No-op on
    /// a zero handle so a double `close()` is safe.
    fn Java_org_tstrans_srt_ManagedMuxSender_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        if let Some(jstruct) = REGISTRY_MUX.close(handle as u64) {
            jstruct.inner.close();
        }
    }
}

jni_native! {
    /// `nFinish(handle)` — drain the muxer's pending bytes to the live transport,
    /// report the first drain error, then close the transport
    /// (`MuxSender::finish`). Unlike `nClose` this does NOT cancel first and does
    /// NOT free the registry entry: the Java side still calls `close()`.
    /// A send parked in the reconnect backoff keeps the slot: `nFinish`
    /// waits behind it (unlike `nClose`, which cancels first).
    /// A zero / already-closed handle is quiet — the same answer a second
    /// `finish()` gets from the shell itself.
    fn Java_org_tstrans_srt_ManagedMuxSender_nFinish(env, _class, handle: jlong) = () => {
        // lossless shutdown: drains the muxer through the transport, documented to wait behind a send in flight
        match REGISTRY_MUX.with_ref_blocking(
            handle as u64,
            BlockingReason::IsItselfBlocking,
            |jstruct| jstruct.inner.finish(),
        ) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_managed_mux_sender_error(env, &e),
            // Closed between the Java-side `peekHandle()` and here, or closed
            // outright: stay quiet, exactly like a second `finish()`.
            Err(HandleState::Closed) => {}
            Err(state) => throw_handle_state(env, "ManagedMuxSender", &state),
        }
    }
}

jni_native! {
    /// `nIsAlive(handle)` — whether the sender owns a live transport. Non-blocking:
    /// a `send*` parked on another thread holds the slot, and reads as alive.
    fn Java_org_tstrans_srt_ManagedMuxSender_nIsAlive(
        _env,
        _class,
        handle: jlong,
    ) -> jboolean = 0 => {
        u8::from(REGISTRY_MUX.is_alive(handle as u64, |jstruct| jstruct.inner.is_alive()))
    }
}

// ---------------------------------------------------------------------------
// JniManagedDemuxReceiver — wraps ManagedDemuxReceiver<SrtTransport>.
// ---------------------------------------------------------------------------

/// Native backing for `org.tstrans.srt.ManagedDemuxReceiver`. Single-threaded
/// box (NO inner mutex, NO byte sink). The attempt counters and
/// the cancel target live in the entry's `Owned` snapshot ([`ManagedHandles`]).
struct JniManagedDemuxReceiver {
    inner: RustManagedDemuxReceiver<SrtTransport>,
    /// Clone of the receiver's `EndReasonHandle`, captured at construction
    /// (obtain-before-move — the same shape as the cancel target). The *live*
    /// reads go through the registry's lock-free slot, which holds another clone
    /// of this same cell; this copy is what `nClose` snapshots from, since by
    /// then the registry entry is gone. See `super::recv_end_reason`.
    end_reason: EndReasonHandle,
}

/// Per-type `Owned`-backed registry for `org.tstrans.srt.ManagedDemuxReceiver`.
/// `OwnedRegistry::close` cancels first, and the public cancel handle reads the
/// same `Owned` entry lock-free.
static REGISTRY_DEMUX: LazyLock<OwnedRegistry<JniManagedDemuxReceiver, ManagedHandles>> =
    LazyLock::new(OwnedRegistry::new);

/// Shared construction body for `nFromUrl` / `nFromUrlWithConfig`: parse the URL
/// (accepting BOTH listener and caller mode), do the initial
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

    // Both modes are the shared `from_url` dispatch: listener ⇒
    // `accept_one` through the FactoryCancel slot (a re-accept parked with no
    // peer in sight is reachable by cancel), caller ⇒ `connect`.
    let (inner, handles) = match tst_srt::shells::managed_demux_receiver_from_url(
        &parsed,
        policy,
        opts.unwrap_or_default(),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            srt_error(env, e);
            return 0;
        }
    };
    // Cancel-on-close: `OwnedRegistry::close` fires the cancel before taking the
    // slot, so a `next()` parked on another thread ends with CLOSED (recording
    // CANCELLED) instead of holding `close()` hostage — the contract tst-py and
    // the C ABI's `tst_managed_demux_receiver_close` already have.
    let cancel = Arc::clone(&handles.cancel);
    let end_reason = handles.end_reason.clone();
    REGISTRY_DEMUX.insert(
        Owned::new(
            JniManagedDemuxReceiver {
                inner,
                end_reason: end_reason.clone(),
            },
            cancel,
            handles,
        )
        .with_end_reason(end_reason),
    ) as jlong
}

jni_native! {
    /// `ManagedDemuxReceiver.nFromUrl(url, ...policyArgs...)` — default demux options.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nFromUrl<'local>(
        env,
        _class,
        url: JString<'local>,
        max_attempts_present: jboolean,
        max_attempts: jint,
        backoff_kind: jint,
        backoff_base_ms: jlong,
        backoff_max_ms: jlong,
        gap_buffer_capacity: jint,
        overflow_policy: jint,
        mode: jint,
    ) -> jlong = 0 => {
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
    }
}

jni_native! {
    /// `ManagedDemuxReceiver.nFromUrlWithConfig(url, ...policyArgs..., ...demuxArgs...)`.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nFromUrlWithConfig<'local>(
        env,
        _class,
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
        unwrap_timestamps: jboolean,
    ) -> jlong = 0 => {
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
            unwrap_timestamps,
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
    }
}

jni_native! {
    /// `nNext(handle)` — block until the next `DemuxEvent`, returning it as a Java
    /// object; Java `null` on clean EOF. Throws `SrtException` / `DemuxException` on
    /// a recv-side error. Emits `DemuxEvent.ReconnectDiscontinuity` once after each
    /// transport reconnect. No byte sink, so no captured-exception
    /// drain.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nNext<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        // recv_event runs INSIDE the registry lease (under the resource lock,
        // single-threaded per the Receiver model).
        let res = match REGISTRY_DEMUX.with_mut(handle as u64, |jstruct| jstruct.inner.recv_event())
        {
            Ok(res) => res,
            Err(state) => {
                throw_handle_state(env, "ManagedDemuxReceiver", &state);
                return JObject::null().into_raw();
            }
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
                    // clobbering a pending exception; it throws RuntimeException:
                    // demux has no INTERNAL kind.
                    crate::error::throw_demux(
                        env,
                        BindingErrorKind::Internal,
                        "event conversion failed",
                    );
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                throw_demux_recv_error(env, &e);
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `nCancelHandle(handle)` — return a shareable cancel handle that wakes a thread
    /// parked in `nNext`. Lock-free: the target was captured at open, so this returns
    /// promptly even while another thread is parked in `nNext` (a blocked receive or
    /// a listener-mode re-accept) and regardless of reconnect state. Throws
    /// `IllegalStateException` on a closed handle.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nCancelHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        match REGISTRY_DEMUX.cancel_view(handle as u64) {
            Some(view) => crate::shells::cancel::cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "ManagedDemuxReceiver");
                0
            }
        }
    }
}

jni_native! {
    /// `nSocketStats(handle)` — scheme-neutral 16-field wire stats. Uses
    /// `unwrap_or_default` so a mid-reconnect receiver yields a zeroed snapshot.
    /// Returns null on a JNI builder error (non-fatal).
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY_DEMUX.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jstruct| jstruct.inner.socket_stats().unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "ManagedDemuxReceiver", &state);
                return JObject::null();
            }
        };
        match build_socket_stats(env, &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    }
}

jni_native! {
    /// `nSrtStats(handle)` — returns the SAME
    /// `SocketStats` view as `nSocketStats` and does NOT throw. Mirrors tst-py's
    /// `PyManagedDemuxReceiver::srt_stats`, which delegates to `socket_stats`.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nSrtStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY_DEMUX.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jstruct| jstruct.inner.socket_stats().unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "ManagedDemuxReceiver", &state);
                return JObject::null();
            }
        };
        match build_socket_stats(env, &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    }
}

jni_native! {
    /// `nLastSeenMicros(handle, pid)` — Unix-epoch microsecond timestamp the
    /// stream identified by `pid` last carried a demuxed item through this
    /// receiver (last emitted event); `-1` if `pid` was never seen — including
    /// an unrecognized PID (no range check beyond the native `u16` truncating
    /// cast, same as `pmtPid`/`pcrPid` elsewhere in this binding) — or a
    /// timestamp predating the Unix epoch. Boxed to `Long` (`null` for `-1`) at
    /// the Java layer. Same registry-lock discipline as `nSocketStats`.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nLastSeenMicros(
        env,
        _class,
        handle: jlong,
        pid: jint,
    ) -> jlong = -1 => {
        // per-PID timestamp lives in the demuxer stats inside the shell; blocking caveat documented with the watchdog recipe
        let last_seen = match REGISTRY_DEMUX.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jstruct| {
                jstruct
                    .inner
                    .stats()
                    .per_stream
                    .get(&(pid as u16))
                    .and_then(|s| s.last_seen)
            },
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "ManagedDemuxReceiver", &state);
                return -1;
            }
        };
        last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as i64)
            .unwrap_or(-1)
    }
}

jni_native! {
    /// `nReconnectAttempts(handle)` — total factory invocations since construction
    /// (`ManagedHandles.attempts`). Lock-free: read off the `Owned` snapshot, so it
    /// answers while `nNext` is parked in a re-accept that has not completed.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nReconnectAttempts(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        REGISTRY_DEMUX
            .snapshot(handle as u64, |h| {
                h.attempts.load(Ordering::Acquire) as jlong
            })
            .unwrap_or_else(|| {
                crate::error::throw_closed(env, "ManagedDemuxReceiver");
                0
            })
    }
}

jni_native! {
    /// `nEndReason(handle)` — why the receive stream ended, as the wire ordinal (see
    /// `super::recv_end_reason`); `-1` if it has not ended yet, or on a
    /// closed/absent handle (the closed case never reaches this native —
    /// `ManagedDemuxReceiver.endReason()` reads the Java-side snapshot once
    /// `peekHandle()` is 0).
    ///
    /// Reads the registry's lock-free end-reason slot, NOT `REGISTRY_DEMUX.with` —
    /// `nNext` holds the resource lease for the whole duration of a native receive,
    /// and during a listener-mode re-accept that receive can park indefinitely. Same
    /// reasoning, and the same slot mechanism, as `nCancelHandle` above.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nEndReason(
        _env,
        _class,
        handle: jlong,
    ) -> jint = -1 => {
        crate::shells::end_reason::wire_value(REGISTRY_DEMUX.end_reason(handle as u64).as_ref())
    }
}

jni_native! {
    /// `nClose(handle)` — close the underlying transport and drop the box. No-op on a
    /// zero handle so a double `close()` is safe.
    ///
    /// Returns the close-time end-reason ordinal, computed here from the shell this
    /// call already exclusively owns: `NativeHandle.close()` zeroes the Java handle
    /// before `nativeClose` runs, and the registry entry (with its end-reason slot)
    /// is permanently removed by `REGISTRY_DEMUX.close`, so there is no handle left
    /// for a follow-up `nEndReason`. `-1` when nothing was recorded, including the
    /// double-close no-op — which `NativeHandle.close()`'s `getAndSet(0)` already
    /// makes unreachable from Java: `nativeClose` runs at most once per receiver.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nClose(
        _env,
        _class,
        handle: jlong,
    ) -> jint = -1 => {
        // Atomic + idempotent: the winning close gets the shell back for teardown.
        // Read the reason AFTER `inner.close()` so a reason recorded by the
        // teardown itself is included.
        let reason = if let Some(mut jstruct) = REGISTRY_DEMUX.close(handle as u64) {
            jstruct.inner.close();
            jstruct.end_reason.get()
        } else {
            None
        };
        crate::shells::end_reason::wire_value(reason.as_ref())
    }
}

jni_native! {
    /// `nIsAlive(handle)` — whether the receiver owns a live transport.
    /// Non-blocking: a `next()` parked on another thread holds the slot, and reads
    /// as alive.
    fn Java_org_tstrans_srt_ManagedDemuxReceiver_nIsAlive(
        _env,
        _class,
        handle: jlong,
    ) -> jboolean = 0 => {
        u8::from(REGISTRY_DEMUX.is_alive(handle as u64, |jstruct| jstruct.inner.is_alive()))
    }
}
