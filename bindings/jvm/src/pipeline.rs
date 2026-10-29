//! JNI bindings for `tst_pipeline::ext::pairing::PairingDemuxer` —
//! the byte-feeding `org.tstrans.pipeline.Pairer`.
//!
//! Wraps the core `PairingDemuxer` (a `Demuxer` + `Pairer` composite):
//! feed raw TS bytes, collect `PairerOutput`s. The owned demuxer means
//! `DemuxEvent`s never round-trip across the boundary — only the
//! projected `VideoSample` / `KlvSample` shapes (and a pass-through
//! `DemuxEvent` for off-PID events) cross. Mirrors
//! `bindings/python/src/pipeline.rs` decision-for-decision, reusing the
//! `crate::mpegts` projection helpers so a `VideoSample` / `KlvSample`
//! is byte-identical to the corresponding `mpegts.Demuxer` projection.
//!
//! Handle convention matches `mod mpegts`: the `jlong` is an opaque key into a
//! per-type [`crate::handle::HandleRegistry`] over the [`PairingDemuxer`];
//! `nOpen`/`nOpenWithConfig` register via `REGISTRY.insert`; per-call fns lease
//! via `REGISTRY.with` (mapping a closed/absent handle to a thrown
//! `IllegalStateException`); `nClose` takes + drops via `REGISTRY.close` (atomic
//! + idempotent, so a double `close()` is UAF/double-free-safe).

use crate::jutil::FindClass;
use crate::panic::jni_native;
use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jboolean, jint, jlong, jobject};
use std::sync::LazyLock;
use std::time::Duration;

use tst_core::mpegts::demux::DemuxerConfig;
use tst_pipeline::ext::pairing::{
    KlvSample, PairerConfig, PairerMode, PairerOutput, PairingDemuxer, PairingDemuxerConfig,
    VideoSample,
};

use crate::error::throw_demux;
use crate::handle::HandleRegistry;
use crate::jutil::{enum_const, wrap_heap_byte_buffer};
use tst_pipeline::binding::BindingErrorKind;

use crate::mpegts::{
    build_demux_config_from_args, build_stream_id, build_video_units, convert_event, metadata_kind,
    opt_long, throw_demux_error, video_codec_name,
};

/// Per-type leased-handle registry for `org.tstrans.pipeline.Pairer`.
static REGISTRY: LazyLock<HandleRegistry<PairingDemuxer>> = LazyLock::new(HandleRegistry::new);

jni_native! {
    /// `nOpen(videoPid, klvPid)` — allocate a default-config [`PairingDemuxer`]
    /// and hand the JVM an opaque registry key as a `jlong` handle.
    fn Java_org_tstrans_pipeline_Pairer_nOpen<'local>(
        _env,
        _class,
        video_pid: jint,
        klv_pid: jint,
    ) -> jlong = 0 => {
        REGISTRY.insert(PairingDemuxer::new(video_pid as u16, klv_pid as u16)) as jlong
    }
}

jni_native! {
    /// `nOpenWithConfig(...)` — build a configured [`PairingDemuxer`]. The
    /// `strict`/`av1` ints are the Java enum ORDINALS (same contract as the
    /// `mpegts.Demuxer.nOpenWithConfig` path — see
    /// [`build_demux_config_from_args`]). When `has_demuxer_config` is false
    /// the demuxer half keeps its `DemuxerConfig::default()`.
    ///
    /// `PairingDemuxerConfig` / `PairerConfig` are non-exhaustive in the
    /// external `tst-pipeline` crate, so they cannot be struct-literal'd here
    /// — they are assembled via `Default::default()` + field assignment,
    /// mirroring tst-py's `build_pairing_demuxer`.
    fn Java_org_tstrans_pipeline_Pairer_nOpenWithConfig<'local>(
        env,
        _class,
        video_pid: jint,
        klv_pid: jint,
        buffered: jboolean,
        max_lag_nanos: jlong,
        tolerance_nanos: jlong,
        max_buffered_klv: jlong,
        max_buffered_video: jlong,
        has_demuxer_config: jboolean,
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
        let mode = if buffered {
            PairerMode::Buffered {
                max_lag: Duration::from_nanos(max_lag_nanos as u64),
            }
        } else {
            PairerMode::Realtime
        };

        let mut pairer = PairerConfig::default();
        pairer.mode = mode;
        pairer.tolerance = Duration::from_nanos(tolerance_nanos as u64);
        pairer.max_buffered_klv = max_buffered_klv as u64;
        pairer.max_buffered_video = max_buffered_video as u64;

        let demuxer = if has_demuxer_config {
            let Some(cfg) = build_demux_config_from_args(
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
            cfg
        } else {
            DemuxerConfig::default()
        };

        let mut cfg = PairingDemuxerConfig::default();
        cfg.pairer = pairer;
        cfg.demuxer = demuxer;

        REGISTRY.insert(PairingDemuxer::with_config(
            video_pid as u16,
            klv_pid as u16,
            cfg,
        )) as jlong
    }
}

jni_native! {
    /// `nFeed(handle, bytes)` — read the Java byte array, feed it, and return
    /// the produced `PairerOutput`s as a `java.util.ArrayList`. A `DemuxError`
    /// is mapped to a thrown `org.tstrans.DemuxException` via
    /// [`throw_demux_error`].
    fn Java_org_tstrans_pipeline_Pairer_nFeed<'local>(
        env,
        _class,
        handle: jlong,
        bytes: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let buf = match env.convert_byte_array(&bytes) {
            Ok(b) => b,
            Err(_) => {
                throw_demux(
                    env,
                    BindingErrorKind::Internal,
                    "failed to read byte[] argument",
                );
                return JObject::null().into_raw();
            }
        };

        // Lease + feed under the resource lock; the owned `PairerOutput`s leave the
        // closure so the Java list is built outside it. `None` → closed handle.
        let Some(feed_result) = REGISTRY.with_poisoning(handle as u64, |pd| pd.feed(&buf)) else {
            closed(env);
            return JObject::null().into_raw();
        };
        let outputs = match feed_result {
            Ok(v) => v,
            Err(e) => {
                throw_demux_error(env, &e);
                return JObject::null().into_raw();
            }
        };

        match build_output_list(env, &outputs) {
            Ok(list) => list.into_raw(),
            Err(()) => JObject::null().into_raw(),
        }
    }
}

jni_native! {
    /// `nFlush(handle)` — drain end-of-stream state and return the trailing
    /// `PairerOutput`s: any unused KLV history as `UnpairedKlv` (in both modes),
    /// plus the buffered video AUs in `Buffered` mode.
    fn Java_org_tstrans_pipeline_Pairer_nFlush<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        let Some(outputs) = REGISTRY.with_poisoning(handle as u64, |pd| pd.flush()) else {
            closed(env);
            return JObject::null().into_raw();
        };
        match build_output_list(env, &outputs) {
            Ok(list) => list.into_raw(),
            Err(()) => JObject::null().into_raw(),
        }
    }
}

/// Build a `java.util.ArrayList<PairerOutput>` from a slice of outputs.
fn build_output_list<'local>(
    env: &mut Env<'local>,
    outs: &[PairerOutput],
) -> Result<JObject<'local>, ()> {
    let list = env
        .new_object(
            FindClass(jni_str!("java/util/ArrayList")),
            jni_sig!("(I)V"),
            &[JValue::Int(outs.len() as i32)],
        )
        .map_err(|_| ())?;
    for o in outs {
        // A batch can hold thousands of outputs, each minting several local
        // refs; a per-item frame reclaims them so the table stays bounded. The
        // only value leaving the frame is `()`: the element is added to `list`
        // (an outer-frame ref) inside it.
        // 16 slots: a Paired item holds about 12-13 locals (two samples, their
        // unit lists, strings, the record and scratch).
        env.with_local_frame(16, |env| -> jni::errors::Result<()> {
            // `Err(())` leaves its exception pending; the frame error only
            // carries the failure out.
            let obj = convert_output(env, o)
                .map_err(|()| jni::errors::Error::JniCall(jni::errors::JniError::Unknown))?;
            env.call_method(
                &list,
                jni_str!("add"),
                jni_sig!("(Ljava/lang/Object;)Z"),
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })
        .map_err(|_| ())?;
    }
    Ok(list)
}

/// Convert one `PairerOutput` to its `org.tstrans.pipeline.PairerOutput.*`
/// sealed-record variant (nested binary names use `$`). `Err(())` = a JNI
/// conversion failed (exception pending).
fn convert_output<'local>(env: &mut Env<'local>, o: &PairerOutput) -> Result<JObject<'local>, ()> {
    let obj = match o {
        PairerOutput::Paired { video, klv } => {
            let v = convert_video_sample(env, video)?;
            let k = convert_klv_sample(env, klv)?;
            env.new_object(
                FindClass(jni_str!("org/tstrans/pipeline/PairerOutput$Paired")),
                jni_sig!("(Lorg/tstrans/pipeline/VideoSample;Lorg/tstrans/pipeline/KlvSample;)V"),
                &[JValue::Object(&v), JValue::Object(&k)],
            )
            .map_err(|_| ())?
        }
        PairerOutput::UnpairedVideo(video) => {
            let v = convert_video_sample(env, video)?;
            env.new_object(
                FindClass(jni_str!("org/tstrans/pipeline/PairerOutput$UnpairedVideo")),
                jni_sig!("(Lorg/tstrans/pipeline/VideoSample;)V"),
                &[JValue::Object(&v)],
            )
            .map_err(|_| ())?
        }
        PairerOutput::UnpairedKlv(klv) => {
            let k = convert_klv_sample(env, klv)?;
            env.new_object(
                FindClass(jni_str!("org/tstrans/pipeline/PairerOutput$UnpairedKlv")),
                jni_sig!("(Lorg/tstrans/pipeline/KlvSample;)V"),
                &[JValue::Object(&k)],
            )
            .map_err(|_| ())?
        }
        PairerOutput::PassThrough(ev) => {
            let ev_obj = convert_event(env, ev)?;
            env.new_object(
                FindClass(jni_str!("org/tstrans/pipeline/PairerOutput$PassThrough")),
                jni_sig!("(Lorg/tstrans/mpegts/DemuxEvent;)V"),
                &[JValue::Object(&ev_obj)],
            )
            .map_err(|_| ())?
        }
    };
    Ok(obj)
}

/// Project a `VideoSample` to a `org.tstrans.pipeline.VideoSample` record.
/// Field order matches the Java canonical ctor:
/// `(StreamId, long pts, Long dts, VideoCodec, List<VideoUnit>)`.
///
/// Forced-minimal raw-first adaptation: the pairing `VideoSample` no longer
/// carries an eager `payload` field, so the NAL/OBU list is produced here by
/// `vs.split_units()` internally. The Java surface is unchanged (same 5-arg
/// record / ctor signature, same `VideoPayload`). JVM `raw`/RAI parity is
/// not exposed: the JVM pairing projection does not expose the raw AU / RAI fields.
fn convert_video_sample<'local>(
    env: &mut Env<'local>,
    vs: &VideoSample,
) -> Result<JObject<'local>, ()> {
    let stream = build_stream_id(env, &vs.stream)?;
    let dts = opt_long(env, vs.dts)?;
    let codec = enum_const(env, "mpegts", "VideoCodec", video_codec_name(vs.codec))?;
    let (payload, _issues) = vs.split_units();
    let payload_list = build_video_units(env, &payload)?;
    env.new_object(
        FindClass(jni_str!("org/tstrans/pipeline/VideoSample")),
        jni_sig!("(Lorg/tstrans/mpegts/StreamId;JLjava/lang/Long;Lorg/tstrans/mpegts/VideoCodec;Ljava/util/List;)V"),
        &[
            JValue::Object(&stream),
            JValue::Long(vs.pts.as_ticks()),
            JValue::Object(&dts),
            JValue::Object(&codec),
            JValue::Object(&payload_list),
        ],
    )
    .map_err(|_| ())
}

/// Project a `KlvSample` to a `org.tstrans.pipeline.KlvSample` record.
/// Field order matches the Java canonical ctor:
/// `(StreamId, long pts, MetadataKind, ByteBuffer)`. Only the
/// [`metadata_kind`] discriminator is used — the `(was_reassembled,
/// cell_count)` pair it also returns is carried on `DemuxEvent.Metadata`,
/// not on the flat `KlvSample` projection.
fn convert_klv_sample<'local>(
    env: &mut Env<'local>,
    ks: &KlvSample,
) -> Result<JObject<'local>, ()> {
    let stream = build_stream_id(env, &ks.stream)?;
    let (kind, _was_reassembled, _cell_count) = metadata_kind(env, &ks.kind)?;
    let payload = wrap_heap_byte_buffer(env, &ks.payload)?;
    env.new_object(
        FindClass(jni_str!("org/tstrans/pipeline/KlvSample")),
        jni_sig!("(Lorg/tstrans/mpegts/StreamId;JLorg/tstrans/mpegts/MetadataKind;Ljava/nio/ByteBuffer;)V"),
        &[
            JValue::Object(&stream),
            JValue::Long(ks.pts.as_ticks()),
            JValue::Object(&kind),
            JValue::Object(&payload),
        ],
    )
    .map_err(|_| ())
}

jni_native! {
    /// `nStats(handle)` — snapshot the pairing counters as a
    /// `org.tstrans.pipeline.PairerStats` record.
    fn Java_org_tstrans_pipeline_Pairer_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        let Some(s) = REGISTRY.with(handle as u64, |pd| pd.stats()) else {
            closed(env);
            return JObject::null().into_raw();
        };
        match crate::generated::stats_pipeline::build_pairer_stats(env, &s) {
            Ok(o) => o.into_raw(),
            Err(_) => JObject::null().into_raw(),
        }
    }
}

jni_native! {
    /// `nDemuxerStats(handle)` — snapshot the underlying demuxer counters as a
    /// `org.tstrans.mpegts.DemuxerStats` record. The two `u32` fields
    /// (`programs_seen`, `subtitle_streams_seen`) widen to `long`, matching
    /// the Java record's all-`long` shape.
    fn Java_org_tstrans_pipeline_Pairer_nDemuxerStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        let Some(s) = REGISTRY.with(handle as u64, |pd| pd.demuxer_stats()) else {
            closed(env);
            return JObject::null().into_raw();
        };
        match crate::generated::stats_mpegts::build_demuxer_stats(env, &s) {
            Ok(o) => o.into_raw(),
            Err(_) => JObject::null().into_raw(),
        }
    }
}

jni_native! {
    /// `nResetStats(handle)` — zero the pairing counters (not demuxer stats).
    fn Java_org_tstrans_pipeline_Pairer_nResetStats<'local>(env, _class, handle: jlong) = () => {
        if REGISTRY
            .with_poisoning(handle as u64, |pd| pd.reset_stats())
            .is_none()
        {
            closed(env);
        }
    }
}

jni_native! {
    /// `nClose(handle)` — take + drop the registered [`PairingDemuxer`]. Atomic +
    /// idempotent via `REGISTRY.close`, so a double `close()` is
    /// UAF/double-free-safe. The pairing demuxer's teardown is a plain drop (no
    /// flush/finalize).
    fn Java_org_tstrans_pipeline_Pairer_nClose<'local>(_env, _class, handle: jlong) = () => {
        // The winning close gets the pairing demuxer back; it has no extra teardown,
        // so just let it drop here.
        let _ = REGISTRY.close(handle as u64);
    }
}

/// Throw `IllegalStateException` for a leased call that found a closed/absent
/// handle — the native-side mirror of the Java `ensureOpen()` check.
fn closed(env: &mut Env) {
    crate::error::throw_closed(env, "Pairer");
}
