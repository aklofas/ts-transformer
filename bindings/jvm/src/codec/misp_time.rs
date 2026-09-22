//! JNI surface for `org.tstrans.codec.Codec.nExtractMispTimestamp`.
//!
//! One native, mirroring Python's `extract_misp_timestamp`:
//!
//! - `Codec.nExtractMispTimestamp(byte[] au, int codecOrdinal) -> MispTimestamp`
//!   → `tst_core::codec::misp_time::extract`
//!   Called via `MispTimestamp.extract` → `Codec.extractMispTimestamp` (package-private).
//!
//! `Ok(None)` → returns Java `null` (MISP SEI absent; not an error).
//! `Ok(Some(ts))` → constructs a Java `MispTimestamp` record directly.
//! `Err(e)` → throws `CodecParseException(ENGINE_ERROR)` via
//! [`crate::error::throw_codec`] (malformed MISP SEI; corresponds to
//! Python's `PyValueError` for the same error conditions).
//!
//! The Java `VideoCodec` ordinal (0=H264, 1=H265, 2=H266, 3=AV1) is mapped
//! to the mux-side `VideoCodec` via the same table as `video_codec()` in
//! `mpegts/muxer.rs` — ordinals outside 0–3 surface as `CodecParseException`
//! (an invalid codec is a caller bug, surfaced as a parse error).

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};
use jni::sys::{jint, jobject};
use tst_core::codec::misp_time::{MispTimeKind, MispTimestamp, extract};
use tst_core::mpegts::mux::VideoCodec;

use crate::error::{CodecErrFields, throw_codec};
use tst_pipeline::binding::BindingErrorKind;

/// Map the Java `VideoCodec` ordinal (0-3) to the mux-side `VideoCodec`.
/// Returns `None` and throws `CodecParseException(ENGINE_ERROR)` for
/// ordinals outside the valid range.
fn java_video_codec_ordinal_to_rust(env: &mut JNIEnv, ordinal: jint) -> Option<VideoCodec> {
    match ordinal {
        0 => Some(VideoCodec::H264),
        1 => Some(VideoCodec::H265),
        2 => Some(VideoCodec::H266),
        3 => Some(VideoCodec::Av1),
        _ => {
            throw_codec(
                env,
                BindingErrorKind::CodecEngineError,
                "misp_time",
                &CodecErrFields::default(),
                &format!("unknown VideoCodec ordinal {ordinal}"),
            );
            None
        }
    }
}

/// Build the Java `org.tstrans.codec.MispTimeKind` enum constant.
/// Throws `CodecParseException(ENGINE_ERROR)` and returns `Err(())` for
/// unrecognised variants (non_exhaustive guard); never returns silently.
fn build_kind<'local>(env: &mut JNIEnv<'local>, kind: MispTimeKind) -> Result<JObject<'local>, ()> {
    let name = match kind {
        MispTimeKind::Micro => "MICRO",
        MispTimeKind::Nano => "NANO",
        // MispTimeKind is non_exhaustive; an unrecognised variant is an
        // ENGINE_ERROR (not silent null — throw so the caller sees an exception).
        _ => {
            throw_codec(
                env,
                BindingErrorKind::CodecEngineError,
                "misp_time",
                &CodecErrFields::default(),
                "unknown MispTimeKind variant crossing JNI",
            );
            return Err(());
        }
    };
    env.get_static_field(
        "org/tstrans/codec/MispTimeKind",
        name,
        "Lorg/tstrans/codec/MispTimeKind;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build a Java `MispTimestamp` record from a Rust `MispTimestamp`.
/// The record ctor is `(MispTimeKind, int timeStatus, long value)`.
fn build_misp_timestamp<'local>(
    env: &mut JNIEnv<'local>,
    ts: &MispTimestamp,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let kind = build_kind(env, ts.kind)?;
    env.new_object(
        "org/tstrans/codec/MispTimestamp",
        "(Lorg/tstrans/codec/MispTimeKind;IJ)V",
        &[
            JValue::Object(&kind),
            JValue::Int(i32::from(ts.time_status)),
            JValue::Long(ts.value as i64),
        ],
    )
    .map_err(|_| ())
}

/// `Codec.nExtractMispTimestamp(byte[] au, int codecOrdinal) -> MispTimestamp`
///
/// Scans an Annex-B AU for the first MISP timestamp SEI.
/// Returns null when absent; constructs a `MispTimestamp` record on
/// success; throws `CodecParseException(ENGINE_ERROR)` on a malformed SEI.
/// Called via `MispTimestamp.extract` which delegates to `Codec.extractMispTimestamp`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_codec_Codec_nExtractMispTimestamp<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    au: JByteArray<'local>,
    codec_ordinal: jint,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let bytes = match env.convert_byte_array(&au) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };
        let Some(codec) = java_video_codec_ordinal_to_rust(env, codec_ordinal) else {
            return JObject::null().into_raw();
        };
        match extract(&bytes, codec) {
            Ok(None) => JObject::null().into_raw(),
            Ok(Some(ts)) => match build_misp_timestamp(env, &ts) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
            },
            Err(e) => {
                let msg = e.to_string();
                throw_codec(
                    env,
                    BindingErrorKind::CodecEngineError,
                    "misp_time",
                    &CodecErrFields::default(),
                    &msg,
                );
                JObject::null().into_raw()
            }
        }
    })
}
