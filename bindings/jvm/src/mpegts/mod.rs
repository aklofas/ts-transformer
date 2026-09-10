//! JNI surface for `org.tstrans.mpegts.Demuxer` — the keystone vertical.
//!
//! Wraps `tst_core::mpegts::demux::Demuxer`, converting each `DemuxEvent` into
//! one of the Java records (`DemuxEvent.ProgramMap` /
//! `DemuxEvent.Video` / `DemuxEvent.Audio` / `DemuxEvent.Subtitle` /
//! `DemuxEvent.UnknownSample` / `DemuxEvent.Metadata` /
//! `DemuxEvent.NonConformant` / `DemuxEvent.Discontinuity` /
//! `DemuxEvent.ReconnectDiscontinuity`) and mapping
//! `DemuxError` to `org.tstrans.DemuxException` via `crate::error::throw_demux`.
//! Mirrors `bindings/python/src/mpegts.rs` (`convert_*`/`demux_error_to_pyerr`)
//! decision-for-decision.
//!
//! Handle convention: the `jlong` is an opaque key into a per-type
//! [`crate::handle::HandleRegistry`] over the [`Demuxer`]; `nOpen`/`nOpenWithConfig`
//! register via `REGISTRY.insert`; per-call fns lease via `REGISTRY.with`
//! (mapping a closed/absent handle to a thrown `IllegalStateException`); `nClose`
//! takes + drops via `REGISTRY.close` (atomic + idempotent, so a double
//! `close()` is UAF/double-free-safe).
//!
//! The sample-record `payload` is a COPIED, Java-owned heap `ByteBuffer` (`ByteBuffer.wrap`
//! over a fresh `byte[]`). The earlier zero-copy direct-buffer over Rust-owned
//! memory was a use-after-free hazard once a consumer retained the buffer past
//! the next pull, and a JDK-17-stable primitive for *defined*-on-stale-read
//! zero-copy does not exist (the real one is FFM `Arena`/`MemorySegment`, stable
//! only in JDK 22+). Real zero-copy is therefore deferred to a JDK-22+ FFM path;
//! the keystone copies, which is unconditionally safe. See the design spec §5.4.

pub mod muxer;

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_core::error::DemuxError;
use tst_core::mpegts::au_cell::CellFragmentIndication;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::event::MultiCellAuReason;
use tst_core::mpegts::demux::{
    AudioCodec, DemuxEvent, Demuxer, DiscontinuityKind, MetadataKind, NonConformantIssue,
    SamplePayload, StreamId, StreamKind, SubtitleCodec, VideoCodec, VideoPayload, split_video,
};
use tst_core::mpegts::mux::Av1CarriageMode;
use tst_core::shared::SharedBytes;

use crate::codec::aac::build_adts_frame;
use crate::codec::mpegaudio::build_mpeg2_audio_frame;
use crate::codec::shared::{build_nal_unit, build_obu};
use crate::error::{map_codec_parse_error, throw_demux};
use crate::handle::HandleRegistry;
use crate::jutil::enum_const;

/// Per-type leased-handle registry for `org.tstrans.mpegts.Demuxer`.
static REGISTRY: LazyLock<HandleRegistry<Demuxer>> = LazyLock::new(HandleRegistry::new);

/// `org.tstrans.mpegts.Demuxer.nOpen()` — allocate a [`Demuxer`] and hand the JVM
/// its raw pointer as a `jlong` handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Demuxer_nOpen<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| REGISTRY.insert(Demuxer::new()) as jlong)
}

/// `nOpenWithConfig(...)` — build a configured [`Demuxer`]. The `strict`/`av1`
/// ints are the Java enum ORDINALS (contract: must mirror the Java enum
/// declaration order — `StrictMode`: 0=OFF,1=TIMING_ONLY,2=PSI_ONLY,3=FULL;
/// `Av1CarriageMode`: 0=MPEG2_TS_BINDING,1=INTEROP_RAW_OBU). A `0` cap means
/// "use the Rust default" (mapped to `None`). Mirrors tst-py's
/// `build_demuxer_config` field-by-field.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Demuxer_nOpenWithConfig<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
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
        REGISTRY.insert(Demuxer::with_config(opts)) as jlong
    })
}

/// Assemble a `tst_core` [`DemuxerConfig`] from the 9 marshalled JNI primitives
/// (the `nOpenWithConfig` arg shape). The `strict`/`av1` ints are the Java enum
/// ORDINALS (contract: must mirror the Java enum declaration order —
/// `StrictMode`: 0=OFF,1=TIMING_ONLY,2=PSI_ONLY,3=FULL; `Av1CarriageMode`:
/// 0=MPEG2_TS_BINDING,1=INTEROP_RAW_OBU). A `0` cap means "use the Rust default"
/// (mapped to `None`). Mirrors tst-py's `build_demuxer_config` field-by-field.
///
/// Shared by `nOpenWithConfig` and the srt `DemuxReceiver.nFromUrlWithConfig` /
/// `Socket.nIntoDemuxReceiverWithConfig` paths so the config assembly is DRY.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_demux_config_from_args(
    env: &mut jni::JNIEnv,
    strict: jint,
    pes_cap_per_pid: jlong,
    pes_cap_total: jlong,
    cfi: jboolean,
    av1: jint,
    au_cell_cap: jlong,
    lenient_psi: jboolean,
    sync_buf_cap: jlong,
    unwrap_timestamps: jboolean,
) -> Option<tst_core::mpegts::demux::DemuxerConfig> {
    use tst_core::mpegts::demux::{DemuxerConfig, StrictMode};

    // `DemuxerConfig` is non-exhaustive in `tst_core`, so it can't be built with
    // struct-expression syntax from this crate — assemble it field-by-field on
    // top of `default()`, mirroring tst-py's `build_demuxer_config`. The Rust-only
    // `klv_link_overrides`/`stream_kind_overrides` keep their defaults (deferred).
    let mut opts = DemuxerConfig::default();
    opts.strict = match strict {
        0 => StrictMode::Off,
        1 => StrictMode::TimingOnly,
        2 => StrictMode::DescriptorsOnly, // Java PSI_ONLY
        _ => StrictMode::Full,            // 3 (and any out-of-range → strictest, safe)
    };
    opts.av1_carriage = match av1 {
        0 => Av1CarriageMode::Mpeg2TsBinding,
        1 => Av1CarriageMode::InteropRawObu,
        other => {
            // Exact ordinal validation, mirroring nSplitVideo: the value
            // comes from our own Java enum, so out-of-range means enum
            // drift — fail loudly rather than silently demuxing with the
            // wrong carriage.
            throw_demux(
                env,
                "INTERNAL",
                &format!("unknown Av1CarriageMode ordinal {other}"),
            );
            return None;
        }
    };
    if pes_cap_per_pid > 0 {
        opts.pes_cap_per_pid = Some(pes_cap_per_pid as usize);
    }
    if pes_cap_total > 0 {
        opts.pes_cap_total = Some(pes_cap_total as usize);
    }
    if au_cell_cap > 0 {
        opts.au_cell_cap_per_pid = Some(au_cell_cap as usize);
    }
    opts.cfi_tolerance = cfi != 0;
    opts.lenient_psi_reassembly = lenient_psi != 0;
    if sync_buf_cap > 0 {
        opts.sync_buf_cap = Some(sync_buf_cap as usize);
    }
    opts.unwrap_timestamps = unwrap_timestamps != 0;
    Some(opts)
}

/// `nClose(handle)` — take + drop the registered [`Demuxer`]. Atomic +
/// idempotent via `REGISTRY.close`, so a double `close()` is
/// UAF/double-free-safe. The demuxer's teardown is a plain drop (no
/// flush/finalize).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Demuxer_nClose<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // The winning close gets the demuxer back; it has no extra teardown, so just
        // let it drop here.
        let _ = REGISTRY.close(handle as u64);
    })
}

/// `nFeed(handle, bytes)` — read the Java byte array into a Rust buffer and feed
/// it to the demuxer. A `DemuxError` is mapped inline to a thrown
/// `DemuxException` (see the `match` below); the literal discriminant per arm is
/// what the error-mapping ratchet greps for.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Demuxer_nFeed<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    bytes: JByteArray<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let buf = match env.convert_byte_array(&bytes) {
            Ok(b) => b,
            Err(_) => {
                throw_demux(env, "INTERNAL", "failed to read byte[] argument");
                return;
            }
        };

        match REGISTRY.with_poisoning(handle as u64, |dx| dx.feed(&buf)) {
            Some(Ok(())) => {}
            Some(Err(e)) => throw_demux_error(env, &e),
            None => closed(env),
        }
    })
}

/// Map a `tst_core` [`DemuxError`] to a thrown `org.tstrans.DemuxException`,
/// mirroring tst-py's `demux_error_to_pyerr` exactly. The discriminant MUST be a
/// string literal as the 2nd arg to `throw_demux` (ratchet contract — the `java
/// demux` error-mapping rail greps the whole tree, so these literals living here
/// rather than at the `nFeed` call site keeps coverage intact).
///
/// Shared by `nFeed` and the srt `DemuxReceiver.nNext` demux-error arm.
pub(crate) fn throw_demux_error(env: &mut JNIEnv, e: &DemuxError) {
    match e {
        DemuxError::SyncBufExhausted { .. } => throw_demux(env, "SYNC_LOSS", &e.to_string()),
        DemuxError::MalformedPsi { .. } => throw_demux(env, "BAD_PMT", &e.to_string()),
        DemuxError::MalformedPes { .. } => throw_demux(env, "BAD_PES", &e.to_string()),
        DemuxError::StrictRejection(_) => throw_demux(env, "STRICT_REJECTION", &e.to_string()),
        DemuxError::Unrecoverable { .. } => throw_demux(env, "INTERNAL", &e.to_string()),
        // DemuxError is marked non-exhaustive; forward-compat catch-all.
        _ => throw_demux(env, "INTERNAL", &e.to_string()),
    }
}

/// `nFlush(handle)` — flush in-flight PES reassembly (call once at EOF).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Demuxer_nFlush<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        if REGISTRY
            .with_poisoning(handle as u64, |dx| dx.flush())
            .is_none()
        {
            closed(env);
        }
    })
}

/// `nNextEvent(handle)` — pull the next event, converting it to a Java
/// `DemuxEvent` record. Every current `DemuxEvent` variant maps to a record, so
/// the loop returns the first event pulled; the `Ok(None) => continue` arm is a
/// retained forward-compat guard (currently unreachable). Returns Java `null`
/// when the queue drains.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Demuxer_nNextEvent<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // Lease + drive the pull loop under the resource lock. `with` runs the
        // closure synchronously, so capturing `env` (`&mut JNIEnv`) to build the
        // Java record in-place is sound. `None` (closed/absent) → IllegalStateException.
        let result = REGISTRY.with_poisoning(handle as u64, |dx| {
            loop {
                let Some(ev) = dx.next_event() else {
                    return JObject::null().into_raw();
                };
                match convert_event(env, &ev) {
                    Ok(Some(obj)) => return obj.into_raw(),
                    // All current `DemuxEvent` variants map to `Ok(Some(..))`, so this
                    // branch is currently unreachable; retained as a forward-compat
                    // guard should a future skip-worthy variant appear.
                    Ok(None) => continue,
                    Err(()) => {
                        throw_demux(env, "INTERNAL", "event conversion failed");
                        return JObject::null().into_raw();
                    }
                }
            }
        });
        match result {
            Some(obj) => obj,
            None => {
                closed(env);
                JObject::null().into_raw()
            }
        }
    })
}

/// `DemuxEventVideoNatives.nSplitVideo(raw, codecOrdinal, av1CarriageOrdinal)` — the
/// opt-in unit-split native backing [`DemuxEvent.Video.parse()`]. Calls
/// `tst_core::mpegts::demux::split_video` on `raw` and converts the resulting
/// [`VideoPayload`] into the same `java.util.List<VideoUnit>` the eager path
/// formerly produced. The codec ordinal maps the Java `VideoCodec` enum
/// declaration order; the av1 carriage ordinal maps `Av1CarriageMode`
/// (0 = MPEG2_TS_BINDING, 1 = INTEROP_RAW_OBU; non-AV1 callers pass 0 and
/// `split_video` ignores it). Mirrors tst-py's `Video.parse()`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_DemuxEventVideoNatives_nSplitVideo<'local>(
    mut env: JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    raw: jni::objects::JByteArray<'local>,
    codec_ordinal: jni::sys::jint,
    av1_carriage_ordinal: jni::sys::jint,
) -> jni::sys::jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let raw_bytes = match env.convert_byte_array(&raw) {
            Ok(b) => b,
            Err(_) => {
                throw_demux(env, "INTERNAL", "failed to read byte[] argument");
                return std::ptr::null_mut();
            }
        };
        // Exact ordinal validation: the values come from our own Java enums,
        // so an out-of-range ordinal means enum drift (a Java variant added
        // without updating this mapping) or a reflective misuse — fail loudly
        // rather than silently parsing with the wrong codec/carriage.
        let codec = match codec_ordinal {
            0 => VideoCodec::H264,
            1 => VideoCodec::H265,
            2 => VideoCodec::H266,
            3 => VideoCodec::Av1,
            other => {
                throw_demux(
                    env,
                    "INTERNAL",
                    &format!("unknown VideoCodec ordinal {other}"),
                );
                return std::ptr::null_mut();
            }
        };
        let av1_carriage = match av1_carriage_ordinal {
            0 => Av1CarriageMode::Mpeg2TsBinding,
            1 => Av1CarriageMode::InteropRawObu,
            other => {
                throw_demux(
                    env,
                    "INTERNAL",
                    &format!("unknown Av1CarriageMode ordinal {other}"),
                );
                return std::ptr::null_mut();
            }
        };
        let shared = SharedBytes::from_vec(raw_bytes);
        let (payload, _issues) = split_video(&shared, codec, av1_carriage);
        match build_video_units(env, &payload) {
            Ok(list) => list.into_raw(),
            Err(()) => {
                throw_demux(env, "INTERNAL", "video unit split failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// `DemuxEventAudioNatives.nParseAudio(raw, codecOrdinal, strict)` — the opt-in
/// frame-parse native backing [`DemuxEvent.Audio.parse()`]. Parses the raw audio
/// elementary-stream bytes into the same `java.util.List<AudioFrame>` the eager
/// path formerly produced — `AdtsFrame`s for AAC, `Mpeg2AudioFrame`s for MP2, and
/// an EMPTY list for codecs with no typed parser (AAC-LATM, AC-3). `strict=false`
/// uses `frames_with_resync` (skips past corruption to the next valid frame, never
/// throws); `strict=true` uses `frames` (throws `CodecParseException` on the first
/// malformed frame). Mirrors tst-py's `codec.parse_audio(raw, codec, strict=...)`
/// decision-for-decision (the strict codec labels `"aac"` / `"mpeg2audio"` match
/// tst-py's `parse_audio`, not the muxer-side `"mp2"` short name).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_DemuxEventAudioNatives_nParseAudio<'local>(
    mut env: JNIEnv<'local>,
    _class: jni::objects::JClass<'local>,
    raw: jni::objects::JByteArray<'local>,
    codec_ordinal: jni::sys::jint,
    strict: jni::sys::jboolean,
) -> jni::sys::jobject {
    use tst_core::codec::aac::{frames as aac_frames, frames_with_resync as aac_resync};
    use tst_core::codec::mpegaudio::{frames as mp2_frames, frames_with_resync as mp2_resync};

    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let raw_bytes = match env.convert_byte_array(&raw) {
            Ok(b) => b,
            Err(_) => {
                throw_demux(env, "INTERNAL", "failed to read byte[] argument");
                return std::ptr::null_mut();
            }
        };
        let strict = strict != 0;
        // Exact ordinal validation, mirroring nSplitVideo: an out-of-range value
        // means AudioCodec enum drift (a Java variant added without updating this
        // mapping) or a reflective misuse — fail loudly.
        let codec = match codec_ordinal {
            0 => AudioCodec::Mp2,
            1 => AudioCodec::Aac,
            2 => AudioCodec::AacLatm,
            3 => AudioCodec::Ac3,
            other => {
                throw_demux(
                    env,
                    "INTERNAL",
                    &format!("unknown AudioCodec ordinal {other}"),
                );
                return std::ptr::null_mut();
            }
        };
        // Dispatch on codec + strict, mirroring tst-py's `parse_audio`:
        // AAC/MP2 → typed frames (strict `frames` vs lenient `frames_with_resync`);
        // AAC-LATM/AC-3 → empty list (no typed parser — read raw() directly).
        match codec {
            AudioCodec::Aac => {
                if strict {
                    // STRICT: the first Err throws + returns null.
                    let owned: Result<Vec<_>, _> = aac_frames(&raw_bytes)
                        .map(|res| res.map(|f| f.to_owned()))
                        .collect();
                    let owned = match owned {
                        Ok(v) => v,
                        Err(e) => {
                            map_codec_parse_error(env, &e, "aac");
                            return std::ptr::null_mut();
                        }
                    };
                    match build_adts_frame_list(env, &owned) {
                        Ok(list) => list.into_raw(),
                        Err(()) => std::ptr::null_mut(),
                    }
                } else {
                    // BEST-EFFORT: Err items are silently skipped (resync).
                    let owned: Vec<_> = aac_resync(&raw_bytes)
                        .filter_map(|res| res.ok())
                        .map(|f| f.to_owned())
                        .collect();
                    match build_adts_frame_list(env, &owned) {
                        Ok(list) => list.into_raw(),
                        Err(()) => std::ptr::null_mut(),
                    }
                }
            }
            AudioCodec::Mp2 => {
                if strict {
                    let owned: Result<Vec<_>, _> = mp2_frames(&raw_bytes)
                        .map(|res| res.map(|f| f.to_owned()))
                        .collect();
                    let owned = match owned {
                        Ok(v) => v,
                        Err(e) => {
                            map_codec_parse_error(env, &e, "mpeg2audio");
                            return std::ptr::null_mut();
                        }
                    };
                    match build_mpeg2_frame_list(env, &owned) {
                        Ok(list) => list.into_raw(),
                        Err(()) => std::ptr::null_mut(),
                    }
                } else {
                    let owned: Vec<_> = mp2_resync(&raw_bytes)
                        .filter_map(|res| res.ok())
                        .map(|f| f.to_owned())
                        .collect();
                    match build_mpeg2_frame_list(env, &owned) {
                        Ok(list) => list.into_raw(),
                        Err(()) => std::ptr::null_mut(),
                    }
                }
            }
            // AAC-LATM + AC-3 typed parsing is deferred — empty list (no typed
            // parser). Matches tst-py's `_ =>` arm.
            _ => match env.new_object("java/util/ArrayList", "()V", &[]) {
                Ok(list) => list.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
        }
    })
}

/// Throw `IllegalStateException` for a leased call that found a closed/absent
/// handle — the native-side enforcement of the same closed-handle contract the
/// Java `ensureOpen()` already checks, so the JNI boundary fails closed even if a
/// private native method is reached by reflection.
fn closed(env: &mut JNIEnv) {
    crate::error::throw_closed(env, "Demuxer");
}

/// Convert one `DemuxEvent` to a Java `DemuxEvent` record.
///
/// `DemuxEvent` is not marked non-exhaustive, so this match is exhaustive and
/// every variant currently builds a record: `Ok(Some(obj))`. The `Ok(None)`
/// "skip this event" channel is retained in the return type as a forward-compat
/// guard (see `nNextEvent`) but is not produced today. `Err(())` — a JNI call
/// failed.
pub(crate) fn convert_event<'local>(
    env: &mut JNIEnv<'local>,
    ev: &DemuxEvent,
) -> Result<Option<JObject<'local>>, ()> {
    match ev {
        DemuxEvent::ProgramMap(pm) => {
            let pids = build_pid_list(env, pm.streams.iter().map(|s| s.pid))?;
            let obj = env
                .new_object(
                    "org/tstrans/mpegts/DemuxEvent$ProgramMap",
                    "(IIILjava/util/List;)V",
                    &[
                        JValue::Int(pm.program_number as i32),
                        JValue::Int(pm.pcr_pid as i32),
                        JValue::Int(pm.pmt_pid as i32),
                        JValue::Object(&pids),
                    ],
                )
                .map_err(|_| ())?;
            Ok(Some(obj))
        }
        DemuxEvent::Sample {
            stream,
            pts,
            dts,
            payload,
        } => {
            // Raw-first sample records. Mirrors tst-py's `convert_sample_event`
            // decision-for-decision: video + audio surface the exact encoded
            // access-unit bytes as a heap `ByteBuffer`; typed unit/frame parsing
            // is deferred to the opt-in `Video.parse()` / `Audio.parse()` calls.
            // subtitle/unknown → raw heap `ByteBuffer`.
            let stream_obj = build_stream_id(env, stream)?;
            let pts_ticks = pts.as_ticks();
            let dts_obj = opt_long(env, *dts)?;
            let obj = match payload {
                SamplePayload::Video {
                    codec,
                    raw,
                    random_access_indicator,
                    av1_carriage,
                    ..
                } => {
                    // Raw-first: emit the encoded AU as a heap-copied ByteBuffer;
                    // NAL/OBU unit splitting is deferred to Video.parse() (opt-in),
                    // mirroring tst-py's model. JDK < 22 forbids direct buffers over
                    // Rust memory, so we copy.
                    let codec_obj =
                        enum_const(env, "mpegts", "VideoCodec", video_codec_name(*codec))?;
                    let raw_buf = wrap_heap_byte_buffer(env, raw.as_slice())?;
                    // av1Carriage: Some(mode) → enum constant; None → null (non-AV1).
                    let av1_carriage_obj = match av1_carriage {
                        Some(mode) => {
                            enum_const(env, "mpegts", "Av1CarriageMode", av1_carriage_name(*mode))?
                        }
                        None => JObject::null(),
                    };
                    env.new_object(
                        "org/tstrans/mpegts/DemuxEvent$Video",
                        "(Lorg/tstrans/mpegts/StreamId;JLjava/lang/Long;Lorg/tstrans/mpegts/VideoCodec;Ljava/nio/ByteBuffer;ZLorg/tstrans/mpegts/Av1CarriageMode;)V",
                        &[
                            JValue::Object(&stream_obj),
                            JValue::Long(pts_ticks),
                            JValue::Object(&dts_obj),
                            JValue::Object(&codec_obj),
                            JValue::Object(&raw_buf),
                            JValue::Bool(*random_access_indicator as u8),
                            JValue::Object(&av1_carriage_obj),
                        ],
                    )
                    .map_err(|_| ())?
                }
                SamplePayload::Audio { codec, frames } => {
                    // Raw-first: emit the encoded audio ES as a heap-copied
                    // ByteBuffer; typed-frame parsing is deferred to Audio.parse()
                    // (opt-in), mirroring tst-py's model and the WP16 Video shape.
                    // JDK < 22 forbids direct buffers over Rust memory, so we copy.
                    let codec_obj =
                        enum_const(env, "mpegts", "AudioCodec", audio_codec_name(*codec))?;
                    let raw_buf = wrap_heap_byte_buffer(env, frames.as_slice())?;
                    env.new_object(
                        "org/tstrans/mpegts/DemuxEvent$Audio",
                        "(Lorg/tstrans/mpegts/StreamId;JLjava/lang/Long;Lorg/tstrans/mpegts/AudioCodec;Ljava/nio/ByteBuffer;)V",
                        &[
                            JValue::Object(&stream_obj),
                            JValue::Long(pts_ticks),
                            JValue::Object(&dts_obj),
                            JValue::Object(&codec_obj),
                            JValue::Object(&raw_buf),
                        ],
                    )
                    .map_err(|_| ())?
                }
                SamplePayload::Subtitle { codec, payload } => {
                    let buf = wrap_heap_byte_buffer(env, payload.as_slice())?;
                    let codec_obj =
                        enum_const(env, "mpegts", "SubtitleCodec", subtitle_codec_name(*codec))?;
                    env.new_object(
                        "org/tstrans/mpegts/DemuxEvent$Subtitle",
                        "(Lorg/tstrans/mpegts/StreamId;JLjava/lang/Long;Lorg/tstrans/mpegts/SubtitleCodec;Ljava/nio/ByteBuffer;)V",
                        &[
                            JValue::Object(&stream_obj),
                            JValue::Long(pts_ticks),
                            JValue::Object(&dts_obj),
                            JValue::Object(&codec_obj),
                            JValue::Object(&buf),
                        ],
                    )
                    .map_err(|_| ())?
                }
                SamplePayload::Unknown { stream_type, raw } => {
                    let buf = wrap_heap_byte_buffer(env, raw.as_slice())?;
                    env.new_object(
                        "org/tstrans/mpegts/DemuxEvent$UnknownSample",
                        "(Lorg/tstrans/mpegts/StreamId;JLjava/lang/Long;ILjava/nio/ByteBuffer;)V",
                        &[
                            JValue::Object(&stream_obj),
                            JValue::Long(pts_ticks),
                            JValue::Object(&dts_obj),
                            JValue::Int(stream_type.as_byte() as i32),
                            JValue::Object(&buf),
                        ],
                    )
                    .map_err(|_| ())?
                }
            };
            Ok(Some(obj))
        }
        DemuxEvent::Metadata {
            stream,
            pts,
            kind,
            payload,
        } => {
            let stream_obj = build_stream_id(env, stream)?;
            let (kind_obj, was_reassembled, cell_count) = metadata_kind(env, kind)?;
            // Raw KLV LS bytes (AU-cell header already stripped). Heap-copied,
            // JVM-owned (same safety story as the sample records).
            let buf = wrap_heap_byte_buffer(env, payload)?;
            let obj = env
                .new_object(
                    "org/tstrans/mpegts/DemuxEvent$Metadata",
                    "(Lorg/tstrans/mpegts/StreamId;JLorg/tstrans/mpegts/MetadataKind;Ljava/nio/ByteBuffer;ZI)V",
                    &[
                        JValue::Object(&stream_obj),
                        JValue::Long(pts.as_ticks()),
                        JValue::Object(&kind_obj),
                        JValue::Object(&buf),
                        JValue::Bool(was_reassembled as u8),
                        JValue::Int(cell_count as i32),
                    ],
                )
                .map_err(|_| ())?;
            Ok(Some(obj))
        }
        DemuxEvent::Discontinuity { stream, kind } => {
            let stream_obj = build_stream_id(env, stream)?;
            let kind_obj = discontinuity_kind(env, kind)?;
            let obj = env
                .new_object(
                    "org/tstrans/mpegts/DemuxEvent$Discontinuity",
                    "(Lorg/tstrans/mpegts/StreamId;Lorg/tstrans/mpegts/DiscontinuityKind;)V",
                    &[JValue::Object(&stream_obj), JValue::Object(&kind_obj)],
                )
                .map_err(|_| ())?;
            Ok(Some(obj))
        }
        DemuxEvent::NonConformant { stream, issue } => {
            let stream_obj = build_stream_id(env, stream)?;
            // The human-readable detail (Rust `NonConformantIssue`'s `Display`).
            let issue_str = env.new_string(issue.to_string()).map_err(|_| ())?;
            let kind_obj = nonconformant_kind(env, issue)?;
            // MultiCellAuReason constant (MULTI_CELL_AU only) or Java null.
            let reason_obj = nonconformant_reason(env, issue)?;
            // (observedCfi, treatedAs) CFI constants (CFI_TOLERATED only) or (null, null).
            let (observed_obj, treated_obj) = nonconformant_cfi(env, issue)?;
            let obj = env
                .new_object(
                    "org/tstrans/mpegts/DemuxEvent$NonConformant",
                    "(Lorg/tstrans/mpegts/StreamId;Ljava/lang/String;Lorg/tstrans/mpegts/NonConformantKind;Lorg/tstrans/mpegts/MultiCellAuReason;Lorg/tstrans/mpegts/CellFragmentIndication;Lorg/tstrans/mpegts/CellFragmentIndication;)V",
                    &[
                        JValue::Object(&stream_obj),
                        JValue::Object(&issue_str),
                        JValue::Object(&kind_obj),
                        JValue::Object(&reason_obj),
                        JValue::Object(&observed_obj),
                        JValue::Object(&treated_obj),
                    ],
                )
                .map_err(|_| ())?;
            Ok(Some(obj))
        }
        DemuxEvent::ReconnectDiscontinuity => {
            let obj = env
                .new_object(
                    "org/tstrans/mpegts/DemuxEvent$ReconnectDiscontinuity",
                    "()V",
                    &[],
                )
                .map_err(|_| ())?;
            Ok(Some(obj))
        }
    }
}

/// Resolve a `tst_core` [`DiscontinuityKind`] to its Java
/// `org.tstrans.mpegts.DiscontinuityKind` enum constant. Mirrors tst-py's
/// `DiscontinuityKindTag` mapping. [`DiscontinuityKind`] is not marked
/// non-exhaustive, so this match is exhaustive (no catch-all).
fn discontinuity_kind<'local>(
    env: &mut JNIEnv<'local>,
    kind: &DiscontinuityKind,
) -> Result<JObject<'local>, ()> {
    let name = match kind {
        DiscontinuityKind::ContinuityJump { .. } => "CONTINUITY_JUMP",
        DiscontinuityKind::PesOversize { .. } => "PES_OVERSIZE",
        DiscontinuityKind::PesTotalOversize => "PES_TOTAL_OVERSIZE",
        DiscontinuityKind::AdaptationFieldFlag => "ADAPTATION_FIELD_FLAG",
    };
    enum_const(env, "mpegts", "DiscontinuityKind", name)
}

/// Resolve the `org.tstrans.mpegts.NonConformantKind` enum constant for a
/// `tst_core` [`NonConformantIssue`]. Mirrors tst-py's `non_conformant_kind_name`
/// byte-for-byte: Rust's 30+ issue variants collapse to one of the Java
/// constants; the per-event `issue` string carries the human-readable detail.
/// [`NonConformantIssue`] is not marked non-exhaustive, so this match is
/// exhaustive (no catch-all) — a new Rust variant breaks the build here.
fn nonconformant_kind<'local>(
    env: &mut JNIEnv<'local>,
    issue: &NonConformantIssue,
) -> Result<JObject<'local>, ()> {
    use NonConformantIssue::*;
    let name = match issue {
        StreamTypeMismatchSyncOnAsyncPid | StreamTypeMismatchAsyncOnSyncPid => {
            "STREAM_TYPE_MISMATCH"
        }
        MissingMetadataDescriptor => "MISSING_METADATA_DESCRIPTOR",
        PcrAnomaly { .. } => "PCR_ANOMALY",
        PsiChecksumMismatch { .. } => "PSI_CHECKSUM_MISMATCH",
        PusiMidPes => "PUSI_MID_PES",
        MalformedPes { .. } => "MALFORMED_PES",
        PidReusedAcrossPrograms { .. } => "PID_REUSED_ACROSS_PROGRAMS",
        SubtitleMissingDescriptor { .. } => "SUBTITLE_MISSING_DESCRIPTOR",
        SubtitleDescriptorAmbiguous { .. } => "SUBTITLE_DESCRIPTOR_AMBIGUOUS",
        SubtitleDescriptorMalformed { .. } => "SUBTITLE_DESCRIPTOR_MALFORMED",
        Av1RegistrationMalformed { .. } => "AV1_REGISTRATION_MALFORMED",
        Av1ObuMissingSizeField { .. } => "AV1_OBU_MISSING_SIZE_FIELD",
        Av1TileListNotAllowed { .. } => "AV1_TILE_LIST_NOT_ALLOWED",
        PsiOverlongSection { .. } => "PSI_OVERLONG_SECTION",
        TransportErrorPacket { .. } => "TRANSPORT_ERROR_PACKET",
        DvbSubDataIdentifier { .. } => "DVB_SUB_DATA_IDENTIFIER",
        PtsAnomaly { .. } => "PTS_ANOMALY",
        MissingRequiredPts { .. } => "MISSING_REQUIRED_PTS",
        PesHeaderMalformed { .. } => "PES_HEADER_MALFORMED",
        SubtitleAlignmentMissing { .. } => "SUBTITLE_ALIGNMENT_MISSING",
        PcrMalformed { .. } => "PCR_MALFORMED",
        NalHeader { .. } => "NAL_HEADER",
        Av1ObuHeader { .. } => "AV1_OBU_HEADER",
        LatmFraming { .. } => "LATM_FRAMING",
        PsiCcDiscontinuity { .. } => "PSI_CC_DISCONTINUITY",
        MultiCellAu { .. } => "MULTI_CELL_AU",
        CfiTolerated { .. } => "CFI_TOLERATED",
        PsiMultiSectionUnsupported { .. } => "PSI_MULTI_SECTION_UNSUPPORTED",
        Ac3SyncMissing { .. } => "AC3_SYNC_MISSING",
        Av1WrongStreamId { .. } => "AV1_WRONG_STREAM_ID",
        Av1MissingTsObuFraming { .. } => "AV1_MISSING_TS_OBU_FRAMING",
        PmtProgramNumberMismatch { .. } => "PMT_PROGRAM_NUMBER_MISMATCH",
        UnsupportedScrambling { .. } => "UNSUPPORTED_SCRAMBLING",
        AdaptationFieldMalformed { .. } => "ADAPTATION_FIELD_MALFORMED",
        ZeroLengthPesNonVideo { .. } => "ZERO_LENGTH_PES_NON_VIDEO",
        PsiSyntax { .. } => "PSI_SYNTAX",
        Other(_) => "OTHER",
    };
    enum_const(env, "mpegts", "NonConformantKind", name)
}

/// The `org.tstrans.mpegts.MultiCellAuReason` constant for a `MultiCellAu` issue,
/// or Java `null` for every other issue kind. Mirrors tst-py: only `MultiCellAu`
/// surfaces a typed reason.
fn nonconformant_reason<'local>(
    env: &mut JNIEnv<'local>,
    issue: &NonConformantIssue,
) -> Result<JObject<'local>, ()> {
    match issue {
        NonConformantIssue::MultiCellAu { reason, .. } => {
            let name = match reason {
                MultiCellAuReason::Orphan => "ORPHAN",
                MultiCellAuReason::SequenceGap => "SEQUENCE_GAP",
                MultiCellAuReason::ConcurrentFirst => "CONCURRENT_FIRST",
                MultiCellAuReason::Overflow => "OVERFLOW",
                MultiCellAuReason::OverflowTotal => "OVERFLOW_TOTAL",
                MultiCellAuReason::TooManyPids => "TOO_MANY_PIDS",
                // MultiCellAuReason is marked non-exhaustive; default to ORPHAN
                // like tst-py for any future variant.
                _ => "ORPHAN",
            };
            enum_const(env, "mpegts", "MultiCellAuReason", name)
        }
        _ => Ok(JObject::null()),
    }
}

/// The `(observedCfi, treatedAs)` pair of `org.tstrans.mpegts.CellFragmentIndication`
/// constants for a `CfiTolerated` issue, or `(null, null)` for every other issue
/// kind. Mirrors tst-py: only `CfiTolerated` surfaces the typed CFI bits.
fn nonconformant_cfi<'local>(
    env: &mut JNIEnv<'local>,
    issue: &NonConformantIssue,
) -> Result<(JObject<'local>, JObject<'local>), ()> {
    match issue {
        NonConformantIssue::CfiTolerated {
            observed_cfi,
            treated_as,
            ..
        } => Ok((cfi_const(env, *observed_cfi)?, cfi_const(env, *treated_as)?)),
        _ => Ok((JObject::null(), JObject::null())),
    }
}

/// Resolve the `org.tstrans.mpegts.CellFragmentIndication` constant for a
/// `tst_core` [`CellFragmentIndication`]. The enum is not marked non-exhaustive,
/// so this match is exhaustive (no catch-all).
fn cfi_const<'local>(
    env: &mut JNIEnv<'local>,
    cfi: CellFragmentIndication,
) -> Result<JObject<'local>, ()> {
    let name = match cfi {
        CellFragmentIndication::Middle => "MIDDLE",
        CellFragmentIndication::Last => "LAST",
        CellFragmentIndication::First => "FIRST",
        CellFragmentIndication::Complete => "COMPLETE",
    };
    enum_const(env, "mpegts", "CellFragmentIndication", name)
}

/// Resolve a `tst_core` [`MetadataKind`] to its Java `org.tstrans.mpegts.MetadataKind`
/// enum constant, along with the `(was_reassembled, cell_count)` pair carried on the
/// `DemuxEvent.Metadata` record. Mirrors tst-py's `convert` for the metadata event:
/// async / unknown collapse to `(false, 1)`.
pub(crate) fn metadata_kind<'local>(
    env: &mut JNIEnv<'local>,
    kind: &MetadataKind,
) -> Result<(JObject<'local>, bool, u32), ()> {
    let (name, wr, cc) = match kind {
        MetadataKind::KlvSyncAuCell {
            was_reassembled,
            cell_count,
            ..
        } => ("KLV_SYNC_AU_CELL", *was_reassembled, *cell_count),
        MetadataKind::KlvAsync => ("KLV_ASYNC", false, 1),
        MetadataKind::Unknown(_) => ("UNKNOWN", false, 1),
    };
    let obj = env
        .get_static_field(
            "org/tstrans/mpegts/MetadataKind",
            name,
            "Lorg/tstrans/mpegts/MetadataKind;",
        )
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())?;
    Ok((obj, wr, cc))
}

/// Box an `Option<Pts90khz>` as a `java.lang.Long` (`Long.valueOf`) or Java
/// `null`. Used for the nullable `dts` field of every sample record.
pub(crate) fn opt_long<'local>(
    env: &mut JNIEnv<'local>,
    v: Option<Pts90khz>,
) -> Result<JObject<'local>, ()> {
    match v {
        Some(p) => env
            .call_static_method(
                "java/lang/Long",
                "valueOf",
                "(J)Ljava/lang/Long;",
                &[JValue::Long(p.as_ticks())],
            )
            .map_err(|_| ())?
            .l()
            .map_err(|_| ()),
        None => Ok(JObject::null()),
    }
}

/// Copy `bytes` into a fresh Java `byte[]` and wrap it as a heap `ByteBuffer`
/// (`java.nio.ByteBuffer.wrap`). The returned buffer is backed by JVM-owned
/// memory, so it is safe to retain past the next pull / after `close()`.
pub(crate) fn wrap_heap_byte_buffer<'local>(
    env: &mut JNIEnv<'local>,
    bytes: &[u8],
) -> Result<JObject<'local>, ()> {
    let arr = env.byte_array_from_slice(bytes).map_err(|_| ())?;
    env.call_static_method(
        "java/nio/ByteBuffer",
        "wrap",
        "([B)Ljava/nio/ByteBuffer;",
        &[JValue::Object(&arr)],
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build a `java.util.List<Integer>` (an `ArrayList`) of boxed PIDs.
fn build_pid_list<'local>(
    env: &mut JNIEnv<'local>,
    pids: impl Iterator<Item = u16>,
) -> Result<JObject<'local>, ()> {
    let list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    // Forward-note: this loop mints per-iteration local refs (the boxed
    // Integer + the static-method result). Fine here — PMT stream counts are
    // tiny and bounded. If this List-building shape is ever reused over an
    // unbounded per-event collection, wrap the body in `env.with_local_frame(..)`
    // (or delete refs per iteration) to avoid local-ref-table overflow.
    for pid in pids {
        let boxed = env
            .call_static_method(
                "java/lang/Integer",
                "valueOf",
                "(I)Ljava/lang/Integer;",
                &[JValue::Int(pid as i32)],
            )
            .map_err(|_| ())?
            .l()
            .map_err(|_| ())?;
        env.call_method(
            &list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[JValue::Object(&boxed)],
        )
        .map_err(|_| ())?;
    }
    Ok(list)
}

/// Build a `java.util.List<VideoUnit>` from a `VideoPayload`: `List<NalUnit>`
/// for NAL-shaped codecs (H.264/H.265/H.266), `List<Obu>` for AV1. Each unit is
/// constructed inside a per-element local frame so its refs are reclaimed —
/// AU unit counts are unbounded, so a flat loop would risk local-ref-table
/// overflow. Mirrors tst-py's `convert_sample_event` video arm.
pub(crate) fn build_video_units<'local>(
    env: &mut JNIEnv<'local>,
    payload: &VideoPayload,
) -> Result<JObject<'local>, ()> {
    let list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    match payload {
        VideoPayload::Nals(nals) => {
            for nal in nals {
                env.with_local_frame(16, |inner| {
                    let val = build_nal_unit(inner, nal)
                        .map_err(|()| jni::errors::Error::JavaException)?;
                    inner.call_method(
                        &list,
                        "add",
                        "(Ljava/lang/Object;)Z",
                        &[JValue::Object(&val)],
                    )?;
                    Ok::<(), jni::errors::Error>(())
                })
                .map_err(|_| ())?;
            }
        }
        VideoPayload::Obus(obus) => {
            for obu in obus {
                env.with_local_frame(16, |inner| {
                    let val =
                        build_obu(inner, obu).map_err(|()| jni::errors::Error::JavaException)?;
                    inner.call_method(
                        &list,
                        "add",
                        "(Ljava/lang/Object;)Z",
                        &[JValue::Object(&val)],
                    )?;
                    Ok::<(), jni::errors::Error>(())
                })
                .map_err(|_| ())?;
            }
        }
    }
    Ok(list)
}

/// Build a `java.util.ArrayList<AdtsFrame>` from owned AAC frames; each frame is
/// constructed inside a per-element local frame so its refs are reclaimed
/// (unbounded frame counts per AU). Reuses [`build_adts_frame`] — the same
/// frame→jobject construction the eager audio path used. Backs
/// [`DemuxEvent.Audio.parse()`] for AAC.
fn build_adts_frame_list<'local>(
    env: &mut JNIEnv<'local>,
    frames: &[tst_core::codec::aac::AdtsFrameOwned],
) -> Result<JObject<'local>, ()> {
    let list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for f in frames {
        env.with_local_frame(24, |inner| {
            let val = build_adts_frame(inner, f).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }
    Ok(list)
}

/// Build a `java.util.ArrayList<Mpeg2AudioFrame>` from owned MP2 frames. The MP2
/// twin of [`build_adts_frame_list`]; reuses [`build_mpeg2_audio_frame`]. Backs
/// [`DemuxEvent.Audio.parse()`] for MP2.
fn build_mpeg2_frame_list<'local>(
    env: &mut JNIEnv<'local>,
    frames: &[tst_core::codec::mpegaudio::FrameOwned],
) -> Result<JObject<'local>, ()> {
    let list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for f in frames {
        env.with_local_frame(24, |inner| {
            let val = build_mpeg2_audio_frame(inner, f)
                .map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }
    Ok(list)
}

/// Build the Java `org.tstrans.mpegts.StreamId` record from a `tst_core`
/// [`StreamId`], constructing its nested [`StreamKind`] via [`build_stream_kind`].
pub(crate) fn build_stream_id<'local>(
    env: &mut JNIEnv<'local>,
    s: &StreamId,
) -> Result<JObject<'local>, ()> {
    let kind = build_stream_kind(env, &s.kind)?;
    env.new_object(
        "org/tstrans/mpegts/StreamId",
        "(ILorg/tstrans/mpegts/StreamKind;I)V",
        &[
            JValue::Int(s.pid as i32),
            JValue::Object(&kind),
            JValue::Int(s.program_number as i32),
        ],
    )
    .map_err(|_| ())
}

/// Build the sealed `org.tstrans.mpegts.StreamKind` variant matching a `tst_core`
/// [`StreamKind`]. Each arm news up the corresponding nested record (JNI class
/// names use `$`). `KlvSync`'s `declared_link: Option<u16>` boxes to a
/// `java.lang.Integer` or Java `null`.
fn build_stream_kind<'local>(
    env: &mut JNIEnv<'local>,
    kind: &StreamKind,
) -> Result<JObject<'local>, ()> {
    match kind {
        StreamKind::Video(c) => {
            let codec = enum_const(env, "mpegts", "VideoCodec", video_codec_name(*c))?;
            env.new_object(
                "org/tstrans/mpegts/StreamKind$Video",
                "(Lorg/tstrans/mpegts/VideoCodec;)V",
                &[JValue::Object(&codec)],
            )
            .map_err(|_| ())
        }
        StreamKind::Audio(c) => {
            let codec = enum_const(env, "mpegts", "AudioCodec", audio_codec_name(*c))?;
            env.new_object(
                "org/tstrans/mpegts/StreamKind$Audio",
                "(Lorg/tstrans/mpegts/AudioCodec;)V",
                &[JValue::Object(&codec)],
            )
            .map_err(|_| ())
        }
        StreamKind::Subtitle(c) => {
            let codec = enum_const(env, "mpegts", "SubtitleCodec", subtitle_codec_name(*c))?;
            env.new_object(
                "org/tstrans/mpegts/StreamKind$Subtitle",
                "(Lorg/tstrans/mpegts/SubtitleCodec;)V",
                &[JValue::Object(&codec)],
            )
            .map_err(|_| ())
        }
        StreamKind::KlvSync { declared_link } => {
            let boxed = opt_boxed_int(env, *declared_link)?;
            env.new_object(
                "org/tstrans/mpegts/StreamKind$KlvSync",
                "(Ljava/lang/Integer;)V",
                &[JValue::Object(&boxed)],
            )
            .map_err(|_| ())
        }
        StreamKind::KlvAsync => env
            .new_object("org/tstrans/mpegts/StreamKind$KlvAsync", "()V", &[])
            .map_err(|_| ()),
        StreamKind::Unknown(b) => env
            .new_object(
                "org/tstrans/mpegts/StreamKind$Unknown",
                "(I)V",
                &[JValue::Int(*b as i32)],
            )
            .map_err(|_| ()),
    }
}

/// Box an `Option<u16>` as a `java.lang.Integer` (`Integer.valueOf`) or Java
/// `null`. Used for `StreamKind$KlvSync`'s nullable `declaredLink`.
fn opt_boxed_int<'local>(
    env: &mut JNIEnv<'local>,
    value: Option<u16>,
) -> Result<JObject<'local>, ()> {
    match value {
        Some(v) => env
            .call_static_method(
                "java/lang/Integer",
                "valueOf",
                "(I)Ljava/lang/Integer;",
                &[JValue::Int(v as i32)],
            )
            .map_err(|_| ())?
            .l()
            .map_err(|_| ()),
        None => Ok(JObject::null()),
    }
}

/// `VideoCodec` enum-constant name in `org.tstrans.mpegts.VideoCodec`.
pub(crate) fn video_codec_name(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "H264",
        VideoCodec::H265 => "H265",
        VideoCodec::H266 => "H266",
        VideoCodec::Av1 => "AV1",
    }
}

/// `AudioCodec` enum-constant name in `org.tstrans.mpegts.AudioCodec`.
fn audio_codec_name(c: AudioCodec) -> &'static str {
    match c {
        AudioCodec::Mp2 => "MP2",
        AudioCodec::Aac => "AAC",
        AudioCodec::AacLatm => "AAC_LATM",
        AudioCodec::Ac3 => "AC3",
    }
}

/// `SubtitleCodec` enum-constant name in `org.tstrans.mpegts.SubtitleCodec`.
fn subtitle_codec_name(c: SubtitleCodec) -> &'static str {
    match c {
        SubtitleCodec::DvbSubtitling => "DVB_SUBTITLING",
        SubtitleCodec::DvbTeletext => "DVB_TELETEXT",
        SubtitleCodec::Cea708Standalone => "CEA708_STANDALONE",
        SubtitleCodec::WebVttInTs => "WEBVTT_IN_TS",
    }
}

/// `Av1CarriageMode` enum-constant name in `org.tstrans.mpegts.Av1CarriageMode`.
/// `Av1CarriageMode` is non-exhaustive; unknown future variants fall back to
/// `MPEG2_TS_BINDING` (the default binding mode) since the demuxer only ever
/// sets the two real variants today.
fn av1_carriage_name(mode: Av1CarriageMode) -> &'static str {
    match mode {
        Av1CarriageMode::Mpeg2TsBinding => "MPEG2_TS_BINDING",
        Av1CarriageMode::InteropRawObu => "INTEROP_RAW_OBU",
        _ => "MPEG2_TS_BINDING",
    }
}

/// Build an `org.tstrans.mpegts.MuxerStats` record from the projected pipeline
/// counters. Ctor sig `(JJJJ)V`. `subtitle_streams_configured` is not tracked by
/// the pipeline shell — default it to 0 (mirrors tst-py). Shared by both the
/// srt and rtp `MuxSender`/`DemuxReceiver`/managed-convenience JNI surfaces.
pub(crate) fn build_muxer_stats<'local>(
    env: &mut JNIEnv<'local>,
    ts_packets_emitted: i64,
    ts_bytes_emitted: i64,
    programs_configured: i64,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    env.new_object(
        "org/tstrans/mpegts/MuxerStats",
        "(JJJJ)V",
        &[
            JValue::Long(ts_packets_emitted),
            JValue::Long(ts_bytes_emitted),
            JValue::Long(programs_configured),
            JValue::Long(0),
        ],
    )
}
