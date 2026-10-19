//! JNI surface for `org.tstrans.codec.Codec`'s AV1 parser entry points.
//!
//! Three natives, each mirroring a tst-py `parse_av1_*` free function:
//!
//! - `nParseAv1SequenceHeader(byte[]) -> Av1SequenceHeader`
//!   → `tst_core::codec::av1::parse_sequence_header` (fallible)
//! - `nParseAv1FrameHeaderLight(byte[], Av1SequenceHeader) -> Av1FrameHeaderLight`
//!   → `parse_frame_header_light` (fallible)
//! - `nParseAv1ObuStream(List<Obu>) -> Av1ObuStream`
//!   → `parse_obu_stream` (**INFALLIBLE** — never throws; failures are
//!   collected into `Av1ObuStream.unparseable`)
//!
//! The two fallible natives call [`crate::error::map_codec_parse_error`] (which
//! throws `CodecParseException`) and return a null object on error — their Java
//! wrappers on `Codec` are declared `throws CodecParseException` so the pending
//! exception propagates. `nParseAv1ObuStream` has no `throws` and always
//! returns the result object (matching the H.26x error-handling shape for the
//! fallible pair, and tst-py's infallible `parse_av1_obu_stream`).
//!
//! ### Frame-header SPS context
//!
//! `parse_frame_header_light` takes a **required** `&tst_core Av1SequenceHeader`
//! (not optional, unlike the H.26x slice parsers). The Java `Av1SequenceHeader`
//! record cannot hold the Rust struct, so the native re-`parse_sequence_header`s
//! the Java seq's `raw()` ByteBuffer to recover an equivalent Rust context —
//! same re-parse pattern as the H.26x modules. tst-py passes its stored inner
//! Rust seq directly; this binding reconstructs it from `raw`.

use crate::panic::jni_native;
use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::codec::av1::{parse_frame_header_light, parse_obu_stream, parse_sequence_header};
use tst_core::codec::{Obu, ObuExtension};

use crate::error::map_codec_parse_error;
use crate::generated::codec::{
    build_av1_frame_header_light, build_av1_obu_stream, build_av1_sequence_header,
};
use crate::jutil::read_byte_buffer;

/// Read one Java `org.tstrans.codec.Obu` into the Rust demux `Obu`. Reads
/// `obuType` + nullable `ObuExtension(temporalId, spatialId)` + the payload
/// `ByteBuffer`. Reverse of `shared::build_obu`. Errors propagate as
/// `jni::errors::Error` (a pending Java exception may be left).
fn read_obu(env: &mut Env, item: &JObject) -> jni::errors::Result<Obu> {
    let obu_type_raw = env
        .call_method(item, jni_str!("obuType"), jni_sig!("()I"), &[])?
        .i()?;
    let obu_type = crate::jutil::checked_u8(env, obu_type_raw as i64, "obuType")?;

    let ext_obj = env
        .call_method(
            item,
            jni_str!("extension"),
            jni_sig!("()Lorg/tstrans/codec/ObuExtension;"),
            &[],
        )?
        .l()?;
    let extension = if ext_obj.is_null() {
        None
    } else {
        let temporal_id_raw = env
            .call_method(&ext_obj, jni_str!("temporalId"), jni_sig!("()I"), &[])?
            .i()?;
        let temporal_id = crate::jutil::checked_u8(env, temporal_id_raw as i64, "temporalId")?;
        let spatial_id_raw = env
            .call_method(&ext_obj, jni_str!("spatialId"), jni_sig!("()I"), &[])?
            .i()?;
        let spatial_id = crate::jutil::checked_u8(env, spatial_id_raw as i64, "spatialId")?;
        Some(ObuExtension {
            temporal_id,
            spatial_id,
        })
    };

    let payload_buf = env
        .call_method(
            item,
            jni_str!("payload"),
            jni_sig!("()Ljava/nio/ByteBuffer;"),
            &[],
        )?
        .l()?;
    let payload = read_byte_buffer(env, &payload_buf)?;

    Ok(Obu {
        obu_type,
        extension,
        payload: payload.into(),
    })
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseAv1SequenceHeader<'local>(
        env,
        _class,
        payload: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&payload) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };
        match parse_sequence_header(&bytes) {
            Ok(seq) => match build_av1_sequence_header(env, &seq) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "av1");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseAv1FrameHeaderLight<'local>(
        env,
        _class,
        payload: JByteArray<'local>,
        seq: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&payload) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };

        // SPS context is REQUIRED. Recover the Rust seq by re-parsing the Java
        // seq's `raw()` ByteBuffer (the Java record can't hold the Rust struct).
        let raw_buf = match env.call_method(&seq, jni_str!("raw"), jni_sig!("()Ljava/nio/ByteBuffer;"), &[]) {
            Ok(v) => match v.l() {
                Ok(o) => o,
                Err(_) => return JObject::null().into_raw(),
            },
            Err(_) => return JObject::null().into_raw(),
        };
        let seq_bytes = match read_byte_buffer(env, &raw_buf) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };
        let seq_ctx = match parse_sequence_header(&seq_bytes) {
            Ok(s) => s,
            Err(e) => {
                map_codec_parse_error(env, &e, "av1");
                return JObject::null().into_raw();
            }
        };

        match parse_frame_header_light(&bytes, &seq_ctx) {
            Ok(fh) => match build_av1_frame_header_light(env, &fh) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "av1");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseAv1ObuStream<'local>(
        env,
        _class,
        obus: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        // INFALLIBLE: failures are collected into the result's `unparseable` list,
        // never thrown. Read each Java Obu inside a per-element local frame.
        let size = match env.call_method(&obus, jni_str!("size"), jni_sig!("()I"), &[]) {
            Ok(v) => match v.i() {
                Ok(n) => n,
                Err(_) => return JObject::null().into_raw(),
            },
            Err(_) => return JObject::null().into_raw(),
        };

        let mut rust_obus: Vec<Obu> = Vec::new();
        for i in 0..size {
            let parsed = env.with_local_frame(16, |inner| {
                let item = inner
                    .call_method(&obus, jni_str!("get"), jni_sig!("(I)Ljava/lang/Object;"), &[JValue::Int(i)])?
                    .l()?;
                read_obu(inner, &item)
            });
            match parsed {
                Ok(o) => rust_obus.push(o),
                Err(_) => return JObject::null().into_raw(),
            }
        }

        let stream = parse_obu_stream(&rust_obus);
        match build_av1_obu_stream(env, &stream) {
            Ok(obj) => obj.into_raw(),
            Err(_) => JObject::null().into_raw(),
        }
    }
}
