//! JNI surface for `org.tstrans.codec.Codec`'s AAC ADTS parser entry points.
//!
//! Two natives, each mirroring a tst-py `parse_aac_frames*` free function:
//!
//! - `nParseAacFrames(byte[]) -> List<AdtsFrame>`
//!   → `tst_core::codec::aac::frames` (STRICT — the first `Err` item throws
//!   `CodecParseException` and returns null, mirroring `parse_aac_frames_py`).
//! - `nParseAacFramesWithResync(byte[]) -> List<AdtsFrame>`
//!   → `frames_with_resync` (BEST-EFFORT — never throws; `Err` items are
//!   silently dropped, mirroring `parse_aac_frames_with_resync_py`'s
//!   `.filter_map(|res| res.ok())`).
//!
//! The strict native's Java wrapper on `Codec` is declared `throws
//! CodecParseException`; the resync wrapper has no `throws`.
//!
//! [`build_adts_frame`] is `pub(crate)` because the demux audio-retype task
//! reuses it to surface typed `AdtsFrame`s on `Sample` payloads.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::codec::aac::{
    AacChannelLayout, AacProfile, AdtsFrameOwned, MpegVersion, frames, frames_with_resync,
};

use crate::error::map_codec_parse_error;
use crate::jutil::wrap_heap_byte_buffer;

/// Build the Java `org.tstrans.codec.AacProfile` enum constant.
fn build_profile<'local>(env: &mut JNIEnv<'local>, p: AacProfile) -> Result<JObject<'local>, ()> {
    let name = match p {
        AacProfile::Main => "MAIN",
        AacProfile::Lc => "LC",
        AacProfile::Ssr => "SSR",
        AacProfile::LongTermPrediction => "LTP",
    };
    env.get_static_field(
        "org/tstrans/codec/AacProfile",
        name,
        "Lorg/tstrans/codec/AacProfile;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build the Java `org.tstrans.codec.MpegVersion` enum constant.
fn build_mpeg_version<'local>(
    env: &mut JNIEnv<'local>,
    v: MpegVersion,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        MpegVersion::Mpeg2 => "MPEG2",
        MpegVersion::Mpeg4 => "MPEG4",
    };
    env.get_static_field(
        "org/tstrans/codec/MpegVersion",
        name,
        "Lorg/tstrans/codec/MpegVersion;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build the Java `org.tstrans.codec.AacChannelLayout` record via its canonical
/// constructor `(boolean pceDefined, Integer channels)`, mirroring tst-py's
/// flattened `is_pce_defined` + nullable `channels` shape.
fn build_channel_layout<'local>(
    env: &mut JNIEnv<'local>,
    layout: AacChannelLayout,
) -> Result<JObject<'local>, ()> {
    // Channels(n) → (false, Integer(n)); PceDefined (+ the non-exhaustive-enum
    // catch-all) → (true, null) — same fallback as tst-py's `From` impl.
    let (pce_defined, channels) = match layout {
        AacChannelLayout::Channels(n) => (
            false,
            env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(i32::from(n))])
                .map_err(|_| ())?,
        ),
        _ => (true, JObject::null()),
    };
    env.new_object(
        "org/tstrans/codec/AacChannelLayout",
        "(ZLjava/lang/Integer;)V",
        &[
            JValue::Bool(u8::from(pce_defined)),
            JValue::Object(&channels),
        ],
    )
    .map_err(|_| ())
}

/// Build one Java `org.tstrans.codec.AdtsFrame` record from an owned Rust frame.
///
/// `pub(crate)` so the demux audio-retype task can reuse it to surface typed
/// `AdtsFrame`s on `Sample` payloads. Returns `Err(())` (leaving a pending Java
/// exception) on any JNI failure.
pub(crate) fn build_adts_frame<'local>(
    env: &mut JNIEnv<'local>,
    f: &AdtsFrameOwned,
) -> Result<JObject<'local>, ()> {
    // profile + mpegVersion + channelLayout + 2 ByteBuffers (+ their scratch
    // arrays) + the record itself; 16 slots safely covers the worst case.
    env.ensure_local_capacity(16).map_err(|_| ())?;

    let profile = build_profile(env, f.profile)?;
    let channel_layout = build_channel_layout(env, f.channel_layout)?;
    let mpeg_version = build_mpeg_version(env, f.mpeg_version)?;
    let raw_header = wrap_heap_byte_buffer(env, &f.raw_header)?;
    // `payload` = full frame bytes (header + body), sourced from the owned
    // `body` slice — matches tst-py's `payload` getter.
    let payload = wrap_heap_byte_buffer(env, &f.body)?;

    env.new_object(
        "org/tstrans/codec/AdtsFrame",
        "(Lorg/tstrans/codec/AacProfile;JILorg/tstrans/codec/AacChannelLayout;JIIZLorg/tstrans/codec/MpegVersion;Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)V",
        &[
            JValue::Object(&profile),
            JValue::Long(i64::from(f.sample_rate_hz)),
            JValue::Int(i32::from(f.channel_configuration)),
            JValue::Object(&channel_layout),
            JValue::Long(i64::from(f.frame_length_bytes)),
            JValue::Int(i32::from(f.samples_per_frame)),
            JValue::Int(i32::from(f.num_raw_data_blocks)),
            JValue::Bool(u8::from(f.has_crc)),
            JValue::Object(&mpeg_version),
            JValue::Object(&raw_header),
            JValue::Object(&payload),
        ],
    )
    .map_err(|_| ())
}

/// Build a `java.util.ArrayList<AdtsFrame>` from owned frames; each frame is
/// constructed inside a per-element local frame so its refs are reclaimed.
fn build_frame_list<'local>(
    env: &mut JNIEnv<'local>,
    frames: &[AdtsFrameOwned],
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

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseAacFrames<'local>(
        env,
        _class,
        bytes: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let buf = match env.convert_byte_array(&bytes) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };

        // STRICT: the first Err throws and returns null (mirrors parse_aac_frames_py).
        let owned: Result<Vec<AdtsFrameOwned>, _> =
            frames(&buf).map(|res| res.map(|f| f.to_owned())).collect();
        let owned = match owned {
            Ok(v) => v,
            Err(e) => {
                map_codec_parse_error(env, &e, "aac");
                return JObject::null().into_raw();
            }
        };

        match build_frame_list(env, &owned) {
            Ok(obj) => obj.into_raw(),
            Err(()) => JObject::null().into_raw(),
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseAacFramesWithResync<'local>(
        env,
        _class,
        bytes: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let buf = match env.convert_byte_array(&bytes) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };

        // BEST-EFFORT: Err items are silently dropped (mirrors
        // parse_aac_frames_with_resync_py's `.filter_map(|res| res.ok())`).
        let owned: Vec<AdtsFrameOwned> = frames_with_resync(&buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect();

        match build_frame_list(env, &owned) {
            Ok(obj) => obj.into_raw(),
            Err(()) => JObject::null().into_raw(),
        }
    }
}
