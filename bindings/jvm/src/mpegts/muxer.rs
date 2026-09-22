//! JNI surface for `org.tstrans.mpegts.Muxer` — the offline byte-feeding muxer.
//!
//! Wraps `tst_core::mpegts::mux::Muxer`. `nOpen` marshals the parallel-array
//! config description (one entry per elementary stream, decoded by Java enum
//! ORDINAL) into a [`MuxerConfig`] via the `tst_core` builders, boxes the
//! [`Muxer`], and returns the raw pointer as a `jlong` handle. The push family
//! (`nPushVideo` / `nPushVideoWire` / `nPushKlv` / `nPushAudio` / `nPushSubtitle` /
//! `nPushData` / `nPushDataTo` / `nPushVideoTo` / `nPushVideoWireTo` /
//! `nPushVideoToWithDts` / `nPushVideoWireToWithDts` /
//! `nPushVideoMispTo` / `nPushVideoMispToWithDts` /
//! `nPushKlvTo` / `nPushAudioTo` / `nPushSubtitleTo`) reads the Java `byte[]`,
//! calls the matching `Muxer::push_*`,
//! and maps any [`MuxError`] to a thrown `org.tstrans.MuxException` via
//! [`throw_mux_error`]. `nDataHandles` returns the configured data-stream
//! handles as a `long[]` of packed raws. `nPull` drains TS
//! packets into the caller's `byte[]`; `nPending` / `nCapacity` report queue
//! depth; `nClose` reconstitutes + drops the box.
//!
//! The `MuxError` → `MuxException.Kind` mapping mirrors tst-py's
//! `mux_error_to_pyerr` decision-for-decision: it routes via the 5-variant
//! [`MuxErrorKind`] (`MuxError::kind()`), NOT a per-variant inline match.
//!
//! Handle convention: the `jlong` is an opaque key into a per-type
//! [`crate::handle::HandleRegistry`] over the [`Muxer`]; per-call methods lease
//! via `REGISTRY.with` (mapping a closed/absent handle to a thrown
//! `IllegalStateException`), and `nClose` takes + drops via `REGISTRY.close`
//! (atomic + idempotent, so a double `close()` is UAF/double-free-safe). All JNI
//! array-read failures fail closed (a thrown `MuxException`/`RuntimeException` +
//! a Rust default), never an `.unwrap()` panic across the FFI boundary.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::{JBooleanArray, JByteArray, JClass, JIntArray, JLongArray, JObject};
use jni::sys::{jboolean, jint, jlong};

use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioCodec, AudioStreamHandle, Av1CarriageMode, DataStreamHandle, KlvStreamHandle,
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, StreamKind, SubtitleCodec,
    SubtitleStreamHandle, VideoCodec, VideoStreamHandle,
};

use crate::error::throw_mux;
use crate::handle::HandleRegistry;
use crate::jutil::decode_stream_handle;
use tst_pipeline::binding::BindingErrorKind;
use tst_pipeline::binding::kind::kind_of_mux;

/// Per-type leased-handle registry for `org.tstrans.mpegts.Muxer`.
static REGISTRY: LazyLock<HandleRegistry<Muxer>> = LazyLock::new(HandleRegistry::new);

/// `org.tstrans.mpegts.Muxer.nOpen(...)` — build a configured [`Muxer`] from the
/// parallel-array config description and hand the JVM its raw pointer as a
/// `jlong` handle. Returns `0` (with a pending `MuxException`) on any config or
/// marshalling failure.
///
/// The per-stream arrays (`stream_pids` / `stream_kinds` / `stream_codecs` /
/// `stream_type_codes` / `stream_carries_pts`) are decoded by Java enum
/// ORDINAL (`stream_type_codes` is the raw PMT stream_type byte for kind=data)
/// — see the `*_codec` / `klv_type` / `av1_mode` mappers below for the exact
/// ordinal → Rust-enum contract.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nOpen<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
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
        // Build the MuxerConfig from the parallel arrays via the shared helper (a
        // pending MuxException is already thrown on `Err(())`).
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
        let mux = match Muxer::new(cfg) {
            Ok(m) => m,
            Err(e) => {
                throw_mux_error(env, &e);
                return 0;
            }
        };
        REGISTRY.insert(mux) as jlong
    })
}

/// Build a [`MuxerConfig`] from the parallel-array config description that both
/// `Muxer::nOpen` and the srt `MuxSender` / `Socket::intoMuxSender` JNI paths
/// marshal in a single call. The per-stream arrays (`stream_pids` /
/// `stream_kinds` / `stream_codecs` / `stream_type_codes` /
/// `stream_carries_pts`) are decoded by Java enum ORDINAL
/// (`stream_type_codes` doubles as the raw PMT stream_type byte for kind=data)
/// — see the `*_codec` / `klv_type` / `av1_mode` mappers for the exact
/// ordinal → Rust-enum contract. `data_desc_bytes` / `data_desc_lens` carry the
/// per-data-stream PMT descriptor loops: the blob is every descriptor TLV
/// concatenated in stream order, the lens array (one entry per stream) is each
/// stream's byte count within it (0 for non-data / descriptor-less).
///
/// On ANY config or marshalling failure this throws the matching
/// `MuxException` (or `RuntimeException` for a raw JNI array-read error) and
/// returns `Err(())`; the caller maps that to its own `0` / null return so the
/// pending Java exception propagates at the call site. Byte-exactness with the
/// pre-refactor inline `nOpen` body is load-bearing for the
/// `MuxRoundtripScenarioTest` golden.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_muxer_config_from_arrays<'local>(
    env: &mut JNIEnv<'local>,
    program_number: jint,
    pmt_pid: jint,
    pcr_pid: jint,
    pcr_interval_ms: jint,
    psi_interval_ms: jint,
    buffer_packets: jint,
    av1_carriage: jint,
    stream_pids: &JIntArray<'local>,
    stream_kinds: &JIntArray<'local>,
    stream_codecs: &JIntArray<'local>,
    stream_type_codes: &JIntArray<'local>,
    stream_carries_pts: &JBooleanArray<'local>,
    data_desc_bytes: &JByteArray<'local>,
    data_desc_lens: &JIntArray<'local>,
) -> Result<MuxerConfig, ()> {
    // Read the parallel arrays. On ANY JNI read error, fail closed (throw
    // INTERNAL, return Err) rather than panic across the FFI boundary. `n` is the
    // stream count, taken from `stream_pids`.
    let pids = read_int_array(env, stream_pids).ok_or(())?;
    let n = pids.len();
    let kinds = read_int_array(env, stream_kinds).ok_or(())?;
    let codecs = read_int_array(env, stream_codecs).ok_or(())?;
    let type_codes = read_int_array(env, stream_type_codes).ok_or(())?;
    let carries = read_boolean_array(env, stream_carries_pts).ok_or(())?;
    let desc_blob = match env.convert_byte_array(data_desc_bytes) {
        Ok(b) => b,
        Err(_) => {
            throw_mux(
                env,
                BindingErrorKind::Internal,
                "failed to read byte[] argument",
            );
            return Err(());
        }
    };
    let desc_lens = read_int_array(env, data_desc_lens).ok_or(())?;
    if kinds.len() != n || codecs.len() != n || type_codes.len() != n || carries.len() != n {
        throw_mux(
            env,
            BindingErrorKind::Internal,
            "stream sibling-array length mismatch",
        );
        return Err(());
    }
    if desc_lens.len() != n {
        throw_mux(
            env,
            BindingErrorKind::Internal,
            "dataDescLens length mismatch",
        );
        return Err(());
    }

    let mut prog = MuxerProgramConfigBuilder::new(program_number as u16, pmt_pid as u16);
    let mut desc_off = 0usize;
    let mut data_idx = 0usize;
    for i in 0..n {
        let pid = pids[i] as u16;
        // stream kind code: 0=video, 1=klv, 2=audio, 3=subtitle, 4=data.
        match kinds[i] {
            0 => {
                let Some(codec) = video_codec(env, codecs[i]) else {
                    return Err(());
                };
                prog.add_video(pid, codec);
            }
            1 => {
                let Some(ktype) = klv_type(env, type_codes[i]) else {
                    return Err(());
                };
                prog.add_klv(pid, ktype, carries[i] != 0);
            }
            2 => {
                let Some(codec) = audio_codec(env, codecs[i]) else {
                    return Err(());
                };
                prog.add_audio(pid, codec);
            }
            3 => {
                // Subtitle ordinals 2/3 only (CEA-708 / WebVTT). Ordinals 0/1 are
                // the DVB codecs the Java builder already rejects (they need config
                // not exposed in the JVM binding); reject defensively if one slips
                // through.
                match codecs[i] {
                    2 => {
                        prog.add_subtitle(pid, SubtitleCodec::Cea708Standalone);
                    }
                    3 => {
                        prog.add_subtitle(pid, SubtitleCodec::WebVttInTs);
                    }
                    _ => {
                        throw_mux(
                            env,
                            BindingErrorKind::ConfigInvalid,
                            "DVB subtitle codecs need config not exposed in the JVM binding",
                        );
                        return Err(());
                    }
                }
            }
            4 => {
                prog.add_data(pid, type_codes[i] as u8, carries[i] != 0);
                // Fail closed on a negative length (a sign-cast would turn it
                // into a huge usize) and on offset overflow — never panic
                // across the FFI boundary, in any build profile.
                let Ok(dl) = usize::try_from(desc_lens[i]) else {
                    throw_mux(
                        env,
                        BindingErrorKind::Internal,
                        "negative data-stream descriptor length",
                    );
                    return Err(());
                };
                if dl > 0 {
                    let Some(end) = desc_off.checked_add(dl) else {
                        throw_mux(
                            env,
                            BindingErrorKind::Internal,
                            "data-stream descriptor offset overflow",
                        );
                        return Err(());
                    };
                    let Some(descs) = desc_blob.get(desc_off..end).and_then(split_descriptor_tlvs)
                    else {
                        throw_mux(
                            env,
                            BindingErrorKind::ConfigInvalid,
                            "malformed data-stream descriptor TLV",
                        );
                        return Err(());
                    };
                    if let Err(e) = prog.stream_descriptors_for_data(data_idx, descs) {
                        throw_mux_error(env, &e);
                        return Err(());
                    }
                    desc_off = end;
                }
                data_idx += 1;
            }
            _ => {
                throw_mux(
                    env,
                    BindingErrorKind::Internal,
                    "unknown stream kind ordinal",
                );
                return Err(());
            }
        }
    }

    // `pcr_pid < 0` means "auto-resolve" — leave the builder default; `>= 0`
    // pins the PCR PID explicitly.
    if pcr_pid >= 0 {
        prog.pcr_pid(pcr_pid as u16);
    }

    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.pcr_interval_ms(pcr_interval_ms as u32);
    b.psi_interval_ms(psi_interval_ms as u32);
    b.buffer_packets(buffer_packets as usize);
    let Some(carriage) = av1_mode(env, av1_carriage) else {
        return Err(());
    };
    b.av1_carriage(carriage);
    match b.build() {
        Ok(c) => Ok(c),
        Err(e) => {
            throw_mux_error(env, &e);
            Err(())
        }
    }
}

/// `nPushVideo(handle, nal, pts, keyFrame)` — push one Annex-B access unit.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_mux_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_video(&buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

/// `nPushVideoWire(handle, wire, pts, keyFrame)` — pass-through push of an
/// already-carried on-wire video AU. Emits `wire` verbatim — no Annex-B
/// start-code validation, no AV1 OBU re-wrapping. Mirrors the C ABI's
/// `tst_muxer_push_video_wire`: resolves the single configured video stream
/// handle via `video_handles()` and then calls `push_video_wire_to`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideoWire<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    wire: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_mux_bytes(env, &wire) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            // Single-stream resolution: same ambiguity contract as push_video /
            // C ABI's tst_muxer_push_video_wire.
            let handles = mux.video_handles();
            match handles.as_slice() {
                [h] => mux.push_video_wire_to(*h, &buf, Pts90khz::new(pts), key_frame != 0),
                _ => Err(MuxError::AmbiguousTarget {
                    kind: StreamKind::Video,
                    count: handles.len(),
                }),
            }
        });
    })
}

/// `nPushKlv(handle, klv, pts, metadataServiceId)` — push one KLV blob. The
/// muxer auto-wraps the AU-cell header for synchronous-metadata streams; the
/// caller passes raw KLV LS bytes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushKlv<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    klv: JByteArray<'local>,
    pts: jlong,
    metadata_service_id: jint,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_mux_bytes(env, &klv) else {
            return;
        };
        // `metadata_service_id` is `u8` in `Muxer::push_klv` (spec default 0x00).
        with_mux_push(env, handle, |mux| {
            mux.push_klv(&buf, Pts90khz::new(pts), metadata_service_id as u8)
        });
    })
}

/// `nPushAudio(handle, frames, pts)` — push one audio access unit.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushAudio<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    frames: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_mux_bytes(env, &frames) else {
            return;
        };
        with_mux_push(env, handle, |mux| mux.push_audio(&buf, Pts90khz::new(pts)));
    })
}

/// `nPushSubtitle(handle, pts, payload)` — push one subtitle access unit. Note
/// the `(pts, payload)` arg order on `Muxer::push_subtitle`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushSubtitle<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    pts: jlong,
    payload: JByteArray<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_mux_bytes(env, &payload) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_subtitle(Pts90khz::new(pts), &buf)
        });
    })
}

/// `nPushData(handle, data, pts)` — pass-through push onto the lone configured
/// data stream. No AU-cell wrap, no framing; one push = one PES on stream_id
/// `0xBD` (`private_stream_1`). `pts` is written into the PES header only for
/// `carries_pts` streams but always drives PSI/PCR pacing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushData<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    data: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_mux_bytes(env, &data) else {
            return;
        };
        with_mux_push(env, handle, |mux| mux.push_data(&buf, Pts90khz::new(pts)));
    })
}

/// `nPushDataTo(handle, streamHandleRaw, data, pts)` — pass-through push onto a
/// specific data stream. The raw handle is validated via
/// `DataStreamHandle::try_from_raw` (Java's `fromRaw` does no validation), so a
/// malformed (bad bit-layout) or out-of-range handle surfaces as `MuxException(INVALID_USAGE)` here.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushDataTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    data: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        // Reject any jlong outside the packed-u32 handle layout (negative, > u32,
        // or high bits set within u32) up front, rather than truncating.
        let Some(h) = decode_stream_handle(stream_handle_raw, DataStreamHandle::try_from_raw)
        else {
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid data stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &data) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_data_to(h, &buf, Pts90khz::new(pts))
        });
    })
}

/// `nPushVideoTo(handle, streamHandleRaw, nal, pts, keyFrame)` — targeted
/// Annex-B AU push. The raw stream handle is validated via
/// `VideoStreamHandle::try_from_raw`; a malformed (bad bit-layout) or out-of-range handle surfaces as
/// `MuxException(INVALID_USAGE)` here.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideoTo<'local>(
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
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid video stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_video_to(h, &buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

/// `nPushVideoWireTo(handle, streamHandleRaw, wire, pts, keyFrame)` — targeted
/// on-wire AU push (emits `wire` verbatim, no Annex-B validation or AV1
/// re-wrapping). The raw stream handle is validated via
/// `VideoStreamHandle::try_from_raw`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideoWireTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    wire: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, VideoStreamHandle::try_from_raw)
        else {
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid video stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &wire) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_video_wire_to(h, &buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

/// `nPushVideoToWithDts(handle, streamHandleRaw, nal, pts, dts, keyFrame)` —
/// targeted Annex-B AU push with an explicit decode timestamp. The raw stream
/// handle is validated via `VideoStreamHandle::try_from_raw`; a malformed
/// (bad bit-layout) or out-of-range handle surfaces as `MuxException(INVALID_USAGE)`.
/// The PES header will carry `PTS_DTS_flags = '11'`, enabling demux of a non-null
/// `DemuxEvent.Video.dts`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideoToWithDts<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    dts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, VideoStreamHandle::try_from_raw)
        else {
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid video stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_video_to_with_dts(
                h,
                &buf,
                Pts90khz::new(pts),
                Pts90khz::new(dts),
                key_frame != 0,
            )
        });
    })
}

/// `nPushVideoWireToWithDts(handle, streamHandleRaw, wire, pts, dts, keyFrame)` —
/// targeted on-wire AU push with an explicit decode timestamp. Emits `wire`
/// verbatim — no Annex-B validation or AV1 re-wrapping. The raw stream handle is
/// validated via `VideoStreamHandle::try_from_raw`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideoWireToWithDts<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    wire: JByteArray<'local>,
    pts: jlong,
    dts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, VideoStreamHandle::try_from_raw)
        else {
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid video stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &wire) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_video_wire_to_with_dts(
                h,
                &buf,
                Pts90khz::new(pts),
                Pts90khz::new(dts),
                key_frame != 0,
            )
        });
    })
}

/// `nPushVideoMispTo(handle, streamHandleRaw, nal, pts, keyFrame, kind, timeStatus, value)` —
/// targeted Annex-B AU push with a MISB ST 0604 MISP timestamp SEI spliced
/// before the first VCL NAL. `kind` is the `MispTimeKind` ordinal (0=MICRO,
/// 1=NANO); `value` is treated as unsigned 64-bit (bit-pattern reinterpret from
/// `jlong`). Out-of-range kind → `MuxException(INVALID_USAGE)`. An H.264 stream
/// with `kind=NANO` → `MuxException(INPUT_MALFORMED)` via the Rust `MispTimeError`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideoMispTo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
    kind: jint,
    time_status: jint,
    value: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, VideoStreamHandle::try_from_raw)
        else {
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid video stream handle",
            );
            return;
        };
        let misp = match build_misp(env, kind, time_status, value) {
            Some(m) => m,
            None => return,
        };
        let Some(buf) = read_mux_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_video_misp_to(h, &buf, Pts90khz::new(pts), key_frame != 0, &misp)
        });
    })
}

/// `nPushVideoMispToWithDts(handle, streamHandleRaw, nal, pts, dts, keyFrame, kind, timeStatus, value)` —
/// targeted Annex-B AU push with explicit DTS and MISP SEI splice.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushVideoMispToWithDts<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    stream_handle_raw: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    dts: jlong,
    key_frame: jboolean,
    kind: jint,
    time_status: jint,
    value: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(h) = decode_stream_handle(stream_handle_raw, VideoStreamHandle::try_from_raw)
        else {
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid video stream handle",
            );
            return;
        };
        let misp = match build_misp(env, kind, time_status, value) {
            Some(m) => m,
            None => return,
        };
        let Some(buf) = read_mux_bytes(env, &nal) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_video_misp_to_with_dts(
                h,
                &buf,
                Pts90khz::new(pts),
                Pts90khz::new(dts),
                key_frame != 0,
                &misp,
            )
        });
    })
}

/// `nPushKlvTo(handle, streamHandleRaw, klv, pts, metadataServiceId)` —
/// targeted KLV push. The raw stream handle is validated via
/// `KlvStreamHandle::try_from_raw`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushKlvTo<'local>(
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
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid klv stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &klv) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_klv_to(h, &buf, Pts90khz::new(pts), metadata_service_id as u8)
        });
    })
}

/// `nPushAudioTo(handle, streamHandleRaw, frames, pts)` — targeted audio
/// push. The raw stream handle is validated via
/// `AudioStreamHandle::try_from_raw`. Note the core's `push_audio_to` takes
/// `(handle, pts, frames)` — the JNI argument order (frames before pts) matches
/// the Java-facing API convention; the core call re-orders them.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushAudioTo<'local>(
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
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid audio stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &frames) else {
            return;
        };
        // Core: push_audio_to(handle, pts, frames) — pts before frames.
        with_mux_push(env, handle, |mux| {
            mux.push_audio_to(h, Pts90khz::new(pts), &buf)
        });
    })
}

/// `nPushSubtitleTo(handle, streamHandleRaw, pts, payload)` — targeted
/// subtitle push. The raw stream handle is validated via
/// `SubtitleStreamHandle::try_from_raw`. Note the `(pts, payload)` arg order
/// matches the core's `push_subtitle_to`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPushSubtitleTo<'local>(
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
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "invalid subtitle stream handle",
            );
            return;
        };
        let Some(buf) = read_mux_bytes(env, &payload) else {
            return;
        };
        with_mux_push(env, handle, |mux| {
            mux.push_subtitle_to(h, Pts90khz::new(pts), &buf)
        });
    })
}

/// `nDataHandles(handle)` → `long[]` of all data stream handles (packed `u32`
/// raws widened to `jlong`), in `addData` declaration order. On a closed/absent
/// handle throws `IllegalStateException` and returns a null array; on a JNI
/// alloc/write failure throws an unchecked `RuntimeException` (the Java decl
/// has no `throws`, so the checked `MuxException` can't be used here) and
/// returns a null array.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nDataHandles<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JLongArray<'local> {
    crate::panic::jni_catch(&mut env, JObject::null().into(), |env| {
        let Some(raws) = REGISTRY.with(handle as u64, |mux| {
            mux.data_handles()
                .into_iter()
                .map(|h| i64::from(h.raw()))
                .collect::<Vec<i64>>()
        }) else {
            closed(env);
            return JObject::null().into();
        };
        let arr = match env.new_long_array(raws.len() as i32) {
            Ok(a) => a,
            Err(_) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    "failed to allocate long[] result",
                );
                return JObject::null().into();
            }
        };
        if env.set_long_array_region(&arr, 0, &raws).is_err() {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                "failed to write long[] result",
            );
            return JObject::null().into();
        }
        arr
    })
}

/// `nVideoHandles(handle)` → `long[]` of all video stream handles (packed `u32`
/// raws widened to `jlong`), in declaration order. On a closed/absent handle
/// throws `IllegalStateException`; on a JNI alloc/write failure throws
/// `RuntimeException`. Mirrors `nDataHandles`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nVideoHandles<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JLongArray<'local> {
    crate::panic::jni_catch(&mut env, JObject::null().into(), |env| {
        let Some(raws) = REGISTRY.with(handle as u64, |mux| {
            mux.video_handles()
                .into_iter()
                .map(|h: VideoStreamHandle| i64::from(h.raw()))
                .collect::<Vec<i64>>()
        }) else {
            closed(env);
            return JObject::null().into();
        };
        let arr = match env.new_long_array(raws.len() as i32) {
            Ok(a) => a,
            Err(_) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    "failed to allocate long[] result",
                );
                return JObject::null().into();
            }
        };
        if env.set_long_array_region(&arr, 0, &raws).is_err() {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                "failed to write long[] result",
            );
            return JObject::null().into();
        }
        arr
    })
}

/// `nAudioHandles(handle)` → `long[]` of all audio stream handles (packed `u32`
/// raws widened to `jlong`), in declaration order. On a closed/absent handle
/// throws `IllegalStateException`; on a JNI alloc/write failure throws
/// `RuntimeException`. Mirrors `nDataHandles`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nAudioHandles<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JLongArray<'local> {
    crate::panic::jni_catch(&mut env, JObject::null().into(), |env| {
        let Some(raws) = REGISTRY.with(handle as u64, |mux| {
            mux.audio_handles()
                .into_iter()
                .map(|h: AudioStreamHandle| i64::from(h.raw()))
                .collect::<Vec<i64>>()
        }) else {
            closed(env);
            return JObject::null().into();
        };
        let arr = match env.new_long_array(raws.len() as i32) {
            Ok(a) => a,
            Err(_) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    "failed to allocate long[] result",
                );
                return JObject::null().into();
            }
        };
        if env.set_long_array_region(&arr, 0, &raws).is_err() {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                "failed to write long[] result",
            );
            return JObject::null().into();
        }
        arr
    })
}

/// `nKlvHandles(handle)` → `long[]` of all KLV stream handles (packed `u32`
/// raws widened to `jlong`), in declaration order. On a closed/absent handle
/// throws `IllegalStateException`; on a JNI alloc/write failure throws
/// `RuntimeException`. Mirrors `nDataHandles`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nKlvHandles<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JLongArray<'local> {
    crate::panic::jni_catch(&mut env, JObject::null().into(), |env| {
        let Some(raws) = REGISTRY.with(handle as u64, |mux| {
            mux.klv_handles()
                .into_iter()
                .map(|h: KlvStreamHandle| i64::from(h.raw()))
                .collect::<Vec<i64>>()
        }) else {
            closed(env);
            return JObject::null().into();
        };
        let arr = match env.new_long_array(raws.len() as i32) {
            Ok(a) => a,
            Err(_) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    "failed to allocate long[] result",
                );
                return JObject::null().into();
            }
        };
        if env.set_long_array_region(&arr, 0, &raws).is_err() {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                "failed to write long[] result",
            );
            return JObject::null().into();
        }
        arr
    })
}

/// `nSubtitleHandles(handle)` → `long[]` of all subtitle stream handles (packed `u32`
/// raws widened to `jlong`), in declaration order. On a closed/absent handle
/// throws `IllegalStateException`; on a JNI alloc/write failure throws
/// `RuntimeException`. Mirrors `nDataHandles`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nSubtitleHandles<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JLongArray<'local> {
    crate::panic::jni_catch(&mut env, JObject::null().into(), |env| {
        let Some(raws) = REGISTRY.with(handle as u64, |mux| {
            mux.subtitle_handles()
                .into_iter()
                .map(|h: SubtitleStreamHandle| i64::from(h.raw()))
                .collect::<Vec<i64>>()
        }) else {
            closed(env);
            return JObject::null().into();
        };
        let arr = match env.new_long_array(raws.len() as i32) {
            Ok(a) => a,
            Err(_) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    "failed to allocate long[] result",
                );
                return JObject::null().into();
            }
        };
        if env.set_long_array_region(&arr, 0, &raws).is_err() {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                "failed to write long[] result",
            );
            return JObject::null().into();
        }
        arr
    })
}

/// `nPull(handle, out)` — drain whole TS packets into the caller's `byte[]`,
/// returning the number of bytes written (a multiple of 188; `0` if `out` is
/// under 188 or the queue is empty).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPull<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    out: JByteArray<'local>,
) -> jint {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let out_len = match env.get_array_length(&out) {
            Ok(l) => l as usize,
            Err(_) => {
                let _ = env.throw_new("java/lang/RuntimeException", "failed to read byte[] length");
                return 0;
            }
        };
        let mut scratch = vec![0u8; out_len];
        let Some(n) = REGISTRY.with_poisoning(handle as u64, |mux| mux.pull(&mut scratch)) else {
            closed(env);
            return 0;
        };
        if n == 0 {
            return 0;
        }
        // jbyte = i8; the byte slice has identical layout. Write the first `n`
        // bytes back into the caller's array.
        // SAFETY: `scratch[..n]` is a live, initialized `[u8]` of length `n`; the
        // i8 view aliases the same bytes (identical size/alignment) and is only
        // read by `set_byte_array_region`.
        let i8_view = unsafe { core::slice::from_raw_parts(scratch.as_ptr() as *const i8, n) };
        if env.set_byte_array_region(&out, 0, i8_view).is_err() {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                "failed to write byte[] result",
            );
            return 0;
        }
        n as jint
    })
}

/// `nPending(handle)` — TS packets currently queued.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nPending<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY.with(handle as u64, |mux| mux.pending_packets() as jlong) {
            Some(v) => v,
            None => {
                closed(env);
                0
            }
        }
    })
}

/// `nCapacity(handle)` — configured queue capacity in TS packets.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nCapacity<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY.with(handle as u64, |mux| mux.capacity_packets() as jlong) {
            Some(v) => v,
            None => {
                closed(env);
                0
            }
        }
    })
}

/// `nClose(handle)` — take + drop the registered [`Muxer`]. Atomic + idempotent
/// via `REGISTRY.close`, so a double `close()` is UAF/double-free-safe. The
/// muxer's teardown is a plain drop (no flush/finalize).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_mpegts_Muxer_nClose<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // The winning close gets the muxer back; it has no extra teardown, so just
        // let it drop here.
        let _ = REGISTRY.close(handle as u64);
    })
}

/// Construct a `MispTimestamp` from the JNI scalar fields (kind ordinal 0/1,
/// timeStatus as low byte, value as unsigned reinterpret). Returns `None`
/// (with a `MuxException(INVALID_USAGE)` thrown) when `kind` is out of range.
fn build_misp(
    env: &mut JNIEnv,
    kind: jint,
    time_status: jint,
    value: jlong,
) -> Option<tst_core::codec::misp_time::MispTimestamp> {
    use tst_core::codec::misp_time::MispTimestamp;
    let ts = time_status as u8;
    let v = value as u64;
    match kind {
        0 => Some(MispTimestamp::micros(v, ts)),
        1 => Some(MispTimestamp::nanos(v, ts)),
        _ => {
            throw_mux(
                env,
                BindingErrorKind::InvalidUsage,
                "misp kind must be 0 (micro) or 1 (nano)",
            );
            None
        }
    }
}

/// Read a Java `byte[]`, throwing `MuxException(INTERNAL)` on failure.
/// Returns `None` when the read fails (exception already thrown); callers
/// should return early in that case.
fn read_mux_bytes(env: &mut JNIEnv, arr: &JByteArray) -> Option<Vec<u8>> {
    match env.convert_byte_array(arr) {
        Ok(b) => Some(b),
        Err(_) => {
            throw_mux(
                env,
                BindingErrorKind::Internal,
                "failed to read byte[] argument",
            );
            None
        }
    }
}

/// Lease the muxer handle, run `op`, and map the outcome to thrown exceptions.
fn with_mux_push(
    env: &mut JNIEnv,
    handle: jlong,
    op: impl FnOnce(&mut Muxer) -> Result<(), MuxError>,
) {
    match REGISTRY.with_poisoning(handle as u64, op) {
        Some(Ok(())) => {}
        Some(Err(e)) => throw_mux_error(env, &e),
        None => closed(env),
    }
}

/// Throw `IllegalStateException` for a leased call that found a
/// closed/absent handle — the native-side enforcement of the Java
/// `ensureOpen()` contract.
fn closed(env: &mut JNIEnv) {
    crate::error::throw_closed(env, "Muxer");
}

/// Map a `MuxError` to a thrown `org.tstrans.MuxException`, mirroring tst-py's
/// `mux_error_to_pyerr` (route via the 5-variant `MuxErrorKind`). Each
/// inline literal is what the error-mapping ratchet greps for.
pub(crate) fn throw_mux_error(env: &mut JNIEnv, e: &MuxError) {
    // A2's classifier, not the coarse `MuxErrorKind` bucket: `InvalidNal`,
    // `KlvTooLarge`, `InvalidAv1Obu` and `MispTime` now get their own members
    // (all four were `INPUT_MALFORMED`); everything else keeps its bucket.
    crate::error::throw_mux(env, kind_of_mux(e), &e.to_string());
}

/// Read a Java `int[]` into a `Vec<i32>`, or throw INTERNAL + return `None` on a
/// JNI read failure.
fn read_int_array(env: &mut JNIEnv, arr: &JIntArray) -> Option<Vec<i32>> {
    let len = match env.get_array_length(arr) {
        Ok(l) => l as usize,
        Err(_) => {
            throw_mux(
                env,
                BindingErrorKind::Internal,
                "failed to read int[] length",
            );
            return None;
        }
    };
    let mut v = vec![0i32; len];
    if env.get_int_array_region(arr, 0, &mut v).is_err() {
        throw_mux(
            env,
            BindingErrorKind::Internal,
            "failed to read int[] region",
        );
        return None;
    }
    Some(v)
}

/// Read a Java `boolean[]` into a `Vec<u8>` (`jboolean` = `u8`; `!= 0` is
/// true), or throw INTERNAL + return `None` on a JNI read failure.
fn read_boolean_array(env: &mut JNIEnv, arr: &JBooleanArray) -> Option<Vec<u8>> {
    let len = match env.get_array_length(arr) {
        Ok(l) => l as usize,
        Err(_) => {
            throw_mux(
                env,
                BindingErrorKind::Internal,
                "failed to read boolean[] length",
            );
            return None;
        }
    };
    let mut v = vec![0u8; len];
    if env.get_boolean_array_region(arr, 0, &mut v).is_err() {
        throw_mux(
            env,
            BindingErrorKind::Internal,
            "failed to read boolean[] region",
        );
        return None;
    }
    Some(v)
}

/// Video codec ordinal (`stream_codecs[i]` when kind=video) → [`VideoCodec`].
/// Matches the Java `VideoCodec` enum declaration order. Throws CONFIG_INVALID
/// and returns `None` on any ordinal outside the valid range (0-3); this
/// catches enum drift across binding versions.
fn video_codec(env: &mut JNIEnv, ordinal: i32) -> Option<VideoCodec> {
    match ordinal {
        0 => Some(VideoCodec::H264),
        1 => Some(VideoCodec::H265),
        2 => Some(VideoCodec::H266),
        3 => Some(VideoCodec::Av1),
        other => {
            throw_mux(
                env,
                BindingErrorKind::ConfigInvalid,
                &format!("unknown VideoCodec ordinal {other}"),
            );
            None
        }
    }
}

/// Audio codec ordinal (`stream_codecs[i]` when kind=audio) → [`AudioCodec`].
/// Matches the Java `AudioCodec` enum declaration order. Throws CONFIG_INVALID
/// and returns `None` on any ordinal outside the valid range (0-3).
fn audio_codec(env: &mut JNIEnv, ordinal: i32) -> Option<AudioCodec> {
    match ordinal {
        0 => Some(AudioCodec::Mp2),
        1 => Some(AudioCodec::Aac),
        2 => Some(AudioCodec::AacLatm),
        3 => Some(AudioCodec::Ac3),
        other => {
            throw_mux(
                env,
                BindingErrorKind::ConfigInvalid,
                &format!("unknown AudioCodec ordinal {other}"),
            );
            None
        }
    }
}

/// Split a concatenated PMT descriptor loop into individual TLV descriptors
/// (tag, length, payload). `None` on a truncated/overrunning length field.
fn split_descriptor_tlvs(blob: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < blob.len() {
        let len = *blob.get(off + 1)? as usize;
        let end = off + 2 + len;
        out.push(blob.get(off..end)?.to_vec());
        off = end;
    }
    Some(out)
}

/// KLV stream-type ordinal (`stream_type_codes[i]` when kind=klv) →
/// [`KlvStreamType`]. Matches the Java `KlvStreamType` enum declaration order:
/// 0 = SynchronousMetadata, 1 = PrivateData. Throws CONFIG_INVALID and returns
/// `None` on ordinals outside 0-1 (ordinal 1 IS valid PrivateData — only
/// truly out-of-range values indicate enum drift).
fn klv_type(env: &mut JNIEnv, ordinal: i32) -> Option<KlvStreamType> {
    match ordinal {
        0 => Some(KlvStreamType::SynchronousMetadata),
        1 => Some(KlvStreamType::PrivateData),
        other => {
            throw_mux(
                env,
                BindingErrorKind::ConfigInvalid,
                &format!("unknown KlvStreamType ordinal {other}"),
            );
            None
        }
    }
}

/// `av1Carriage` scalar ordinal → [`Av1CarriageMode`]. Matches the Java
/// `Av1CarriageMode` enum declaration order: 0 = Mpeg2TsBinding,
/// 1 = InteropRawObu. Throws CONFIG_INVALID and returns `None` on ordinals
/// outside 0-1; mirrors the demux path hardened in commit 38d51690.
fn av1_mode(env: &mut JNIEnv, ordinal: i32) -> Option<Av1CarriageMode> {
    match ordinal {
        0 => Some(Av1CarriageMode::Mpeg2TsBinding),
        1 => Some(Av1CarriageMode::InteropRawObu),
        other => {
            throw_mux(
                env,
                BindingErrorKind::ConfigInvalid,
                &format!("unknown Av1CarriageMode ordinal {other}"),
            );
            None
        }
    }
}
