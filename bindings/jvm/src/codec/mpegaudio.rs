//! JNI surface for `org.tstrans.codec.Codec`'s MPEG audio parser entry points.
//!
//! Two natives, each mirroring a tst-py `parse_mpeg2_audio_frames*` free
//! function:
//!
//! - `nParseMpeg2AudioFrames(byte[]) -> List<Mpeg2AudioFrame>`
//!   → `tst_core::codec::mpegaudio::frames` (STRICT — the first `Err` item
//!   throws `CodecParseException` and returns null, mirroring
//!   `parse_mpeg2_audio_frames_py`).
//! - `nParseMpeg2AudioFramesWithResync(byte[]) -> List<Mpeg2AudioFrame>`
//!   → `frames_with_resync` (BEST-EFFORT — never throws; `Err` items are
//!   silently dropped, mirroring `parse_mpeg2_audio_frames_with_resync_py`'s
//!   `.filter_map(|res| res.ok())`).
//!
//! The strict native's Java wrapper on `Codec` is declared `throws
//! CodecParseException`; the resync wrapper has no `throws`.
//!
//! The `Mpeg2AudioFrame` record is built by the generated
//! `build_frame_owned`, which the demux audio-retype path reuses to surface
//! typed frames on `Sample` payloads.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::codec::mpegaudio::{
    ChannelMode, FrameOwned, Layer, Version, frames, frames_with_resync,
};

use crate::error::map_codec_parse_error;
use crate::generated::codec::build_frame_owned;

/// Build the Java `org.tstrans.codec.Layer` enum constant.
pub(crate) fn build_layer<'local>(
    env: &mut JNIEnv<'local>,
    l: Layer,
) -> Result<JObject<'local>, ()> {
    let name = match l {
        Layer::I => "I",
        Layer::II => "II",
        Layer::III => "III",
    };
    env.get_static_field("org/tstrans/codec/Layer", name, "Lorg/tstrans/codec/Layer;")
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build the Java `org.tstrans.codec.Version` enum constant.
pub(crate) fn build_version<'local>(
    env: &mut JNIEnv<'local>,
    v: Version,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        Version::Mpeg1 => "MPEG1",
        Version::Mpeg2 => "MPEG2",
        Version::Mpeg2_5 => "MPEG2_5",
    };
    env.get_static_field(
        "org/tstrans/codec/Version",
        name,
        "Lorg/tstrans/codec/Version;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build the Java `org.tstrans.codec.ChannelMode` enum constant.
pub(crate) fn build_channel_mode<'local>(
    env: &mut JNIEnv<'local>,
    m: ChannelMode,
) -> Result<JObject<'local>, ()> {
    let name = match m {
        ChannelMode::Stereo => "STEREO",
        ChannelMode::JointStereo => "JOINT_STEREO",
        ChannelMode::DualChannel => "DUAL_CHANNEL",
        ChannelMode::Mono => "MONO",
    };
    env.get_static_field(
        "org/tstrans/codec/ChannelMode",
        name,
        "Lorg/tstrans/codec/ChannelMode;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build a `java.util.ArrayList<Mpeg2AudioFrame>` from owned frames; each frame
/// is constructed inside a per-element local frame so its refs are reclaimed.
fn build_frame_list<'local>(
    env: &mut JNIEnv<'local>,
    frames: &[FrameOwned],
) -> Result<JObject<'local>, ()> {
    let list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for f in frames {
        env.with_local_frame(24, |inner| {
            let val = build_frame_owned(inner, f)?;
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
    fn Java_org_tstrans_codec_Codec_nParseMpeg2AudioFrames<'local>(
        env,
        _class,
        bytes: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let buf = match env.convert_byte_array(&bytes) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };

        // STRICT: the first Err throws and returns null (mirrors
        // parse_mpeg2_audio_frames_py).
        let owned: Result<Vec<FrameOwned>, _> =
            frames(&buf).map(|res| res.map(|f| f.to_owned())).collect();
        let owned = match owned {
            Ok(v) => v,
            Err(e) => {
                map_codec_parse_error(env, &e, "mpeg2audio");
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
    fn Java_org_tstrans_codec_Codec_nParseMpeg2AudioFramesWithResync<'local>(
        env,
        _class,
        bytes: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let buf = match env.convert_byte_array(&bytes) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };

        // BEST-EFFORT: Err items are silently dropped (mirrors
        // parse_mpeg2_audio_frames_with_resync_py's `.filter_map(|res| res.ok())`).
        let owned: Vec<FrameOwned> = frames_with_resync(&buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect();

        match build_frame_list(env, &owned) {
            Ok(obj) => obj.into_raw(),
            Err(_) => JObject::null().into_raw(),
        }
    }
}
