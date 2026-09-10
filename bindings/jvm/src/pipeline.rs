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

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};
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
use crate::jutil::enum_const;
use crate::mpegts::{
    build_demux_config_from_args, build_stream_id, build_video_units, convert_event, metadata_kind,
    opt_long, throw_demux_error, video_codec_name, wrap_heap_byte_buffer,
};

/// Per-type leased-handle registry for `org.tstrans.pipeline.Pairer`.
static REGISTRY: LazyLock<HandleRegistry<PairingDemuxer>> = LazyLock::new(HandleRegistry::new);

/// `nOpen(videoPid, klvPid)` — allocate a default-config [`PairingDemuxer`]
/// and hand the JVM its raw pointer as a `jlong` handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nOpen<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    video_pid: jint,
    klv_pid: jint,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        REGISTRY.insert(PairingDemuxer::new(video_pid as u16, klv_pid as u16)) as jlong
    })
}

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
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nOpenWithConfig<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
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
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let mode = if buffered != 0 {
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

        let demuxer = if has_demuxer_config != 0 {
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
    })
}

/// `nFeed(handle, bytes)` — read the Java byte array, feed it, and return
/// the produced `PairerOutput`s as a `java.util.ArrayList`. A `DemuxError`
/// is mapped to a thrown `org.tstrans.DemuxException` via
/// [`throw_demux_error`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nFeed<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    bytes: JByteArray<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let buf = match env.convert_byte_array(&bytes) {
            Ok(b) => b,
            Err(_) => {
                throw_demux(env, "INTERNAL", "failed to read byte[] argument");
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
    })
}

/// `nFlush(handle)` — drain end-of-stream state and return the trailing
/// `PairerOutput`s: any unused KLV history as `UnpairedKlv` (in both modes),
/// plus the buffered video AUs in `Buffered` mode.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nFlush<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(outputs) = REGISTRY.with_poisoning(handle as u64, |pd| pd.flush()) else {
            closed(env);
            return JObject::null().into_raw();
        };
        match build_output_list(env, &outputs) {
            Ok(list) => list.into_raw(),
            Err(()) => JObject::null().into_raw(),
        }
    })
}

/// Build a `java.util.ArrayList<PairerOutput>` from a slice of outputs.
fn build_output_list<'local>(
    env: &mut JNIEnv<'local>,
    outs: &[PairerOutput],
) -> Result<JObject<'local>, ()> {
    let list = env
        .new_object(
            "java/util/ArrayList",
            "(I)V",
            &[JValue::Int(outs.len() as i32)],
        )
        .map_err(|_| ())?;
    for o in outs {
        // `Ok(None)` = skip this output (a forward-compat unmapped event;
        // unreachable today). Mirrors `mpegts::nNextEvent`'s skip behavior so a
        // future skip-worthy variant is dropped from the batch, not failed.
        if let Some(obj) = convert_output(env, o)? {
            env.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )
            .map_err(|_| ())?;
        }
    }
    Ok(list)
}

/// Convert one `PairerOutput` to its `org.tstrans.pipeline.PairerOutput.*`
/// sealed-record variant (nested binary names use `$`). `Ok(None)` = skip this
/// output (a forward-compat unmapped `DemuxEvent` in a `PassThrough`; not
/// produced today); `Err(())` = a JNI conversion failed (exception pending).
fn convert_output<'local>(
    env: &mut JNIEnv<'local>,
    o: &PairerOutput,
) -> Result<Option<JObject<'local>>, ()> {
    let obj = match o {
        PairerOutput::Paired { video, klv } => {
            let v = convert_video_sample(env, video)?;
            let k = convert_klv_sample(env, klv)?;
            env.new_object(
                "org/tstrans/pipeline/PairerOutput$Paired",
                "(Lorg/tstrans/pipeline/VideoSample;Lorg/tstrans/pipeline/KlvSample;)V",
                &[JValue::Object(&v), JValue::Object(&k)],
            )
            .map_err(|_| ())?
        }
        PairerOutput::UnpairedVideo(video) => {
            let v = convert_video_sample(env, video)?;
            env.new_object(
                "org/tstrans/pipeline/PairerOutput$UnpairedVideo",
                "(Lorg/tstrans/pipeline/VideoSample;)V",
                &[JValue::Object(&v)],
            )
            .map_err(|_| ())?
        }
        PairerOutput::UnpairedKlv(klv) => {
            let k = convert_klv_sample(env, klv)?;
            env.new_object(
                "org/tstrans/pipeline/PairerOutput$UnpairedKlv",
                "(Lorg/tstrans/pipeline/KlvSample;)V",
                &[JValue::Object(&k)],
            )
            .map_err(|_| ())?
        }
        PairerOutput::PassThrough(ev) => {
            // `convert_event` can in principle signal "skip this event"
            // (`Ok(None)`) — a forward-compat channel no current variant
            // exercises. Propagate the skip so the unmapped event is dropped
            // from the batch (matching `mpegts::nNextEvent`), not failed.
            let Some(ev_obj) = convert_event(env, ev)? else {
                return Ok(None);
            };
            env.new_object(
                "org/tstrans/pipeline/PairerOutput$PassThrough",
                "(Lorg/tstrans/mpegts/DemuxEvent;)V",
                &[JValue::Object(&ev_obj)],
            )
            .map_err(|_| ())?
        }
    };
    Ok(Some(obj))
}

/// Project a `VideoSample` to a `org.tstrans.pipeline.VideoSample` record.
/// Field order matches the Java canonical ctor:
/// `(StreamId, long pts, Long dts, VideoCodec, List<VideoUnit>)`.
///
/// Forced-minimal raw-first adaptation: the pairing `VideoSample` no longer
/// carries an eager `payload` field, so the NAL/OBU list is produced here by
/// `vs.split_units()` internally. The Java surface is unchanged (same 5-arg
/// record / ctor signature, same `VideoPayload`). JVM `raw`/RAI parity is
/// deferred to the design §5 fast-follow.
fn convert_video_sample<'local>(
    env: &mut JNIEnv<'local>,
    vs: &VideoSample,
) -> Result<JObject<'local>, ()> {
    let stream = build_stream_id(env, &vs.stream)?;
    let dts = opt_long(env, vs.dts)?;
    let codec = enum_const(env, "mpegts", "VideoCodec", video_codec_name(vs.codec))?;
    let (payload, _issues) = vs.split_units();
    let payload_list = build_video_units(env, &payload)?;
    env.new_object(
        "org/tstrans/pipeline/VideoSample",
        "(Lorg/tstrans/mpegts/StreamId;JLjava/lang/Long;Lorg/tstrans/mpegts/VideoCodec;Ljava/util/List;)V",
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
    env: &mut JNIEnv<'local>,
    ks: &KlvSample,
) -> Result<JObject<'local>, ()> {
    let stream = build_stream_id(env, &ks.stream)?;
    let (kind, _was_reassembled, _cell_count) = metadata_kind(env, &ks.kind)?;
    let payload = wrap_heap_byte_buffer(env, &ks.payload)?;
    env.new_object(
        "org/tstrans/pipeline/KlvSample",
        "(Lorg/tstrans/mpegts/StreamId;JLorg/tstrans/mpegts/MetadataKind;Ljava/nio/ByteBuffer;)V",
        &[
            JValue::Object(&stream),
            JValue::Long(ks.pts.as_ticks()),
            JValue::Object(&kind),
            JValue::Object(&payload),
        ],
    )
    .map_err(|_| ())
}

/// `nStats(handle)` — snapshot the pairing counters as a
/// `org.tstrans.pipeline.PairerStats` record.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(s) = REGISTRY.with(handle as u64, |pd| pd.stats()) else {
            closed(env);
            return JObject::null().into_raw();
        };
        match env.new_object(
            "org/tstrans/pipeline/PairerStats",
            "(JJJJ)V",
            &[
                JValue::Long(s.paired as i64),
                JValue::Long(s.unpaired_video as i64),
                JValue::Long(s.unpaired_klv as i64),
                JValue::Long(s.pass_through as i64),
            ],
        ) {
            Ok(o) => o.into_raw(),
            Err(_) => JObject::null().into_raw(),
        }
    })
}

/// `nDemuxerStats(handle)` — snapshot the underlying demuxer counters as a
/// `org.tstrans.mpegts.DemuxerStats` record. The two `u32` fields
/// (`programs_seen`, `subtitle_streams_seen`) widen to `long`, matching
/// the Java record's all-`long` shape.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nDemuxerStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let Some(s) = REGISTRY.with(handle as u64, |pd| pd.demuxer_stats()) else {
            closed(env);
            return JObject::null().into_raw();
        };
        match env.new_object(
            "org/tstrans/mpegts/DemuxerStats",
            "(JJJJJJ)V",
            &[
                JValue::Long(s.program_maps_seen as i64),
                JValue::Long(s.pmt_versions_seen as i64),
                JValue::Long(s.discontinuities as i64),
                JValue::Long(s.nonconformant as i64),
                JValue::Long(s.programs_seen as i64),
                JValue::Long(s.subtitle_streams_seen as i64),
            ],
        ) {
            Ok(o) => o.into_raw(),
            Err(_) => JObject::null().into_raw(),
        }
    })
}

/// `nResetStats(handle)` — zero the pairing counters (not demuxer stats).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nResetStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        if REGISTRY
            .with_poisoning(handle as u64, |pd| pd.reset_stats())
            .is_none()
        {
            closed(env);
        }
    })
}

/// `nClose(handle)` — take + drop the registered [`PairingDemuxer`]. Atomic +
/// idempotent via `REGISTRY.close`, so a double `close()` is
/// UAF/double-free-safe. The pairing demuxer's teardown is a plain drop (no
/// flush/finalize).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_pipeline_Pairer_nClose<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // The winning close gets the pairing demuxer back; it has no extra teardown,
        // so just let it drop here.
        let _ = REGISTRY.close(handle as u64);
    })
}

/// Throw `IllegalStateException` for a leased call that found a closed/absent
/// handle — the native-side mirror of the Java `ensureOpen()` check.
fn closed(env: &mut JNIEnv) {
    crate::error::throw_closed(env, "Pairer");
}
