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
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::codec::av1::{
    Av1FrameHeaderLight, Av1ObuStream, Av1SequenceHeader, parse_frame_header_light,
    parse_obu_stream, parse_sequence_header,
};
use tst_core::codec::{Obu, ObuExtension};

use crate::codec::shared::{build_chroma_format, build_color_info, build_rational};
use crate::error::map_codec_parse_error;
use crate::jutil::{read_byte_buffer, wrap_heap_byte_buffer};

/// Build a Java `Av1SequenceHeader` via its `Builder` (13 fields). Nullable
/// `colorInfo`/`frameRate` are set only when present. Returns `Err(())` if any
/// JNI call fails (a pending Java exception is left).
fn build_sequence_header<'local>(
    env: &mut JNIEnv<'local>,
    seq: &Av1SequenceHeader,
) -> Result<JObject<'local>, ()> {
    // 13 fields + ColorInfo/Rational/ChromaFormat constants + builder + raw
    // buffer + scratch; 32 slots safely covers the worst case.
    env.ensure_local_capacity(32).map_err(|_| ())?;

    let b = env
        .new_object("org/tstrans/codec/Av1SequenceHeader$Builder", "()V", &[])
        .map_err(|_| ())?;

    let set_int = |env: &mut JNIEnv<'local>, name: &str, v: i32| -> Result<(), ()> {
        env.call_method(
            &b,
            name,
            "(I)Lorg/tstrans/codec/Av1SequenceHeader$Builder;",
            &[JValue::Int(v)],
        )
        .map_err(|_| ())?;
        Ok(())
    };
    let set_long = |env: &mut JNIEnv<'local>, name: &str, v: i64| -> Result<(), ()> {
        env.call_method(
            &b,
            name,
            "(J)Lorg/tstrans/codec/Av1SequenceHeader$Builder;",
            &[JValue::Long(v)],
        )
        .map_err(|_| ())?;
        Ok(())
    };
    let set_bool = |env: &mut JNIEnv<'local>, name: &str, v: bool| -> Result<(), ()> {
        env.call_method(
            &b,
            name,
            "(Z)Lorg/tstrans/codec/Av1SequenceHeader$Builder;",
            &[JValue::Bool(u8::from(v))],
        )
        .map_err(|_| ())?;
        Ok(())
    };

    set_int(env, "profile", i32::from(seq.profile))?;
    set_int(env, "level", i32::from(seq.level))?;
    set_int(env, "tier", i32::from(seq.tier))?;
    set_long(env, "maxFrameWidth", i64::from(seq.max_frame_width))?;
    set_long(env, "maxFrameHeight", i64::from(seq.max_frame_height))?;
    set_int(env, "bitDepth", i32::from(seq.bit_depth))?;
    set_bool(env, "monochrome", seq.monochrome)?;

    {
        let cf = build_chroma_format(env, seq.chroma_format)?;
        env.call_method(
            &b,
            "chromaFormat",
            "(Lorg/tstrans/codec/ChromaFormat;)Lorg/tstrans/codec/Av1SequenceHeader$Builder;",
            &[JValue::Object(&cf)],
        )
        .map_err(|_| ())?;
    }

    set_bool(env, "stillPicture", seq.still_picture)?;
    set_bool(
        env,
        "reducedStillPictureHeader",
        seq.reduced_still_picture_header,
    )?;

    if let Some(ref c) = seq.color_info {
        let jc = build_color_info(env, c)?;
        env.call_method(
            &b,
            "colorInfo",
            "(Lorg/tstrans/codec/ColorInfo;)Lorg/tstrans/codec/Av1SequenceHeader$Builder;",
            &[JValue::Object(&jc)],
        )
        .map_err(|_| ())?;
    }

    if let Some(ref r) = seq.frame_rate {
        let jr = build_rational(env, r)?;
        env.call_method(
            &b,
            "frameRate",
            "(Lorg/tstrans/codec/Rational;)Lorg/tstrans/codec/Av1SequenceHeader$Builder;",
            &[JValue::Object(&jr)],
        )
        .map_err(|_| ())?;
    }

    {
        let buf = wrap_heap_byte_buffer(env, &seq.raw)?;
        env.call_method(
            &b,
            "raw",
            "(Ljava/nio/ByteBuffer;)Lorg/tstrans/codec/Av1SequenceHeader$Builder;",
            &[JValue::Object(&buf)],
        )
        .map_err(|_| ())?;
    }

    env.call_method(&b, "build", "()Lorg/tstrans/codec/Av1SequenceHeader;", &[])
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build a Java `Av1FrameHeaderLight` record directly. `frame_size` is mapped
/// to a nullable nested `Av1FrameHeaderLight.FrameSize` (currently always
/// `None`, but the mapping is faithful to the Rust `Option<(u32, u32)>`).
fn build_frame_header<'local>(
    env: &mut JNIEnv<'local>,
    fh: &Av1FrameHeaderLight,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;

    let frame_size = match fh.frame_size {
        Some((w, h)) => env
            .new_object(
                "org/tstrans/codec/Av1FrameHeaderLight$FrameSize",
                "(JJ)V",
                &[JValue::Long(i64::from(w)), JValue::Long(i64::from(h))],
            )
            .map_err(|_| ())?,
        None => JObject::null(),
    };
    let buf = wrap_heap_byte_buffer(env, &fh.raw)?;
    env.new_object(
        "org/tstrans/codec/Av1FrameHeaderLight",
        "(IZZLorg/tstrans/codec/Av1FrameHeaderLight$FrameSize;Ljava/nio/ByteBuffer;)V",
        &[
            JValue::Int(i32::from(fh.frame_type)),
            JValue::Bool(u8::from(fh.show_frame)),
            JValue::Bool(u8::from(fh.show_existing_frame)),
            JValue::Object(&frame_size),
            JValue::Object(&buf),
        ],
    )
    .map_err(|_| ())
}

/// Read one Java `org.tstrans.codec.Obu` into the Rust demux `Obu`. Reads
/// `obuType` + nullable `ObuExtension(temporalId, spatialId)` + the payload
/// `ByteBuffer`. Reverse of `shared::build_obu`. Errors propagate as
/// `jni::errors::Error` (a pending Java exception may be left).
fn read_obu(env: &mut JNIEnv, item: &JObject) -> jni::errors::Result<Obu> {
    let obu_type_raw = env.call_method(item, "obuType", "()I", &[])?.i()?;
    let obu_type = crate::jutil::checked_u8(env, obu_type_raw as i64, "obuType")?;

    let ext_obj = env
        .call_method(item, "extension", "()Lorg/tstrans/codec/ObuExtension;", &[])?
        .l()?;
    let extension = if ext_obj.is_null() {
        None
    } else {
        let temporal_id_raw = env.call_method(&ext_obj, "temporalId", "()I", &[])?.i()?;
        let temporal_id = crate::jutil::checked_u8(env, temporal_id_raw as i64, "temporalId")?;
        let spatial_id_raw = env.call_method(&ext_obj, "spatialId", "()I", &[])?.i()?;
        let spatial_id = crate::jutil::checked_u8(env, spatial_id_raw as i64, "spatialId")?;
        Some(ObuExtension {
            temporal_id,
            spatial_id,
        })
    };

    let payload_buf = env
        .call_method(item, "payload", "()Ljava/nio/ByteBuffer;", &[])?
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
            Ok(seq) => match build_sequence_header(env, &seq) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
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
        let raw_buf = match env.call_method(&seq, "raw", "()Ljava/nio/ByteBuffer;", &[]) {
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
            Ok(fh) => match build_frame_header(env, &fh) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
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
        let size = match env.call_method(&obus, "size", "()I", &[]) {
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
                    .call_method(&obus, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                    .l()?;
                read_obu(inner, &item)
            });
            match parsed {
                Ok(o) => rust_obus.push(o),
                Err(_) => return JObject::null().into_raw(),
            }
        }

        let stream = parse_obu_stream(&rust_obus);
        match build_obu_stream(env, &stream) {
            Ok(obj) => obj.into_raw(),
            Err(()) => JObject::null().into_raw(),
        }
    }
}

/// Build a Java
/// `Av1ObuStream(List<Av1SequenceHeader>, List<Av1FrameHeaderLight>, List<UnparseableObu>)`.
/// Each list is a `java.util.ArrayList`; entries are added inside a per-entry
/// local frame so the element refs are reclaimed each iteration.
fn build_obu_stream<'local>(
    env: &mut JNIEnv<'local>,
    stream: &Av1ObuStream,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(16).map_err(|_| ())?;

    let seq_list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for seq in &stream.sequence_headers {
        env.with_local_frame(48, |inner| {
            let val = build_sequence_header(inner, seq)
                .map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &seq_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    let frame_list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for fh in &stream.frame_headers {
        env.with_local_frame(16, |inner| {
            let val =
                build_frame_header(inner, fh).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &frame_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    let unparseable_list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for (obu_type, err) in &stream.unparseable {
        env.with_local_frame(16, |inner| {
            let msg = inner.new_string(format!("{err}"))?;
            let val = inner.new_object(
                "org/tstrans/codec/Av1ObuStream$UnparseableObu",
                "(ILjava/lang/String;)V",
                &[JValue::Int(i32::from(*obu_type)), JValue::Object(&msg)],
            )?;
            inner.call_method(
                &unparseable_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    env.new_object(
        "org/tstrans/codec/Av1ObuStream",
        "(Ljava/util/List;Ljava/util/List;Ljava/util/List;)V",
        &[
            JValue::Object(&seq_list),
            JValue::Object(&frame_list),
            JValue::Object(&unparseable_list),
        ],
    )
    .map_err(|_| ())
}
