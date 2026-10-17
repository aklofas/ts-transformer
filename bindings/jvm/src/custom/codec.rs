//! The hand-written parts of the generated codec builders
//! (`generated/codec.rs`): the three fields whose Java type is not the one
//! their Rust type implies.
//!
//! The generator calls these by name: `build_<record>_<field>`.

use jni::JNIEnv;
use jni::objects::{JObject, JValue};
use tst_core::codec::CodecParseError;

/// `frame_num` is a Java `Integer`, null when no SPS context was supplied.
pub(crate) fn build_h264_slice_header_light_frame_num<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<u32>,
) -> jni::errors::Result<JObject<'l>> {
    match v {
        // At most 16 bits wide, so it always fits an `int`.
        Some(n) => env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(*n as i32)]),
        None => Ok(JObject::null()),
    }
}

/// `frame_size` is a nullable nested `Av1FrameHeaderLight.FrameSize`.
pub(crate) fn build_av1_frame_header_light_frame_size<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<(u32, u32)>,
) -> jni::errors::Result<JObject<'l>> {
    match v {
        Some((w, h)) => env.new_object(
            "org/tstrans/codec/Av1FrameHeaderLight$FrameSize",
            "(JJ)V",
            &[JValue::Long(i64::from(*w)), JValue::Long(i64::from(*h))],
        ),
        None => Ok(JObject::null()),
    }
}

/// The OBUs a walk could not parse, as a list of
/// `Av1ObuStream.UnparseableObu(obuType, error)`.
pub(crate) fn build_av1_obu_stream_unparseable<'l>(
    env: &mut JNIEnv<'l>,
    v: &[(u8, CodecParseError)],
) -> jni::errors::Result<JObject<'l>> {
    let list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for (obu_type, err) in v {
        env.with_local_frame(16, |inner| {
            let msg = inner.new_string(format!("{err}"))?;
            let val = inner.new_object(
                "org/tstrans/codec/Av1ObuStream$UnparseableObu",
                "(ILjava/lang/String;)V",
                &[JValue::Int(i32::from(*obu_type)), JValue::Object(&msg)],
            )?;
            inner.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })?;
    }
    Ok(list)
}
