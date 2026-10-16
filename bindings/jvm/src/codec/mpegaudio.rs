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
//! [`build_mpeg2_audio_frame`] is `pub(crate)` because the demux audio-retype
//! task reuses it to surface typed `Mpeg2AudioFrame`s on `Sample` payloads.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::codec::mpegaudio::{
    ChannelMode, FrameOwned, Layer, Version, frames, frames_with_resync,
};

use crate::error::map_codec_parse_error;
use crate::jutil::wrap_heap_byte_buffer;

/// Build the Java `org.tstrans.codec.Layer` enum constant.
fn build_layer<'local>(env: &mut JNIEnv<'local>, l: Layer) -> Result<JObject<'local>, ()> {
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
fn build_version<'local>(env: &mut JNIEnv<'local>, v: Version) -> Result<JObject<'local>, ()> {
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
fn build_channel_mode<'local>(
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

/// Build one Java `org.tstrans.codec.Mpeg2AudioFrame` record from an owned
/// Rust frame.
///
/// `pub(crate)` so the demux audio-retype task can reuse it to surface typed
/// `Mpeg2AudioFrame`s on `Sample` payloads. Returns `Err(())` (leaving a
/// pending Java exception) on any JNI failure.
pub(crate) fn build_mpeg2_audio_frame<'local>(
    env: &mut JNIEnv<'local>,
    f: &FrameOwned,
) -> Result<JObject<'local>, ()> {
    // layer + version + channelMode enums + 2 ByteBuffers (+ their scratch
    // arrays) + the record itself; 16 slots safely covers the worst case.
    env.ensure_local_capacity(16).map_err(|_| ())?;

    let layer = build_layer(env, f.layer)?;
    let version = build_version(env, f.version)?;
    let channel_mode = build_channel_mode(env, f.channel_mode)?;
    let raw_header = wrap_heap_byte_buffer(env, &f.raw_header)?;
    // `payload` = full frame bytes (header + body), sourced from the owned
    // `body` slice — matches tst-py's `payload` getter.
    let payload = wrap_heap_byte_buffer(env, &f.body)?;

    env.new_object(
        "org/tstrans/codec/Mpeg2AudioFrame",
        "(Lorg/tstrans/codec/Layer;Lorg/tstrans/codec/Version;JJLorg/tstrans/codec/ChannelMode;IJIZLjava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)V",
        &[
            JValue::Object(&layer),
            JValue::Object(&version),
            JValue::Long(i64::from(f.bitrate_kbps)),
            JValue::Long(i64::from(f.sample_rate_hz)),
            JValue::Object(&channel_mode),
            JValue::Int(i32::from(f.channels)),
            JValue::Long(i64::from(f.frame_length_bytes)),
            JValue::Int(i32::from(f.samples_per_frame)),
            JValue::Bool(u8::from(f.has_crc)),
            JValue::Object(&raw_header),
            JValue::Object(&payload),
        ],
    )
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
            Err(()) => JObject::null().into_raw(),
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
            Err(()) => JObject::null().into_raw(),
        }
    }
}
