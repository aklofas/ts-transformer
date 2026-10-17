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
//! The `AdtsFrame` record is built by the generated `build_adts_frame_owned`,
//! which the demux audio-retype path reuses to surface typed frames on
//! `Sample` payloads.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::codec::aac::{
    AacChannelLayout, AacProfile, AdtsFrameOwned, MpegVersion, frames, frames_with_resync,
};

use crate::error::map_codec_parse_error;
use crate::generated::codec::build_adts_frame_owned;

/// Build the Java `org.tstrans.codec.AacProfile` enum constant.
pub(crate) fn build_profile<'local>(
    env: &mut JNIEnv<'local>,
    p: AacProfile,
) -> Result<JObject<'local>, ()> {
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
pub(crate) fn build_mpeg_version<'local>(
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
pub(crate) fn build_channel_layout<'local>(
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
            let val = build_adts_frame_owned(inner, f)?;
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
            Err(_) => JObject::null().into_raw(),
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
            Err(_) => JObject::null().into_raw(),
        }
    }
}
