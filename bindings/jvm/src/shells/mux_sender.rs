//! JNI exports for `org.tstrans.shells.MuxSender`: push elementary streams,
//! any transport out.
//!
//! Wraps `tst_pipeline::MuxSender<Box<dyn Transport>>`, opened from a URL and a
//! `MuxerConfig` by `nOpen`. The config comes from the parallel-array program
//! description (via the shared
//! [`crate::mpegts::muxer::build_muxer_config_from_arrays`] helper, byte-exact
//! with `Muxer::nOpen`); each push ends in a transport send.
//!
//! The handle is a `jlong` key into an [`OwnedRegistry`] with
//! `S = Snapshot<StreamHandles>`: the stream set and the transport's error
//! domain are fixed at construction, so the `n*Handle` getters never take the
//! slot a parked `send*` holds and a failed push throws through the right
//! domain even after a concurrent `close()` removed the key. Every
//! `MuxSender::send_*` takes `&self` and serialises internally. `nClose`
//! cancels BEFORE it takes the slot, so a `send*` parked on another thread
//! ends with the transport's `CLOSED` instead of holding `close()` hostage.
//!
//! Errors: a mux-sourced failure is a `MuxException`; every other source is
//! thrown in the shell's transport domain.

use crate::panic::jni_native;
use std::sync::LazyLock;

use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JBooleanArray, JByteArray, JIntArray, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::Transport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot, StreamHandles};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, HandleState, Opened, Owned};
use tst_pipeline::{MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource};

use super::cancel::cancel_view_handle;
use super::open::{open_send, read_url};
use super::{domain_or_closed, throw_state};
use crate::error::{throw_binding, throw_handle_state};
use crate::generated::stats_mpegts::build_muxer_stats;
use crate::generated::stats_shells::build_socket_stats;
use crate::handle::OwnedRegistry;
use crate::jutil::{checked_u8, decode_stream_handle, read_bytes};
use crate::mpegts::muxer::{build_muxer_config_from_arrays, throw_mux_error};
use crate::mpegts::shell_muxer_stats;

type Inner = RustMuxSender<Box<dyn Transport>>;

static REGISTRY: LazyLock<OwnedRegistry<Inner, Snapshot<StreamHandles>>> =
    LazyLock::new(OwnedRegistry::new);

/// Map a `MuxSenderError` (from any `send_*`) to a thrown Java exception.
/// `Mux(...)` keeps `MuxException`; every other source is a `BindingError`
/// thrown in the shell's `domain`.
fn throw_mux_sender_error(env: &mut Env, domain: Domain, e: MuxSenderError) {
    match e.source {
        MuxSenderErrorSource::Mux(m) => throw_mux_error(env, &m),
        _ => throw_binding(env, domain, &BindingError::from(e)),
    }
}

/// Throw for a raw stream handle outside `u32` / not issued by this binding:
/// `CONFIG_INVALID` in the shell's transport domain, as the per-transport
/// senders reported it. A closed Java handle is `IllegalStateException`.
fn throw_invalid_stream_handle(env: &mut Env, handle: jlong) {
    let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "MuxSender") else {
        return;
    };
    throw_binding(
        env,
        domain,
        &BindingError::new(BindingErrorKind::ConfigInvalid, "invalid stream handle"),
    );
}

/// Lease the sender and run a push op under the slot. A closed/absent handle
/// throws `IllegalStateException`; otherwise any `MuxSenderError` from the op
/// is thrown in the right domain. `send_*` take `&self`.
fn with_push(env: &mut Env, handle: jlong, op: impl FnOnce(&Inner) -> Result<(), MuxSenderError>) {
    let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "MuxSender") else {
        return;
    };
    match REGISTRY.with_mut(handle as u64, |inner| op(inner)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => throw_mux_sender_error(env, domain, e),
        Err(state) => throw_state(env, domain, "MuxSender", &state),
    }
}

/// Return the first handle-of-kind from the construction-time snapshot (`-1`
/// if none). A closed handle throws `IllegalStateException` and returns `-1`.
fn first_handle(
    env: &mut Env,
    handle: jlong,
    pick: impl FnOnce(&StreamHandles) -> Option<u32>,
) -> jlong {
    crate::handle::first_handle(env, &REGISTRY, handle, "MuxSender", |s| pick(&s.handles))
}

jni_native! {
    /// `MuxSender.nOpen(url, ...programConfig...)` — build the muxer config, open
    /// the transport named by the URL's scheme and return the shell's key.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_shells_MuxSender_nOpen<'local>(
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
    ) -> jlong = 0 => {
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
        let Some(url) = read_url(env, &url) else {
            return 0;
        };
        let Some(opened) = open_send(env, &url) else {
            return 0;
        };
        register(env, opened, cfg)
    }
}

/// Wrap an opened send transport in a `MuxSender` for `cfg` and register it;
/// `0` with a `MuxException` pending when the muxer rejects the configuration.
/// Shared by `nOpen` and by `srt.Socket.intoMuxSender`.
pub(crate) fn register(
    env: &mut Env,
    opened: Opened<Box<dyn Transport>>,
    cfg: tst_core::mpegts::mux::MuxerConfig,
) -> jlong {
    let Opened {
        transport,
        cancel,
        domain,
        ..
    } = opened;
    match RustMuxSender::new(transport, cfg) {
        Ok(sender) => {
            // Read before the sender moves into the slot: the handle
            // getters answer from this, never from the slot a parked send holds.
            let handles = StreamHandles::of(&sender);
            REGISTRY.insert(Owned::new(sender, cancel, Snapshot { domain, handles })) as jlong
        }
        Err(e) => {
            throw_mux_error(env, &e);
            0
        }
    }
}

// ── Send family — single-stream variants ───────────────────────────────────

jni_native! {
    /// `nSendVideo(handle, nal, pts, keyFrame)` — send one video access unit.
    fn Java_org_tstrans_shells_MuxSender_nSendVideo<'local>(
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
        with_push(env, handle, |inner| {
            inner.send_video(&buf, Pts90khz::new(pts), key_frame)
        });
    }
}

jni_native! {
    /// `nSendKlv(handle, klv, pts, metadataServiceId)` — send one KLV blob. The
    /// muxer auto-wraps the AU-cell header for synchronous-metadata streams; the
    /// caller passes raw KLV LS bytes.
    fn Java_org_tstrans_shells_MuxSender_nSendKlv<'local>(
        env,
        _class,
        handle: jlong,
        klv: JByteArray<'local>,
        pts: jlong,
        metadata_service_id: jint,
    ) = () => {
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
    }
}

jni_native! {
    /// `nSendAudio(handle, frames, pts)` — send one encoded audio frame.
    fn Java_org_tstrans_shells_MuxSender_nSendAudio<'local>(
        env,
        _class,
        handle: jlong,
        frames: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_audio(&buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendSubtitle(handle, pts, payload)` — send one subtitle access unit.
    fn Java_org_tstrans_shells_MuxSender_nSendSubtitle<'local>(
        env,
        _class,
        handle: jlong,
        pts: jlong,
        payload: JByteArray<'local>,
    ) = () => {
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_subtitle(&buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendData(handle, data, pts)` — pass-through send onto the lone configured
    /// data stream. No AU-cell wrap, no framing; one send = one PES on stream_id
    /// `0xBD` (`private_stream_1`). `pts` is written into the PES header only for
    /// `carries_pts` streams but always drives PSI/PCR pacing.
    fn Java_org_tstrans_shells_MuxSender_nSendData<'local>(
        env,
        _class,
        handle: jlong,
        data: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &data) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_data(&buf, Pts90khz::new(pts))
        });
    }
}

// ── Send family — handle-targeted variants ─────────────────────────────────

jni_native! {
    /// `nSendVideoTo(handle, streamHandleRaw, nal, pts, keyFrame)`.
    fn Java_org_tstrans_shells_MuxSender_nSendVideoTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        nal: JByteArray<'local>,
        pts: jlong,
        key_frame: jboolean,
    ) = () => {
        let Some(h) = decode_stream_handle(stream_handle_raw, VideoStreamHandle::try_from_raw)
        else {
            throw_invalid_stream_handle(env, handle);
            return;
        };
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_video_to(h, &buf, Pts90khz::new(pts), key_frame)
        });
    }
}

jni_native! {
    /// `nSendKlvTo(handle, streamHandleRaw, klv, pts, metadataServiceId)`.
    fn Java_org_tstrans_shells_MuxSender_nSendKlvTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        klv: JByteArray<'local>,
        pts: jlong,
        metadata_service_id: jint,
    ) = () => {
        let Some(h) = decode_stream_handle(stream_handle_raw, KlvStreamHandle::try_from_raw) else {
            throw_invalid_stream_handle(env, handle);
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
    }
}

jni_native! {
    /// `nSendAudioTo(handle, streamHandleRaw, frames, pts)`.
    fn Java_org_tstrans_shells_MuxSender_nSendAudioTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        frames: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(h) = decode_stream_handle(stream_handle_raw, AudioStreamHandle::try_from_raw)
        else {
            throw_invalid_stream_handle(env, handle);
            return;
        };
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_audio_to(h, &buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendSubtitleTo(handle, streamHandleRaw, pts, payload)`.
    fn Java_org_tstrans_shells_MuxSender_nSendSubtitleTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        pts: jlong,
        payload: JByteArray<'local>,
    ) = () => {
        let Some(h) = decode_stream_handle(stream_handle_raw, SubtitleStreamHandle::try_from_raw)
        else {
            throw_invalid_stream_handle(env, handle);
            return;
        };
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_subtitle_to(h, &buf, Pts90khz::new(pts))
        });
    }
}

jni_native! {
    /// `nSendDataTo(handle, streamHandleRaw, data, pts)`. The raw handle is
    /// validated via the strict `u32::try_from` + `DataStreamHandle::try_from_raw`
    /// chain (rejecting negative / out-of-u32 values up front rather than
    /// truncating, mirroring `Muxer::nPushDataTo`).
    fn Java_org_tstrans_shells_MuxSender_nSendDataTo<'local>(
        env,
        _class,
        handle: jlong,
        stream_handle_raw: jlong,
        data: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(h) = decode_stream_handle(stream_handle_raw, DataStreamHandle::try_from_raw)
        else {
            throw_invalid_stream_handle(env, handle);
            return;
        };
        let Some(buf) = read_bytes(env, &data) else {
            return;
        };
        with_push(env, handle, |inner| {
            inner.send_data_to(h, &buf, Pts90khz::new(pts))
        });
    }
}

// ── Handle getters ─────────────────────────────────────────────────────────
//
// Return the first configured handle of each kind across all programs (which
// for the single-program ctor is also the only program). `-1` = none, which
// the Java side maps to `Optional.empty()`.

jni_native! {
    /// `nVideoHandle(handle)` — first configured video stream handle, or `-1`.
    fn Java_org_tstrans_shells_MuxSender_nVideoHandle(env, _class, handle: jlong) -> jlong = 0 => {
        first_handle(env, handle, |s| s.video.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nKlvHandle(handle)` — first configured KLV stream handle, or `-1`.
    fn Java_org_tstrans_shells_MuxSender_nKlvHandle(env, _class, handle: jlong) -> jlong = 0 => {
        first_handle(env, handle, |s| s.klv.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nAudioHandle(handle)` — first configured audio stream handle, or `-1`.
    fn Java_org_tstrans_shells_MuxSender_nAudioHandle(env, _class, handle: jlong) -> jlong = 0 => {
        first_handle(env, handle, |s| s.audio.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nSubtitleHandle(handle)` — first configured subtitle stream handle, or `-1`.
    fn Java_org_tstrans_shells_MuxSender_nSubtitleHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        first_handle(env, handle, |s| s.subtitle.map(|h| h.raw()))
    }
}

jni_native! {
    /// `nDataHandle(handle)` — first configured data stream handle, or `-1`.
    fn Java_org_tstrans_shells_MuxSender_nDataHandle(env, _class, handle: jlong) -> jlong = 0 => {
        first_handle(env, handle, |s| s.data.map(|h| h.raw()))
    }
}

// ── Stats ──────────────────────────────────────────────────────────────────

/// Build an `org.tstrans.shells.TransportStats` record from a `SocketStats` +
/// `MuxerStats` pair.
pub(crate) fn build_transport_stats<'local>(
    env: &mut Env<'local>,
    socket_stats: &JObject<'local>,
    muxer_stats: &JObject<'local>,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    env.new_object(
        jni_str!("org/tstrans/shells/TransportStats"),
        jni_sig!("(Lorg/tstrans/shells/SocketStats;Lorg/tstrans/mpegts/MuxerStats;)V"),
        &[JValue::Object(socket_stats), JValue::Object(muxer_stats)],
    )
}

jni_native! {
    /// `nStats(handle)` — a `TransportStats` projecting the transport's socket
    /// counters + the muxer's program/packet totals. Null on a JNI builder error
    /// (non-fatal).
    fn Java_org_tstrans_shells_MuxSender_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let (sock, pipe) = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |inner| (inner.socket_stats().unwrap_or_default(), inner.stats()),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "MuxSender", &state);
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
        build_transport_stats(env, &sock_obj, &mux_obj).unwrap_or_else(|_| JObject::null())
    }
}

// ── Cancellation ───────────────────────────────────────────────────────────

jni_native! {
    /// `nCancelHandle(handle)` — a shareable cancel handle that wakes a thread
    /// parked in any `nSend*`. Lock-free: answers while a send is parked.
    fn Java_org_tstrans_shells_MuxSender_nCancelHandle(env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY.cancel_view(handle as u64) {
            Some(view) => cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "MuxSender");
                0
            }
        }
    }
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

jni_native! {
    /// `nClose(handle)` — cancel first, then take the sender and close it
    /// (best-effort drain). Atomic + idempotent: a double `close()` is safe.
    fn Java_org_tstrans_shells_MuxSender_nClose(_env, _class, handle: jlong) = () => {
        if let Some(inner) = REGISTRY.close(handle as u64) {
            inner.close();
        }
    }
}

jni_native! {
    /// `nFinish(handle)` — drain the muxer's pending bytes to the live transport,
    /// report the first drain error, then close the transport
    /// (`MuxSender::finish`). Unlike `nClose` this does NOT cancel first and does
    /// NOT free the registry entry: the Java side still calls `close()`. A
    /// closed handle is quiet, the same answer a second `finish()` gets from the
    /// shell itself.
    fn Java_org_tstrans_shells_MuxSender_nFinish(env, _class, handle: jlong) = () => {
        // Closed between the Java-side `peekHandle()` and here: stay quiet.
        let Some(domain) = REGISTRY.snapshot(handle as u64, |s| s.domain) else {
            return;
        };
        // lossless shutdown: drains the muxer through the transport, documented to wait behind a send in flight
        match REGISTRY.with_ref_blocking(handle as u64, BlockingReason::IsItselfBlocking, |inner| {
            inner.finish()
        }) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_mux_sender_error(env, domain, e),
            Err(HandleState::Closed) => {}
            Err(state) => throw_state(env, domain, "MuxSender", &state),
        }
    }
}

jni_native! {
    /// `nIsAlive(handle)` — whether the sender owns a live transport. Non-blocking:
    /// a `send*` parked on another thread holds the slot, and reads as alive.
    fn Java_org_tstrans_shells_MuxSender_nIsAlive(_env, _class, handle: jlong) -> jboolean = false => {
        REGISTRY.is_alive(handle as u64, |inner| inner.is_alive())
    }
}
